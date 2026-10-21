//! Whether the repeater is still there, kept pure so it is tested without a
//! socket. The same rules as hc-caseta's session.rs.

use std::time::{Duration, Instant};

/// How often a keepalive (a bare CR/LF) is sent.
pub const KEEPALIVE_EVERY: Duration = Duration::from_secs(60);
/// How long the repeater has to answer one (`GNET>`).
pub const KEEPALIVE_REPLY: Duration = Duration::from_secs(10);
/// Hearing nothing at all for this long ends the session regardless.
pub const SILENCE: Duration = Duration::from_secs(150);

/// A repeater that loses power or drops off the network often leaves the TCP
/// connection half-open: no FIN, no RST, and reads that wait for ever. The
/// session used to sit there — every device "online", commands going nowhere
/// — until the kernel gave up on it, about 15 minutes. Now a keepalive must be
/// answered within [`KEEPALIVE_REPLY`], and [`SILENCE`] ends it whatever else
/// happens.
#[derive(Debug)]
pub struct Liveness {
    last_heard: Instant,
    keepalive_at: Option<Instant>,
    next_keepalive: Instant,
}

impl Liveness {
    pub fn new(now: Instant) -> Self {
        Self {
            last_heard: now,
            keepalive_at: None,
            next_keepalive: now + KEEPALIVE_EVERY,
        }
    }

    /// A line arrived: the repeater is there.
    pub fn heard(&mut self, now: Instant) {
        self.last_heard = now;
        self.keepalive_at = None;
    }

    /// Whether to send a keepalive now.
    pub fn keepalive_due(&self, now: Instant) -> bool {
        self.keepalive_at.is_none() && now >= self.next_keepalive
    }

    pub fn sent_keepalive(&mut self, now: Instant) {
        self.keepalive_at = Some(now);
        self.next_keepalive = now + KEEPALIVE_EVERY;
    }

    /// Why the session should end now, if it should.
    pub fn check(&self, now: Instant) -> Option<String> {
        if let Some(at) = self.keepalive_at {
            if now.duration_since(at) > KEEPALIVE_REPLY {
                return Some(format!(
                    "the repeater didn't answer a keepalive within {} s",
                    KEEPALIVE_REPLY.as_secs()
                ));
            }
        }
        if now.duration_since(self.last_heard) > SILENCE {
            return Some(format!(
                "heard nothing from the repeater for {} s",
                SILENCE.as_secs()
            ));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quiet_but_answering_repeater_stays() {
        let t0 = Instant::now();
        let mut l = Liveness::new(t0);
        let t1 = t0 + KEEPALIVE_EVERY;
        assert!(l.keepalive_due(t1));
        l.sent_keepalive(t1);
        assert!(!l.keepalive_due(t1));
        l.heard(t1 + Duration::from_secs(1));
        assert_eq!(l.check(t1 + Duration::from_secs(30)), None);
    }

    #[test]
    fn an_unanswered_keepalive_ends_the_session() {
        let t0 = Instant::now();
        let mut l = Liveness::new(t0);
        let t1 = t0 + KEEPALIVE_EVERY;
        l.sent_keepalive(t1);
        assert_eq!(l.check(t1 + KEEPALIVE_REPLY), None);
        assert!(l
            .check(t1 + KEEPALIVE_REPLY + Duration::from_secs(1))
            .unwrap()
            .contains("keepalive"));
    }

    #[test]
    fn long_silence_ends_the_session() {
        let t0 = Instant::now();
        let l = Liveness::new(t0);
        assert_eq!(l.check(t0 + SILENCE), None);
        assert!(l
            .check(t0 + SILENCE + Duration::from_secs(1))
            .unwrap()
            .contains("heard nothing"));
    }

    #[test]
    fn events_keep_it_alive_without_keepalives() {
        let t0 = Instant::now();
        let mut l = Liveness::new(t0);
        for s in (10..600).step_by(10) {
            l.heard(t0 + Duration::from_secs(s));
        }
        assert_eq!(l.check(t0 + Duration::from_secs(600)), None);
    }
}
