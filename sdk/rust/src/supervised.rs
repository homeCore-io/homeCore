//! A plugin homeCore started goes when homeCore does.
//!
//! Core sets [`SUPERVISED_ENV`] to its own process id on every plugin it
//! starts. Core stopped the usual way stops its plugins first. Core killed or
//! crashed leaves them running, and the next core starts a second copy of each
//! under the same broker id; the two knock each other off the broker every few
//! seconds for as long as both run. So a plugin whose parent was core, and is
//! no longer, stops.
//!
//! A plugin started any other way (by hand, by systemd, in its own container)
//! has no such variable and is left alone. One started by core through
//! another process, a launcher script that doesn't `exec`, has a parent that
//! isn't core, and is left alone too, rather than stopped by mistake.

use std::time::Duration;
use tracing::{info, warn};

/// The variable core sets on each plugin it starts: core's process id.
pub const SUPERVISED_ENV: &str = "HOMECORE_SUPERVISED";

/// How often the parent is looked at.
#[cfg(unix)]
const EVERY: Duration = Duration::from_secs(2);

/// Whether to watch for core going, from the variable and the parent now.
#[cfg_attr(not(unix), allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Watch {
    /// Not started by core, or not directly: nothing to watch.
    Off,
    /// Core, with this id, is the parent: stop when it no longer is.
    Core(u32),
}

#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn decide(var: Option<&str>, parent: u32) -> Watch {
    match var.and_then(|v| v.trim().parse::<u32>().ok()) {
        Some(core) if core == parent => Watch::Core(core),
        _ => Watch::Off,
    }
}

/// Start watching, once per process, if core is the parent.
pub(crate) fn watch(plugin_id: &str) {
    #[cfg(unix)]
    {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let var = std::env::var(SUPERVISED_ENV).ok();
            let parent = std::os::unix::process::parent_id();
            match decide(var.as_deref(), parent) {
                Watch::Off => {
                    if var.is_some() {
                        info!(
                            plugin_id,
                            "Started by homeCore through another process, so not watching for homeCore stopping"
                        );
                    }
                }
                Watch::Core(core) => {
                    let id = plugin_id.to_string();
                    let started = std::thread::Builder::new()
                        .name("homecore-watch".into())
                        .spawn(move || loop {
                            std::thread::sleep(EVERY);
                            if std::os::unix::process::parent_id() != core {
                                warn!(
                                    plugin_id = %id,
                                    core_pid = core,
                                    "homeCore, which started this plugin, has stopped; stopping too, so the next homeCore's copy runs alone"
                                );
                                std::process::exit(0);
                            }
                        });
                    if let Err(e) = started {
                        warn!(plugin_id, error = %e, "Can't watch for homeCore stopping");
                    }
                }
            }
        });
    }
    #[cfg(not(unix))]
    let _ = plugin_id;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watches_only_when_core_is_the_parent() {
        assert_eq!(decide(Some("4242"), 4242), Watch::Core(4242));
        // Started through a launcher that stayed: its parent isn't core.
        assert_eq!(decide(Some("4242"), 5000), Watch::Off);
        // Not started by core at all.
        assert_eq!(decide(None, 4242), Watch::Off);
        assert_eq!(decide(Some(""), 4242), Watch::Off);
        assert_eq!(decide(Some("core"), 4242), Watch::Off);
    }
}
