//! Output formatting — human or JSON.

use anyhow::Result;
use serde::Serialize;

pub fn print<T: Serialize>(value: &T, fmt: &str) -> Result<()> {
    match fmt {
        "json" => {
            let s = serde_json::to_string_pretty(value)?;
            println!("{s}");
        }
        _ => {
            // Default "human" — pretty JSON is a reasonable fallback; the
            // command-specific handlers may print their own human-friendly
            // rendering before calling this.
            let s = serde_json::to_string_pretty(value)?;
            println!("{s}");
        }
    }
    Ok(())
}
