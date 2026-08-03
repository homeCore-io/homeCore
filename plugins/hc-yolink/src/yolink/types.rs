use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// BDDP — Basic Downlink Data Packet (client → YoLink)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct Bddp<'a> {
    /// Current Unix timestamp in milliseconds
    pub time: u64,
    /// JSON-RPC method name, e.g. "Outlet.setState"
    pub method: &'a str,
    /// Caller-supplied message ID for correlation (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub msgid: Option<String>,
    /// Target device ID (required for device-specific methods)
    #[serde(rename = "targetDevice", skip_serializing_if = "Option::is_none")]
    pub target_device: Option<&'a str>,
    /// Per-device auth token obtained from the device list
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<&'a str>,
    /// Method-specific parameters
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

// ---------------------------------------------------------------------------
// BUDP — Basic Uplink Data Packet (YoLink → client)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct Budp {
    /// "000000" = success; any other value is an error
    pub code: String,
    /// Human-readable status description
    pub desc: Option<String>,
    /// Method-specific response payload
    pub data: Option<Value>,
}

/// A non-success BUDP, with the code kept intact.
///
/// This used to be flattened into an `anyhow!` string the moment it was
/// created, which meant every caller could only log it. The code is the useful
/// part — it is what separates "try again in a second" from "give up" from
/// "this device is dead".
#[derive(Debug, thiserror::Error)]
#[error("YoLink API error {code} — {desc}")]
pub struct ApiError {
    pub code: String,
    pub desc: String,
}

impl ApiError {
    /// "Cannot connect to the device."
    ///
    /// The hub answered us perfectly well; it could not reach the device over
    /// LoRa. Usually that is the radio channel being busy — more devices, more
    /// contention — and a retry a moment later succeeds. Sometimes it is the
    /// device being genuinely dead: flat batteries look exactly the same from
    /// here.
    ///
    /// So it is neither "an error" nor "offline" on its own. It is a maybe, and
    /// only repetition tells the two apart. See
    /// [`Bridge::get_state_retrying`](crate::bridge::Bridge).
    pub const DEVICE_UNREACHABLE: &'static str = "000201";

    /// More than six requests to one device inside a minute. This one really is
    /// a rate limit, and the fix really is to poll less often.
    pub const DEVICE_RATE_LIMITED: &'static str = "020104";

    /// Account-level throttling: "access denied due to reaching limits".
    pub const ACCOUNT_RATE_LIMITED: &'static str = "010301";

    pub fn is_device_unreachable(&self) -> bool {
        self.code == Self::DEVICE_UNREACHABLE
    }

    pub fn is_rate_limited(&self) -> bool {
        self.code == Self::DEVICE_RATE_LIMITED || self.code == Self::ACCOUNT_RATE_LIMITED
    }

    /// Worth trying again shortly: a busy radio or a throttle both clear on
    /// their own. A malformed request never will.
    pub fn is_transient(&self) -> bool {
        self.is_device_unreachable() || self.is_rate_limited()
    }
}

/// Does this error mean "the hub could not reach that device"?
pub fn is_device_unreachable(err: &anyhow::Error) -> bool {
    err.downcast_ref::<ApiError>()
        .is_some_and(ApiError::is_device_unreachable)
}

/// Is this worth retrying?
pub fn is_transient(err: &anyhow::Error) -> bool {
    err.downcast_ref::<ApiError>()
        .is_some_and(ApiError::is_transient)
}

impl Budp {
    /// Unwrap the response data, returning an [`ApiError`] if `code != "000000"`.
    pub fn into_data(self) -> anyhow::Result<Value> {
        if self.code != "000000" {
            return Err(ApiError {
                code: self.code,
                desc: self.desc.unwrap_or_default(),
            }
            .into());
        }
        Ok(self.data.unwrap_or(Value::Null))
    }
}

// ---------------------------------------------------------------------------
// DeviceInfo — entry in the device list response
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct DeviceInfo {
    #[serde(rename = "deviceId")]
    pub device_id: String,

    pub name: String,

    /// YoLink device type string, e.g. "Outlet", "DoorSensor", "THSensor"
    #[serde(rename = "type")]
    pub device_type: String,

    /// Per-device network token; required for all device-specific API calls
    pub token: String,

    /// Hub / gateway this device is paired to
    #[serde(rename = "parentDeviceId")]
    #[allow(dead_code)]
    pub parent_device_id: Option<String>,
}

// ---------------------------------------------------------------------------
// YolinkReport — parsed real-time event from the MQTT broker
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct YolinkReport {
    /// YoLink device ID
    pub device_id: String,
    /// Event type: "StatusChange", "Alert", "Report", etc.
    pub event: String,
    /// Raw event payload (device-type-specific)
    pub data: Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unreachable_code_is_recognised_through_anyhow() {
        let budp = Budp {
            code: "000201".into(),
            desc: Some("Cannot connect to the device".into()),
            data: None,
        };
        let err = budp.into_data().unwrap_err();
        assert!(is_device_unreachable(&err));
        // The message the operator sees is unchanged.
        assert_eq!(
            err.to_string(),
            "YoLink API error 000201 — Cannot connect to the device"
        );
    }

    #[test]
    fn other_error_codes_are_not_unreachable() {
        let budp = Budp {
            code: "010203".into(),
            desc: Some("Invalid data packet".into()),
            data: None,
        };
        assert!(!is_device_unreachable(&budp.into_data().unwrap_err()));
    }

    /// A busy radio and a throttle both clear on their own, so both get retried.
    #[test]
    fn transient_codes_are_retryable() {
        for code in [
            ApiError::DEVICE_UNREACHABLE,
            ApiError::DEVICE_RATE_LIMITED,
            ApiError::ACCOUNT_RATE_LIMITED,
        ] {
            let err = Budp {
                code: code.into(),
                desc: None,
                data: None,
            }
            .into_data()
            .unwrap_err();
            assert!(is_transient(&err), "{code} should be retryable");
        }
    }

    /// A malformed request will never succeed, so retrying it just wastes the
    /// radio the other devices are waiting on.
    #[test]
    fn malformed_request_is_not_retryable() {
        let err = Budp {
            code: "010203".into(),
            desc: Some("Invalid data packet".into()),
            data: None,
        }
        .into_data()
        .unwrap_err();
        assert!(!is_transient(&err));
    }

    /// The rate limits are a different thing from an unreachable device, and
    /// must not be mistaken for one — that would publish a throttled device as
    /// offline.
    #[test]
    fn rate_limits_are_not_unreachable() {
        for code in [
            ApiError::DEVICE_RATE_LIMITED,
            ApiError::ACCOUNT_RATE_LIMITED,
        ] {
            let err = Budp {
                code: code.into(),
                desc: None,
                data: None,
            }
            .into_data()
            .unwrap_err();
            assert!(!is_device_unreachable(&err), "{code} is not unreachable");
        }
    }

    #[test]
    fn success_still_unwraps_data() {
        let budp = Budp {
            code: "000000".into(),
            desc: Some("Success".into()),
            data: Some(serde_json::json!({ "state": "open" })),
        };
        assert_eq!(budp.into_data().unwrap()["state"], "open");
    }
}
