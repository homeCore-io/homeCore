//! The WebSocket to zwave-js-server: one connection at a time, requests
//! matched to their answers, and events handed on.
//!
//! **Every command waits for its own answer.** zwave-js-server answers each
//! command with a `result` frame carrying the `messageId` it was sent with.
//! This keeps a table of the callers waiting, by id, and wakes the right one —
//! so a `node.set_value` that the device refused is a refusal the caller sees,
//! not a line in the log, and an inclusion step knows whether it was accepted.
//!
//! **Nothing waits forever.** Connecting, the server's version greeting and
//! every request have deadlines. The socket is pinged every
//! [`PING_EVERY`]; a connection that sends nothing at all — not even the pong
//! — for [`IDLE_LIMIT`] is treated as gone, which is how a half-open TCP
//! connection (a server that lost power, a cable pulled) is noticed instead of
//! looking connected for hours.
//!
//! **Nothing sent while disconnected.** A request made with no connection
//! fails at once. There is no queue to replay later: a door unlocked an hour
//! after somebody asked, because that is when the server came back, is worse
//! than an unlock that said it failed.
//!
//! **Frames never go into errors.** A frame can carry a lock's user codes; an
//! error message ends up in the log and on the plugin page. Errors here say
//! what went wrong and how long the frame was, never what was in it.

use anyhow::{anyhow, bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, warn};
use uuid::Uuid;

use crate::types::{RawEvent, VersionMsg};

/// How long the TCP + WebSocket handshake may take.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the server has to say hello once the socket is up.
pub const VERSION_TIMEOUT: Duration = Duration::from_secs(10);
/// A ping this often keeps the connection's silence meaningful.
pub const PING_EVERY: Duration = Duration::from_secs(20);
/// Nothing at all from the server for this long: the connection is gone.
pub const IDLE_LIMIT: Duration = Duration::from_secs(50);
/// The deadline for an ordinary request.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// What zwave-js said instead of an answer — or that there was no answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZwError {
    /// `not_connected`, `timeout`, `disconnected`, or zwave-js's own
    /// `errorCode` (`zwave_error`, `node_not_found`, …).
    pub code: String,
    /// In words, for a person. zwave-js's own message where it gave one.
    pub message: String,
}

impl ZwError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn not_connected() -> Self {
        Self::new(
            "not_connected",
            "Z-Wave JS is not connected, so nothing was sent",
        )
    }
}

impl std::fmt::Display for ZwError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ZwError {}

type Pending = HashMap<String, oneshot::Sender<Result<Value, ZwError>>>;

struct Inner {
    /// The live connection's outbox, with the connection's number; `None`
    /// while disconnected. The number stops a connection that is still
    /// closing from clearing the one that replaced it.
    out: Mutex<Option<(u64, mpsc::UnboundedSender<Message>)>>,
    generation: std::sync::atomic::AtomicU64,
    pending: Mutex<Pending>,
    /// Connected and past the handshake: device commands may go.
    ready: watch::Sender<bool>,
    /// How often to ping, and how long silence means the connection is gone.
    ping_every: Duration,
    idle_limit: Duration,
    greet_limit: Duration,
}

/// A handle on the connection, cheap to clone. Requests go through whichever
/// connection is up.
#[derive(Clone)]
pub struct ZwClient {
    inner: Arc<Inner>,
}

impl Default for ZwClient {
    fn default() -> Self {
        Self::new()
    }
}

/// What the connection hands the bridge.
#[derive(Debug)]
pub enum Incoming {
    Event(RawEvent),
    /// The connection ended; why, in words that carry no frame content.
    Closed(String),
}

/// One live connection.
pub struct Session {
    pub version: VersionMsg,
    pub incoming: mpsc::UnboundedReceiver<Incoming>,
}

impl ZwClient {
    pub fn new() -> Self {
        Self::with_timing(PING_EVERY, IDLE_LIMIT)
    }

    /// How long the server has to say which version it is.
    #[cfg(test)]
    fn greeting_within(self, limit: Duration) -> Self {
        let inner = Arc::try_unwrap(self.inner).ok().expect("not shared yet");
        Self {
            inner: Arc::new(Inner {
                greet_limit: limit,
                ..inner
            }),
        }
    }

    /// With its own ping interval and silence limit (tests use short ones).
    pub fn with_timing(ping_every: Duration, idle_limit: Duration) -> Self {
        let (ready, _) = watch::channel(false);
        Self {
            inner: Arc::new(Inner {
                out: Mutex::new(None),
                pending: Mutex::new(HashMap::new()),
                generation: std::sync::atomic::AtomicU64::new(0),
                ready,
                ping_every,
                idle_limit,
                greet_limit: VERSION_TIMEOUT,
            }),
        }
    }

    /// Connected and through the handshake.
    pub fn is_ready(&self) -> bool {
        *self.inner.ready.borrow()
    }

    pub(crate) fn set_ready(&self, ready: bool) {
        self.inner.ready.send_replace(ready);
    }

    /// Wait up to `limit` for the connection to be ready. `false` if it was
    /// not by then.
    pub async fn wait_ready(&self, limit: Duration) -> bool {
        let mut rx = self.inner.ready.subscribe();
        if *rx.borrow() {
            return true;
        }
        tokio::time::timeout(limit, async {
            while rx.changed().await.is_ok() {
                if *rx.borrow() {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false)
    }

    /// Send a command once the connection is ready, and wait for its answer.
    pub async fn request(&self, cmd: Value, timeout: Duration) -> Result<Value, ZwError> {
        if !self.is_ready() {
            return Err(ZwError::not_connected());
        }
        self.request_raw(cmd, timeout).await
    }

    /// Send a command on the connection whether or not the handshake is done
    /// (the handshake itself), and wait for its answer.
    pub async fn request_raw(&self, mut cmd: Value, timeout: Duration) -> Result<Value, ZwError> {
        let id = format!("hc-{}", Uuid::new_v4());
        cmd["messageId"] = json!(id);
        let text = serde_json::to_string(&cmd)
            .map_err(|_| ZwError::new("invalid", "the command could not be written"))?;
        let (tx, rx) = oneshot::channel();
        {
            let out = self.inner.out.lock().unwrap_or_else(|e| e.into_inner());
            let Some((_, out)) = out.as_ref() else {
                return Err(ZwError::not_connected());
            };
            self.inner
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(id.clone(), tx);
            if out.send(Message::Text(text.into())).is_err() {
                self.forget(&id);
                return Err(ZwError::not_connected());
            }
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(answer)) => answer,
            Ok(Err(_)) => Err(ZwError::new(
                "disconnected",
                "the connection to Z-Wave JS closed before it answered",
            )),
            Err(_) => {
                self.forget(&id);
                Err(ZwError::new(
                    "timeout",
                    format!("Z-Wave JS did not answer within {} s", timeout.as_secs()),
                ))
            }
        }
    }

    fn forget(&self, id: &str) {
        self.inner
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
    }

    /// Hand an answer to whoever is waiting for it.
    fn answer(&self, id: &str, answer: Result<Value, ZwError>) {
        let waiting = self
            .inner
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
        if let Some(tx) = waiting {
            let _ = tx.send(answer);
        }
    }

    /// Connection `generation` is gone: nobody gets an answer, nothing more
    /// is sent. A connection already replaced leaves its successor alone.
    fn drop_connection(&self, generation: u64) {
        {
            let mut out = self.inner.out.lock().unwrap_or_else(|e| e.into_inner());
            match out.as_ref() {
                Some((g, _)) if *g != generation => return,
                _ => *out = None,
            }
        }
        self.set_ready(false);
        let waiting: Vec<_> = self
            .inner
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .collect();
        for (_, tx) in waiting {
            let _ = tx.send(Err(ZwError::new(
                "disconnected",
                "the connection to Z-Wave JS closed before it answered",
            )));
        }
    }

    /// Connect, and wait for the server's greeting. Events and the end of the
    /// connection arrive on the session's `incoming`.
    pub async fn open(&self, url: &str) -> Result<Session> {
        let (ws, _) = tokio::time::timeout(CONNECT_TIMEOUT, connect_async(url))
            .await
            .map_err(|_| anyhow!("no answer within {} s", CONNECT_TIMEOUT.as_secs()))?
            .context("could not connect")?;
        let (mut sink, mut stream) = ws.split();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Message>();
        let (in_tx, in_rx) = mpsc::unbounded_channel::<Incoming>();
        let (version_tx, version_rx) = oneshot::channel::<VersionMsg>();
        let generation = self
            .inner
            .generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        *self.inner.out.lock().unwrap_or_else(|e| e.into_inner()) = Some((generation, out_tx));

        let me = self.clone();
        tokio::spawn(async move {
            let mut version_tx = Some(version_tx);
            let idle_limit = me.inner.idle_limit;
            let mut ping = tokio::time::interval(me.inner.ping_every);
            ping.tick().await;
            // When the server was last heard from — anything at all, a pong
            // included. A deadline of its own: a timeout around the read
            // would start again every time the ping went out, and never
            // fire.
            let mut heard = tokio::time::Instant::now();
            let why = loop {
                tokio::select! {
                    out = out_rx.recv() => match out {
                        Some(m) => {
                            if let Err(e) = sink.send(m).await {
                                break format!("could not send: {e}");
                            }
                        }
                        None => break "closed by the plugin".to_string(),
                    },
                    _ = ping.tick() => {
                        if let Err(e) = sink.send(Message::Ping(Vec::new().into())).await {
                            break format!("could not send: {e}");
                        }
                    }
                    _ = tokio::time::sleep_until(heard + idle_limit) => {
                        break format!(
                            "nothing heard for {} s — the connection is gone",
                            idle_limit.as_secs_f32().round()
                        );
                    }
                    frame = stream.next() => {
                        heard = tokio::time::Instant::now();
                        match frame {
                            None => break "the server closed the connection".to_string(),
                            Some(Err(e)) => break format!("read error: {e}"),
                            Some(Ok(Message::Text(text))) => {
                                me.on_text(&text, &mut version_tx, &in_tx);
                            }
                            Some(Ok(Message::Close(_))) => {
                                break "the server closed the connection".to_string();
                            }
                            Some(Ok(_)) => {}
                        }
                    }
                }
            };
            me.drop_connection(generation);
            let _ = sink.close().await;
            let _ = in_tx.send(Incoming::Closed(why));
        });

        let version = match tokio::time::timeout(self.inner.greet_limit, version_rx).await {
            Ok(Ok(v)) => v,
            Ok(Err(_)) => {
                self.close();
                bail!("the connection closed before the server said which version it is");
            }
            Err(_) => {
                self.close();
                bail!(
                    "the server did not say which version it is within {} s",
                    self.inner.greet_limit.as_secs()
                );
            }
        };
        Ok(Session {
            version,
            incoming: in_rx,
        })
    }

    /// End the current connection, if any.
    pub fn close(&self) {
        // Dropping the outbox ends the connection task, which tells everyone.
        *self.inner.out.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.set_ready(false);
    }

    fn on_text(
        &self,
        text: &str,
        version_tx: &mut Option<oneshot::Sender<VersionMsg>>,
        in_tx: &mpsc::UnboundedSender<Incoming>,
    ) {
        let Ok(frame) = serde_json::from_str::<Value>(text) else {
            debug!(bytes = text.len(), "A frame that is not JSON — ignored");
            return;
        };
        match frame.get("type").and_then(Value::as_str) {
            Some("version") => match serde_json::from_value::<VersionMsg>(frame) {
                Ok(v) => {
                    if let Some(tx) = version_tx.take() {
                        let _ = tx.send(v);
                    }
                }
                Err(_) => warn!(
                    bytes = text.len(),
                    "The server's version frame did not parse"
                ),
            },
            Some("result") => {
                let Some(id) = frame.get("messageId").and_then(Value::as_str) else {
                    return;
                };
                let id = id.to_string();
                self.answer(&id, result_of(&frame));
            }
            Some("event") => {
                if let Some(ev) = frame.get("event").cloned().and_then(RawEvent::from_value) {
                    let _ = in_tx.send(Incoming::Event(ev));
                }
            }
            _ => debug!(bytes = text.len(), "A frame of a type this does not know"),
        }
    }
}

/// A `result` frame as an answer: its `result` on success, zwave-js's error
/// otherwise.
pub fn result_of(frame: &Value) -> Result<Value, ZwError> {
    if frame.get("success").and_then(Value::as_bool) == Some(true) {
        return Ok(frame.get("result").cloned().unwrap_or(Value::Null));
    }
    let code = frame
        .get("errorCode")
        .and_then(Value::as_str)
        .unwrap_or("failed")
        .to_string();
    let message = frame
        .get("zwaveErrorMessage")
        .or_else(|| frame.get("message"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| match code.as_str() {
            "unknown_command" => "Z-Wave JS does not know that command".into(),
            "node_not_found" => "Z-Wave JS does not know that node".into(),
            "schema_incompatible" => "Z-Wave JS cannot speak this plugin's schema".into(),
            "inclusion_already_in_progress" => "the controller is already adding a device".into(),
            "inclusion_phase_not_in_progress" => "the controller is not waiting for that".into(),
            _ => format!("Z-Wave JS refused it ({code})"),
        });
    Err(ZwError { code, message })
}

/// How a `node.set_value` went, from its answer.
///
/// Schema 29+ answers `{result: {status, message?}}` — success *or not* — with
/// `success: true` on the frame; older schemas answer `{success: bool}`.
/// `SetValueStatus`: 0 NoDeviceSupport, 1 Working, 2 Fail, 3 EndpointNotFound,
/// 4 NotImplemented, 5 InvalidValue, 254 SuccessUnsupervised, 255 Success.
pub fn set_value_outcome(answer: &Value) -> Result<(), String> {
    if let Some(status) = answer
        .get("result")
        .and_then(|r| r.get("status"))
        .and_then(Value::as_u64)
    {
        let said = answer
            .get("result")
            .and_then(|r| r.get("message"))
            .and_then(Value::as_str);
        return match status {
            1 | 254 | 255 => Ok(()),
            0 => Err("the device does not support that".into()),
            2 => Err(said
                .map(str::to_string)
                .unwrap_or_else(|| "the device did not do it".into())),
            3 => Err("the device has no such endpoint".into()),
            4 => Err("Z-Wave JS cannot do that on this device".into()),
            5 => Err(said
                .map(str::to_string)
                .unwrap_or_else(|| "the device does not take that value".into())),
            other => Err(format!("the device answered {other}")),
        };
    }
    match answer.get("success").and_then(Value::as_bool) {
        Some(false) => Err("the device did not do it".into()),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_value_status_is_read_even_when_the_frame_says_success() {
        assert!(set_value_outcome(&json!({ "result": { "status": 255 } })).is_ok());
        assert!(set_value_outcome(&json!({ "result": { "status": 254 } })).is_ok());
        assert!(set_value_outcome(&json!({ "result": { "status": 1 } })).is_ok());
        assert_eq!(
            set_value_outcome(&json!({ "result": { "status": 2 } })).unwrap_err(),
            "the device did not do it"
        );
        assert!(set_value_outcome(&json!({ "result": { "status": 0 } })).is_err());
        assert!(set_value_outcome(
            &json!({ "result": { "status": 5, "message": "out of range" } })
        )
        .unwrap_err()
        .contains("out of range"));
        // Schema < 29.
        assert!(set_value_outcome(&json!({ "success": true })).is_ok());
        assert!(set_value_outcome(&json!({ "success": false })).is_err());
        assert!(set_value_outcome(&json!({})).is_ok());
    }

    #[test]
    fn a_failed_result_says_what_zwave_js_said_and_nothing_else() {
        let e = result_of(&json!({
            "type": "result", "success": false, "messageId": "x",
            "errorCode": "zwave_error", "zwaveErrorCode": 202,
            "zwaveErrorMessage": "Node 5 is not reachable"
        }))
        .unwrap_err();
        assert_eq!(e.code, "zwave_error");
        assert_eq!(e.message, "Node 5 is not reachable");
        let e = result_of(&json!({ "success": false, "errorCode": "node_not_found" })).unwrap_err();
        assert_eq!(e.message, "Z-Wave JS does not know that node");
        assert_eq!(
            result_of(&json!({ "success": true, "result": { "a": 1 } })).unwrap(),
            json!({ "a": 1 })
        );
    }

    /// With no connection a request fails at once — nothing is kept to send
    /// later (H6).
    #[tokio::test]
    async fn a_request_with_no_connection_fails_at_once_and_is_not_kept() {
        let c = ZwClient::new();
        let t = std::time::Instant::now();
        let e = c
            .request(json!({ "command": "node.set_value" }), REQUEST_TIMEOUT)
            .await
            .unwrap_err();
        assert_eq!(e.code, "not_connected");
        assert!(t.elapsed() < Duration::from_millis(200));
        let e = c
            .request_raw(json!({ "command": "node.set_value" }), REQUEST_TIMEOUT)
            .await
            .unwrap_err();
        assert_eq!(e.code, "not_connected");
        assert!(c.inner.pending.lock().unwrap().is_empty());
    }

    /// M2: a server that says hello and then nothing — not even a pong —
    /// is noticed as gone, though the plugin keeps pinging it.
    #[tokio::test]
    async fn a_server_that_goes_silent_is_noticed() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            ws.send(Message::Text(
                json!({ "type": "version", "driverVersion": "x", "serverVersion": "y",
                        "minSchemaVersion": 0, "maxSchemaVersion": 40 })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
            // Hold the socket open and never read it: no pong goes back.
            tokio::time::sleep(Duration::from_secs(30)).await;
            drop(ws);
        });
        let c = ZwClient::with_timing(Duration::from_millis(100), Duration::from_millis(600));
        let mut session = c.open(&format!("ws://{addr}")).await.unwrap();
        assert_eq!(session.version.server_version, "y");
        let t = std::time::Instant::now();
        let why = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(Incoming::Closed(why)) = session.incoming.recv().await {
                    return why;
                }
            }
        })
        .await
        .expect("the silence was noticed");
        assert!(why.contains("nothing heard"), "{why}");
        assert!(t.elapsed() < Duration::from_secs(3));
        assert!(!c.is_ready());
    }

    /// A server that never says which version it is fails the connection.
    #[tokio::test]
    async fn a_server_that_never_greets_fails_to_connect() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let _ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        let c = ZwClient::new().greeting_within(Duration::from_millis(500));
        let t = std::time::Instant::now();
        let e = c.open(&format!("ws://{addr}")).await.err().unwrap();
        assert!(e.to_string().contains("version"), "{e}");
        assert!(t.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn waiting_for_ready_gives_up() {
        let c = ZwClient::new();
        assert!(!c.wait_ready(Duration::from_millis(50)).await);
        c.set_ready(true);
        assert!(c.wait_ready(Duration::from_millis(50)).await);
    }

    /// An answer reaches the caller it belongs to, and only that one.
    #[tokio::test]
    async fn answers_are_matched_by_message_id() {
        let c = ZwClient::new();
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        *c.inner.out.lock().unwrap() = Some((1, tx));
        c.set_ready(true);
        let c2 = c.clone();
        let waiter = tokio::spawn(async move {
            c2.request(
                json!({ "command": "node.ping", "nodeId": 3 }),
                REQUEST_TIMEOUT,
            )
            .await
        });
        let Some(Message::Text(sent)) = rx.recv().await else {
            panic!("nothing sent");
        };
        let sent: Value = serde_json::from_str(&sent).unwrap();
        let id = sent["messageId"].as_str().unwrap().to_string();
        assert_eq!(sent["command"], "node.ping");
        c.answer("someone-else", Ok(json!({ "responded": false })));
        c.answer(
            &id,
            result_of(&json!({ "success": true, "result": { "responded": true } })),
        );
        assert_eq!(waiter.await.unwrap().unwrap(), json!({ "responded": true }));
        // A dropped connection answers everyone still waiting.
        let c3 = c.clone();
        let waiter = tokio::spawn(async move {
            c3.request(json!({ "command": "node.ping" }), REQUEST_TIMEOUT)
                .await
        });
        let _ = rx.recv().await;
        // An older connection closing late leaves this one alone.
        c.drop_connection(0);
        assert!(c.inner.out.lock().unwrap().is_some());
        c.drop_connection(1);
        assert_eq!(waiter.await.unwrap().unwrap_err().code, "disconnected");
        assert!(!c.is_ready());
    }
}
