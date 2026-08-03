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
    /// `~ERROR,...`
    Error(String),
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

/// `#OUTPUT,{id},{action}` for raise(2)/lower(3)/stop(4)
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
        assert_eq!(cmd_set_level(7, 75.0, 2.0), "#OUTPUT,7,1,75.00,0:00:02");
    }

    #[test]
    fn cmd_level_instant() {
        assert_eq!(cmd_set_level(7, 0.0, 0.0), "#OUTPUT,7,1,0.00");
    }
}
