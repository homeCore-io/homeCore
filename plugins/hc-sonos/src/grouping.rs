//! Making a group exactly what someone asked for, in one go.
//!
//! `{"action": "group", "members": [...]}` on the speaker that should lead:
//! the listed speakers join it, every other speaker in its group leaves, and
//! each one says what happened — joined, left, already there, or no answer.
//! The Play together sheet shows those answers speaker by speaker, and an
//! automation gets the same thing from one step instead of a join per
//! speaker and an unjoin for the rest.
//!
//! [`plan`] decides what to do and is pure; [`apply`] does it against the
//! speakers, each under a deadline so one asleep speaker answers "no answer"
//! instead of holding up the rest.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{json, Value};
use sonor::Speaker;

use crate::shared_state::AppState;

/// How long one speaker gets to answer a join or a leave.
pub const STEP_TIMEOUT: Duration = Duration::from_secs(4);

/// One change to one speaker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Leave its group, becoming a group of its own.
    Leave(String),
    /// Join the leader's group.
    Join(String),
}

/// What it takes to make `leader`'s group exactly `leader` + `wanted`.
///
/// - `leader_coordinator`: who leads the leader's group now. When that is
///   another speaker, the leader leaves it first, to lead a group of its own.
/// - `current`: the members of the leader's group now (it among them).
///
/// Speakers already in the group are left alone; listed ones join; the rest of
/// the group leaves. The leader is never asked to join itself, and a speaker
/// listed twice joins once.
pub fn plan(
    leader: &str,
    leader_coordinator: &str,
    current: &[String],
    wanted: &[String],
) -> Vec<Step> {
    let mut steps = Vec::new();
    let current: Vec<&str> = if leader_coordinator != leader {
        steps.push(Step::Leave(leader.to_string()));
        vec![leader]
    } else {
        current.iter().map(String::as_str).collect()
    };
    for m in &current {
        if *m != leader && !wanted.iter().any(|w| w == m) {
            steps.push(Step::Leave((*m).to_string()));
        }
    }
    let mut seen: Vec<&str> = Vec::new();
    for w in wanted {
        if w != leader && !current.contains(&w.as_str()) && !seen.contains(&w.as_str()) {
            seen.push(w);
            steps.push(Step::Join(w.clone()));
        }
    }
    steps
}

/// What happened to one speaker, in the words the sheet shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Joined,
    Left,
    Already,
    NoAnswer,
    Failed(String),
}

impl Outcome {
    pub fn word(&self) -> &'static str {
        match self {
            Outcome::Joined => "joined",
            Outcome::Left => "left",
            Outcome::Already => "already",
            Outcome::NoAnswer => "no_answer",
            Outcome::Failed(_) => "failed",
        }
    }
}

/// An error that means the speaker never answered, as opposed to answering no.
pub fn is_no_answer(e: &str) -> bool {
    let e = e.to_ascii_lowercase();
    [
        "timed out",
        "deadline",
        "connection refused",
        "connect error",
        "error trying to connect",
        "no route",
        "unreachable",
        "connection reset",
        "broken pipe",
    ]
    .iter()
    .any(|s| e.contains(s))
}

/// The reply: the leader, and what happened to each speaker asked about or
/// removed — in the order the request listed them, then the ones that left.
pub fn report(
    leader: &str,
    wanted: &[String],
    outcomes: &HashMap<String, Outcome>,
    names: &HashMap<String, String>,
) -> Value {
    let mut order: Vec<&String> = Vec::new();
    for w in wanted {
        if w != leader && !order.contains(&w) {
            order.push(w);
        }
    }
    let mut left: Vec<&String> = outcomes
        .keys()
        .filter(|k| *k != leader && !order.contains(k))
        .collect();
    left.sort();
    order.extend(left);

    let results: Vec<Value> = order
        .iter()
        .map(|id| {
            let o = outcomes.get(*id).cloned().unwrap_or(Outcome::Already);
            let mut r = json!({
                "device_id": id,
                "name": names.get(*id).cloned().unwrap_or_else(|| (*id).clone()),
                "result": o.word(),
            });
            if let Outcome::Failed(e) = &o {
                r["error"] = json!(e);
            }
            r
        })
        .collect();
    let joined = results.iter().filter(|r| r["result"] == "joined").count();
    let unanswered = results
        .iter()
        .filter(|r| r["result"] == "no_answer")
        .count();
    let failed = results.iter().filter(|r| r["result"] == "failed").count();
    json!({
        "status": if unanswered + failed == 0 { "ok" } else { "partial" },
        "leader": leader,
        "results": results,
        "joined": joined,
        "no_answer": unanswered,
        "failed": failed,
    })
}

/// Make `leader_hc`'s group exactly it and `members_hc` (homeCore device
/// ids), and say what happened to each.
///
/// Reads the topology fresh from the leader rather than trusting the last
/// poll — the whole point is that a group changed elsewhere a minute ago is
/// handled right.
pub async fn apply(state: &AppState, leader_hc: &str, members_hc: &[String]) -> Value {
    // Everything known, by homeCore id and by UUID.
    let (by_hc, names, handles) = {
        let st = state.read().await;
        let by_hc: HashMap<String, String> = st
            .speakers
            .values()
            .map(|e| (e.hc_id.clone(), e.uuid.clone()))
            .collect();
        let names: HashMap<String, String> = st
            .speakers
            .values()
            .map(|e| (e.hc_id.clone(), e.room_name.clone()))
            .collect();
        let handles: HashMap<String, Speaker> = st
            .speakers
            .values()
            .map(|e| (e.uuid.clone(), e.speaker.clone()))
            .collect();
        (by_hc, names, handles)
    };
    let to_hc: HashMap<String, String> =
        by_hc.iter().map(|(h, u)| (u.clone(), h.clone())).collect();

    let Some(leader) = by_hc.get(leader_hc).cloned() else {
        return json!({ "status": "error", "error": format!("{leader_hc} is not a Sonos speaker this plugin knows") });
    };
    let unknown: Vec<&String> = members_hc
        .iter()
        .filter(|m| !by_hc.contains_key(*m))
        .collect();
    if !unknown.is_empty() {
        return json!({ "status": "error", "error": format!("not Sonos speakers this plugin knows: {unknown:?}") });
    }
    let Some(leader_speaker) = handles.get(&leader).cloned() else {
        return json!({ "status": "error", "error": "the leader has no handle" });
    };

    // The leader's group now, from the leader itself.
    let topology = match tokio::time::timeout(STEP_TIMEOUT, crate::topology::fetch(&leader_speaker))
        .await
    {
        Ok(Ok(t)) => t,
        Ok(Err(e)) => {
            return json!({ "status": "error", "error": format!("{} could not say its group: {e}", names[leader_hc]) })
        }
        Err(_) => {
            return json!({ "status": "error", "error": format!("{} did not answer", names[leader_hc]) })
        }
    };
    // Speakers only: a stereo pair's second half and a soundbar's
    // satellites are part of their speaker, and never asked to leave.
    let (coordinator, current) = topology
        .by_member()
        .remove(&leader)
        .unwrap_or_else(|| (leader.clone(), vec![leader.clone()]));

    let wanted: Vec<String> = members_hc
        .iter()
        .filter_map(|m| by_hc.get(m).cloned())
        .collect();
    let steps = plan(&leader, &coordinator, &current, &wanted);

    let mut outcomes: HashMap<String, Outcome> = HashMap::new();
    // The leader leaving someone else's group comes first: the joins are to
    // the group it leads afterwards.
    let mut rest = Vec::new();
    for step in steps {
        match step {
            Step::Leave(u) if u == leader => {
                if let Err(o) = run(leave(&leader_speaker)).await {
                    let mut out = HashMap::new();
                    out.insert(leader_hc.to_string(), o);
                    let mut r = report(leader_hc, &[], &out, &names);
                    r["status"] = json!("error");
                    r["error"] = json!(format!("{} could not leave its group", names[leader_hc]));
                    return r;
                }
            }
            s => rest.push(s),
        }
    }

    let x_rincon = format!("x-rincon:{leader}");
    let jobs = rest.into_iter().filter_map(|step| {
        let (uuid, joining) = match &step {
            Step::Join(u) => (u.clone(), true),
            Step::Leave(u) => (u.clone(), false),
        };
        let speaker = handles.get(&uuid)?.clone();
        let x_rincon = x_rincon.clone();
        Some(async move {
            let done = if joining {
                run(async move {
                    speaker
                        .set_transport_uri(&x_rincon, "")
                        .await
                        .map_err(anyhow::Error::from)
                })
                .await
            } else {
                run(leave(&speaker)).await
            };
            let outcome = match done {
                Ok(()) if joining => Outcome::Joined,
                Ok(()) => Outcome::Left,
                Err(o) => o,
            };
            (uuid, outcome)
        })
    });
    for (uuid, outcome) in futures::future::join_all(jobs).await {
        if let Some(hc) = to_hc.get(&uuid) {
            outcomes.insert(hc.clone(), outcome);
        }
    }
    for m in members_hc {
        if m != leader_hc {
            outcomes.entry(m.clone()).or_insert(Outcome::Already);
        }
    }
    report(leader_hc, members_hc, &outcomes, &names)
}

async fn leave(speaker: &Speaker) -> anyhow::Result<()> {
    speaker.leave().await.map_err(anyhow::Error::from)
}

/// One step under [`STEP_TIMEOUT`]; a speaker that never answers is
/// [`Outcome::NoAnswer`], one that answers no is [`Outcome::Failed`].
async fn run(f: impl std::future::Future<Output = anyhow::Result<()>>) -> Result<(), Outcome> {
    match tokio::time::timeout(STEP_TIMEOUT, f).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => {
            let text = format!("{e:#}");
            Err(if is_no_answer(&text) {
                Outcome::NoAnswer
            } else {
                Outcome::Failed(text)
            })
        }
        Err(_) => Err(Outcome::NoAnswer),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_speaker_alone_gains_the_listed_ones() {
        assert_eq!(
            plan("O1", "O1", &v(&["O1"]), &v(&["O2", "BA"])),
            vec![Step::Join("O2".into()), Step::Join("BA".into())]
        );
    }

    #[test]
    fn the_membership_becomes_exactly_the_list() {
        // O1 leads O1+O2+LR; asked for O1+O2+BA: LR leaves, BA joins, O2 stays.
        assert_eq!(
            plan("O1", "O1", &v(&["O1", "O2", "LR"]), &v(&["O2", "BA"])),
            vec![Step::Leave("LR".into()), Step::Join("BA".into())]
        );
    }

    #[test]
    fn an_empty_list_plays_it_on_its_own() {
        assert_eq!(
            plan("O1", "O1", &v(&["O1", "O2"]), &[]),
            vec![Step::Leave("O2".into())]
        );
        assert!(plan("O1", "O1", &v(&["O1"]), &[]).is_empty());
    }

    #[test]
    fn a_member_asked_to_lead_leaves_its_group_first() {
        // O2 is in O1's group and is asked to lead BA: it leaves O1's group
        // (whose other members are O1's business), then BA joins it.
        assert_eq!(
            plan("O2", "O1", &v(&["O1", "O2"]), &v(&["BA"])),
            vec![Step::Leave("O2".into()), Step::Join("BA".into())]
        );
        // …and asked to lead its old coordinator, it takes it along.
        assert_eq!(
            plan("O2", "O1", &v(&["O1", "O2"]), &v(&["O1"])),
            vec![Step::Leave("O2".into()), Step::Join("O1".into())]
        );
    }

    #[test]
    fn the_leader_never_joins_itself_and_a_repeat_joins_once() {
        assert_eq!(
            plan("O1", "O1", &v(&["O1"]), &v(&["O1", "O2", "O2"])),
            vec![Step::Join("O2".into())]
        );
    }

    #[test]
    fn nothing_to_do_is_nothing_done() {
        assert!(plan("O1", "O1", &v(&["O1", "O2"]), &v(&["O2"])).is_empty());
    }

    #[test]
    fn an_unreachable_speaker_is_no_answer_and_a_refusal_is_a_failure() {
        assert!(is_no_answer("operation timed out"));
        assert!(is_no_answer(
            "error trying to connect: tcp connect error: Connection refused (os error 111)"
        ));
        assert!(!is_no_answer("the UPnP action failed with code 800"));
    }

    #[test]
    fn the_report_says_each_speaker_in_the_order_asked() {
        let names: HashMap<String, String> = [
            ("o2", "Office-2"),
            ("ba", "Bathroom"),
            ("lr", "Living Room"),
            ("k", "Kitchen"),
        ]
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
        let outcomes: HashMap<String, Outcome> = [
            ("o2".to_string(), Outcome::Already),
            ("ba".to_string(), Outcome::Joined),
            ("lr".to_string(), Outcome::NoAnswer),
            ("k".to_string(), Outcome::Left),
        ]
        .into_iter()
        .collect();
        let r = report("o1", &v(&["o2", "ba", "lr"]), &outcomes, &names);
        let said: Vec<(String, String)> = r["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| {
                (
                    x["name"].as_str().unwrap().to_string(),
                    x["result"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        assert_eq!(
            said,
            vec![
                ("Office-2".into(), "already".into()),
                ("Bathroom".into(), "joined".into()),
                ("Living Room".into(), "no_answer".into()),
                ("Kitchen".into(), "left".into()),
            ]
        );
        assert_eq!(r["status"], "partial");
        assert_eq!(r["no_answer"], 1);
        assert_eq!(r["joined"], 1);
    }
}
