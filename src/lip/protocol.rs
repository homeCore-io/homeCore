//! Lutron Integration Protocol (LIP) message types and wire formatting.
//!
//! All unsolicited and query-response messages from the RA2 controller start
//! with `~`.  Client commands use `#` (execute) or `?` (query).
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
    /// Keypad button or LED event: `~DEVICE,{id},{component},{action}[,{value}]`
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
    /// `~ERROR,...`
    Error(String),
    /// Any line that didn't match the above (login prompts, echoed commands, etc.)
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
    Led(u8),
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

        // Ready prompt — may or may not have trailing space
        if line == "GNET>" || line.starts_with("GNET>") {
            return Self::Prompt;
        }

        // All valid controller responses start with '~'
        if !line.starts_with('~') {
            return Self::Unknown(line.to_string());
        }

        let body = &line[1..];
        let parts: Vec<&str> = body.split(',').collect();

        if parts.len() < 3 {
            return Self::Unknown(line.to_string());
        }

        match parts[0] {
            "OUTPUT" => Self::parse_output(&parts),
            "DEVICE" => Self::parse_device(&parts),
            "GROUP" => Self::parse_group(&parts),
            "ERROR" => Self::Error(parts[1..].join(",")),
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
        let value = parts
            .get(3)
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0);

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
        let Ok(action) = parts[3].parse::<u8>() else {
            return Self::Unknown(parts.join(","));
        };

        let action = match action {
            3 => DeviceAction::Press,
            4 => DeviceAction::Release,
            6 => DeviceAction::DoubleClick,
            9 => {
                // No invented value. A keypad that does not answer must not
                // appear to report an LED level: 255 is not a state the
                // protocol defines, and publishing it put `led_6: 255` on
                // devices whose sixth button is simply unprogrammed — and
                // every LED of a HOMEOWNER (virtual) keypad, which has no
                // physical LEDs at all.
                let Some(state) = parts.get(4).and_then(|v| v.parse::<u8>().ok()) else {
                    return Self::Unknown(parts.join(","));
                };
                DeviceAction::Led(state)
            }
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
        // parts[2] = action (always "3" for occupancy state queries/updates)
        let state = match parts.get(3).and_then(|v| v.parse::<u32>().ok()) {
            Some(3) => OccupancyState::Occupied,
            Some(4) => OccupancyState::Vacant,
            // Anything else is genuinely unknown, and the bridge must not
            // publish it as vacancy — "we did not hear" is not "nobody home".
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

/// `#OUTPUT,{id},{action}` for raise(2)/lower(3)/stop(4)
pub fn cmd_shade_action(integration_id: u32, action: u8) -> String {
    format!("#OUTPUT,{integration_id},{action}")
}

/// `#OUTPUT,{id},6` — pulse a momentary CCO.
///
/// Action 6 with no parameters, per the RA2 Integration Guide's CCO table.
/// The relay closes for its configured pulse time (one second by default) and
/// opens itself; there is nothing to set back. Levelling it with action 1
/// would latch a *maintained* CCO instead, which is a different device.
pub fn cmd_pulse_output(integration_id: u32) -> String {
    format!("#OUTPUT,{integration_id},6")
}

/// `#DEVICE,{id},{component},{action}` for press(3)/release(4)
pub fn cmd_device_action(integration_id: u32, component: u32, action: u8) -> String {
    format!("#DEVICE,{integration_id},{component},{action}")
}

/// Is this a real LED state?
///
/// The Integration Guide defines exactly four: 0 Off, 1 On, 2 Normal Flash,
/// 3 Rapid Flash. A live repeater answers **255** for a button with no LED
/// assigned — every button of a HOMEOWNER (virtual) keypad, and any
/// unprogrammed button on a physical one. It is not in the guide's table, so
/// anything outside 0-3 is treated as "no LED here" rather than a level.
///
/// This matters beyond tidiness: scene state is derived with `state > 0`, so
/// an unassigned 255 read as *on*.
pub fn is_led_state(state: u8) -> bool {
    state <= 3
}

/// LED component number for a given button component.
///
/// Per the Lutron Integration Guide (all keypad types), LED component = button + 80.
/// For example: button 1 → LED component 81, button 6 → LED component 86.
pub const LED_COMPONENT_OFFSET: u32 = 80;

pub fn led_component_for_button(button: u32) -> u32 {
    button + LED_COMPONENT_OFFSET
}

/// Reverse mapping: button component from a received LED component number.
/// Returns `None` if the component number is not in the LED range (≤ 80).
pub fn button_for_led_component(led_component: u32) -> Option<u32> {
    led_component
        .checked_sub(LED_COMPONENT_OFFSET)
        .filter(|&b| b > 0)
}

/// `?DEVICE,{id},{led_component},9` — query LED state for one button.
/// Pass the LED component number (button + 80), not the button number.
pub fn query_device_led(integration_id: u32, led_component: u32) -> String {
    format!("?DEVICE,{integration_id},{led_component},9")
}

/// `#DEVICE,{id},{led_component},9,{state}` — set LED state.
/// `state`: 0 = off, 1 = on, 2 = normal-flash (1 Hz), 3 = rapid-flash (10 Hz).
/// Pass the LED component number (button + 80), not the button number.
pub fn cmd_device_led(integration_id: u32, led_component: u32, state: u8) -> String {
    format!("#DEVICE,{integration_id},{led_component},9,{state}")
}

/// `#TIMECLOCK,{id},6,{event_index},{1=Enable|2=Disable}` — enable or disable a timeclock event.
///
/// Note: the Lutron protocol uses 1 for Enable and 2 for Disable (not 0/1 boolean).
pub fn cmd_timeclock_enable(timeclock_id: u32, event_index: u32, enable: bool) -> String {
    let state = if enable { 1 } else { 2 };
    format!("#TIMECLOCK,{timeclock_id},6,{event_index},{state}")
}

/// `#TIMECLOCK,{id},5,{event_index}` — execute/test a timeclock event immediately.
pub fn cmd_timeclock_execute(timeclock_id: u32, event_index: u32) -> String {
    format!("#TIMECLOCK,{timeclock_id},5,{event_index}")
}

/// `?OUTPUT,{id},1`
pub fn query_output(integration_id: u32) -> String {
    format!("?OUTPUT,{integration_id},1")
}

/// Format fade seconds as `H:MM:SS`.  Returns empty string for 0 or negative.
fn format_fade(secs: f64) -> String {
    if secs <= 0.0 {
        return String::new();
    }
    let total = secs as u64;
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    format!("{h}:{m:02}:{s:02}")
}

// ---------------------------------------------------------------------------
// Monitoring subscription commands
// ---------------------------------------------------------------------------

/// All `#MONITORING` commands sent immediately after login.
pub fn monitoring_commands() -> Vec<String> {
    vec![
        "#MONITORING,12,2".into(), // prompt state (suppress GNET> during bulk output)
        "#MONITORING,255,2".into(), // all event types
        "#MONITORING,3,1".into(),  // button press/release
        "#MONITORING,4,1".into(),  // LED state changes
        "#MONITORING,5,1".into(),  // zone output level changes
        "#MONITORING,6,1".into(),  // individual occupancy sensor
        "#MONITORING,8,1".into(),  // scene activations
        "#MONITORING,13,1".into(), // occupancy group changes
    ]
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
    fn parse_device_release() {
        let msg = LipMessage::parse("~DEVICE,10,2,4");
        let LipMessage::Device { action, .. } = msg else {
            panic!()
        };
        assert_eq!(action, DeviceAction::Release);
    }

    #[test]
    fn parse_device_double_click() {
        let msg = LipMessage::parse("~DEVICE,10,3,6");
        let LipMessage::Device { action, .. } = msg else {
            panic!()
        };
        assert_eq!(action, DeviceAction::DoubleClick);
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
    fn parse_group_vacant() {
        let msg = LipMessage::parse("~GROUP,5,3,4");
        let LipMessage::Group { state, .. } = msg else {
            panic!()
        };
        assert_eq!(state, OccupancyState::Vacant);
    }

    #[test]
    fn parse_prompt() {
        assert!(matches!(LipMessage::parse("GNET> "), LipMessage::Prompt));
        assert!(matches!(LipMessage::parse("GNET>"), LipMessage::Prompt));
    }

    #[test]
    fn parse_unknown_skips_non_tilde() {
        let msg = LipMessage::parse("login: ");
        assert!(matches!(msg, LipMessage::Unknown(_)));
    }

    #[test]
    fn led_component_offset() {
        assert_eq!(led_component_for_button(1), 81);
        assert_eq!(led_component_for_button(6), 86);
        assert_eq!(button_for_led_component(81), Some(1));
        assert_eq!(button_for_led_component(86), Some(6));
        assert_eq!(button_for_led_component(80), None); // offset itself is not a valid LED
        assert_eq!(button_for_led_component(0), None);
    }

    #[test]
    fn query_led_format() {
        assert_eq!(query_device_led(72, 81), "?DEVICE,72,81,9");
    }

    #[test]
    fn cmd_led_format() {
        assert_eq!(cmd_device_led(72, 83, 1), "#DEVICE,72,83,9,1");
        assert_eq!(cmd_device_led(72, 83, 0), "#DEVICE,72,83,9,0");
    }

    /// The guide defines four LED states. A live repeater answers 255 for a
    /// button with no LED — and scene state is derived with `state > 0`, so
    /// that read as *on*.
    #[test]
    fn only_the_four_documented_led_states_are_states() {
        for real in 0..=3u8 {
            assert!(is_led_state(real), "{real} is documented");
        }
        assert!(!is_led_state(255), "255 means no LED assigned");
        assert!(!is_led_state(4));
    }

    /// A keypad that does not answer must not look like one reporting a level.
    /// `255` used to be invented here, and reached homeCore as `led_6: 255` on
    /// unprogrammed buttons — and on every button of a virtual keypad.
    #[test]
    fn an_unanswered_led_query_is_not_a_state() {
        let msg = LipMessage::parse("~DEVICE,52,81,9");
        assert!(
            matches!(msg, LipMessage::Unknown(_)),
            "expected Unknown, got {msg:?}"
        );
        let msg = LipMessage::parse("~DEVICE,52,81,9,notanumber");
        assert!(matches!(msg, LipMessage::Unknown(_)));
    }

    /// "We did not hear" is not "nobody is home".
    #[test]
    fn an_unparseable_occupancy_state_stays_unknown() {
        let LipMessage::Group { state, .. } = LipMessage::parse("~GROUP,62,3,9") else {
            panic!("expected a Group message");
        };
        assert_eq!(state, OccupancyState::Unknown);
        let LipMessage::Group { state, .. } = LipMessage::parse("~GROUP,62,3") else {
            panic!("expected a Group message");
        };
        assert_eq!(state, OccupancyState::Unknown);
    }

    #[test]
    fn parse_device_led() {
        let msg = LipMessage::parse("~DEVICE,72,83,9,1");
        let LipMessage::Device {
            integration_id,
            component,
            action,
        } = msg
        else {
            panic!()
        };
        assert_eq!(integration_id, 72);
        assert_eq!(component, 83);
        assert_eq!(action, DeviceAction::Led(1));
    }

    #[test]
    fn cmd_level_with_fade() {
        // 2 seconds = 0 hours, 0 minutes, 2 seconds → "0:00:02"
        assert_eq!(cmd_set_level(7, 75.0, 2.0), "#OUTPUT,7,1,75.00,0:00:02");
        // 120 seconds = 0 hours, 2 minutes, 0 seconds → "0:02:00"
        assert_eq!(cmd_set_level(7, 75.0, 120.0), "#OUTPUT,7,1,75.00,0:02:00");
    }

    #[test]
    fn cmd_level_instant() {
        assert_eq!(cmd_set_level(7, 0.0, 0.0), "#OUTPUT,7,1,0.00");
    }

    #[test]
    fn fade_format_sub_minute() {
        assert_eq!(cmd_set_level(1, 100.0, 3.0), "#OUTPUT,1,1,100.00,0:00:03");
    }
}
