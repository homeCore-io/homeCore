//! A stub ECP server, shared by the tests that need to see what actually
//! goes on the wire.
//!
//! Parsing tests can't catch a wrong HTTP verb, a double-encoded path
//! segment, or a command that sends two requests where it should send
//! one — and all three produce a Roku that appears to ignore homeCore
//! while every log line says success.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::ecp::EcpClient;

/// Requests the stub received, as `("POST", "/keypress/Home")`.
pub type Log = Arc<Mutex<Vec<(String, String)>>>;

/// Serve `count` requests, answering each with `status` and `body`, and
/// record what was asked for.
///
/// The listener is bound to port 0 so tests can run concurrently, and it
/// stops after `count` requests — a test that expects three requests and
/// gets four hangs on the fourth rather than passing quietly.
pub async fn stub(count: usize, status: u16, body: &'static str) -> (EcpClient, Log) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let log_for_task = Arc::clone(&log);

    tokio::spawn(async move {
        for _ in 0..count {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let mut buf = vec![0u8; 4096];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            if let Some(line) = req.lines().next() {
                let mut parts = line.split_whitespace();
                let method = parts.next().unwrap_or_default().to_string();
                let path = parts.next().unwrap_or_default().to_string();
                log_for_task.lock().unwrap().push((method, path));
            }
            let reason = if status == 200 { "OK" } else { "Forbidden" };
            let resp = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: text/xml\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.flush().await;
        }
    });

    let client =
        EcpClient::new(&addr.ip().to_string(), addr.port(), Duration::from_secs(5)).unwrap();
    (client, log)
}

/// Paths the stub was asked for, in order.
pub fn paths(log: &Log) -> Vec<String> {
    log.lock().unwrap().iter().map(|(_, p)| p.clone()).collect()
}
