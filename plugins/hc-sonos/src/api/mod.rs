//! Standalone HTTP API for the Sonos plugin.
//!
//! Mirrors the node-sonos-http-api URL scheme:
//!   GET /zones
//!   GET /favorites
//!   GET /playlists
//!   GET /pauseall
//!   GET /:room/state
//!   GET /:room/{play,pause,playpause,stop,next,previous}
//!   GET /:room/volume/:level           (level = 0-100, +N, -N)
//!   GET /:room/{mute,unmute,togglemute}
//!   GET /:room/bass/:level
//!   GET /:room/treble/:level
//!   GET /:room/loudness/:state         (on/off/toggle)
//!   GET /:room/shuffle/:state          (on/off/toggle)
//!   GET /:room/repeat/:state           (on/off/one/toggle)
//!   GET /:room/crossfade/:state        (on/off/toggle)
//!   GET /:room/seek/:seconds
//!   GET /:room/seekby/:seconds          (relative, ±N)
//!   GET /:room/trackseek/:index
//!   GET /:room/join/:target
//!   GET /:room/leave
//!   GET /:room/queue
//!   GET /:room/clearqueue
//!   GET /:room/queue/remove/:index
//!   GET /:room/queue/adduri/:uri
//!   GET /:room/queue/addnexturi/:uri
//!   GET /:room/favorites
//!   GET /:room/playlists
//!   GET /:room/favorite/:name
//!   GET /:room/playlist/:name
//!   GET /:room/playuri/:uri
//!
//! Who may call it: the speakers' event callbacks
//! (`/sonos/callback/...`) are open to the network, since the speakers POST
//! NOTIFY there. Everything else, the web interface and the control routes
//! above, answers only requests from this machine (loopback, or one of this
//! machine's own addresses) unless `api.allow_remote_control` is true. The
//! API has no sign-in, so opening it to the network lets anyone there control
//! the speakers.

pub mod content;
pub mod handlers;

use std::net::{IpAddr, SocketAddr};

use axum::{
    extract::{connect_info::Connected, ConnectInfo, Request},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{any, get},
    serve::IncomingStream,
    Router,
};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tower::ServiceBuilder;
use tower_http::cors::CorsLayer;

use crate::events::NotifyEvent;
use crate::shared_state::AppState;

/// Both ends of an accepted connection, so a request can be told apart as
/// coming from this machine or from the network.
#[derive(Clone, Copy, Debug)]
pub struct Peer {
    /// The address the request came from.
    pub remote: SocketAddr,
    /// The address of ours it reached.
    pub local: SocketAddr,
}

impl Connected<IncomingStream<'_, TcpListener>> for Peer {
    fn connect_info(stream: IncomingStream<'_, TcpListener>) -> Self {
        let remote = *stream.remote_addr();
        let local = stream.io().local_addr().unwrap_or(remote);
        Peer { remote, local }
    }
}

/// Whether a request came from this machine: a loopback address, or one of
/// this machine's own addresses (a rule calling `http://<LAN IP>:5005/...`
/// from the same host arrives from that LAN IP, to that LAN IP).
pub fn is_local(peer: &Peer) -> bool {
    let remote = canonical(peer.remote.ip());
    remote.is_loopback() || remote == canonical(peer.local.ip())
}

/// `::ffff:127.0.0.1` (an IPv4 client on a `::` listener) as plain IPv4.
fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    }
}

/// Turn away control requests from other machines unless
/// `allow_remote_control` is set. The speakers' event callbacks are not
/// routed through this.
async fn local_only(allow_remote: bool, req: Request, next: Next) -> Response {
    let peer = req.extensions().get::<ConnectInfo<Peer>>().map(|c| c.0);
    match peer {
        Some(p) if allow_remote || is_local(&p) => next.run(req).await,
        _ => {
            tracing::warn!(
                remote = ?peer.map(|p| p.remote),
                path = %req.uri().path(),
                "Refused a Sonos HTTP API request from another machine"
            );
            (
                StatusCode::FORBIDDEN,
                "The Sonos HTTP API only answers requests from this machine. \
                 To control the speakers from other machines, set \
                 allow_remote_control = true under [api] in the hc-sonos settings.\n",
            )
                .into_response()
        }
    }
}

/// Build the Axum router.
///
/// The GENA callback route stays open to the network, because the speakers
/// deliver their events there. Every other route (the web interface and the
/// control API) answers only requests from this machine unless
/// `allow_remote_control` is true.
pub fn router(
    state: AppState,
    event_tx: mpsc::Sender<(String, NotifyEvent)>,
    allow_remote_control: bool,
) -> Router {
    use handlers::*;

    let callbacks = Router::new()
        // ── GENA NOTIFY callback (used by Sonos to push state changes) ───────
        .route("/sonos/callback/{uuid}/{service}", any(sonos_notify));

    let control = Router::new()
        .route("/", get(landing))
        // ── System ──────────────────────────────────────────────────────────
        .route("/zones", get(zones))
        .route("/favorites", get(all_favorites))
        .route("/playlists", get(all_playlists))
        .route("/pauseall", get(pause_all))
        // ── Per-room: state ─────────────────────────────────────────────────
        .route("/{room}/state", get(room_state))
        .route("/{room}/favorites", get(room_favorites))
        .route("/{room}/playlists", get(room_playlists))
        .route("/{room}/queue", get(queue))
        // ── Per-room: transport ─────────────────────────────────────────────
        .route("/{room}/play", get(play))
        .route("/{room}/pause", get(pause))
        .route("/{room}/playpause", get(playpause))
        .route("/{room}/stop", get(stop))
        .route("/{room}/next", get(next))
        .route("/{room}/previous", get(previous))
        // ── Per-room: volume ────────────────────────────────────────────────
        .route("/{room}/volume/{level}", get(volume))
        .route("/{room}/mute", get(mute))
        .route("/{room}/unmute", get(unmute))
        .route("/{room}/togglemute", get(togglemute))
        // ── Per-room: EQ ────────────────────────────────────────────────────
        .route("/{room}/bass/{level}", get(bass))
        .route("/{room}/treble/{level}", get(treble))
        .route("/{room}/loudness/{state}", get(loudness))
        // ── Per-room: play modes ─────────────────────────────────────────────
        .route("/{room}/shuffle/{state}", get(shuffle))
        .route("/{room}/repeat/{state}", get(repeat))
        .route("/{room}/crossfade/{state}", get(crossfade))
        // ── Per-room: seek ───────────────────────────────────────────────────
        .route("/{room}/seek/{seconds}", get(seek))
        .route("/{room}/seekby/{seconds}", get(seekby))
        .route("/{room}/trackseek/{index}", get(trackseek))
        // ── Per-room: grouping ───────────────────────────────────────────────
        .route("/{room}/join/{target}", get(join))
        .route("/{room}/leave", get(leave))
        // ── Per-room: queue mgmt ─────────────────────────────────────────────
        .route("/{room}/clearqueue", get(clearqueue))
        .route("/{room}/queue/remove/{index}", get(queue_remove))
        .route("/{room}/queue/adduri/{uri}", get(queue_add))
        .route("/{room}/queue/addnexturi/{uri}", get(queue_add_next))
        // ── Per-room: content ────────────────────────────────────────────────
        .route("/{room}/favorite/{name}", get(play_favorite))
        .route("/{room}/playlist/{name}", get(play_playlist))
        .route("/{room}/playuri/{uri}", get(play_uri))
        .route_layer(middleware::from_fn(move |req: Request, next: Next| {
            local_only(allow_remote_control, req, next)
        }));

    callbacks
        .merge(control)
        .layer(
            ServiceBuilder::new()
                .layer(axum::Extension(event_tx))
                .layer(CorsLayer::permissive()),
        )
        .with_state(state)
}

/// Start the HTTP server.  Returns an error only if the bind address is invalid.
pub async fn serve(
    host: &str,
    port: u16,
    allow_remote_control: bool,
    state: AppState,
    event_tx: mpsc::Sender<(String, NotifyEvent)>,
) -> anyhow::Result<()> {
    let addr = format!("{host}:{port}");
    let listener = TcpListener::bind(&addr)
        .await
        .map_err(|e| anyhow::anyhow!("API server bind {addr}: {e}"))?;

    tracing::info!(addr, allow_remote_control, "Sonos HTTP API listening");
    let app = router(state, event_tx, allow_remote_control);
    axum::serve(listener, app.into_make_service_with_connect_info::<Peer>())
        .await
        .map_err(|e| anyhow::anyhow!("API server error: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tower::ServiceExt;

    fn peer(remote: &str, local: &str) -> Peer {
        Peer {
            remote: remote.parse().unwrap(),
            local: local.parse().unwrap(),
        }
    }

    fn app(allow_remote: bool) -> Router {
        let (tx, _rx) = mpsc::channel(8);
        router(crate::shared_state::new_state(), tx, allow_remote)
    }

    async fn status(app: Router, method: &str, path: &str, from: Option<Peer>) -> StatusCode {
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .body(Body::empty())
            .unwrap();
        if let Some(p) = from {
            req.extensions_mut().insert(ConnectInfo(p));
        }
        app.oneshot(req).await.unwrap().status()
    }

    #[test]
    fn local_means_loopback_or_our_own_address() {
        assert!(is_local(&peer("127.0.0.1:50000", "127.0.0.1:5005")));
        assert!(is_local(&peer("[::1]:50000", "[::1]:5005")));
        assert!(is_local(&peer(
            "[::ffff:127.0.0.1]:50000",
            "[::ffff:127.0.0.1]:5005"
        )));
        // A rule on this machine calling its own LAN address.
        assert!(is_local(&peer("192.168.1.10:50000", "192.168.1.10:5005")));
        assert!(!is_local(&peer("192.168.1.50:50000", "192.168.1.10:5005")));
        assert!(!is_local(&peer(
            "[::ffff:192.168.1.50]:50000",
            "[::ffff:192.168.1.10]:5005"
        )));
    }

    #[tokio::test]
    async fn control_routes_refuse_other_machines_by_default() {
        let lan = Some(peer("192.168.1.50:50000", "192.168.1.10:5005"));
        for path in [
            "/",
            "/zones",
            "/kitchen/play",
            "/kitchen/volume/40",
            "/pauseall",
        ] {
            assert_eq!(
                status(app(false), "GET", path, lan).await,
                StatusCode::FORBIDDEN,
                "{path}"
            );
        }
        // No connection info at all: refused, never assumed local.
        assert_eq!(
            status(app(false), "GET", "/zones", None).await,
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn speakers_can_always_deliver_events() {
        let lan = Some(peer("192.168.1.50:1400", "192.168.1.10:5005"));
        assert_eq!(
            status(app(false), "NOTIFY", "/sonos/callback/RINCON_1/avt", lan).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn this_machine_and_the_setting_get_through() {
        let here = Some(peer("127.0.0.1:50000", "127.0.0.1:5005"));
        assert_eq!(status(app(false), "GET", "/", here).await, StatusCode::OK);
        // No speakers known: the handler itself answers, not the guard.
        assert_eq!(
            status(app(false), "GET", "/zones", here).await,
            StatusCode::SERVICE_UNAVAILABLE
        );
        let lan = Some(peer("192.168.1.50:50000", "192.168.1.10:5005"));
        assert_eq!(
            status(app(true), "GET", "/zones", lan).await,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    /// The real server fills in the connection info; a loopback client is
    /// answered.
    #[tokio::test]
    async fn served_over_loopback() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let svc = app(false).into_make_service_with_connect_info::<Peer>();
        tokio::spawn(async move { axum::serve(listener, svc).await });

        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        assert!(out.starts_with("HTTP/1.1 200"), "{out}");
    }
}
