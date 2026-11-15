//! Where the broker is, as seen from inside this container.
//!
//! Core tells a runtime, and every plugin placed on it, the broker address from
//! its own `[broker].host`. That is the address the broker *listens* on, and the
//! usual value for a house with containers in it is `0.0.0.0`. Dialled from
//! here, `0.0.0.0` reaches this container, not core, so the runtime and all of
//! its plugins would fail to connect with an error that names the right port and
//! the wrong machine.
//!
//! So an unspecified address is replaced with the host the operator already
//! gave in `HOMECORE_URL`, which by definition reaches core from here.
//! `HC_RUNTIME_BROKER_HOST` overrides both, for a broker that is not on core's
//! machine at all.

use std::net::IpAddr;

/// How to turn the broker address core offered into one that works here.
#[derive(Debug, Clone)]
pub struct BrokerHost {
    /// The host part of `HOMECORE_URL`.
    core_host: String,
    /// `HC_RUNTIME_BROKER_HOST`, when the operator set it.
    pinned: Option<String>,
}

impl BrokerHost {
    pub fn new(base_url: &str, pinned: Option<String>) -> Self {
        let core_host = reqwest::Url::parse(base_url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .map(|h| h.trim_start_matches('[').trim_end_matches(']').to_string())
            .unwrap_or_default();
        Self {
            core_host,
            pinned: pinned.filter(|p| !p.trim().is_empty()),
        }
    }

    /// The address to dial for a broker core described as `offered`.
    pub fn resolve(&self, offered: &str) -> String {
        if let Some(pinned) = &self.pinned {
            return pinned.clone();
        }
        if is_unspecified(offered) && !self.core_host.is_empty() {
            return self.core_host.clone();
        }
        offered.to_string()
    }

    /// True when core's broker answers only on its own loopback, and this
    /// runtime reached core some other way. Worth a warning: unless the
    /// container shares core's network, the broker cannot be reached from here.
    pub fn loopback_only(&self, offered: &str) -> bool {
        self.pinned.is_none() && is_loopback(offered) && !is_loopback(&self.core_host)
    }

    /// A hosted plugin's config with its `[homecore] broker_host` made
    /// reachable from here. Everything else, comments included, is left as
    /// core wrote it: the operator edits this file in the app, and the copy on
    /// disk here should still read like theirs.
    pub fn rewrite_config(&self, config: &str) -> String {
        let mut out = String::with_capacity(config.len());
        let mut in_homecore = false;
        for line in config.split_inclusive('\n') {
            let trimmed = line.trim_start();
            if trimmed.starts_with('[') {
                in_homecore = trimmed.starts_with("[homecore]");
            } else if in_homecore {
                if let Some(value) = broker_host_value(trimmed) {
                    let resolved = self.resolve(&value);
                    if resolved != value {
                        let ending = if line.ends_with("\r\n") {
                            "\r\n"
                        } else if line.ends_with('\n') {
                            "\n"
                        } else {
                            ""
                        };
                        out.push_str(&format!("broker_host = \"{resolved}\"{ending}"));
                        continue;
                    }
                }
            }
            out.push_str(line);
        }
        out
    }
}

/// The quoted value of a `broker_host = "…"` line, if this is one.
fn broker_host_value(line: &str) -> Option<String> {
    let rest = line.strip_prefix("broker_host")?.trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'')?;
    let inner = &rest[1..];
    let end = inner.find(quote)?;
    Some(inner[..end].to_string())
}

fn is_unspecified(host: &str) -> bool {
    let h = host.trim().trim_start_matches('[').trim_end_matches(']');
    h.is_empty() || h.parse::<IpAddr>().is_ok_and(|ip| ip.is_unspecified())
}

fn is_loopback(host: &str) -> bool {
    let h = host.trim().trim_start_matches('[').trim_end_matches(']');
    h.eq_ignore_ascii_case("localhost") || h.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_listen_address_becomes_the_address_core_was_reached_at() {
        let b = BrokerHost::new("http://10.0.0.5:8080", None);
        assert_eq!(b.resolve("0.0.0.0"), "10.0.0.5");
        assert_eq!(b.resolve("::"), "10.0.0.5");
        assert_eq!(b.resolve(""), "10.0.0.5");
        // A real address is left alone.
        assert_eq!(b.resolve("10.0.0.9"), "10.0.0.9");
        assert_eq!(b.resolve("broker.lan"), "broker.lan");
    }

    #[test]
    fn an_ipv6_core_url_gives_a_bare_address() {
        let b = BrokerHost::new("http://[fd00::5]:8080", None);
        assert_eq!(b.resolve("0.0.0.0"), "fd00::5");
    }

    #[test]
    fn the_operator_can_pin_the_broker() {
        let b = BrokerHost::new("http://10.0.0.5:8080", Some("mqtt.lan".into()));
        assert_eq!(b.resolve("0.0.0.0"), "mqtt.lan");
        assert_eq!(b.resolve("10.0.0.9"), "mqtt.lan");
    }

    #[test]
    fn a_loopback_broker_is_flagged_only_when_core_was_reached_elsewhere() {
        assert!(BrokerHost::new("http://10.0.0.5:8080", None).loopback_only("127.0.0.1"));
        assert!(!BrokerHost::new("http://127.0.0.1:8080", None).loopback_only("127.0.0.1"));
        assert!(!BrokerHost::new("http://10.0.0.5:8080", None).loopback_only("0.0.0.0"));
    }

    #[test]
    fn only_the_homecore_broker_host_is_rewritten() {
        let b = BrokerHost::new("http://10.0.0.5:8080", None);
        let config = "# mine\n[homecore]\nbroker_host = \"0.0.0.0\"\nbroker_port = 1883\n\n[device]\nbroker_host = \"0.0.0.0\"\n";
        assert_eq!(
            b.rewrite_config(config),
            "# mine\n[homecore]\nbroker_host = \"10.0.0.5\"\nbroker_port = 1883\n\n[device]\nbroker_host = \"0.0.0.0\"\n"
        );
    }

    #[test]
    fn a_reachable_config_is_returned_unchanged() {
        let b = BrokerHost::new("http://10.0.0.5:8080", None);
        let config = "[homecore]\nbroker_host = \"10.0.0.5\"\npassword = \"x\"\n";
        assert_eq!(b.rewrite_config(config), config);
    }
}
