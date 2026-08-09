//! Deciding where a plugin runs.
//!
//! Installing a plugin is placement: core resolves the artifact, and if it is
//! not native it goes to a runtime that can host it rather than being unpacked
//! locally. The admin clicks Install and does not need to know where it lands.
//!
//! Decision logic only — no HTTP, no downloads. See
//! `docs/pluginRuntimesPlan.md`, piece 2.

use hc_state::plugin_runtime_store::{RuntimeRecord, RuntimeStatus};

/// A runtime that could host something, reduced to what matching needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub runtime_id: String,
    pub kind: String,
    pub abi: String,
    pub arch: String,
}

impl From<&RuntimeRecord> for Candidate {
    fn from(r: &RuntimeRecord) -> Self {
        Self {
            runtime_id: r.runtime_id.clone(),
            kind: r.kind.clone(),
            abi: r.abi.clone(),
            arch: r.arch.clone(),
        }
    }
}

/// Only approved runtimes can host anything. A pending one is a request, not a
/// deployment target.
pub fn candidates(records: &[RuntimeRecord]) -> Vec<Candidate> {
    records
        .iter()
        .filter(|r| r.status == RuntimeStatus::Approved)
        .map(Candidate::from)
        .collect()
}

/// Where a plugin should be installed.
#[derive(Debug, PartialEq, Eq)]
pub enum Placement {
    /// Core unpacks and supervises it, as it always has.
    Core,
    /// Hand it to this runtime.
    Runtime(String),
    /// Ask which — several could host it and the choice is the operator's.
    Ambiguous(Vec<String>),
    /// Explain what is missing, in terms of what to go and do about it.
    Impossible(String),
}

/// What the registry offers for one version, reduced to what matching needs.
pub struct Offered<'a> {
    /// True when a native artifact exists for this host.
    pub native_for_this_host: bool,
    /// `(kind, abi, arch)` for every runtime artifact this version publishes.
    pub runtime_artifacts: Vec<(&'a str, &'a str, &'a str)>,
    /// Runtime kinds published, for the error message.
    pub kinds: Vec<&'a str>,
}

/// Decide where a plugin version goes.
///
/// `requested` pins the choice to one runtime; without it a single match is
/// taken automatically and several are handed back for the operator to choose.
pub fn decide(offered: &Offered, runtimes: &[Candidate], requested: Option<&str>) -> Placement {
    // An explicit request is answered on its own terms, including when it is
    // wrong — silently placing somewhere else would be worse than refusing.
    if let Some(want) = requested {
        let Some(rt) = runtimes.iter().find(|c| c.runtime_id == want) else {
            return Placement::Impossible(format!(
                "no approved runtime called `{want}` — check Settings for one waiting to be approved"
            ));
        };
        return if matches(offered, rt) {
            Placement::Runtime(rt.runtime_id.clone())
        } else {
            Placement::Impossible(format!(
                "`{want}` hosts {} {} on {}, and this plugin publishes nothing for that",
                rt.kind, rt.abi, rt.arch
            ))
        };
    }

    // Native wins when it exists: it needs nothing else installed, and it is
    // what every plugin published before runtimes existed is.
    if offered.native_for_this_host {
        return Placement::Core;
    }

    let fits: Vec<String> = runtimes
        .iter()
        .filter(|c| matches(offered, c))
        .map(|c| c.runtime_id.clone())
        .collect();

    match fits.len() {
        1 => Placement::Runtime(fits.into_iter().next().expect("len == 1")),
        0 if offered.runtime_artifacts.is_empty() => {
            Placement::Impossible("this plugin publishes no artifact for this machine".into())
        }
        0 => Placement::Impossible(explain_no_fit(offered, runtimes)),
        _ => Placement::Ambiguous(fits),
    }
}

fn matches(offered: &Offered, c: &Candidate) -> bool {
    offered
        .runtime_artifacts
        .iter()
        .any(|(kind, abi, arch)| *kind == c.kind && *abi == c.abi && *arch == c.arch)
}

/// Say what is missing in terms of the next action, not in terms of matching.
///
/// "No artifact matched" tells an operator nothing they can act on. Whether they
/// need to enroll a runtime at all, or already have one of the right kind that is
/// simply the wrong ABI or architecture, are different problems with different
/// fixes.
fn explain_no_fit(offered: &Offered, runtimes: &[Candidate]) -> String {
    let kinds = offered.kinds.join(", ");
    let same_kind: Vec<&Candidate> = runtimes
        .iter()
        .filter(|c| offered.kinds.contains(&c.kind.as_str()))
        .collect();

    if same_kind.is_empty() {
        return format!(
            "this plugin needs a {kinds} runtime, and none is enrolled — start one and approve it in Settings"
        );
    }
    let have = same_kind
        .iter()
        .map(|c| format!("{} ({} on {})", c.runtime_id, c.abi, c.arch))
        .collect::<Vec<_>>()
        .join(", ");
    let want = offered
        .runtime_artifacts
        .iter()
        .map(|(k, abi, arch)| format!("{k} {abi} on {arch}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("this plugin publishes {want}; the enrolled {kinds} runtime does not match — {have}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt(id: &str, kind: &str, abi: &str, arch: &str) -> Candidate {
        Candidate {
            runtime_id: id.into(),
            kind: kind.into(),
            abi: abi.into(),
            arch: arch.into(),
        }
    }

    fn python_only<'a>() -> Offered<'a> {
        Offered {
            native_for_this_host: false,
            runtime_artifacts: vec![("python", "cp312-manylinux_2_28", "x86_64")],
            kinds: vec!["python"],
        }
    }

    /// A plugin that ships a binary for this host stays where it always was.
    /// Every entry published before runtimes existed is this case.
    #[test]
    fn native_goes_to_core() {
        let offered = Offered {
            native_for_this_host: true,
            runtime_artifacts: vec![("python", "cp312-manylinux_2_28", "x86_64")],
            kinds: vec!["python"],
        };
        let rts = vec![rt("rt-a", "python", "cp312-manylinux_2_28", "x86_64")];
        assert_eq!(decide(&offered, &rts, None), Placement::Core);
    }

    #[test]
    fn one_matching_runtime_is_chosen_without_asking() {
        let rts = vec![rt("rt-a", "python", "cp312-manylinux_2_28", "x86_64")];
        assert_eq!(
            decide(&python_only(), &rts, None),
            Placement::Runtime("rt-a".into())
        );
    }

    /// Two homes is the operator's decision, not a coin toss — picking one would
    /// put a plugin somewhere they did not intend and would not think to check.
    #[test]
    fn several_matching_runtimes_ask() {
        let rts = vec![
            rt("rt-a", "python", "cp312-manylinux_2_28", "x86_64"),
            rt("rt-b", "python", "cp312-manylinux_2_28", "x86_64"),
        ];
        match decide(&python_only(), &rts, None) {
            Placement::Ambiguous(ids) => assert_eq!(ids, vec!["rt-a", "rt-b"]),
            other => panic!("expected a choice, got {other:?}"),
        }
    }

    /// Only approved runtimes are targets. A pending one is a request someone
    /// has not answered yet.
    #[test]
    fn pending_runtimes_are_not_candidates() {
        use chrono::Utc;
        let mut rec = RuntimeRecord {
            runtime_id: "rt-waiting".into(),
            public_key: "K".into(),
            kind: "python".into(),
            abi: "cp312-manylinux_2_28".into(),
            arch: "x86_64".into(),
            host_version: "0.1.0".into(),
            sdk_version: "0.2.0".into(),
            hostname: "h".into(),
            network_mode: "host".into(),
            status: RuntimeStatus::Pending,
            code: None,
            secret_hash: None,
            api_key_hash: None,
            source_ip: None,
            plugin_id: None,
            denial_count: 0,
            cooldown_until: None,
            created_at: Utc::now(),
            expires_at: None,
            last_seen_at: None,
        };
        assert!(candidates(std::slice::from_ref(&rec)).is_empty());

        rec.status = RuntimeStatus::Approved;
        assert_eq!(candidates(&[rec]).len(), 1);
    }

    /// The error has to name the next action. "No artifact matched" is true and
    /// useless.
    #[test]
    fn no_runtime_at_all_says_to_enrol_one() {
        let msg = match decide(&python_only(), &[], None) {
            Placement::Impossible(m) => m,
            other => panic!("expected impossible, got {other:?}"),
        };
        assert!(msg.contains("python"), "{msg}");
        assert!(msg.contains("none is enrolled"), "{msg}");
    }

    /// Having a python runtime of the wrong ABI is a different problem from
    /// having none, and the fix is different too.
    #[test]
    fn a_mismatched_runtime_says_what_it_has() {
        let rts = vec![rt("rt-old", "python", "cp311-manylinux_2_28", "x86_64")];
        let msg = match decide(&python_only(), &rts, None) {
            Placement::Impossible(m) => m,
            other => panic!("expected impossible, got {other:?}"),
        };
        assert!(msg.contains("rt-old"), "{msg}");
        assert!(msg.contains("cp311"), "names what it actually has: {msg}");
        assert!(msg.contains("cp312"), "and what was needed: {msg}");
    }

    /// An explicit request that cannot be honoured is refused rather than
    /// quietly redirected — the operator asked for somewhere specific.
    #[test]
    fn an_explicit_request_that_does_not_fit_is_refused_not_redirected() {
        let rts = vec![
            rt("rt-good", "python", "cp312-manylinux_2_28", "x86_64"),
            rt("rt-wrong", "python", "cp311-manylinux_2_28", "x86_64"),
        ];
        match decide(&python_only(), &rts, Some("rt-wrong")) {
            Placement::Impossible(m) => assert!(m.contains("rt-wrong"), "{m}"),
            other => panic!("must not silently use rt-good: {other:?}"),
        }
    }

    #[test]
    fn an_unknown_requested_runtime_is_named() {
        let msg = match decide(&python_only(), &[], Some("rt-typo")) {
            Placement::Impossible(m) => m,
            other => panic!("expected impossible, got {other:?}"),
        };
        assert!(msg.contains("rt-typo"), "{msg}");
    }

    /// A plugin with no artifact for this machine at all — neither native nor
    /// runtime — is its own message, not a confusing runtime one.
    #[test]
    fn a_plugin_with_nothing_published_says_so() {
        let offered = Offered {
            native_for_this_host: false,
            runtime_artifacts: vec![],
            kinds: vec![],
        };
        match decide(&offered, &[], None) {
            Placement::Impossible(m) => assert!(m.contains("no artifact"), "{m}"),
            other => panic!("expected impossible, got {other:?}"),
        }
    }
}
