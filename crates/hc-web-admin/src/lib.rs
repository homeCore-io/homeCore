//! Minimal internal admin UI mount for HomeCore.
//!
//! This crate is intentionally small in the first integration slice:
//! it provides a stable internal router mount point without yet moving the
//! full external Leptos application into the `core` workspace.

use axum::{response::Html, routing::get, Router};

const ADMIN_INDEX_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>HomeCore Admin</title>
  <style>
    :root {
      color-scheme: light;
      --bg: #f2efe7;
      --panel: #fffdf8;
      --ink: #1f2933;
      --muted: #52606d;
      --line: #d9e2ec;
      --accent: #0f766e;
    }
    body {
      margin: 0;
      font-family: "Iowan Old Style", "Palatino Linotype", Georgia, serif;
      background:
        radial-gradient(circle at top left, rgba(15, 118, 110, 0.10), transparent 28rem),
        linear-gradient(180deg, #f8f6f0 0%, var(--bg) 100%);
      color: var(--ink);
    }
    main {
      max-width: 52rem;
      margin: 4rem auto;
      padding: 0 1.5rem;
    }
    section {
      background: var(--panel);
      border: 1px solid var(--line);
      border-radius: 1rem;
      padding: 2rem;
      box-shadow: 0 20px 60px rgba(15, 23, 42, 0.08);
    }
    h1 {
      margin: 0 0 0.75rem;
      font-size: clamp(2rem, 4vw, 3.4rem);
      line-height: 1.05;
    }
    p {
      margin: 0.75rem 0;
      color: var(--muted);
      font-size: 1.05rem;
      line-height: 1.6;
    }
    code {
      font-family: "SFMono-Regular", ui-monospace, monospace;
      background: #f0f4f8;
      border-radius: 0.35rem;
      padding: 0.1rem 0.35rem;
      color: var(--ink);
    }
    a {
      color: var(--accent);
      text-decoration: none;
      font-weight: 600;
    }
    a:hover {
      text-decoration: underline;
    }
    ul {
      margin: 1.25rem 0 0;
      padding-left: 1.2rem;
      color: var(--ink);
    }
    li + li {
      margin-top: 0.5rem;
    }
  </style>
</head>
<body>
  <main>
    <section>
      <h1>HomeCore Admin</h1>
      <p>
        The internal <code>hc-web-admin</code> mount is enabled. This is the
        initial integration scaffold inside the HomeCore server.
      </p>
      <p>
        Existing external clients and the machine-facing API remain unchanged
        under <code>/api/v1</code>.
      </p>
      <ul>
        <li><a href="/api/v1/health">API health</a></li>
        <li><a href="/api/v1/system/status">System status</a></li>
        <li><a href="/api/v1/events">Recent events</a></li>
      </ul>
    </section>
  </main>
</body>
</html>
"#;

/// Build the minimal admin router.
///
/// This router is mounted by `hc-api` at `/admin`.
pub fn router() -> Router {
    Router::new().route("/", get(index))
}

async fn index() -> Html<&'static str> {
    Html(ADMIN_INDEX_HTML)
}
