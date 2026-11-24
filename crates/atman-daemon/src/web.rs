//! Static same-origin browser assets. Credentials are supplied to the existing API by the client.

use axum::{Router, http::header, response::IntoResponse, routing::get};

macro_rules! web_policy {
    ($extra:literal) => {
        concat!(
            "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' blob:; base-uri 'none'; form-action 'self'; frame-ancestors 'none'; object-src 'none'",
            $extra
        )
    };
}
const POLICY: &str = web_policy!("");
const PAIRED_POLICY: &str = web_policy!("; manifest-src 'self'; worker-src 'self'");
const INDEX: &str = include_str!("../assets/web/index.html");

fn asset(content_type: &'static str, body: impl IntoResponse) -> impl IntoResponse {
    protected_asset(content_type, POLICY, body)
}

fn protected_asset(
    content_type: &'static str,
    policy: &'static str,
    body: impl IntoResponse,
) -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-store"),
            (header::CONTENT_SECURITY_POLICY, policy),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        body,
    )
}

pub(crate) fn router() -> Router {
    assets().route(
        "/",
        get(|| async { asset("text/html; charset=utf-8", INDEX) }),
    )
}

pub(crate) fn paired_router<S: Clone + Send + Sync + 'static>() -> Router<S> {
    assets()
        .route(
            "/",
            get(|| async {
                protected_asset(
                    "text/html; charset=utf-8",
                    PAIRED_POLICY,
                    INDEX
                        .replace("id=\"app\"", "id=\"app\" data-auth=\"paired\"")
                        .replace(
                            "</head>",
                            "  <link rel=\"manifest\" href=\"/manifest.webmanifest\">\n  </head>",
                        ),
                )
            }),
        )
        .route(
            "/manifest.webmanifest",
            get(|| async {
                asset(
                    "application/manifest+json",
                    include_str!("../assets/web/manifest.webmanifest"),
                )
            }),
        )
        .route(
            "/notification-worker.js",
            get(|| async {
                asset(
                    "text/javascript; charset=utf-8",
                    include_str!("../assets/web/notification-worker.js"),
                )
            }),
        )
}

fn assets<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route(
            "/assets/atman.js",
            get(|| async {
                asset(
                    "text/javascript; charset=utf-8",
                    include_str!("../assets/web/atman.js"),
                )
            }),
        )
        .route(
            "/assets/atman.css",
            get(|| async {
                asset(
                    "text/css; charset=utf-8",
                    include_str!("../assets/web/atman.css"),
                )
            }),
        )
        .route(
            "/assets/ATMAN-LOGO.png",
            get(|| async {
                asset(
                    "image/png",
                    include_bytes!("../assets/web/ATMAN-LOGO.png").as_slice(),
                )
            }),
        )
}
