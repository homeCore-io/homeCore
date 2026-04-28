//! Admin-only HTTP server on a Unix domain socket.
//!
//! Same `Router` as the TCP server, but every request arriving on the UDS
//! carries a `LocalAdminMarker` in its extensions. The auth middleware
//! (`require_auth`) short-circuits on that marker and grants Admin scope
//! after a defensive `SO_PEERCRED` check against the configured UID
//! allow-list.
//!
//! The binding, chown, and chmod of the socket file happen in the core
//! binary (`main.rs`) so the systemd unit can own the file permissions.
//! This module only handles the accept loop + HTTP serving.

use anyhow::{Context, Result};
use axum::{body::Body, http::Request, Router};
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder;
use std::path::Path;
use tokio::net::UnixListener;
use tower::Service;
use tracing::{debug, warn};

use crate::auth_middleware::LocalAdminMarker;

/// Accept connections on the given UDS and serve the axum router on each.
/// Runs until the listener errors or is shut down.
///
/// The file at `listener.local_addr()` must already be configured with the
/// correct ownership and mode; this function does not touch filesystem perms.
pub async fn serve(listener: UnixListener, app: Router) -> Result<()> {
    loop {
        let (stream, _) = listener
            .accept()
            .await
            .context("UDS admin listener accept failed")?;

        let peer_uid = stream.peer_cred().ok().map(|c| c.uid());
        let marker = LocalAdminMarker { peer_uid };
        let app_for_conn = app.clone();

        tokio::spawn(async move {
            let svc = service_fn(move |mut req: Request<hyper::body::Incoming>| {
                // Inject the per-connection marker as a request extension.
                req.extensions_mut().insert(marker);
                // Delegate to the shared axum Router (clone-per-call — cheap,
                // Router is Arc-backed internally). Convert incoming body.
                let (parts, body) = req.into_parts();
                let req = Request::from_parts(parts, Body::new(body));
                let mut app = app_for_conn.clone();
                
                app.call(req)
            });

            let io = TokioIo::new(stream);
            if let Err(e) = Builder::new(TokioExecutor::new())
                .serve_connection(io, svc)
                .await
            {
                debug!(error = %e, "UDS admin connection closed with error");
            }
        });
    }
}

/// Resolve a set of UIDs from config: always includes the current process
/// UID (so the homecore service itself can hit its own UDS), plus any
/// explicitly listed extras.
pub fn resolve_allowed_uids(extras: &[u32]) -> std::collections::HashSet<u32> {
    let mut set: std::collections::HashSet<u32> = extras.iter().copied().collect();
    set.insert(nix::unistd::geteuid().as_raw());
    set
}

/// Look up a group GID by name, for chown-ing the socket.
/// Returns an error if the group does not exist.
pub fn resolve_group_gid(group: &str) -> Result<u32> {
    use nix::unistd::Group;
    let grp = Group::from_name(group)
        .with_context(|| format!("group lookup for {group}"))?
        .with_context(|| format!("group {group} not found"))?;
    Ok(grp.gid.as_raw())
}

/// Log a sanity-check warning if the socket path has a wider-than-expected
/// mode (e.g. 0666). Defensive — catches misconfigured systemd drops-in
/// overriding RuntimeDirectoryMode.
pub fn warn_if_mode_too_loose(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o002 != 0 {
        warn!(
            path = %path.display(),
            mode = format!("{mode:o}"),
            "Admin UDS is world-writable — fix permissions immediately"
        );
    }
}
