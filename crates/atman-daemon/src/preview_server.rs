use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::extract::{DefaultBodyLimit, Path as UrlPath, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

type ApiError = (StatusCode, String);
type ApiResult = Result<axum::Json<Value>, ApiError>;

#[derive(Clone)]
struct PreviewState {
    data_root: PathBuf,
    port: u16,
    write_lock: Arc<Mutex<()>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProjectRecord {
    id: String,
    name: String,
    path: PathBuf,
    scope: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TopicRecord {
    id: String,
    title: String,
    created_at: String,
    updated_at: String,
    blocks: Vec<BlockRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BlockRecord {
    id: String,
    created_at: String,
    kind: String,
    payload: Value,
}

#[derive(Deserialize)]
struct RegisterProject {
    abs_path: String,
    hint_slug: Option<String>,
}

#[derive(Deserialize)]
struct RegisterTopic {
    id: String,
    title: String,
}

pub async fn serve(port: u16) -> anyhow::Result<()> {
    let data_root = atman_runtime::storage::data_dir()?.join("preview");
    std::fs::create_dir_all(&data_root)?;
    let state = PreviewState {
        data_root,
        port,
        write_lock: Arc::new(Mutex::new(())),
    };
    let router = router(state);
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
    println!("[atman] preview listening on http://127.0.0.1:{port}");
    axum::serve(listener, router).await?;
    Ok(())
}

fn router(state: PreviewState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/p/{pid}/t/{topic}", get(index))
        .route("/preview.css", get(styles))
        .route("/preview_diagram.css", get(diagram_styles))
        .route("/preview.js", get(script))
        .route("/mermaid.min.js", get(mermaid_script))
        .route("/api/health", get(health))
        .route("/api/projects", get(list_projects).post(register_project))
        .route(
            "/api/projects/{pid}/topics",
            get(list_topics).post(register_topic),
        )
        .route("/api/projects/{pid}/topics/{topic}", get(read_topic))
        .route(
            "/api/projects/{pid}/topics/{topic}/blocks/{block}/html",
            get(read_html_block),
        )
        .route(
            "/api/projects/{pid}/topics/{topic}/blocks",
            post(push_block),
        )
        .layer(DefaultBodyLimit::max(1_200_000))
        .with_state(state)
}

async fn index() -> impl IntoResponse {
    (
        [(
            header::CONTENT_SECURITY_POLICY,
            "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; frame-src 'self' about:; object-src 'none'; base-uri 'none'",
        )],
        Html(include_str!("preview_ui.html")),
    )
}

async fn styles() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("preview_ui.css"),
    )
}

async fn diagram_styles() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("preview_ui_diagram.css"),
    )
}

async fn script() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        include_str!("preview_ui.js"),
    )
}

async fn mermaid_script() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        include_str!("preview_mermaid.min.js"),
    )
}

async fn health() -> axum::Json<Value> {
    axum::Json(json!({"status":"ok","server":"atman"}))
}

async fn list_projects(State(state): State<PreviewState>) -> ApiResult {
    let projects = read_projects(&state)?;
    Ok(axum::Json(json!(
        projects
            .iter()
            .map(|project| json!({
                "id": project.id,
                "name": project.name,
                "url": format!("http://127.0.0.1:{}/?project={}", state.port, project.id),
            }))
            .collect::<Vec<_>>()
    )))
}

async fn register_project(
    State(state): State<PreviewState>,
    headers: HeaderMap,
    axum::Json(request): axum::Json<RegisterProject>,
) -> ApiResult {
    check_write(&headers, state.port)?;
    let path = std::fs::canonicalize(&request.abs_path).map_err(invalid_io)?;
    if !path.is_dir() {
        return Err(bad_request("abs_path must be a directory"));
    }
    let name = request
        .hint_slug
        .as_deref()
        .filter(|slug| !slug.is_empty())
        .unwrap_or_else(|| {
            path.file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("project")
        });
    let slug = safe_slug(name);
    let fingerprint = atman_runtime::session_meta::fingerprint_from_root(&path);
    let id = format!("{slug}-{}", &fingerprint[..8]);
    let scope = atman_runtime::storage::resolve_project_scope_for(&path).map_err(internal)?;
    let project = ProjectRecord {
        id: id.clone(),
        name: name.to_string(),
        path,
        scope,
    };
    let _guard = state.write_lock.lock().await;
    let mut projects = read_projects(&state)?;
    let id = upsert_project(&mut projects, project)?;
    write_json(&state.data_root.join("projects.json"), &projects)?;
    Ok(axum::Json(json!({"id":id})))
}

fn upsert_project(
    projects: &mut Vec<ProjectRecord>,
    mut project: ProjectRecord,
) -> Result<String, ApiError> {
    if let Some(existing) = projects
        .iter_mut()
        .find(|existing| existing.path == project.path)
    {
        project.id = existing.id.clone();
        let id = project.id.clone();
        *existing = project;
        return Ok(id);
    }
    if projects.iter().any(|existing| existing.id == project.id) {
        return Err((StatusCode::CONFLICT, "project id collision".into()));
    }
    let id = project.id.clone();
    projects.push(project);
    Ok(id)
}

async fn list_topics(
    State(state): State<PreviewState>,
    UrlPath(pid): UrlPath<String>,
) -> ApiResult {
    let project = find_project(&state, &pid)?;
    let mut topics = read_topics(&project)?;
    topics.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok(axum::Json(json!(
        topics
            .iter()
            .map(|topic| json!({
                "id": topic.id,
                "title": topic.title,
                "updated_at": topic.updated_at,
                "block_count": topic.blocks.len(),
            }))
            .collect::<Vec<_>>()
    )))
}

async fn register_topic(
    State(state): State<PreviewState>,
    UrlPath(pid): UrlPath<String>,
    headers: HeaderMap,
    axum::Json(request): axum::Json<RegisterTopic>,
) -> ApiResult {
    check_write(&headers, state.port)?;
    valid_id(&request.id)?;
    if request.title.trim().is_empty() || request.title.len() > 240 {
        return Err(bad_request("title must be 1-240 bytes"));
    }
    let project = find_project(&state, &pid)?;
    let _guard = state.write_lock.lock().await;
    let path = topic_path(&project, &request.id);
    let now = chrono::Utc::now().to_rfc3339();
    let mut topic = read_json::<TopicRecord>(&path)?.unwrap_or(TopicRecord {
        id: request.id.clone(),
        title: request.title.clone(),
        created_at: now.clone(),
        updated_at: now.clone(),
        blocks: Vec::new(),
    });
    topic.title = request.title;
    topic.updated_at = now;
    write_json(&path, &topic)?;
    Ok(axum::Json(
        json!({"id":topic.id,"url":topic_url(&state,&pid,&topic.id)}),
    ))
}

async fn read_topic(
    State(state): State<PreviewState>,
    UrlPath((pid, topic)): UrlPath<(String, String)>,
) -> ApiResult {
    valid_id(&topic)?;
    let project = find_project(&state, &pid)?;
    let Some(topic) = read_json::<TopicRecord>(&topic_path(&project, &topic))? else {
        return Err((StatusCode::NOT_FOUND, "topic not found".into()));
    };
    let blocks = topic
        .blocks
        .iter()
        .map(|block| {
            let mut value = json!(block);
            if block.kind == "markdown" {
                value["rendered_html"] = Value::String(render_markdown(
                    block.payload["content"].as_str().unwrap_or(""),
                ));
            }
            value
        })
        .collect::<Vec<_>>();
    Ok(axum::Json(json!({
        "id":topic.id,"title":topic.title,"created_at":topic.created_at,
        "updated_at":topic.updated_at,"blocks":blocks,
    })))
}

async fn read_html_block(
    State(state): State<PreviewState>,
    UrlPath((pid, topic_id, block_id)): UrlPath<(String, String, String)>,
) -> Result<impl IntoResponse, ApiError> {
    valid_id(&topic_id)?;
    valid_id(&block_id)?;
    let project = find_project(&state, &pid)?;
    let Some(topic) = read_json::<TopicRecord>(&topic_path(&project, &topic_id))? else {
        return Err((StatusCode::NOT_FOUND, "topic not found".into()));
    };
    let Some(block) = topic
        .blocks
        .iter()
        .find(|block| block.id == block_id && block.kind == "html")
    else {
        return Err((StatusCode::NOT_FOUND, "HTML block not found".into()));
    };
    let Some(fragment) = block.payload["fragment"].as_str() else {
        return Err(internal("HTML block is missing its fragment"));
    };
    Ok((
        [
            (
                header::CONTENT_SECURITY_POLICY,
                "sandbox allow-scripts; default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data:; font-src data:; connect-src 'none'; frame-src 'none'; form-action 'none'; base-uri 'none'",
            ),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        Html(fragment.to_owned()),
    ))
}

async fn push_block(
    State(state): State<PreviewState>,
    UrlPath((pid, topic_id)): UrlPath<(String, String)>,
    headers: HeaderMap,
    axum::Json(mut payload): axum::Json<Value>,
) -> ApiResult {
    check_write(&headers, state.port)?;
    valid_id(&topic_id)?;
    let project = find_project(&state, &pid)?;
    let kind = payload["kind"]
        .as_str()
        .ok_or_else(|| bad_request("kind is required"))?
        .to_string();
    validate_block(&mut payload, &kind, &project)?;
    let _guard = state.write_lock.lock().await;
    let path = topic_path(&project, &topic_id);
    let Some(mut topic) = read_json::<TopicRecord>(&path)? else {
        return Err((StatusCode::NOT_FOUND, "topic not found".into()));
    };
    let id = format!("blk_{}", uuid::Uuid::now_v7().simple());
    topic.blocks.push(BlockRecord {
        id: id.clone(),
        created_at: chrono::Utc::now().to_rfc3339(),
        kind,
        payload,
    });
    topic.updated_at = chrono::Utc::now().to_rfc3339();
    let position = topic.blocks.len() - 1;
    write_json(&path, &topic)?;
    Ok(axum::Json(json!({
        "id":id,"position":position,
        "rendered_html_preview_url":format!("{}#{}",topic_url(&state,&pid,&topic_id),id),
    })))
}

fn validate_block(
    payload: &mut Value,
    kind: &str,
    project: &ProjectRecord,
) -> Result<(), ApiError> {
    match kind {
        "markdown" => {
            require_text(payload, "content")?;
        }
        "mermaid" => {
            require_text(payload, "source")?;
        }
        "html" => {
            require_text(payload, "fragment")?;
        }
        "image" => {
            let image = require_text(payload, "image_base64")?;
            if image.len() > 1_000_000 {
                return Err(bad_request("image too large"));
            }
            use base64::Engine;
            base64::engine::general_purpose::STANDARD
                .decode(image)
                .map_err(|_| bad_request("image_base64 is invalid"))?;
            let media = payload["media_type"].as_str().unwrap_or("image/png");
            if !["image/png", "image/jpeg", "image/gif", "image/webp"].contains(&media) {
                return Err(bad_request("unsupported image media_type"));
            }
        }
        "diff" => {
            if payload["mode"] == "commit_diff" {
                let sha = require_text(payload, "commit_sha")?;
                let repo = require_text(payload, "repo_path")?;
                if sha.len() < 7 || sha.len() > 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(bad_request("commit_sha must be a hex object id"));
                }
                let path = std::fs::canonicalize(repo).map_err(invalid_io)?;
                if !path.starts_with(&project.path) {
                    return Err(bad_request(
                        "repo_path must be inside the registered project",
                    ));
                }
                let output = std::process::Command::new("git")
                    .arg("-C")
                    .arg(&path)
                    .arg("show")
                    .arg("--format=")
                    .arg("--no-ext-diff")
                    .arg("--no-textconv")
                    .arg(sha)
                    .output()
                    .map_err(internal)?;
                if !output.status.success() || output.stdout.len() > 1_000_000 {
                    return Err(bad_request("commit diff unavailable or too large"));
                }
                payload["patch_text"] =
                    Value::String(String::from_utf8_lossy(&output.stdout).into_owned());
            } else {
                require_text(payload, "patch_text")?;
            }
        }
        _ => return Err(bad_request("unsupported block kind")),
    }
    Ok(())
}

fn require_text<'a>(payload: &'a Value, name: &str) -> Result<&'a str, ApiError> {
    payload[name]
        .as_str()
        .ok_or_else(|| bad_request(format!("{name} is required")))
}

fn render_markdown(source: &str) -> String {
    use pulldown_cmark::{Event, Options, Parser, html};
    let events = Parser::new_ext(source, Options::all()).map(|event| match event {
        Event::Html(raw) | Event::InlineHtml(raw) => Event::Text(raw),
        other => other,
    });
    let mut rendered = String::new();
    html::push_html(&mut rendered, events);
    rendered
}

fn read_projects(state: &PreviewState) -> Result<Vec<ProjectRecord>, ApiError> {
    Ok(read_json(&state.data_root.join("projects.json"))?.unwrap_or_default())
}

fn find_project(state: &PreviewState, pid: &str) -> Result<ProjectRecord, ApiError> {
    valid_id(pid)?;
    read_projects(state)?
        .into_iter()
        .find(|project| project.id == pid)
        .ok_or((StatusCode::NOT_FOUND, "project not found".into()))
}

fn read_topics(project: &ProjectRecord) -> Result<Vec<TopicRecord>, ApiError> {
    let dir = project.scope.join("preview/topics");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(Vec::new());
    };
    let mut topics = Vec::new();
    for entry in entries.flatten() {
        if entry.path().extension().is_some_and(|ext| ext == "json")
            && let Some(topic) = read_json(&entry.path())?
        {
            topics.push(topic);
        }
    }
    Ok(topics)
}

fn topic_path(project: &ProjectRecord, id: &str) -> PathBuf {
    project
        .scope
        .join("preview/topics")
        .join(format!("{id}.json"))
}

fn topic_url(state: &PreviewState, pid: &str, topic: &str) -> String {
    format!("http://127.0.0.1:{}/p/{pid}/t/{topic}", state.port)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>, ApiError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(internal(error)),
    };
    serde_json::from_str(&text).map(Some).map_err(internal)
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), ApiError> {
    std::fs::create_dir_all(path.parent().unwrap()).map_err(internal)?;
    let tmp = path.with_extension(format!("{}.tmp", uuid::Uuid::now_v7()));
    let bytes = serde_json::to_vec(value).map_err(internal)?;
    std::fs::write(&tmp, bytes).map_err(internal)?;
    std::fs::rename(&tmp, path).map_err(internal)
}

fn valid_id(id: &str) -> Result<(), ApiError> {
    if id.is_empty()
        || id.len() > 100
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(bad_request("id must use letters, digits, '-' or '_'"));
    }
    Ok(())
}

fn safe_slug(name: &str) -> String {
    let slug = name
        .chars()
        .take(40)
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    let trimmed = slug.trim_matches('-');
    if trimmed.is_empty() {
        "project".into()
    } else {
        trimmed.into()
    }
}

fn check_write(headers: &HeaderMap, port: u16) -> Result<(), ApiError> {
    if !headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"))
    {
        return Err((
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "application/json required".into(),
        ));
    }
    if let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        && origin != format!("http://127.0.0.1:{port}")
        && origin != format!("http://localhost:{port}")
    {
        return Err((
            StatusCode::FORBIDDEN,
            "cross-origin writes are not allowed".into(),
        ));
    }
    Ok(())
}

fn bad_request(message: impl Into<String>) -> ApiError {
    (StatusCode::BAD_REQUEST, message.into())
}
fn invalid_io(error: impl std::fmt::Display) -> ApiError {
    bad_request(error.to_string())
}
fn internal(error: impl std::fmt::Display) -> ApiError {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use tower::ServiceExt;

    fn test_state(root: &Path) -> PreviewState {
        let state = PreviewState {
            data_root: root.join("registry"),
            port: 65097,
            write_lock: Arc::new(Mutex::new(())),
        };
        let project = ProjectRecord {
            id: "demo-12345678".into(),
            name: "demo".into(),
            path: root.to_path_buf(),
            scope: root.join("scope"),
        };
        write_json(&state.data_root.join("projects.json"), &vec![project]).unwrap();
        state
    }

    async fn request(app: Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
        let response = app
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 2_000_000).await.unwrap();
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, body)
    }

    #[tokio::test]
    async fn topic_and_block_round_trip() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state(root.path());
        let app = router(state.clone());
        let (status, _) = request(
            app.clone(),
            "POST",
            "/api/projects/demo-12345678/topics",
            json!({"id":"review","title":"Code review"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, block) = request(
            app.clone(),
            "POST",
            "/api/projects/demo-12345678/topics/review/blocks",
            json!({"kind":"markdown","content":"# Ready\n<script>alert(1)</script>"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            block["rendered_html_preview_url"]
                .as_str()
                .unwrap()
                .contains("/p/demo-12345678/t/review#blk_")
        );
        let (status, topic) = request(
            app,
            "GET",
            "/api/projects/demo-12345678/topics/review",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(topic["blocks"][0]["kind"], "markdown");
        let html = topic["blocks"][0]["rendered_html"].as_str().unwrap();
        assert!(html.contains("<h1>Ready</h1>"));
        assert!(!html.contains("<script>"));
        assert!(state.data_root.join("projects.json").exists());
        assert!(
            root.path()
                .join("scope/preview/topics/review.json")
                .exists()
        );
    }

    #[tokio::test]
    async fn html_blocks_run_scripts_only_in_an_opaque_sandbox() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state(root.path());
        let topic = TopicRecord {
            id: "prototype".into(),
            title: "Prototype".into(),
            created_at: "now".into(),
            updated_at: "now".into(),
            blocks: vec![BlockRecord {
                id: "blk_demo".into(),
                created_at: "now".into(),
                kind: "html".into(),
                payload: json!({"kind":"html","fragment":"<script>document.body.dataset.ready = 'yes'</script>"}),
            }],
        };
        write_json(
            &topic_path(&find_project(&state, "demo-12345678").unwrap(), "prototype"),
            &topic,
        )
        .unwrap();
        let response = router(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/api/projects/demo-12345678/topics/prototype/blocks/blk_demo/html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let policy = response.headers()[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap();
        assert!(policy.contains("sandbox allow-scripts"));
        assert!(!policy.contains("allow-same-origin"));
        assert!(policy.contains("connect-src 'none'"));
        let body = to_bytes(response.into_body(), 1_000_000).await.unwrap();
        assert_eq!(body, "<script>document.body.dataset.ready = 'yes'</script>");

        let (status, _) = request(
            router(state),
            "GET",
            "/api/projects/demo-12345678/topics/prototype/blocks/missing/html",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn rejects_path_ids_and_cross_origin_writes() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state(root.path());
        let (status, _) = request(
            router(state.clone()),
            "POST",
            "/api/projects/demo-12345678/topics",
            json!({"id":"..","title":"escape"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let response = router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/projects/demo-12345678/topics")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::ORIGIN, "https://example.com")
                    .body(Body::from(r#"{"id":"review","title":"Review"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn project_registration_uses_path_identity_across_display_names() {
        let root = tempfile::tempdir().unwrap();
        let mut projects = vec![ProjectRecord {
            id: "first-12345678".into(),
            name: "First".into(),
            path: root.path().to_path_buf(),
            scope: root.path().join("old-scope"),
        }];
        let id = upsert_project(
            &mut projects,
            ProjectRecord {
                id: "second-12345678".into(),
                name: "Second".into(),
                path: root.path().to_path_buf(),
                scope: root.path().join("new-scope"),
            },
        )
        .unwrap();
        assert_eq!(id, "first-12345678");
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].name, "Second");
        assert_eq!(projects[0].scope, root.path().join("new-scope"));
    }
}
