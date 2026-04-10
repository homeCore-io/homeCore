//! Glue device: timer — countdown timer with start/pause/resume/cancel/restart.
//!
//! This module delegates to the existing `timer_manager` implementation.
//! Full migration of timer logic into this module is planned for a future step.

pub const TIMER_ID_PREFIX: &str = "timer_";

use crate::EventBus;
use hc_state::StateStore;
use tracing::debug;

/// Timer commands are handled by the existing TimerManager which runs
/// its own event loop. This stub exists so GlueManager can recognize
/// timer command topics and log them, but actual processing is done
/// by TimerManager until the full migration is complete.
pub async fn handle_cmd(
    _state: &StateStore,
    _pub_bus: &EventBus,
    device_id: &str,
    _payload: &[u8],
) {
    // TimerManager handles this via its own event loop subscription.
    // This is a no-op placeholder until timer logic is fully migrated.
    debug!(%device_id, "Timer command routed to TimerManager");
}
