//! A Reolink for the tests, answering `/cgi-bin/api.cgi` on a local port:
//! sign-in, and the siren (`AudioAlarmPlay`), which it records and keeps the
//! state of. It can refuse every command, as an account without the
//! permission is refused, or only a manual play, as some firmware does.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Default)]
struct State {
    sent: Vec<Value>,
    sounding: HashMap<u32, bool>,
    refuse_all: bool,
    refuse_manual_on: bool,
}

#[derive(Clone)]
pub struct FakeReolink {
    pub addr: String,
    state: Arc<Mutex<State>>,
}

impl FakeReolink {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let state = Arc::new(Mutex::new(State::default()));
        let st = state.clone();
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                tokio::spawn(serve(sock, st.clone()));
            }
        });
        Self { addr, state }
    }

    /// The siren commands it took, their params, in order.
    pub fn sent(&self) -> Vec<Value> {
        self.state.lock().unwrap().sent.clone()
    }

    pub fn sounding(&self, channel: u32) -> bool {
        self.state
            .lock()
            .unwrap()
            .sounding
            .get(&channel)
            .copied()
            .unwrap_or(false)
    }

    pub fn refuse_all(&self, yes: bool) {
        self.state.lock().unwrap().refuse_all = yes;
    }

    pub fn refuse_manual_on(&self, yes: bool) {
        self.state.lock().unwrap().refuse_manual_on = yes;
    }
}

fn answer(c: &Value, st: &Mutex<State>) -> Value {
    let name = c["cmd"].as_str().unwrap_or_default();
    let ok = |v: Value| json!({ "cmd": name, "code": 0, "value": v });
    let err = |code: i64, detail: &str| json!({ "cmd": name, "code": 1, "error": { "rspCode": code, "detail": detail } });
    match name {
        "Login" => ok(json!({ "Token": { "name": "tok", "leaseTime": 3600 } })),
        "Logout" => ok(json!({ "rspCode": 200 })),
        "AudioAlarmPlay" => {
            let mut s = st.lock().unwrap();
            let p = &c["param"];
            let manual_on = p["alarm_mode"] == "manul" && p["manual_switch"] == 1;
            if s.refuse_all {
                return err(-26, "ability error");
            }
            if manual_on && s.refuse_manual_on {
                return err(-9, "not support");
            }
            let ch = p["channel"].as_u64().unwrap_or(0) as u32;
            let on = p["alarm_mode"] == "times" || manual_on;
            s.sounding.insert(ch, on);
            s.sent.push(p.clone());
            ok(json!({ "rspCode": 200 }))
        }
        _ => err(-9, "not support"),
    }
}

/// HTTP/1.1, kept alive, one JSON array of commands per request.
async fn serve(mut sock: TcpStream, st: Arc<Mutex<State>>) {
    let mut buf = Vec::new();
    loop {
        let head_end = loop {
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
            let mut chunk = [0u8; 4096];
            match sock.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
        let len = head
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.eq_ignore_ascii_case("content-length")
                    .then(|| v.trim().parse::<usize>().ok())?
            })
            .unwrap_or(0);
        while buf.len() < head_end + len {
            let mut chunk = [0u8; 4096];
            match sock.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        let body: Value =
            serde_json::from_slice(&buf[head_end..head_end + len]).unwrap_or(json!([]));
        buf.drain(..head_end + len);
        let out: Vec<Value> = body
            .as_array()
            .map(|cmds| cmds.iter().map(|c| answer(c, &st)).collect())
            .unwrap_or_default();
        let text = serde_json::to_string(&out).unwrap();
        let reply = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{text}",
            text.len()
        );
        if sock.write_all(reply.as_bytes()).await.is_err() {
            return;
        }
    }
}
