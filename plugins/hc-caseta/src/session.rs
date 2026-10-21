//! The bookkeeping of one session with the bridge, kept pure so it is
//! tested without a socket: is the bridge still there, and which command did
//! the bridge just answer.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use serde_json::Value;

// ---------------------------------------------------------------------------
// Liveness
// ---------------------------------------------------------------------------

/// How often a keepalive (a bare CR/LF) is sent.
pub const KEEPALIVE_EVERY: Duration = Duration::from_secs(60);
/// How long the bridge has to answer one (`GNET>`).
pub const KEEPALIVE_REPLY: Duration = Duration::from_secs(10);
/// Hearing nothing at all for this long ends the session regardless.
pub const SILENCE: Duration = Duration::from_secs(150);

/// Whether the bridge is still there.
///
/// A bridge that loses power or drops off the network often leaves the TCP
/// connection half-open: no FIN, no RST, and reads that wait for ever. The
/// session used to sit there — "connected", every Pico press lost — until
/// something else broke it. Now a keepalive must be answered within
/// [`KEEPALIVE_REPLY`], and [`SILENCE`] ends it whatever else happens.
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

    /// A line arrived: the bridge is there.
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
                    "the bridge didn't answer a keepalive within {} s",
                    KEEPALIVE_REPLY.as_secs()
                ));
            }
        }
        if now.duration_since(self.last_heard) > SILENCE {
            return Some(format!(
                "heard nothing from the bridge for {} s",
                SILENCE.as_secs()
            ));
        }
        None
    }
}

/// The next wait between attempts: doubled, within `[base, max]`.
pub fn next_backoff(current: Duration, base: Duration, max: Duration) -> Duration {
    (current * 2).clamp(base, max.max(base))
}

// ---------------------------------------------------------------------------
// Commands in flight
// ---------------------------------------------------------------------------

/// How long a line may wait for the bridge's answer. The bridge says nothing
/// about a command that worked beyond its prompt, so one that has had no
/// `~ERROR` by then is taken to have worked.
pub const ANSWER_WITHIN: Duration = Duration::from_secs(3);

/// What was sent for one command (or query, or keepalive), and what came of
/// it.
#[derive(Debug, Clone, Default)]
pub struct Sent {
    /// The device it was for; `None` for the plugin's own lines.
    pub hc_id: Option<String>,
    /// What it asked, for `plugin_command_result` (`"on"`, `"activate"`).
    pub operation: String,
    /// A `?OUTPUT` for this integration ID: an error means the bridge has no
    /// such zone.
    pub query: Option<u32>,
    /// The state to put back if the bridge refuses it.
    pub restore: Option<Value>,
    /// What the bridge said against it.
    pub error: Option<String>,
    lines: usize,
}

impl Sent {
    pub fn command(hc_id: &str, operation: &str, restore: Option<Value>) -> Self {
        Self {
            hc_id: Some(hc_id.to_string()),
            operation: operation.to_string(),
            restore,
            ..Default::default()
        }
    }

    pub fn query(integration_id: u32) -> Self {
        Self {
            query: Some(integration_id),
            operation: "query".into(),
            ..Default::default()
        }
    }

    pub fn keepalive() -> Self {
        Self {
            operation: "keepalive".into(),
            ..Default::default()
        }
    }
}

/// Lines sent and not yet answered, oldest first.
///
/// The bridge answers every line it is sent in order — `GNET>` when done,
/// `~ERROR,N` first when it refused it — and never names the command it is
/// answering. So the answer belongs to the oldest line still waiting: an
/// `~ERROR` marks that line's command failed, and the prompt after it
/// finishes the line. A command of several lines (a scene's press and
/// release) is done when its last line is.
#[derive(Debug, Default)]
pub struct InFlight {
    lines: VecDeque<(u64, Instant)>,
    sent: HashMap<u64, Sent>,
    next: u64,
}

impl InFlight {
    /// Note `lines` lines sent for `what`.
    pub fn push(&mut self, mut what: Sent, lines: usize, now: Instant) {
        let id = self.next;
        self.next += 1;
        what.lines = lines.max(1);
        for _ in 0..what.lines {
            self.lines.push_back((id, now));
        }
        self.sent.insert(id, what);
    }

    /// `~ERROR` — against the oldest line waiting. `None` when nothing was.
    pub fn error(&mut self, words: &str) -> Option<()> {
        let (id, _) = self.lines.front()?;
        let s = self.sent.get_mut(id)?;
        s.error.get_or_insert_with(|| words.to_string());
        Some(())
    }

    /// `GNET>` — the oldest line is answered. Its command, once all of its
    /// lines are.
    pub fn prompt(&mut self) -> Option<Sent> {
        let (id, _) = self.lines.pop_front()?;
        self.finish_line(id)
    }

    /// Lines past [`ANSWER_WITHIN`]: answered by silence.
    pub fn expire(&mut self, now: Instant) -> Vec<Sent> {
        let mut done = vec![];
        while let Some(&(id, at)) = self.lines.front() {
            if now.duration_since(at) <= ANSWER_WITHIN {
                break;
            }
            self.lines.pop_front();
            done.extend(self.finish_line(id));
        }
        done
    }

    /// Everything still waiting, when the session ends.
    pub fn drain(&mut self) -> Vec<Sent> {
        self.lines.clear();
        let mut out: Vec<(u64, Sent)> = self.sent.drain().collect();
        out.sort_by_key(|(id, _)| *id);
        out.into_iter().map(|(_, s)| s).collect()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    fn finish_line(&mut self, id: u64) -> Option<Sent> {
        let s = self.sent.get_mut(&id)?;
        s.lines -= 1;
        if s.lines == 0 {
            self.sent.remove(&id)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_keepalive_unanswered_ends_the_session() {
        let t0 = Instant::now();
        let mut l = Liveness::new(t0);
        assert!(!l.keepalive_due(t0));
        let t1 = t0 + KEEPALIVE_EVERY;
        assert!(l.keepalive_due(t1));
        l.sent_keepalive(t1);
        assert!(!l.keepalive_due(t1), "one at a time");
        assert!(l.check(t1 + Duration::from_secs(5)).is_none());
        let why = l.check(t1 + Duration::from_secs(11)).expect("dead");
        assert!(why.contains("keepalive"), "{why}");
    }

    #[test]
    fn an_answer_keeps_it_alive() {
        let t0 = Instant::now();
        let mut l = Liveness::new(t0);
        let t1 = t0 + KEEPALIVE_EVERY;
        l.sent_keepalive(t1);
        l.heard(t1 + Duration::from_millis(30));
        assert!(l.check(t1 + Duration::from_secs(30)).is_none());
    }

    /// Half-open: nothing is ever heard. Found within the keepalive period
    /// plus its reply window — well inside three minutes.
    #[test]
    fn half_open_silence_is_found_within_three_minutes() {
        let t0 = Instant::now();
        let mut l = Liveness::new(t0);
        let mut t = t0;
        let found = loop {
            t += Duration::from_secs(1);
            if l.keepalive_due(t) {
                l.sent_keepalive(t);
            }
            if l.check(t).is_some() {
                break t.duration_since(t0);
            } else if t.duration_since(t0) > Duration::from_secs(600) {
                panic!("never found")
            }
        };
        assert!(found <= KEEPALIVE_EVERY + KEEPALIVE_REPLY + Duration::from_secs(1));
        assert!(found < Duration::from_secs(180));
    }

    #[test]
    fn silence_alone_ends_it_too() {
        let t0 = Instant::now();
        let l = Liveness::new(t0);
        assert!(l.check(t0 + SILENCE + Duration::from_secs(1)).is_some());
    }

    #[test]
    fn backoff_doubles_within_bounds() {
        let (b, m) = (Duration::from_secs(5), Duration::from_secs(60));
        assert_eq!(next_backoff(b, b, m), Duration::from_secs(10));
        assert_eq!(next_backoff(Duration::from_secs(40), b, m), m);
        assert_eq!(next_backoff(Duration::ZERO, b, m), b);
    }

    /// `#OUTPUT,99,1,100` → `~ERROR,2`, `GNET>`: that command failed.
    #[test]
    fn an_error_belongs_to_the_oldest_line_waiting() {
        let now = Instant::now();
        let mut f = InFlight::default();
        f.push(Sent::command("caseta_99", "on", None), 1, now);
        f.push(Sent::command("caseta_2", "on", None), 1, now);
        f.error("the bridge doesn't know that integration ID");
        let first = f.prompt().expect("answered");
        assert_eq!(first.hc_id.as_deref(), Some("caseta_99"));
        assert!(first.error.is_some());
        let second = f.prompt().expect("answered");
        assert!(second.error.is_none());
        assert!(f.prompt().is_none());
        assert!(f.error("x").is_none(), "an error with nothing waiting");
    }

    /// A scene is two lines; it is done, and failed, as a whole.
    #[test]
    fn a_command_of_two_lines_is_done_with_its_last() {
        let now = Instant::now();
        let mut f = InFlight::default();
        f.push(Sent::command("caseta_scene_x", "activate", None), 2, now);
        f.error("refused");
        assert!(f.prompt().is_none(), "one line still to go");
        let s = f.prompt().expect("done");
        assert_eq!(s.error.as_deref(), Some("refused"));
    }

    /// The bridge answers success with silence and a prompt; with no prompt
    /// at all the line is taken as done after a while.
    #[test]
    fn unanswered_lines_expire_as_done() {
        let now = Instant::now();
        let mut f = InFlight::default();
        f.push(Sent::keepalive(), 1, now);
        f.push(
            Sent::command("caseta_2", "on", None),
            1,
            now + Duration::from_secs(2),
        );
        let gone = f.expire(now + Duration::from_secs(4));
        assert_eq!(gone.len(), 1);
        assert_eq!(gone[0].operation, "keepalive");
        assert!(!f.is_empty());
        assert_eq!(f.drain().len(), 1);
        assert!(f.is_empty());
    }
}
