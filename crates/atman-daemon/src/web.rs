//! Static same-origin browser assets. Credentials are supplied to the existing API by the client.

use axum::{Router, http::header, response::IntoResponse, routing::get};

const POLICY: &str = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' blob:; base-uri 'none'; form-action 'self'; frame-ancestors 'none'; object-src 'none'";
const INDEX: &str = include_str!("../assets/web/index.html");

fn asset(content_type: &'static str, body: impl IntoResponse) -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-store"),
            (header::CONTENT_SECURITY_POLICY, POLICY),
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
    assets().route(
        "/",
        get(|| async {
            asset(
                "text/html; charset=utf-8",
                INDEX.replace("id=\"app\"", "id=\"app\" data-auth=\"paired\""),
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
