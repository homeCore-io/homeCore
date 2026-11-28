//! `/rotations`: shared lists of device commands a rule plays from, one at a
//! a time (see [`hc_types::rotation`]).
//!
//! Reading and changing them is `automations:read` / `automations:write`, as
//! for the rules that use them. Playing one, or moving it on, sends a device
//! a command, so it takes `devices:command`.

use crate::auth_middleware::{AutomationsRead, AutomationsWrite, DevicesCommand};
use crate::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use hc_core::rotations::{self, Outcome};
use hc_types::device::{with_command_change_metadata, DeviceChange};
use hc_types::rotation::{Rotation, RotationAdvance, RotationChoice, RotationOrder, RotationState};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

/// Which of its choices may play now; all of them when that can't be read.
async fn for_now(s: &AppState, r: &Rotation) -> Vec<bool> {
    rotations::for_now(&s.store, r, &hc_time::now_local())
        .await
        .unwrap_or_else(|_| vec![true; r.choices.len()])
}

/// The view, with what may play now and the device's own names.
async fn shown(s: &AppState, r: &Rotation, state: &RotationState) -> Value {
    let names = rotations::names(&s.store, r).await.unwrap_or_default();
    view(r, state, &for_now(s, r).await, &names)
}

fn err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({ "error": msg.into() }))).into_response()
}

fn failed(e: impl std::fmt::Display) -> Response {
    err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

/// A rotation as the API shows it: as written, and where it is.
/// `for_now`: which choices may play now (their times, days, modes);
/// "next" passes over the others, as playing would.
/// `names`: each choice as its device lists it (a WLED preset's name, not
/// its number); the command's own words where it has none.
fn view(rotation: &Rotation, state: &RotationState, for_now: &[bool], names: &[String]) -> Value {
    let name = |i: usize| {
        names
            .get(i)
            .cloned()
            .or_else(|| rotation.choices.get(i).map(RotationChoice::name))
    };
    // What plays next, where that can be known: in turn, or the rest of a
    // shuffled round. Random can't say.
    // A choice known to be missing is passed over, as playing it would.
    let n = rotation.choices.len();
    let here = |i: &usize| !state.missing.contains(i) && for_now.get(*i).copied().unwrap_or(true);
    let next = match rotation.order {
        RotationOrder::InTurn if n > 0 => (0..n).map(|k| (state.position + k) % n).find(here),
        RotationOrder::Shuffle => state.order.iter().skip(state.position).copied().find(here),
        _ => None,
    };
    let mut v = json!(rotation);
    v["now"] = json!({
        "current": state.current,
        "current_name": state.current.and_then(name),
        "next": next,
        "next_name": next.and_then(name),
        "missing": state.missing,
        "missing_names": state.missing.iter().filter_map(|&i| name(i)).collect::<Vec<_>>(),
        "last_played": state.last_played,
        "for_now": for_now,
    });
    v
}

#[derive(Debug, Deserialize)]
pub struct RotationBody {
    pub name: String,
    pub device_id: String,
    pub choices: Vec<RotationChoice>,
    #[serde(default)]
    pub order: RotationOrder,
    #[serde(default)]
    pub advance: RotationAdvance,
}

/// What's wrong with a rotation, in words for whoever is making it.
async fn invalid(s: &AppState, b: &RotationBody) -> Option<String> {
    if b.name.trim().is_empty() {
        return Some("a rotation needs a name".into());
    }
    if b.choices.is_empty() {
        return Some("a rotation needs at least one choice".into());
    }
    if let Some(i) = b
        .choices
        .iter()
        .position(|c| c.command.as_object().is_none_or(|o| o.is_empty()))
    {
        return Some(format!("choice {} has no command", i + 1));
    }
    match &b.advance {
        RotationAdvance::Every { n: 0, .. } => {
            return Some("a rotation moves on every 1 or more hours, days or weeks".into())
        }
        RotationAdvance::Schedule { cron } if !rotations::cron_is_valid(cron) => {
            return Some(format!(
                "“{cron}” isn't a schedule homeCore can read: six parts, seconds first (0 0 */2 * * * is every two hours)"
            ))
        }
        _ => {}
    }
    for id in b
        .choices
        .iter()
        .filter_map(|c| c.only_when.as_ref())
        .flat_map(|w| &w.modes)
    {
        match s.store.get_device(id).await {
            Ok(Some(_)) => {}
            Ok(None) => return Some(format!("there is no mode “{id}”")),
            Err(e) => return Some(e.to_string()),
        }
    }
    match s.store.get_device(&b.device_id).await {
        Ok(Some(_)) => {}
        Ok(None) => return Some(format!("there is no device “{}”", b.device_id)),
        Err(e) => return Some(e.to_string()),
    }
    // Each choice's command is one the device takes: a typo'd or another
    // plugin's action was saved, and every play then sent what the device
    // ignored. Only checked when the device says what it takes.
    let schema = s.store.get_device_schema(&b.device_id).await.ok().flatten();
    if let Some(schema) = schema.filter(|sc| !sc.actions.is_empty()) {
        for (i, c) in b.choices.iter().enumerate() {
            let Some(action) = c.command.get("action").and_then(|a| a.as_str()) else {
                continue;
            };
            if !schema.actions.iter().any(|a| a.id == action) {
                return Some(format!(
                    "choice {}: “{}” has no “{action}” command",
                    i + 1,
                    b.device_id
                ));
            }
        }
    }
    None
}

/// `GET /rotations`
pub async fn list_rotations(State(s): State<AppState>, _: AutomationsRead) -> Response {
    let list = match s.store.list_rotations().await {
        Ok(l) => l,
        Err(e) => return failed(e),
    };
    let mut out = Vec::with_capacity(list.len());
    for r in &list {
        let state = s.store.rotation_state(r.id).await.unwrap_or_default();
        out.push(shown(&s, r, &state).await);
    }
    (StatusCode::OK, Json(json!(out))).into_response()
}

/// `GET /rotations/{id}`
pub async fn get_rotation(
    State(s): State<AppState>,
    _: AutomationsRead,
    Path(id): Path<Uuid>,
) -> Response {
    match s.store.get_rotation(id).await {
        Ok(Some(r)) => {
            let state = s.store.rotation_state(id).await.unwrap_or_default();
            (StatusCode::OK, Json(shown(&s, &r, &state).await)).into_response()
        }
        Ok(None) => err(StatusCode::NOT_FOUND, "rotation not found"),
        Err(e) => failed(e),
    }
}

/// `POST /rotations`
pub async fn create_rotation(
    State(s): State<AppState>,
    _: AutomationsWrite,
    Json(body): Json<RotationBody>,
) -> Response {
    if let Some(why) = invalid(&s, &body).await {
        return err(StatusCode::UNPROCESSABLE_ENTITY, why);
    }
    let r = Rotation {
        id: Uuid::new_v4(),
        name: body.name.trim().to_string(),
        device_id: body.device_id,
        choices: body.choices,
        order: body.order,
        advance: body.advance,
    };
    match rotations::save(&s.store, &r).await {
        Ok(()) => (
            StatusCode::CREATED,
            Json(shown(&s, &r, &RotationState::default()).await),
        )
            .into_response(),
        Err(e) => failed(e),
    }
}

/// `PUT /rotations/{id}`. Changing its choices, order or device starts it afresh;
/// a new name or pace keeps its place.
pub async fn update_rotation(
    State(s): State<AppState>,
    _: AutomationsWrite,
    Path(id): Path<Uuid>,
    Json(body): Json<RotationBody>,
) -> Response {
    match s.store.get_rotation(id).await {
        Ok(Some(_)) => {}
        Ok(None) => return err(StatusCode::NOT_FOUND, "rotation not found"),
        Err(e) => return failed(e),
    }
    if let Some(why) = invalid(&s, &body).await {
        return err(StatusCode::UNPROCESSABLE_ENTITY, why);
    }
    let r = Rotation {
        id,
        name: body.name.trim().to_string(),
        device_id: body.device_id,
        choices: body.choices,
        order: body.order,
        advance: body.advance,
    };
    if let Err(e) = rotations::save(&s.store, &r).await {
        return failed(e);
    }
    let state = s.store.rotation_state(id).await.unwrap_or_default();
    (StatusCode::OK, Json(shown(&s, &r, &state).await)).into_response()
}

/// `DELETE /rotations/{id}`
pub async fn delete_rotation(
    State(s): State<AppState>,
    _: AutomationsWrite,
    Path(id): Path<Uuid>,
) -> Response {
    match rotations::delete(&s.store, id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => err(StatusCode::NOT_FOUND, "rotation not found"),
        Err(e) => failed(e),
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct PlayBody {
    /// Another device of the same kind to play it on.
    #[serde(default)]
    pub device_id: Option<String>,
}

/// `POST /rotations/{id}/play`: play the choice for now, as the rule step
/// does.
pub async fn play_rotation(
    State(s): State<AppState>,
    DevicesCommand(claims): DevicesCommand,
    Path(id): Path<Uuid>,
    body: Option<Json<PlayBody>>,
) -> Response {
    let device = body.and_then(|Json(b)| b.device_id);
    let c = match rotations::choose(&s.store, id, device.as_deref(), false).await {
        Ok(Outcome::Chosen(c)) => c,
        Ok(Outcome::NoRotation) => return err(StatusCode::NOT_FOUND, "rotation not found"),
        Ok(Outcome::BadDevice { why, .. }) => return err(StatusCode::UNPROCESSABLE_ENTITY, why),
        Ok(Outcome::NothingForNow { rotation }) => {
            return err(
                StatusCode::CONFLICT,
                format!("none of {}’s choices is for now", rotation.name),
            )
        }
        Ok(Outcome::NothingOffered { rotation, missing }) => {
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": format!(
                        "of {}’s choices for now, the device has none any more",
                        rotation.name
                    ),
                    "missing": missing,
                })),
            )
                .into_response()
        }
        Err(e) => return failed(e),
    };
    let change = DeviceChange::homecore("rotation")
        .with_actor(Some(claims.uid.clone()), Some(claims.sub.clone()))
        .with_correlation_id(Some(Uuid::new_v4().to_string()));
    let payload = with_command_change_metadata(c.command.clone(), &change);
    if let Some(ph) = &s.publish {
        let topic = format!("homecore/devices/{}/cmd", c.device_id);
        let bytes = serde_json::to_vec(&payload).unwrap_or_default();
        if let Err(e) = ph.publish(&topic, bytes).await {
            // It didn't play: the rotation stays where it was, rather than
            // having moved on past a choice that never played.
            let _ = rotations::undo(&s.store, &c).await;
            return failed(e);
        }
    }
    let _ = s
        .event_bus
        .publish(hc_types::event::Event::DeviceCommandSent {
            timestamp: chrono::Utc::now(),
            device_id: c.device_id.clone(),
            device_name: None,
            command: payload,
            source: "rotation".into(),
            source_id: Some(id.to_string()),
            correlation_id: change.correlation_id.clone(),
        });
    (
        StatusCode::OK,
        Json(json!({
            "played": c.name,
            "index": c.index,
            "device_id": c.device_id,
            "missing": c.missing,
        })),
    )
        .into_response()
}

/// `POST /rotations/{id}/advance`: move on without playing ("skip this
/// one"); the next use plays the new choice.
pub async fn advance_rotation(
    State(s): State<AppState>,
    _: DevicesCommand,
    Path(id): Path<Uuid>,
) -> Response {
    match rotations::choose(&s.store, id, None, true).await {
        Ok(Outcome::Chosen(c)) => (
            StatusCode::OK,
            Json(json!({ "current": c.index, "current_name": c.name, "missing": c.missing })),
        )
            .into_response(),
        Ok(Outcome::NoRotation) => err(StatusCode::NOT_FOUND, "rotation not found"),
        Ok(Outcome::BadDevice { why, .. }) => err(StatusCode::CONFLICT, why),
        Ok(Outcome::NothingForNow { rotation }) => err(
            StatusCode::CONFLICT,
            format!("none of {}’s choices is for now", rotation.name),
        ),
        Ok(Outcome::NothingOffered { rotation, .. }) => err(
            StatusCode::CONFLICT,
            format!(
                "the device no longer offers any of {}’s choices",
                rotation.name
            ),
        ),
        Err(e) => failed(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rotation(order: RotationOrder) -> Rotation {
        Rotation {
            id: Uuid::nil(),
            name: "Bathroom music".into(),
            device_id: "sonos_bathroom".into(),
            choices: ["Chill", "Jazz", "Folk"]
                .iter()
                .map(|p| RotationChoice {
                    label: None,
                    command: json!({ "action": "play_playlist", "playlist": p }),
                    only_when: None,
                })
                .collect(),
            order,
            advance: RotationAdvance::EachUse,
        }
    }

    #[test]
    fn the_view_says_what_plays_now_and_next() {
        let state = RotationState {
            current: Some(0),
            position: 1,
            missing: vec![2],
            ..Default::default()
        };
        let v = view(&rotation(RotationOrder::InTurn), &state, &[], &[]);
        assert_eq!(v["now"]["current_name"], "Chill");
        assert_eq!(v["now"]["next_name"], "Jazz");
        // Folk is missing: after Jazz comes Chill again.
        let after_jazz = RotationState {
            position: 2,
            ..state.clone()
        };
        let v2 = view(&rotation(RotationOrder::InTurn), &after_jazz, &[], &[]);
        // Chill isn't for now: after Jazz comes nothing else that is.
        let v3 = view(
            &rotation(RotationOrder::InTurn),
            &after_jazz,
            &[false, true, true],
            &[],
        );
        assert_eq!(v3["now"]["next_name"], "Jazz");
        assert_eq!(v2["now"]["next_name"], "Chill");
        assert_eq!(v["now"]["missing_names"], json!(["Folk"]));
        assert_eq!(v["name"], "Bathroom music");
        // Random can't know what's next.
        let v = view(&rotation(RotationOrder::Random), &state, &[], &[]);
        assert!(v["now"]["next"].is_null());
    }
}
