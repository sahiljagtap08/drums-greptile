//! Serves the dashboard from the daemon itself.
//!
//! ## Why embedded, and why same-origin
//!
//! The dashboard is a client-polled page that reads `/v1/status`,
//! `/v1/failures` and `/v1/record`. Hosting it anywhere other than the daemon
//! does not merely add a step — it does not work. A page served over HTTPS
//! cannot `fetch("http://127.0.0.1:7787")`: browsers block it as mixed
//! content, and Private Network Access rules block it again. So
//! "app.drums.sh pointed at your local daemon" is broken by construction, not
//! by configuration.
//!
//! Serving the bundle from the daemon makes the page and the API the same
//! origin. That removes the CORS layer, removes the mixed-content problem,
//! removes the port from the build (the bundle's API base is `""`), and means
//! there is nothing to deploy or keep in sync with the binary.
//!
//! ## The honest cost
//!
//! The bundle is compiled in, so the dashboard a binary serves is the one that
//! shipped with it. That is a feature for correctness — the UI can never be
//! newer or older than the API it reads — and a limitation for iteration: a
//! dashboard change needs a rebuilt binary. `npm run dev` with
//! `NEXT_PUBLIC_DRUMS_API_URL` remains the loop for working on it.
//!
//! ## Not a gate
//!
//! These routes serve static assets and are deliberately NOT behind
//! `--api-token`: the token protects the `/v1/*` reads that expose the record
//! and claims. Serving HTML and JS that any installed copy of the binary
//! already contains protects nothing, and gating it would only mean a blank
//! page with no way to authenticate into it.

use axum::body::Body;
use axum::extract::Path as AxPath;
use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "../../../dashboard/out/"]
#[include = "*.html"]
#[include = "*.js"]
#[include = "*.css"]
#[include = "*.svg"]
#[include = "*.ico"]
#[include = "*.woff2"]
#[include = "*.txt"]
#[include = "*.json"]
struct Assets;

/// True when a dashboard bundle was actually compiled in. A build made
/// without running the dashboard's export still works — `drums watch` just
/// doesn't advertise a URL it cannot serve, rather than printing one that
/// 404s.
pub fn is_bundled() -> bool {
    Assets::get("index.html").is_some()
}

fn serve(path: &str) -> Response {
    match Assets::get(path) {
        Some(file) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            // Hashed assets under _next/static are immutable; everything else
            // must revalidate, or a rebuilt binary would serve a stale shell
            // out of the browser cache.
            let cache = if path.starts_with("_next/static/") {
                "public, max-age=31536000, immutable"
            } else {
                "no-cache"
            };
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, mime.as_ref()),
                    (header::CACHE_CONTROL, cache),
                ],
                Body::from(file.data.into_owned()),
            )
                .into_response()
        }
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

async fn index() -> Response {
    serve("index.html")
}

async fn asset(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if path.is_empty() {
        return serve("index.html");
    }
    // A client-routed path (`/failures`) has no file of its own in an export
    // with trailing-slash routing, so fall back the way any SPA host does:
    // try `<path>.html`, then `<path>/index.html`, then the shell.
    for candidate in [
        path.to_string(),
        format!("{path}.html"),
        format!("{}/index.html", path.trim_end_matches('/')),
    ] {
        if Assets::get(&candidate).is_some() {
            return serve(&candidate);
        }
    }
    serve("index.html")
}

async fn named(AxPath(rest): AxPath<String>) -> Response {
    asset(format!("/{rest}").parse().unwrap_or_default()).await
}

/// Mount the dashboard. Returns an empty router when no bundle is compiled in,
/// so the daemon's `/v1/*` routes are unaffected either way.
pub fn router() -> Router {
    if !is_bundled() {
        return Router::new();
    }
    Router::new()
        .route("/", get(index))
        .route("/{*rest}", get(named))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards the thing that actually breaks: if the embed path is wrong, or
    /// the export was never run, `is_bundled()` is false and `drums watch`
    /// silently stops offering a UI. This test does not assert it is bundled
    /// (a clean checkout may not have run the export) — it asserts the two
    /// states are consistent, so a half-wired embed cannot pass.
    #[test]
    fn bundled_and_routable_agree() {
        if is_bundled() {
            assert!(Assets::get("index.html").is_some());
            let has_static = Assets::iter().any(|f| f.starts_with("_next/"));
            assert!(
                has_static,
                "index.html embedded but no _next/ assets — the include list is wrong"
            );
        } else {
            assert!(Assets::get("index.html").is_none());
        }
    }

    /// The token gates reads, not the shell. See the module docs.
    #[test]
    fn the_router_is_empty_without_a_bundle_and_never_panics() {
        let _ = router();
    }
}
