use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::error::RuntimeError;
use crate::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::Value;

#[derive(Debug, Clone)]
pub struct PreviewConfig {
    pub base_url: String,
    pub timeout_ms: u64,
    pub project_abs_path: String,
    pub project_hint_slug: Option<String>,
    pub max_body_bytes: usize,
}

impl Default for PreviewConfig {
    fn default() -> Self {
        Self {
            base_url: "http://127.0.0.1:65097".into(),
            timeout_ms: 3000,
            project_abs_path: std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            project_hint_slug: None,
            max_body_bytes: 1_000_000,
        }
    }
}

pub struct PreviewPush {
    config: Arc<PreviewConfig>,
    client: reqwest::Client,
    project_id: Mutex<Option<String>>,
}

impl PreviewPush {
    pub fn new(config: PreviewConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(config.timeout_ms))
            .build()
            .expect("build reqwest client");
        Self {
            config: Arc::new(config),
            client,
            project_id: Mutex::new(None),
        }
    }

    async fn ensure_project(&self) -> ResolveOutcome<String> {
        if let Some(pid) = self.project_id.lock().unwrap().clone() {
            return ResolveOutcome::Ok(pid);
        }
        let mut body = serde_json::Map::new();
        body.insert(
            "abs_path".into(),
            serde_json::Value::String(self.config.project_abs_path.clone()),
        );
        if let Some(slug) = &self.config.project_hint_slug {
            body.insert("hint_slug".into(), serde_json::Value::String(slug.clone()));
        }
        let url = format!("{}/api/projects", self.config.base_url);
        let resp = self.client.post(&url).json(&body).send().await;
        match resp {
            Ok(r) if r.status().is_success() => {
                let json: serde_json::Value = match r.json().await {
                    Ok(v) => v,
                    Err(e) => return ResolveOutcome::Fail(format!("decode projects: {e}")),
                };
                let pid = json
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                if pid.is_empty() {
                    return ResolveOutcome::Fail("register response missing id".into());
                }
                *self.project_id.lock().unwrap() = Some(pid.clone());
                ResolveOutcome::Ok(pid)
            }
            Ok(r) => ResolveOutcome::Fail(format!(
                "register project http {}: {}",
                r.status(),
                r.text().await.unwrap_or_default()
            )),
            Err(e) if is_connection_refused(&e) => ResolveOutcome::Unavailable,
            Err(e) => ResolveOutcome::Fail(format!("register project net: {e}")),
        }
    }

    async fn ensure_topic(&self, pid: &str, topic_id: &str, title: &str) -> Result<(), String> {
        let url = format!("{}/api/projects/{pid}/topics", self.config.base_url);
        let body = serde_json::json!({
            "id": topic_id,
            "title": title,
        });
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("topic net: {e}"))?;
        let status = resp.status();
        if status.is_success() || status.as_u16() == 409 {
            return Ok(());
        }
        Err(format!(
            "topic http {status}: {}",
            resp.text().await.unwrap_or_default()
        ))
    }
}

impl Tool for PreviewPush {
    fn name(&self) -> &str {
        "preview.push"
    }

    fn tier(&self) -> Tier {
        Tier::One
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Push markdown, mermaid, HTML, image, or diff content to the local preview server for browser review. Use it when the user asks to see a rendered artifact, diagram, diff, or audit page.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "topic": {"type": "string", "description": "Preview topic id to group related blocks."},
                "title": {"type": "string", "description": "Human-readable topic title."},
                "kind": {"type": "string", "enum": ["markdown", "mermaid", "html", "image", "diff"], "default": "markdown", "description": "Block type to render."},
                "content": {"type": "string", "description": "Block content. Required for markdown, mermaid, and HTML; optional for image and diff when their specific fields are supplied."},
                "image_base64": {"type": "string", "description": "Base64 image data for kind=image."},
                "image_path": {"type": "string", "description": "Local image path for kind=image."},
                "media_type": {"type": "string", "description": "Optional MIME type for image_base64, such as image/png."},
                "raw_diff": {"type": "string", "description": "Raw patch text for kind=diff."},
                "commit_sha": {"type": "string", "description": "Commit SHA for kind=diff commit mode. Requires repo_path."},
                "repo_path": {"type": "string", "description": "Repository path for kind=diff commit mode. Requires commit_sha."}
            },
            "required": ["topic", "title"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let topic = extract_string(&args, "topic", 0)?;
            let title = extract_string(&args, "title", 1)?;
            let kind = extract_optional_string(&args, "kind").unwrap_or_else(|| "markdown".into());
            let content = match kind.as_str() {
                "image" | "diff" => extract_optional_string(&args, "content").unwrap_or_default(),
                _ => extract_string(&args, "content", 2)?,
            };

            if content.len() > self.config.max_body_bytes {
                return Err(RuntimeError::ToolFailed(format!(
                    "preview.push: content {} bytes exceeds max {}",
                    content.len(),
                    self.config.max_body_bytes
                )));
            }

            let pid = match self.ensure_project().await {
                ResolveOutcome::Ok(id) => id,
                ResolveOutcome::Unavailable => return Ok(unavailable()),
                ResolveOutcome::Fail(msg) => {
                    return Err(RuntimeError::ToolFailed(format!("preview.push: {msg}")));
                }
            };

            self.ensure_topic(&pid, &topic, &title)
                .await
                .map_err(|e| RuntimeError::ToolFailed(format!("preview.push: {e}")))?;

            let block = build_block(&kind, &content, &args, self.config.max_body_bytes)?;
            let url = format!(
                "{}/api/projects/{pid}/topics/{topic}/blocks",
                self.config.base_url
            );
            let resp = self.client.post(&url).json(&block).send().await;
            match resp {
                Ok(r) if r.status().is_success() => {
                    let json: serde_json::Value = r.json().await.map_err(|e| {
                        RuntimeError::ToolFailed(format!("preview.push decode: {e}"))
                    })?;
                    let block_id = json
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let preview_url = json
                        .get("rendered_html_preview_url")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    Ok(Value::Struct(vec![
                        ("status".into(), Value::Str("ok".into())),
                        ("project_id".into(), Value::Str(pid)),
                        ("topic_id".into(), Value::Str(topic)),
                        ("block_id".into(), Value::Str(block_id)),
                        ("url".into(), Value::Str(preview_url)),
                    ]))
                }
                Ok(r) => Err(RuntimeError::ToolFailed(format!(
                    "preview.push http {}: {}",
                    r.status(),
                    r.text().await.unwrap_or_default()
                ))),
                Err(e) if is_connection_refused(&e) => Ok(unavailable()),
                Err(e) => Err(RuntimeError::ToolFailed(format!("preview.push net: {e}"))),
            }
        })
    }
}

pub async fn ping(base_url: &str, timeout_ms: u64) -> PingResult {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_millis(timeout_ms))
        .build()
    {
        Ok(c) => c,
        Err(e) => return PingResult::Fail(format!("build client: {e}")),
    };
    match client.get(format!("{base_url}/api/health")).send().await {
        Ok(r) if r.status().is_success() => PingResult::Ok,
        Ok(r) => PingResult::Fail(format!("http {}", r.status())),
        Err(e) if is_connection_refused(&e) => PingResult::Unavailable,
        Err(e) => PingResult::Fail(format!("net: {e}")),
    }
}

#[derive(Debug)]
pub enum PingResult {
    Ok,
    Unavailable,
    Fail(String),
}

enum ResolveOutcome<T> {
    Ok(T),
    Unavailable,
    Fail(String),
}

fn unavailable() -> Value {
    Value::Struct(vec![
        ("status".into(), Value::Str("unavailable".into())),
        (
            "hint".into(),
            Value::Str("preview server not reachable on configured base_url".into()),
        ),
    ])
}

fn is_connection_refused(e: &reqwest::Error) -> bool {
    let msg = format!("{e}");
    msg.contains("Connection refused")
        || msg.contains("connection refused")
        || msg.contains("tcp connect error")
        || e.is_connect()
}

fn build_block(
    kind: &str,
    content: &str,
    args: &ToolArgs,
    max_body_bytes: usize,
) -> Result<serde_json::Value, RuntimeError> {
    Ok(match kind {
        "markdown" => serde_json::json!({ "kind": "markdown", "content": content }),
        "mermaid" => serde_json::json!({ "kind": "mermaid", "source": content }),
        "html" => serde_json::json!({ "kind": "html", "fragment": content }),
        "image" => build_image_block(args, max_body_bytes)?,
        "diff" => build_diff_block(content, args)?,
        other => {
            return Err(RuntimeError::ToolFailed(format!(
                "preview.push: unsupported kind `{other}` (want markdown | mermaid | html | image | diff)"
            )));
        }
    })
}

fn build_image_block(
    args: &ToolArgs,
    max_body_bytes: usize,
) -> Result<serde_json::Value, RuntimeError> {
    let base64 = extract_optional_string(args, "image_base64");
    let path = extract_optional_string(args, "image_path");
    let (b64, media_type) = match (base64, path) {
        (Some(b), _) => (b, extract_optional_string(args, "media_type")),
        (None, Some(p)) => {
            let bytes = std::fs::read(&p).map_err(|e| {
                RuntimeError::ToolFailed(format!("preview.push image_path {p}: {e}"))
            })?;
            if bytes.len() > max_body_bytes {
                return Err(RuntimeError::ToolFailed(format!(
                    "preview.push: image_path {} bytes exceeds max_body_bytes {max_body_bytes}; upload endpoint not implemented",
                    bytes.len()
                )));
            }
            use base64::Engine;
            let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
            let media = guess_media_type(&p);
            (encoded, Some(media))
        }
        _ => {
            return Err(RuntimeError::ToolFailed(
                "preview.push image: expect one of image_base64 or image_path".into(),
            ));
        }
    };
    if b64.len() > max_body_bytes {
        return Err(RuntimeError::ToolFailed(format!(
            "preview.push image: base64 {} bytes exceeds max_body_bytes {max_body_bytes}",
            b64.len()
        )));
    }
    let mut block = serde_json::json!({ "kind": "image", "image_base64": b64 });
    if let Some(m) = media_type
        && let Some(obj) = block.as_object_mut()
    {
        obj.insert("media_type".to_string(), serde_json::Value::String(m));
    }
    Ok(block)
}

fn build_diff_block(content: &str, args: &ToolArgs) -> Result<serde_json::Value, RuntimeError> {
    let raw_diff = extract_optional_string(args, "raw_diff");
    let commit_sha = extract_optional_string(args, "commit_sha");
    let repo_path = extract_optional_string(args, "repo_path");
    if let (Some(sha), Some(repo)) = (commit_sha.as_ref(), repo_path.as_ref()) {
        return Ok(serde_json::json!({
            "kind": "diff",
            "mode": "commit_diff",
            "commit_sha": sha,
            "repo_path": repo,
        }));
    }
    let patch = raw_diff.or_else(|| {
        if content.is_empty() {
            None
        } else {
            Some(content.to_string())
        }
    });
    let Some(patch) = patch else {
        return Err(RuntimeError::ToolFailed(
            "preview.push diff: expect either (commit_sha + repo_path) or raw_diff / content"
                .into(),
        ));
    };
    Ok(serde_json::json!({
        "kind": "diff",
        "mode": "raw_diff",
        "patch_text": patch,
    }))
}

fn guess_media_type(path: &str) -> String {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".png") {
        "image/png".into()
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg".into()
    } else if lower.ends_with(".gif") {
        "image/gif".into()
    } else if lower.ends_with(".webp") {
        "image/webp".into()
    } else {
        "application/octet-stream".into()
    }
}

fn extract_string(args: &ToolArgs, name: &str, pos: usize) -> Result<String, RuntimeError> {
    let value = match args.named(name) {
        Some(v) => v,
        None => args.positional(pos)?,
    };
    match value {
        Value::Str(s) => Ok(s.clone()),
        Value::Path(p) => Ok(p.display().to_string()),
        other => Err(RuntimeError::TypeMismatch {
            expected: "string or path".into(),
            actual: other.kind_name().into(),
        }),
    }
}

fn extract_optional_string(args: &ToolArgs, name: &str) -> Option<String> {
    match args.named(name)? {
        Value::Str(s) => Some(s.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn cfg(base: String) -> PreviewConfig {
        PreviewConfig {
            base_url: base,
            timeout_ms: 500,
            project_abs_path: "/tmp/atman-test".into(),
            project_hint_slug: Some("atman-test".into()),
            max_body_bytes: 1_000_000,
        }
    }

    #[tokio::test]
    async fn push_markdown_block_registers_project_then_topic_then_block() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/projects"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": "atman-test-abc",
                "slug": "atman-test",
                "id_source": "fallback_random",
                "project_paths": [],
                "agents_md_was_injected": false,
                "url": "http://x",
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/projects/atman-test-abc/topics"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": "review-2026-07-03",
                "url": "http://x",
                "blocks_endpoint": "http://x",
                "assets_endpoint": "http://x",
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(
                "/api/projects/atman-test-abc/topics/review-2026-07-03/blocks",
            ))
            .and(wiremock::matchers::body_partial_json(serde_json::json!({
                "kind": "markdown",
                "content": "# hello",
            })))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": "blk_1",
                "position": 0,
                "rendered_html_preview_url": "http://x/blk1",
            })))
            .expect(1)
            .mount(&server)
            .await;

        let tool = PreviewPush::new(cfg(server.uri()));
        let ctx = ToolCtx::new();
        let args = ToolArgs {
            positional: vec![
                Value::Str("review-2026-07-03".into()),
                Value::Str("Review".into()),
                Value::Str("# hello".into()),
            ],
            named: vec![],
        };
        let v = tool.call(args, &ctx).await.unwrap();
        let Value::Struct(fields) = v else {
            panic!("expected struct");
        };
        assert!(matches!(
            &fields.iter().find(|(k, _)| k == "status").unwrap().1,
            Value::Str(s) if s == "ok"
        ));
    }

    #[tokio::test]
    async fn push_returns_unavailable_on_connection_refused() {
        let tool = PreviewPush::new(cfg("http://127.0.0.1:1".into()));
        let ctx = ToolCtx::new();
        let args = ToolArgs {
            positional: vec![
                Value::Str("t".into()),
                Value::Str("T".into()),
                Value::Str("c".into()),
            ],
            named: vec![],
        };
        let v = tool.call(args, &ctx).await.unwrap();
        let Value::Struct(fields) = v else {
            panic!("expected struct");
        };
        assert!(matches!(
            &fields.iter().find(|(k, _)| k == "status").unwrap().1,
            Value::Str(s) if s == "unavailable"
        ));
    }

    #[tokio::test]
    async fn push_treats_409_on_topic_as_idempotent_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/projects"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": "p1",
                "slug": "p",
                "id_source": "fallback_random",
                "project_paths": [],
                "agents_md_was_injected": false,
                "url": "http://x",
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/projects/p1/topics"))
            .respond_with(ResponseTemplate::new(409).set_body_string("duplicate"))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/projects/p1/topics/t/blocks"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": "b1",
                "position": 1,
                "rendered_html_preview_url": "http://x",
            })))
            .expect(1)
            .mount(&server)
            .await;

        let tool = PreviewPush::new(cfg(server.uri()));
        let ctx = ToolCtx::new();
        let args = ToolArgs {
            positional: vec![
                Value::Str("t".into()),
                Value::Str("T".into()),
                Value::Str("c".into()),
            ],
            named: vec![],
        };
        let v = tool.call(args, &ctx).await.unwrap();
        let Value::Struct(fields) = v else {
            panic!("expected struct");
        };
        assert!(matches!(
            &fields.iter().find(|(k, _)| k == "status").unwrap().1,
            Value::Str(s) if s == "ok"
        ));
    }

    #[tokio::test]
    async fn push_rejects_unknown_block_kind() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/projects"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": "p",
                "slug": "p",
                "id_source": "fallback_random",
                "project_paths": [],
                "agents_md_was_injected": false,
                "url": "http://x",
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/projects/p/topics"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": "t",
                "url": "",
                "blocks_endpoint": "",
                "assets_endpoint": ""
            })))
            .mount(&server)
            .await;
        let tool = PreviewPush::new(cfg(server.uri()));
        let ctx = ToolCtx::new();
        let args = ToolArgs {
            positional: vec![
                Value::Str("t".into()),
                Value::Str("T".into()),
                Value::Str("c".into()),
            ],
            named: vec![("kind".into(), Value::Str("video".into()))],
        };
        let err = tool.call(args, &ctx).await.unwrap_err();
        assert!(format!("{err}").contains("unsupported kind"));
    }

    async fn mount_project_and_topic(server: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/api/projects"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": "p",
                "slug": "p",
                "id_source": "fallback_random",
                "project_paths": [],
                "agents_md_was_injected": false,
                "url": "http://x",
            })))
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/projects/p/topics"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": "t",
                "url": "",
                "blocks_endpoint": "",
                "assets_endpoint": ""
            })))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn push_image_base64_lands_as_image_block_with_media_type() {
        let server = MockServer::start().await;
        mount_project_and_topic(&server).await;
        Mock::given(method("POST"))
            .and(path("/api/projects/p/topics/t/blocks"))
            .and(wiremock::matchers::body_partial_json(serde_json::json!({
                "kind": "image",
                "image_base64": "iVBOR",
                "media_type": "image/png",
            })))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": "b_img",
                "position": 0,
                "rendered_html_preview_url": "http://x/b_img",
            })))
            .expect(1)
            .mount(&server)
            .await;

        let tool = PreviewPush::new(cfg(server.uri()));
        let ctx = ToolCtx::new();
        let args = ToolArgs {
            positional: vec![Value::Str("t".into()), Value::Str("T".into())],
            named: vec![
                ("kind".into(), Value::Str("image".into())),
                ("image_base64".into(), Value::Str("iVBOR".into())),
                ("media_type".into(), Value::Str("image/png".into())),
            ],
        };
        let v = tool.call(args, &ctx).await.unwrap();
        let Value::Struct(fields) = v else {
            panic!("expected struct");
        };
        assert!(matches!(
            &fields.iter().find(|(k, _)| k == "status").unwrap().1,
            Value::Str(s) if s == "ok"
        ));
    }

    #[tokio::test]
    async fn push_image_path_reads_bytes_and_encodes_base64() {
        let server = MockServer::start().await;
        mount_project_and_topic(&server).await;
        let tmp = tempfile::NamedTempFile::with_suffix(".png").unwrap();
        std::fs::write(tmp.path(), b"fake-png-bytes").unwrap();
        Mock::given(method("POST"))
            .and(path("/api/projects/p/topics/t/blocks"))
            .and(wiremock::matchers::body_partial_json(serde_json::json!({
                "kind": "image",
                "media_type": "image/png",
            })))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": "b_img_path",
                "position": 0,
                "rendered_html_preview_url": "http://x",
            })))
            .expect(1)
            .mount(&server)
            .await;

        let tool = PreviewPush::new(cfg(server.uri()));
        let ctx = ToolCtx::new();
        let args = ToolArgs {
            positional: vec![Value::Str("t".into()), Value::Str("T".into())],
            named: vec![
                ("kind".into(), Value::Str("image".into())),
                (
                    "image_path".into(),
                    Value::Str(tmp.path().display().to_string()),
                ),
            ],
        };
        let v = tool.call(args, &ctx).await.unwrap();
        let Value::Struct(fields) = v else {
            panic!("expected struct");
        };
        assert!(matches!(
            &fields.iter().find(|(k, _)| k == "status").unwrap().1,
            Value::Str(s) if s == "ok"
        ));
    }

    #[tokio::test]
    async fn push_diff_raw_diff_carries_patch_text() {
        let server = MockServer::start().await;
        mount_project_and_topic(&server).await;
        let patch = "--- a/foo\n+++ b/foo\n@@ -1 +1 @@\n-old\n+new\n";
        Mock::given(method("POST"))
            .and(path("/api/projects/p/topics/t/blocks"))
            .and(wiremock::matchers::body_partial_json(serde_json::json!({
                "kind": "diff",
                "mode": "raw_diff",
                "patch_text": patch,
            })))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": "b_diff",
                "position": 0,
                "rendered_html_preview_url": "http://x/diff",
            })))
            .expect(1)
            .mount(&server)
            .await;

        let tool = PreviewPush::new(cfg(server.uri()));
        let ctx = ToolCtx::new();
        let args = ToolArgs {
            positional: vec![Value::Str("t".into()), Value::Str("T".into())],
            named: vec![
                ("kind".into(), Value::Str("diff".into())),
                ("raw_diff".into(), Value::Str(patch.into())),
            ],
        };
        let v = tool.call(args, &ctx).await.unwrap();
        let Value::Struct(fields) = v else {
            panic!("expected struct");
        };
        assert!(matches!(
            &fields.iter().find(|(k, _)| k == "status").unwrap().1,
            Value::Str(s) if s == "ok"
        ));
    }

    #[tokio::test]
    async fn push_diff_commit_sha_switches_mode() {
        let server = MockServer::start().await;
        mount_project_and_topic(&server).await;
        Mock::given(method("POST"))
            .and(path("/api/projects/p/topics/t/blocks"))
            .and(wiremock::matchers::body_partial_json(serde_json::json!({
                "kind": "diff",
                "mode": "commit_diff",
                "commit_sha": "abc123",
                "repo_path": "/repo",
            })))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": "b_diff_sha",
                "position": 0,
                "rendered_html_preview_url": "http://x/diff_sha",
            })))
            .expect(1)
            .mount(&server)
            .await;

        let tool = PreviewPush::new(cfg(server.uri()));
        let ctx = ToolCtx::new();
        let args = ToolArgs {
            positional: vec![Value::Str("t".into()), Value::Str("T".into())],
            named: vec![
                ("kind".into(), Value::Str("diff".into())),
                ("commit_sha".into(), Value::Str("abc123".into())),
                ("repo_path".into(), Value::Str("/repo".into())),
            ],
        };
        let v = tool.call(args, &ctx).await.unwrap();
        let Value::Struct(fields) = v else {
            panic!("expected struct");
        };
        assert!(matches!(
            &fields.iter().find(|(k, _)| k == "status").unwrap().1,
            Value::Str(s) if s == "ok"
        ));
    }

    #[tokio::test]
    async fn push_image_without_data_returns_error() {
        let tool = PreviewPush::new(cfg("http://127.0.0.1:1".into()));
        let ctx = ToolCtx::new();
        let args = ToolArgs {
            positional: vec![Value::Str("t".into()), Value::Str("T".into())],
            named: vec![("kind".into(), Value::Str("image".into()))],
        };
        let err = tool.call(args, &ctx).await;
        match err {
            Err(RuntimeError::ToolFailed(msg)) => {
                assert!(
                    msg.contains("image_base64") || msg.contains("image_path"),
                    "err: {msg}"
                );
            }
            Ok(Value::Struct(fields))
                if matches!(
                    &fields.iter().find(|(k, _)| k == "status").unwrap().1,
                    Value::Str(s) if s == "unavailable"
                ) => {}
            other => panic!("expected image data error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn push_rejects_content_over_max_bytes() {
        let mut c = cfg("http://127.0.0.1:1".into());
        c.max_body_bytes = 10;
        let tool = PreviewPush::new(c);
        let ctx = ToolCtx::new();
        let args = ToolArgs {
            positional: vec![
                Value::Str("t".into()),
                Value::Str("T".into()),
                Value::Str("x".repeat(100)),
            ],
            named: vec![],
        };
        let err = tool.call(args, &ctx).await.unwrap_err();
        assert!(format!("{err}").contains("exceeds max"));
    }

    #[tokio::test]
    async fn ping_returns_ok_on_healthy_server() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/health"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&server)
            .await;
        assert!(matches!(ping(&server.uri(), 500).await, PingResult::Ok));
    }

    #[tokio::test]
    async fn ping_returns_unavailable_on_connection_refused() {
        assert!(matches!(
            ping("http://127.0.0.1:1", 200).await,
            PingResult::Unavailable
        ));
    }
}
