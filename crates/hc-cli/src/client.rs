//! HTTP client that speaks to the homeCore API over either TCP or
//! a Unix domain socket.
//!
//! The UDS path grants implicit Admin scope (see `auth.admin_uds` on the
//! server side), so same-host admin tooling needs no token. The TCP path
//! is used for remote or token-auth operations.

use anyhow::{anyhow, bail, Context, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, StatusCode};
use hyper_util::client::legacy::Client as HyperClient;
use hyperlocal::{UnixClientExt, UnixConnector, Uri as UnixUri};
use serde::{de::DeserializeOwned, Serialize};
use std::path::{Path, PathBuf};

/// Transport selection — how the client talks to the server.
#[derive(Debug, Clone)]
pub enum Transport {
    Tcp {
        /// Base URL like `http://127.0.0.1:8080`. No trailing slash.
        base_url: String,
        /// Optional bearer token — applied as `Authorization: Bearer ...`.
        token: Option<String>,
    },
    Uds {
        /// Path to the admin socket, e.g. `/run/homecore/admin.sock`.
        socket: PathBuf,
    },
}

/// HTTP client for the homeCore API.
#[derive(Clone)]
pub struct Client {
    transport: Transport,
    uds_client: HyperClient<UnixConnector, Full<Bytes>>,
    tcp_client: reqwest::Client,
}

impl Client {
    pub fn new(transport: Transport) -> Self {
        let uds_client: HyperClient<UnixConnector, Full<Bytes>> =
            HyperClient::unix();
        let tcp_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("reqwest client builder should never fail with default options");
        Self {
            transport,
            uds_client,
            tcp_client,
        }
    }

    pub fn transport(&self) -> &Transport {
        &self.transport
    }

    /// Cheap reachability probe. Hits `/api/v1/health` with a short timeout.
    pub async fn probe(&self) -> Result<()> {
        let _: serde_json::Value = self.get("/health").await?;
        Ok(())
    }

    /// `GET {base}/api/v1/{path}` — `path` starts with `/`.
    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.request(Method::GET, path, Option::<&()>::None).await
    }

    /// `POST {base}/api/v1/{path}` with a JSON body.
    pub async fn post<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T> {
        self.request(Method::POST, path, Some(body)).await
    }

    /// `DELETE {base}/api/v1/{path}` — no body, ignores response body.
    pub async fn delete(&self, path: &str) -> Result<()> {
        let _: serde_json::Value = self
            .request(Method::DELETE, path, Option::<&()>::None)
            .await
            .unwrap_or(serde_json::Value::Null);
        Ok(())
    }

    /// `PATCH {base}/api/v1/{path}` with a JSON body.
    pub async fn patch<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T> {
        self.request(Method::PATCH, path, Some(body)).await
    }

    async fn request<B: Serialize, T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<T> {
        let full_path = format!("/api/v1{path}");
        let (status, bytes) = match &self.transport {
            Transport::Tcp { base_url, token } => {
                let url = format!("{}{}", base_url.trim_end_matches('/'), full_path);
                let mut req = self.tcp_client.request(method.clone(), &url);
                if let Some(t) = token {
                    req = req.bearer_auth(t);
                }
                if let Some(b) = body {
                    req = req.json(b);
                }
                let resp = req.send().await.with_context(|| format!("{method} {url}"))?;
                let status = resp.status();
                let bytes = resp.bytes().await.context("reading response body")?;
                (StatusCode::from_u16(status.as_u16()).unwrap(), bytes.to_vec())
            }
            Transport::Uds { socket } => {
                let uri: hyper::Uri = UnixUri::new(socket.clone(), &full_path).into();
                let mut builder = Request::builder().method(method.clone()).uri(uri);
                let payload = if let Some(b) = body {
                    let bytes = serde_json::to_vec(b).context("serialising request body")?;
                    builder = builder.header(hyper::header::CONTENT_TYPE, "application/json");
                    Full::new(Bytes::from(bytes))
                } else {
                    Full::new(Bytes::new())
                };
                let req = builder.body(payload).context("building UDS request")?;
                let resp = self
                    .uds_client
                    .request(req)
                    .await
                    .with_context(|| format!("{method} {}", full_path))?;
                let status = resp.status();
                let body = resp
                    .into_body()
                    .collect()
                    .await
                    .context("reading UDS response body")?
                    .to_bytes();
                (status, body.to_vec())
            }
        };

        if !status.is_success() {
            let msg = std::str::from_utf8(&bytes).unwrap_or("<binary>");
            bail!("{method} {full_path} → {status}: {msg}");
        }

        if bytes.is_empty() {
            // Some endpoints return 204 No Content. The generic deserialiser
            // needs *something*; try the unit-typed Null fallback.
            return serde_json::from_slice::<T>(b"null").map_err(|_| {
                anyhow!("server returned empty body; expected a value of the requested type")
            });
        }
        serde_json::from_slice(&bytes)
            .with_context(|| format!("deserialising response from {full_path}"))
    }
}

/// Pick a transport for same-host use. Tries UDS first (Admin bypass),
/// falls back to TCP on `http://127.0.0.1:<port>` with the supplied token.
///
/// Returns an error if neither transport is reachable.
pub async fn pick_local(
    socket_path: &Path,
    tcp_fallback_url: &str,
    token: Option<String>,
) -> Result<Client> {
    if socket_path.exists() {
        let c = Client::new(Transport::Uds {
            socket: socket_path.to_path_buf(),
        });
        match c.probe().await {
            Ok(()) => return Ok(c),
            Err(e) => {
                let msg = format!("{e}");
                if msg.contains("Permission denied") || msg.contains("EACCES") {
                    tracing::warn!(
                        "UDS at {} is present but not accessible — check \
                         homecore-admin group membership. Falling back to TCP.",
                        socket_path.display()
                    );
                } else {
                    tracing::debug!("UDS probe failed ({e}); falling back to TCP");
                }
            }
        }
    }

    let c = Client::new(Transport::Tcp {
        base_url: tcp_fallback_url.trim_end_matches('/').to_string(),
        token,
    });
    c.probe()
        .await
        .with_context(|| format!("TCP fallback at {tcp_fallback_url} unreachable"))?;
    Ok(c)
}
