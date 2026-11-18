use std::sync::Arc;

use atman_daemon::{
    DaemonState,
    http::{HttpState, router},
};
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use tower::ServiceExt;

#[tokio::test]
async fn public_static_assets_keep_credentials_outside_the_same_origin_application() {
    let data = tempfile::tempdir().unwrap();
    let app = router(Arc::new(HttpState {
        daemon: Arc::new(DaemonState::new(data.path().to_path_buf())),
        auth_token: "private-web-fixture-token".into(),
    }));
    for (path, content_type) in [
        ("/", "text/html; charset=utf-8"),
        ("/assets/atman.js", "text/javascript; charset=utf-8"),
        ("/assets/atman.css", "text/css; charset=utf-8"),
        ("/assets/ATMAN-LOGO.png", "image/png"),
    ] {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let headers = response.headers();
        assert_eq!(headers["content-type"], content_type);
        assert_eq!(headers["cache-control"], "no-store");
        assert_eq!(headers["x-content-type-options"], "nosniff");
        assert_eq!(headers["referrer-policy"], "no-referrer");
        assert!(
            headers["content-security-policy"]
                .to_str()
                .unwrap()
                .contains("connect-src 'self'")
        );
        assert!(
            headers["content-security-policy"]
                .to_str()
                .unwrap()
                .contains("frame-ancestors 'none'")
        );
        assert!(!headers.contains_key("set-cookie"));
        assert!(!headers.contains_key("access-control-allow-origin"));
        let body = to_bytes(response.into_body(), 1_048_576).await.unwrap();
        if content_type == "image/png" {
            assert!(body.starts_with(b"\x89PNG\r\n\x1a\n"));
        } else {
            assert!(
                !std::str::from_utf8(&body)
                    .unwrap()
                    .contains("private-web-fixture-token")
            );
        }
    }
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rpc")
                .body(Body::from(
                    r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":{}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/assets/unknown.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
