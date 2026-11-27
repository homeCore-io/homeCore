//! Secrets in a plugin's TOML config, masked for `GET /plugins/{id}/config`
//! and put back on `PUT`, on the parsed document rather than line by line.
//!
//! The line-by-line version looked only at `key = value` lines, so it missed:
//! - a secret inside an inline table: `bridges = [{ host = "…", app_key = "…" }]`;
//! - the body of a multi-line string: `private_key = """` was masked, the key
//!   material on the lines after it was not, and the result wasn't TOML;
//! - the values of a multi-line array: `tokens = [` then one per line.
//!
//! `toml_edit` keeps comments and layout, which the editor in the app shows.
//! Both functions give `None` for text that doesn't parse; the caller then
//! falls back to the line-by-line version.

use crate::handlers::{is_secret_key, REDACTED_SECRET};
use toml_edit::{Array, ArrayOfTables, DocumentMut, InlineTable, Item, Table, Value};

/// Fields that say which entry of an array an element is, strongest first,
/// so a restore finds an entry's own secret after one before it is removed.
const ID_KEYS: [&str; 6] = ["id", "bridge_id", "uuid", "serial", "name", "host"];

/// `src` with every secret replaced by [`REDACTED_SECRET`].
pub(crate) fn redact(src: &str) -> Option<String> {
    let mut doc: DocumentMut = src.parse().ok()?;
    redact_table(doc.as_table_mut());
    Some(doc.to_string())
}

fn redact_table(t: &mut Table) {
    for (k, item) in t.iter_mut() {
        let secret = is_secret_key(k.get());
        match item {
            Item::Table(t) => redact_table(t),
            Item::ArrayOfTables(a) => a.iter_mut().for_each(redact_table),
            Item::Value(v) => redact_value(secret, v),
            Item::None => {}
        }
    }
}

/// A table is walked key by key whatever its own key is called; everything
/// else under a secret key, an array's elements included, is masked.
fn redact_value(secret: bool, v: &mut Value) {
    match v {
        Value::InlineTable(t) => redact_inline(t),
        Value::Array(a) => a.iter_mut().for_each(|v| redact_value(secret, v)),
        _ if secret => {
            let decor = v.decor().clone();
            *v = Value::from(REDACTED_SECRET);
            *v.decor_mut() = decor;
        }
        _ => {}
    }
}

fn redact_inline(t: &mut InlineTable) {
    for (k, v) in t.iter_mut() {
        redact_value(is_secret_key(k.get()), v);
    }
}

/// `incoming` with each [`REDACTED_SECRET`] replaced by the value at the same
/// place in `current`. A placeholder with nothing to stand for stays, and the
/// caller decides what that means.
pub(crate) fn restore(incoming: &str, current: &str) -> Option<String> {
    let mut inc: DocumentMut = incoming.parse().ok()?;
    let cur: DocumentMut = current.parse().ok()?;
    restore_table(inc.as_table_mut(), Some(cur.as_table()));
    Some(inc.to_string())
}

fn restore_table(inc: &mut Table, cur: Option<&Table>) {
    for (k, item) in inc.iter_mut() {
        let c = cur.and_then(|c| c.get(k.get()));
        match item {
            Item::Table(t) => restore_table(t, c.and_then(Item::as_table)),
            Item::ArrayOfTables(a) => restore_tables(a, c.and_then(Item::as_array_of_tables)),
            Item::Value(v) => restore_value(v, c.and_then(Item::as_value)),
            Item::None => {}
        }
    }
}

fn restore_tables(inc: &mut ArrayOfTables, cur: Option<&ArrayOfTables>) {
    for (i, t) in inc.iter_mut().enumerate() {
        let ids = identity(|k| t.get(k).and_then(Item::as_str));
        let c = cur.and_then(|cur| {
            let all: Vec<&Table> = cur.iter().collect();
            same_entry(&ids, |e, k| e.get(k).and_then(Item::as_str), &all).or_else(|| cur.get(i))
        });
        restore_table(t, c);
    }
}

fn restore_value(inc: &mut Value, cur: Option<&Value>) {
    match inc {
        Value::String(s) if s.value() == REDACTED_SECRET => {
            if let Some(c) = cur {
                let decor = inc.decor().clone();
                *inc = c.clone();
                *inc.decor_mut() = decor;
            }
        }
        Value::InlineTable(t) => {
            let c = cur.and_then(Value::as_inline_table);
            for (k, v) in t.iter_mut() {
                restore_value(v, c.and_then(|c| c.get(k.get())));
            }
        }
        Value::Array(a) => restore_array(a, cur.and_then(Value::as_array)),
        _ => {}
    }
}

fn restore_array(inc: &mut Array, cur: Option<&Array>) {
    for (i, v) in inc.iter_mut().enumerate() {
        let ids = match v.as_inline_table() {
            Some(t) => identity(|k| t.get(k).and_then(Value::as_str)),
            None => Vec::new(),
        };
        let c = cur.and_then(|cur| {
            let all: Vec<&Value> = cur.iter().collect();
            same_entry(&ids, |e, k| e.as_inline_table()?.get(k)?.as_str(), &all)
                .or_else(|| cur.get(i))
        });
        restore_value(v, c);
    }
}

/// The identity fields an incoming entry has, strongest first.
fn identity<'s>(get: impl Fn(&str) -> Option<&'s str>) -> Vec<(&'static str, String)> {
    ID_KEYS
        .iter()
        .filter_map(|k| Some((*k, get(k)?.to_string())))
        .collect()
}

/// The candidate that shares the first identity field with the incoming entry.
fn same_entry<'a, E>(
    ids: &[(&'static str, String)],
    field: impl Fn(&'a E, &str) -> Option<&'a str>,
    candidates: &[&'a E],
) -> Option<&'a E> {
    ids.iter().find_map(|(k, want)| {
        candidates
            .iter()
            .copied()
            .find(|e| field(e, k) == Some(want.as_str()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"# The plugin's settings
[homecore]
broker_host = "127.0.0.1"
password = "mqtt-pass"   # from homeCore

bridges = [
  { host = "10.0.0.2", app_key = "first-key" },
  { host = "10.0.0.3", app_key = "second-key" },
]

private_key = """
-----BEGIN KEY-----
key-material
-----END KEY-----
"""

api_tokens = [
  "token-a",
  "token-b",
]

[[cameras]]
name = "porch"
password = "porch-pass"
"#;

    #[test]
    fn every_secret_is_masked_and_the_rest_kept() {
        let shown = redact(CONFIG).unwrap();
        for secret in [
            "mqtt-pass",
            "first-key",
            "second-key",
            "key-material",
            "token-a",
            "token-b",
            "porch-pass",
        ] {
            assert!(!shown.contains(secret), "{secret} leaked:\n{shown}");
        }
        assert!(shown.contains("# The plugin's settings"));
        assert!(shown.contains("# from homeCore"));
        assert!(shown.contains("10.0.0.3"));
        assert!(shown.contains("porch"));
        assert!(shown.parse::<DocumentMut>().is_ok(), "still TOML:\n{shown}");
    }

    #[test]
    fn saving_what_was_shown_writes_the_secrets_back() {
        let shown = redact(CONFIG).unwrap();
        let written = restore(&shown, CONFIG).unwrap();
        assert_eq!(
            written.parse::<DocumentMut>().unwrap().to_string(),
            CONFIG.parse::<DocumentMut>().unwrap().to_string()
        );
    }

    #[test]
    fn a_removed_entry_does_not_shift_the_others_secrets() {
        let shown = redact(CONFIG).unwrap().replace(
            "  { host = \"10.0.0.2\", app_key = \"__redacted__\" },\n",
            "",
        );
        let written = restore(&shown, CONFIG).unwrap();
        assert!(written.contains("second-key"));
        assert!(!written.contains("first-key"));
    }

    #[test]
    fn text_that_is_not_toml_is_left_to_the_caller() {
        assert!(redact("password = ").is_none());
        assert!(restore("a = 1", "password = ").is_none());
    }
}
