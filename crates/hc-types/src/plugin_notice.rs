//! Operator-facing notices a plugin raises about its own condition.
//!
//! `PluginRecord::status` answers "is the process alive" — `active`, `offline`,
//! `stopped`, `starting`. It cannot answer "alive, but structurally unable to
//! do its job", and that is the state operators actually get stuck in. The
//! Ecowitt receiver bound to loopback is the motivating case: the plugin starts
//! cleanly, heartbeats, reports `active`, and silently drops every gateway
//! upload because the gateway is a different host on the network. The condition
//! was detectable at startup and was written to the log, where nobody was
//! looking. On the dashboard it read as healthy.
//!
//! A notice carries the diagnosis to the UI so it appears next to the plugin
//! rather than only in a log stream.
//!
//! **Notices are current state, not an event log.** A plugin publishes the full
//! set it currently believes, on every heartbeat, and core replaces what it
//! held. A condition that clears simply stops being sent and disappears on the
//! next beat — no acknowledge, no expiry, nothing to garbage-collect. That also
//! means a notice must be cheap to re-derive: compute it from current config
//! and state each time, don't accumulate it.

use serde::{Deserialize, Serialize};

/// How much the operator should care.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeLevel {
    /// Worth knowing, nothing is wrong. A deliberate non-default mode, say.
    Info,
    /// The plugin runs but something it needs is missing or misconfigured, and
    /// some or all of its function is unavailable. The common case.
    Warning,
    /// The plugin cannot do its job at all and operator action is required.
    Error,
}

/// One condition a plugin is reporting about itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginNotice {
    pub level: NoticeLevel,
    /// Stable machine-readable identifier, `snake_case`
    /// (`receiver_unreachable`, `credentials_missing`).
    ///
    /// Stable is the point: the UI keys off this to dedupe and to decide
    /// presentation, so `message` stays free to be reworded without anything
    /// downstream noticing. Keep it specific to the condition, not the plugin —
    /// two plugins with the same problem should use the same code.
    pub code: String,
    /// What is wrong, in a sentence an operator can act on. Says what is
    /// happening and why it matters, not just which setting is unset.
    pub message: String,
    /// What to do about it, when that can be stated concretely — the setting to
    /// change and the value to use. `None` when the remedy is situational
    /// enough that guessing would mislead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remedy: Option<String>,
}

impl PluginNotice {
    pub fn new(level: NoticeLevel, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            level,
            code: code.into(),
            message: message.into(),
            remedy: None,
        }
    }

    pub fn warning(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(NoticeLevel::Warning, code, message)
    }

    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(NoticeLevel::Error, code, message)
    }

    pub fn info(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(NoticeLevel::Info, code, message)
    }

    pub fn with_remedy(mut self, remedy: impl Into<String>) -> Self {
        self.remedy = Some(remedy.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_wire_form_is_snake_case() {
        assert_eq!(
            serde_json::to_string(&NoticeLevel::Warning).unwrap(),
            "\"warning\""
        );
    }

    #[test]
    fn remedy_is_omitted_when_absent() {
        let n = PluginNotice::warning("receiver_unreachable", "nothing can reach the receiver");
        let json = serde_json::to_string(&n).unwrap();
        assert!(
            !json.contains("remedy"),
            "absent remedy must not serialise: {json}"
        );
    }

    #[test]
    fn decodes_a_payload_with_no_remedy() {
        // Plugins on older SDKs, or ones with nothing concrete to suggest,
        // omit the field entirely — that must not fail the whole heartbeat.
        let n: PluginNotice =
            serde_json::from_str(r#"{"level":"error","code":"x","message":"y"}"#).unwrap();
        assert_eq!(n.level, NoticeLevel::Error);
        assert!(n.remedy.is_none());
    }

    #[test]
    fn builder_round_trips() {
        let n = PluginNotice::warning("receiver_unreachable", "msg").with_remedy("set bind_addr");
        let back: PluginNotice = serde_json::from_str(&serde_json::to_string(&n).unwrap()).unwrap();
        assert_eq!(n, back);
    }
}
