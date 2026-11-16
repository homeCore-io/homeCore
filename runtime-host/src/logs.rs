//! The runtime's recent output, for its page: the host's own log and each
//! hosted plugin's stdout and stderr, the last few hundred lines of each.
//!
//! A plugin forwards its own log to homeCore once it is connected. What it
//! prints before that (an import error, a bad setting, a traceback on the way
//! down) only exists here and in the container log, which is the reason for
//! this. Everything still goes to the container log as well.
//!
//! The page has no login, so passwords, keys and tokens are masked before a
//! line is kept.

use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};

/// Lines kept per source.
const KEEP: usize = 500;
/// The host's own log, beside the plugin ids.
pub const RUNTIME: &str = "runtime";

#[derive(Debug, Clone, Serialize)]
pub struct Line {
    pub at: DateTime<Utc>,
    pub text: String,
}

#[derive(Default)]
pub struct LogBook {
    sources: Mutex<HashMap<String, VecDeque<Line>>>,
}

/// The one log book for this process.
pub fn book() -> &'static LogBook {
    static BOOK: OnceLock<LogBook> = OnceLock::new();
    BOOK.get_or_init(LogBook::default)
}

impl LogBook {
    pub fn push(&self, source: &str, text: &str) {
        let text = text.trim_end();
        if text.is_empty() {
            return;
        }
        let line = Line {
            at: Utc::now(),
            text: redact(text),
        };
        if let Ok(mut map) = self.sources.lock() {
            let q = map.entry(source.to_string()).or_default();
            if q.len() == KEEP {
                q.pop_front();
            }
            q.push_back(line);
        }
    }

    /// The last `n` lines from `source`, oldest first.
    pub fn tail(&self, source: &str, n: usize) -> Vec<Line> {
        self.sources
            .lock()
            .ok()
            .and_then(|map| {
                map.get(source)
                    .map(|q| q.iter().skip(q.len().saturating_sub(n)).cloned().collect())
            })
            .unwrap_or_default()
    }

    /// Every source with lines, the runtime first, then plugins by id.
    pub fn sources(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .sources
            .lock()
            .map(|map| map.keys().filter(|k| *k != RUNTIME).cloned().collect())
            .unwrap_or_default();
        out.sort();
        out.insert(0, RUNTIME.to_string());
        out
    }
}

/// Mask what looks like a secret: homeCore keys, and the value after a word
/// such as password or token.
pub fn redact(text: &str) -> String {
    const WORDS: [&str; 6] = ["password", "passwd", "secret", "token", "api_key", "apikey"];
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        // homeCore's own key shapes, wherever they appear.
        if let Some(stripped) = ["hc_sk_", "hcrt1."]
            .iter()
            .find_map(|p| rest.strip_prefix(p).map(|s| (p, s)))
        {
            let (prefix, after) = stripped;
            let len = after
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.'))
                .unwrap_or(after.len());
            out.push_str(prefix);
            out.push_str("•••");
            rest = &after[len..];
            continue;
        }
        // `password=…`, `token: …`, `"secret": "…"`
        let lower = rest.to_ascii_lowercase();
        if let Some(word) = WORDS.iter().find(|w| lower.starts_with(**w)) {
            let after = &rest[word.len()..];
            let sep_len = after
                .find(|c: char| !(c == '"' || c == '\'' || c == ' ' || c == ':' || c == '='))
                .unwrap_or(after.len());
            let sep = &after[..sep_len];
            if sep.contains(':') || sep.contains('=') {
                let value = &after[sep_len..];
                let value_len = value
                    .find(|c: char| {
                        c.is_whitespace() || c == '"' || c == '\'' || c == ',' || c == '}'
                    })
                    .unwrap_or(value.len());
                out.push_str(&rest[..word.len()]);
                out.push_str(sep);
                out.push_str("•••");
                rest = &value[value_len..];
                continue;
            }
        }
        let ch = rest.chars().next().expect("rest is not empty");
        out.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    out
}

/// A tracing writer that prints as before and keeps a copy for the page.
#[derive(Clone, Copy, Default)]
pub struct Tee;

impl std::io::Write for Tee {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        std::io::stdout().write_all(buf)?;
        for line in String::from_utf8_lossy(buf).lines() {
            book().push(RUNTIME, line);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        std::io::stdout().flush()
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Tee {
    type Writer = Tee;

    fn make_writer(&'a self) -> Self::Writer {
        Tee
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_masked_and_the_rest_kept() {
        assert_eq!(
            redact("connecting with key hcrt1.eyJhbGciOi.xyz now"),
            "connecting with key hcrt1.••• now"
        );
        assert_eq!(redact("auth hc_sk_AbC-123_x ok"), "auth hc_sk_••• ok");
        assert_eq!(redact("password=hunter2 user=bob"), "password=••• user=bob");
        assert_eq!(
            redact(r#"{"Token": "abc", "n": 1}"#),
            r#"{"Token": "•••", "n": 1}"#
        );
        assert_eq!(redact("the token expired"), "the token expired");
        assert_eq!(redact("plain line"), "plain line");
    }

    #[test]
    fn each_source_keeps_its_last_lines() {
        let book = LogBook::default();
        for i in 0..(KEEP + 5) {
            book.push("plugin.a", &format!("line {i}"));
        }
        book.push(RUNTIME, "host");
        let tail = book.tail("plugin.a", 2);
        assert_eq!(
            tail.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(),
            vec![format!("line {}", KEEP + 3), format!("line {}", KEEP + 4)]
        );
        assert_eq!(book.tail("plugin.a", 10_000).len(), KEEP);
        assert_eq!(book.sources(), vec!["runtime", "plugin.a"]);
        assert!(book.tail("plugin.none", 5).is_empty());
    }
}
