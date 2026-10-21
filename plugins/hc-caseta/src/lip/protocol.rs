//! Lutron Integration Protocol (LIP) message types and wire formatting.
//!
//! All unsolicited and query-response messages from the Caseta Pro bridge
//! start with `~`.  Client commands use `#` (execute) or `?` (query).
//!
//! Format:  `~CMD_TYPE,integration_id,action[,value...]`

// ---------------------------------------------------------------------------
// Incoming message types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum LipMessage {
    /// Zone/dimmer level change: `~OUTPUT,{id},1,{level}`
    Output {
        integration_id: u32,
        action: OutputAction,
        value: f64,
    },
    /// Button event: `~DEVICE,{id},{component},{action}`
    Device {
        integration_id: u32,
        component: u32,
        action: DeviceAction,
    },
    /// Occupancy group state: `~GROUP,{id},3,{state}`
    Group {
        integration_id: u32,
        state: OccupancyState,
    },
    /// `GNET> ` ready prompt
    Prompt,
    /// `~ERROR,N` — the bridge refused the command it was last sent. Caséta
    /// sends the code alone (`~ERROR,6`); `code` is `None` for a line whose
    /// code is not a number.
    Error { code: Option<u32>, raw: String },
    /// `login:` — the bridge asking for a login. After the password this is
    /// the bridge refusing it: it asks again rather than saying so.
    LoginPrompt,
    /// Any line that didn't match the above
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum OutputAction {
    ZoneLevel,
    Raise,
    Lower,
    Stop,
    Flash,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DeviceAction {
    Press,
    Release,
    DoubleClick,
}

#[derive(Debug, Clone, PartialEq)]
pub enum OccupancyState {
    Occupied,
    Vacant,
    Unknown,
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

impl LipMessage {
    pub fn parse(line: &str) -> Self {
        let line = line.trim();

        if line == "GNET>" || line.starts_with("GNET>") {
            return Self::Prompt;
        }
        if line.to_ascii_lowercase().ends_with("login:") {
            return Self::LoginPrompt;
        }

        let Some(body) = line.strip_prefix('~') else {
            return Self::Unknown(line.to_string());
        };
        let parts: Vec<&str> = body.split(',').map(str::trim).collect();

        // `~ERROR,6` has two fields, not three — the length check below used
        // to turn every error the bridge sent into an Unknown that nobody saw.
        if parts[0] == "ERROR" {
            return Self::Error {
                code: parts.get(1).and_then(|c| c.parse().ok()),
                raw: line.to_string(),
            };
        }

        if parts.len() < 3 {
            return Self::Unknown(line.to_string());
        }

        match parts[0] {
            "OUTPUT" => Self::parse_output(&parts),
            "DEVICE" => Self::parse_device(&parts),
            "GROUP" => Self::parse_group(&parts),
            _ => Self::Unknown(line.to_string()),
        }
    }

    fn parse_output(parts: &[&str]) -> Self {
        let Ok(id) = parts[1].parse::<u32>() else {
            return Self::Unknown(parts.join(","));
        };
        let Ok(action) = parts[2].parse::<u8>() else {
            return Self::Unknown(parts.join(","));
        };
        let value = parts.get(3).and_then(|v| v.parse::<f64>().ok());
        // A level line without a level is not "0 %": reading it as one would
        // turn a light off in homeCore that is on in the room.
        if action == 1 && !value.is_some_and(f64::is_finite) {
            return Self::Unknown(parts.join(","));
        }
        let value = value.unwrap_or(0.0);

        let action = match action {
            1 => OutputAction::ZoneLevel,
            2 => OutputAction::Raise,
            3 => OutputAction::Lower,
            4 => OutputAction::Stop,
            5 => OutputAction::Flash,
            _ => return Self::Unknown(parts.join(",")),
        };

        Self::Output {
            integration_id: id,
            action,
            value,
        }
    }

    fn parse_device(parts: &[&str]) -> Self {
        let Ok(id) = parts[1].parse::<u32>() else {
            return Self::Unknown(parts.join(","));
        };
        let Ok(component) = parts[2].parse::<u32>() else {
            return Self::Unknown(parts.join(","));
        };
        // `~DEVICE,1,2` — three fields — used to index past the end and panic.
        let Some(Ok(action)) = parts.get(3).map(|a| a.parse::<u8>()) else {
            return Self::Unknown(parts.join(","));
        };

        let action = match action {
            3 => DeviceAction::Press,
            4 => DeviceAction::Release,
            6 => DeviceAction::DoubleClick,
            _ => return Self::Unknown(parts.join(",")),
        };

        Self::Device {
            integration_id: id,
            component,
            action,
        }
    }

    fn parse_group(parts: &[&str]) -> Self {
        let Ok(id) = parts[1].parse::<u32>() else {
            return Self::Unknown(parts.join(","));
        };
        // Action 3 is the occupancy state; anything else is not, and reading
        // its value as one would make up an occupied or a vacant.
        if parts[2] != "3" {
            return Self::Unknown(parts.join(","));
        }
        let state_val = parts
            .get(3)
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(255);
        let state = match state_val {
            3 => OccupancyState::Occupied,
            4 => OccupancyState::Vacant,
            _ => OccupancyState::Unknown,
        };
        Self::Group {
            integration_id: id,
            state,
        }
    }
}

// ---------------------------------------------------------------------------
// Outgoing command formatting
// ---------------------------------------------------------------------------

/// `#OUTPUT,{id},1,{level:.2}[,{fade}]`
pub fn cmd_set_level(integration_id: u32, level: f64, fade_secs: f64) -> String {
    let fade = format_fade(fade_secs);
    if fade.is_empty() {
        format!("#OUTPUT,{integration_id},1,{level:.2}")
    } else {
        format!("#OUTPUT,{integration_id},1,{level:.2},{fade}")
    }
}

/// `#OUTPUT,{id},{action}` for raise(2)/lower(3)/stop(4) — a shade's travel,
/// or a dimmer's level ramping up or down until stopped.
pub fn cmd_shade_action(integration_id: u32, action: u8) -> String {
    format!("#OUTPUT,{integration_id},{action}")
}

/// `?OUTPUT,{id},1`
pub fn query_output(integration_id: u32) -> String {
    format!("?OUTPUT,{integration_id},1")
}

/// `#DEVICE,{id},{component},{action}` for press(3)/release(4).
///
/// Used to activate a scene: the Smart Bridge's phantom buttons are pressed
/// exactly as a physical button would be.
pub fn cmd_device_action(integration_id: u32, component: u32, action: u8) -> String {
    format!("#DEVICE,{integration_id},{component},{action}")
}

/// Format a fade for the wire. Empty for 0 or negative (no fade parameter).
///
/// The Integration Guide takes `SS.ss`, `MM:SS` or `HH:MM:SS`, and rounds to
/// the nearest quarter second. Under a minute it is sent as seconds with their
/// fraction — `0.5`, `1.25`, `2` — so a `transition_ms` of 500 is a half-second
/// fade rather than none at all. A minute or more is `H:MM:SS`, whole seconds.
fn format_fade(secs: f64) -> String {
    if !secs.is_finite() || secs <= 0.0 {
        return String::new();
    }
    let hundredths = (secs * 100.0).round() as u64;
    if hundredths == 0 {
        return String::new();
    }
    if hundredths < 6000 {
        let s = format!("{}.{:02}", hundredths / 100, hundredths % 100);
        return s.trim_end_matches('0').trim_end_matches('.').to_string();
    }
    let total = secs.round() as u64;
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    format!("{h}:{m:02}:{s:02}")
}

/// What an `~ERROR,N` code means, in words for `last_command_error`.
///
/// The codes are the Integration Protocol's: 1 parameter count, 2 object
/// does not exist, 3 invalid action number, 4 parameter out of range, 5
/// parameter malformed, 6 unsupported command.
pub fn error_words(code: Option<u32>) -> String {
    match code {
        Some(1) => "the bridge refused it: wrong number of parameters".into(),
        Some(2) => "the bridge doesn't know that integration ID".into(),
        Some(3) => "the bridge refused it: that device can't do this".into(),
        Some(4) => "the bridge refused it: a value was out of range".into(),
        Some(5) => "the bridge refused it: a value was malformed".into(),
        Some(6) => "the bridge doesn't support that command".into(),
        Some(n) => format!("the bridge refused it (error {n})"),
        None => "the bridge refused it".into(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// `~ERROR,6` is two fields; it used to fall under the three-field check
    /// and read as Unknown, so no refusal was ever seen.
    #[test]
    fn errors_parse_with_their_code() {
        let LipMessage::Error { code, raw } = LipMessage::parse("~ERROR,6") else {
            panic!("~ERROR,6 must be an error")
        };
        assert_eq!(code, Some(6));
        assert_eq!(raw, "~ERROR,6");
        assert!(matches!(
            LipMessage::parse("~ERROR,2\r"),
            LipMessage::Error { code: Some(2), .. }
        ));
        assert!(matches!(
            LipMessage::parse("~ERROR"),
            LipMessage::Error { code: None, .. }
        ));
        assert!(matches!(
            LipMessage::parse("~ERROR,x"),
            LipMessage::Error { code: None, .. }
        ));
        assert!(error_words(Some(2)).contains("integration ID"));
    }

    /// The bridge asks for the login again when the password is wrong.
    #[test]
    fn a_login_prompt_is_recognised() {
        assert!(matches!(
            LipMessage::parse("login:"),
            LipMessage::LoginPrompt
        ));
        assert!(matches!(
            LipMessage::parse("bad login\r\nlogin:"),
            LipMessage::LoginPrompt
        ));
        assert!(!matches!(
            LipMessage::parse("password:"),
            LipMessage::LoginPrompt
        ));
    }

    /// Lines a bridge could send cut short, or that a noisy line could
    /// garble: none may panic, and none may become an event.
    #[test]
    fn malformed_lines_never_panic_or_become_events() {
        let junk = [
            "~DEVICE,1,2",
            "~DEVICE,1",
            "~DEVICE",
            "~DEVICE,,,",
            "~DEVICE,6,2,",
            "~DEVICE,6,2,99",
            "~DEVICE,x,2,3",
            "~DEVICE,6,y,3",
            "~OUTPUT,2",
            "~OUTPUT,2,",
            "~OUTPUT,2,9,50",
            "~OUTPUT,2,1",
            "~OUTPUT,2,1,",
            "~OUTPUT,2,1,abc",
            "~OUTPUT,2,1,NaN",
            "~OUTPUT,a,1,50",
            "~GROUP,5",
            "~GROUP,5,1,3",
            "~GROUP,z,3,3",
            "~",
            "~~~",
            ",,,",
            "",
            "~MONITORING,5,1",
            "~DEVICE,4294967296,2,3",
            "\u{fffd}\u{fffd}",
        ];
        for line in junk {
            let msg = LipMessage::parse(line);
            assert!(
                matches!(msg, LipMessage::Unknown(_)),
                "{line:?} parsed as {msg:?}"
            );
        }
        // A long line of commas is still just a line.
        let long = format!("~DEVICE{}", ",".repeat(5000));
        assert!(matches!(LipMessage::parse(&long), LipMessage::Unknown(_)));
    }

    /// `~GROUP,id,3,state` only; another action's value is not occupancy.
    #[test]
    fn a_group_line_needs_the_occupancy_action() {
        assert!(matches!(
            LipMessage::parse("~GROUP,5,3,4"),
            LipMessage::Group {
                state: OccupancyState::Vacant,
                ..
            }
        ));
        assert!(matches!(
            LipMessage::parse("~GROUP,5,7,3"),
            LipMessage::Unknown(_)
        ));
    }

    #[test]
    fn release_and_pico_presses_parse() {
        assert!(matches!(
            LipMessage::parse("~DEVICE,9,8,3"),
            LipMessage::Device {
                integration_id: 9,
                component: 8,
                action: DeviceAction::Press
            }
        ));
        assert!(matches!(
            LipMessage::parse("~DEVICE,9,8,4\r\n"),
            LipMessage::Device {
                action: DeviceAction::Release,
                ..
            }
        ));
    }

    #[test]
    fn parse_output_level() {
        let msg = LipMessage::parse("~OUTPUT,7,1,75.00");
        let LipMessage::Output {
            integration_id,
            action,
            value,
        } = msg
        else {
            panic!()
        };
        assert_eq!(integration_id, 7);
        assert_eq!(action, OutputAction::ZoneLevel);
        assert!((value - 75.0).abs() < 0.001);
    }

    #[test]
    fn parse_device_press() {
        let msg = LipMessage::parse("~DEVICE,10,2,3");
        let LipMessage::Device {
            integration_id,
            component,
            action,
        } = msg
        else {
            panic!()
        };
        assert_eq!(integration_id, 10);
        assert_eq!(component, 2);
        assert_eq!(action, DeviceAction::Press);
    }

    #[test]
    fn parse_group_occupied() {
        let msg = LipMessage::parse("~GROUP,5,3,3");
        let LipMessage::Group {
            integration_id,
            state,
        } = msg
        else {
            panic!()
        };
        assert_eq!(integration_id, 5);
        assert_eq!(state, OccupancyState::Occupied);
    }

    #[test]
    fn parse_prompt() {
        assert!(matches!(LipMessage::parse("GNET> "), LipMessage::Prompt));
    }

    #[test]
    fn cmd_level_with_fade() {
        assert_eq!(cmd_set_level(7, 75.0, 2.0), "#OUTPUT,7,1,75.00,2");
        assert_eq!(cmd_set_level(7, 75.0, 120.0), "#OUTPUT,7,1,75.00,0:02:00");
    }

    /// `transition_ms` is milliseconds; the wire was whole seconds, so a
    /// half-second fade used to be truncated to none at all.
    #[test]
    fn fractional_fades_reach_the_wire() {
        assert_eq!(cmd_set_level(1, 100.0, 0.5), "#OUTPUT,1,1,100.00,0.5");
        assert_eq!(cmd_set_level(1, 100.0, 1.25), "#OUTPUT,1,1,100.00,1.25");
        assert_eq!(cmd_set_level(1, 100.0, 60.0), "#OUTPUT,1,1,100.00,0:01:00");
        assert_eq!(cmd_set_level(1, 100.0, 0.001), "#OUTPUT,1,1,100.00");
    }

    #[test]
    fn cmd_level_instant() {
        assert_eq!(cmd_set_level(7, 0.0, 0.0), "#OUTPUT,7,1,0.00");
    }
}
