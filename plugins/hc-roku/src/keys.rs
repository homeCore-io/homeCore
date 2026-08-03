//! Roku ECP remote-key catalogue and the `Lit_` text-entry encoding.
//!
//! ECP takes a key name as a *path segment* (`POST /keypress/Home`), so
//! anything user-supplied has to be percent-encoded before it goes in the
//! URL — most visibly for `Lit_` keys, which carry an arbitrary UTF-8
//! character (`Lit_%E2%82%AC` is a euro sign).
//!
//! The catalogue exists so the plugin can (a) accept friendly aliases
//! (`ok` → `Select`, `rewind` → `Rev`) without every caller memorising
//! Roku's spelling, and (b) publish the real list on the device schema so
//! a UI can render a remote.

/// Every key ECP accepts, in the order a remote lays them out. Keys past
/// `FindRemote` are only honoured by Roku TVs and audio devices — the
/// device advertises what it has via `device-info`
/// (`supports-find-remote`, `is-tv`, `supports-audio-volume-control`),
/// but ECP itself returns 200 for an unsupported key rather than an
/// error, so there is nothing to validate against at request time.
pub const ALL_KEYS: &[&str] = &[
    // Navigation
    "Home",
    "Back",
    "Select",
    "Left",
    "Right",
    "Up",
    "Down",
    "Info",
    // Transport
    "Play",
    "Rev",
    "Fwd",
    "InstantReplay",
    // Text entry
    "Backspace",
    "Enter",
    "Search",
    // Device-dependent
    "FindRemote",
    "VolumeUp",
    "VolumeDown",
    "VolumeMute",
    "PowerOff",
    "PowerOn",
    "Power",
    "ChannelUp",
    "ChannelDown",
    "InputTuner",
    "InputHDMI1",
    "InputHDMI2",
    "InputHDMI3",
    "InputHDMI4",
    "InputAV1",
];

/// Resolve a caller-supplied key name to ECP's spelling.
///
/// Accepts the canonical name in any case (`home`, `HOME`, `Home`), a
/// set of common aliases, and passes `Lit_…` through untouched so
/// single-character entry still works. Returns `None` for anything
/// unrecognised — the caller reports that rather than sending a request
/// the device would silently 200 on.
pub fn resolve(name: &str) -> Option<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Literal keys carry an arbitrary character; the prefix is
    // case-sensitive in ECP, the payload must not be touched.
    if let Some(rest) = trimmed.strip_prefix("Lit_") {
        return Some(format!("Lit_{rest}"));
    }

    let lower = trimmed.to_ascii_lowercase().replace(['-', ' '], "_");
    let canonical = match lower.as_str() {
        // Aliases first — anything that is not the ECP spelling.
        "ok" => "Select",
        "menu" | "asterisk" | "star" | "options" => "Info",
        "rewind" | "rew" | "reverse" => "Rev",
        "forward" | "ff" | "fast_forward" => "Fwd",
        "replay" | "instant_replay" => "InstantReplay",
        "volume_up" | "vol_up" => "VolumeUp",
        "volume_down" | "vol_down" => "VolumeDown",
        "mute" | "volume_mute" => "VolumeMute",
        "power_off" | "poweroff" | "off" => "PowerOff",
        "power_on" | "poweron" | "on" => "PowerOn",
        "power" | "power_toggle" => "Power",
        "channel_up" | "chan_up" => "ChannelUp",
        "channel_down" | "chan_down" => "ChannelDown",
        "tuner" | "input_tuner" | "antenna" => "InputTuner",
        "input_hdmi1" | "hdmi1" => "InputHDMI1",
        "input_hdmi2" | "hdmi2" => "InputHDMI2",
        "input_hdmi3" | "hdmi3" => "InputHDMI3",
        "input_hdmi4" | "hdmi4" => "InputHDMI4",
        "input_av1" | "av1" => "InputAV1",
        "find_remote" => "FindRemote",
        "instant" => "InstantReplay",
        // Otherwise match the catalogue case-insensitively.
        _ => {
            return ALL_KEYS
                .iter()
                .find(|k| k.eq_ignore_ascii_case(trimmed))
                .map(|k| (*k).to_string())
        }
    };
    Some(canonical.to_string())
}

/// Expand a string into the `Lit_` keypress sequence that types it on the
/// on-screen keyboard.
///
/// One key per *character* (not per byte): ECP wants the whole UTF-8
/// character percent-encoded as one `Lit_` payload, so `€` becomes a
/// single `Lit_%E2%82%AC`, not three keys.
pub fn literal_sequence(text: &str) -> Vec<String> {
    text.chars()
        .map(|c| {
            let mut buf = [0u8; 4];
            let encoded = percent_encode(c.encode_utf8(&mut buf));
            format!("Lit_{encoded}")
        })
        .collect()
}

/// Percent-encode a URL *path segment*.
///
/// Deliberately conservative: everything outside the RFC 3986 unreserved
/// set is escaped. `Lit_ ` (a space) has to become `Lit_%20` — `+` is
/// query-string encoding and ECP reads it literally in a path.
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Encode a resolved key for use as a path segment. `Lit_` keys keep
/// their prefix readable and only escape the payload; every other key is
/// a bare ASCII identifier and passes through unchanged.
pub fn encode_key_path(key: &str) -> String {
    match key.strip_prefix("Lit_") {
        // Already-encoded payloads (`Lit_%E2%82%AC`, produced by
        // `literal_sequence`) must not be double-escaped, so only encode
        // when the payload still contains raw bytes needing it.
        Some(rest) if rest.starts_with('%') => key.to_string(),
        Some(rest) => format!("Lit_{}", percent_encode(rest)),
        None => percent_encode(key),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_resolve_to_ecp_spelling() {
        assert_eq!(resolve("ok").as_deref(), Some("Select"));
        assert_eq!(resolve("rewind").as_deref(), Some("Rev"));
        assert_eq!(resolve("volume_up").as_deref(), Some("VolumeUp"));
        assert_eq!(resolve("hdmi2").as_deref(), Some("InputHDMI2"));
        assert_eq!(resolve("instant replay").as_deref(), Some("InstantReplay"));
    }

    #[test]
    fn canonical_names_are_case_insensitive() {
        assert_eq!(resolve("HOME").as_deref(), Some("Home"));
        assert_eq!(resolve("inputhdmi3").as_deref(), Some("InputHDMI3"));
    }

    #[test]
    fn unknown_keys_are_rejected_rather_than_sent() {
        // ECP answers 200 to a key it doesn't have, so an unrecognised
        // name would look like success. Catch it here instead.
        assert!(resolve("Sleep").is_none());
        assert!(resolve("").is_none());
    }

    /// The euro sign is the doc's own example: one character, one key,
    /// three percent-escaped bytes.
    #[test]
    fn multibyte_char_is_one_literal_key() {
        assert_eq!(literal_sequence("€"), vec!["Lit_%E2%82%AC"]);
    }

    #[test]
    fn space_encodes_as_percent_20_not_plus() {
        assert_eq!(literal_sequence("a b"), vec!["Lit_a", "Lit_%20", "Lit_b"]);
    }

    #[test]
    fn already_encoded_literals_are_not_double_escaped() {
        assert_eq!(encode_key_path("Lit_%E2%82%AC"), "Lit_%E2%82%AC");
        assert_eq!(encode_key_path("Lit_&"), "Lit_%26");
        assert_eq!(encode_key_path("Home"), "Home");
    }
}
