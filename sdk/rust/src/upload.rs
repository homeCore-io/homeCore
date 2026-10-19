//! Files sent to a plugin's actions.
//!
//! `POST /plugins/{id}/upload/{action}` sends the file in pieces
//! (`upload_chunk`), then runs the action with `upload: {id, size, name}`.
//! The SDK keeps the pieces in a temporary file and hands the action a
//! `file_path` param in place of them — so an action reads a firmware image,
//! say, from a path, whoever sent it — and removes the file once the action
//! has replied. An action that keeps working after replying (a stream) should
//! copy the file first.

use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;

use base64::Engine;
use serde_json::{json, Value};

/// The most one upload may hold.
const MAX: u64 = 64 * 1024 * 1024;

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn file_for(id: &str) -> PathBuf {
    std::env::temp_dir().join(format!("hc-upload-{id}"))
}

/// Write one piece where it goes.
pub fn chunk(cmd: &Value) -> Value {
    let request_id = cmd["request_id"].as_str().unwrap_or("");
    let fail = |e: &str| json!({ "request_id": request_id, "status": "error", "error": e });
    let id = cmd["upload_id"].as_str().unwrap_or("");
    if !valid_id(id) {
        return fail("bad upload id");
    }
    let offset = cmd["offset"].as_u64().unwrap_or(0);
    let Ok(data) =
        base64::engine::general_purpose::STANDARD.decode(cmd["data"].as_str().unwrap_or(""))
    else {
        return fail("bad piece");
    };
    if offset + data.len() as u64 > MAX {
        return fail("the file is too large");
    }
    let path = file_for(id);
    let opened = if offset == 0 {
        std::fs::File::create(&path)
    } else {
        std::fs::OpenOptions::new().write(true).open(&path)
    };
    let written = opened.and_then(|mut f| {
        f.seek(SeekFrom::Start(offset))?;
        f.write_all(&data)
    });
    match written {
        Ok(()) => json!({ "request_id": request_id, "status": "ok" }),
        Err(e) => fail(&format!("keeping the piece: {e}")),
    }
}

/// The file an action was sent, when it was sent one and it is all there.
pub fn path_for(cmd: &Value) -> Option<PathBuf> {
    let up = cmd.get("upload")?;
    let id = up["id"].as_str().filter(|i| valid_id(i))?;
    let path = file_for(id);
    let size = std::fs::metadata(&path).ok()?.len();
    (up["size"].as_u64() == Some(size)).then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pieces_make_the_file_an_action_is_handed() {
        let id = format!("t{}", std::process::id());
        let b64 = |s: &[u8]| base64::engine::general_purpose::STANDARD.encode(s);
        let ok = |v: Value| assert_eq!(v["status"], "ok", "{v}");
        ok(chunk(
            &json!({"upload_id": id, "offset": 0, "data": b64(b"hello ")}),
        ));
        ok(chunk(
            &json!({"upload_id": id, "offset": 6, "data": b64(b"world")}),
        ));
        let cmd = json!({"action": "x", "upload": {"id": id, "size": 11}});
        let path = path_for(&cmd).expect("a complete file");
        assert_eq!(std::fs::read(&path).unwrap(), b"hello world");
        // A size that doesn't match is not a file to act on.
        assert!(path_for(&json!({"upload": {"id": id, "size": 12}})).is_none());
        std::fs::remove_file(path).unwrap();
        assert_eq!(
            chunk(&json!({"upload_id": "../x", "offset": 0, "data": ""}))["status"],
            "error"
        );
    }
}
