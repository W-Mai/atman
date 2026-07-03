use std::sync::Arc;

use crate::error::RuntimeError;
use crate::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::Value;

#[derive(Debug, Clone)]
pub struct WebConfig {
    pub max_bytes: usize,
    // Empty allowlist means "any URL allowed"; denylist runs first.
    pub url_allowlist: Vec<String>,
    pub url_denylist: Vec<String>,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            max_bytes: 1_000_000,
            url_allowlist: Vec::new(),
            url_denylist: Vec::new(),
        }
    }
}

impl WebConfig {
    pub fn url_allowed(&self, url: &str) -> bool {
        if self.url_denylist.iter().any(|p| url.starts_with(p)) {
            return false;
        }
        if self.url_allowlist.is_empty() {
            return true;
        }
        self.url_allowlist.iter().any(|p| url.starts_with(p))
    }
}

pub struct WebFetch {
    pub config: Arc<WebConfig>,
    pub client: reqwest::Client,
}

impl WebFetch {
    pub fn new(config: WebConfig) -> Self {
        Self {
            config: Arc::new(config),
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::limited(5))
                .build()
                .expect("build reqwest client"),
        }
    }
}

impl Tool for WebFetch {
    fn name(&self) -> &str {
        "web.fetch"
    }

    fn tier(&self) -> Tier {
        Tier::Three
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let url = extract_string(&args, "url", 0)?;
            if !self.config.url_allowed(&url) {
                return Err(RuntimeError::ToolFailed(format!(
                    "web.fetch: url `{url}` blocked by policy (denylist / allowlist)"
                )));
            }
            let resp = self
                .client
                .get(&url)
                .send()
                .await
                .map_err(|e| RuntimeError::ToolFailed(format!("web.fetch({url}): {e}")))?;
            let status = resp.status().as_u16() as i64;
            let bytes = resp
                .bytes()
                .await
                .map_err(|e| RuntimeError::ToolFailed(format!("web.fetch read body: {e}")))?;
            let truncated = bytes.len() > self.config.max_bytes;
            let body = if truncated {
                let slice = &bytes[..self.config.max_bytes];
                format!(
                    "{}\n[atman: truncated at {} bytes; full length {}]",
                    String::from_utf8_lossy(slice),
                    self.config.max_bytes,
                    bytes.len()
                )
            } else {
                String::from_utf8_lossy(&bytes).into_owned()
            };
            Ok(Value::Struct(vec![
                ("status".into(), Value::Int(status)),
                ("body".into(), Value::Str(body)),
                ("truncated".into(), Value::Bool(truncated)),
            ]))
        })
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
            expected: "string".into(),
            actual: other.kind_name().into(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn allowlist_empty_permits_any() {
        let cfg = WebConfig::default();
        assert!(cfg.url_allowed("https://anywhere.example/foo"));
    }

    #[test]
    fn denylist_takes_precedence() {
        let cfg = WebConfig {
            url_allowlist: vec!["https://ok.example".into()],
            url_denylist: vec!["https://ok.example/secret".into()],
            ..WebConfig::default()
        };
        assert!(cfg.url_allowed("https://ok.example/public"));
        assert!(!cfg.url_allowed("https://ok.example/secret/x"));
    }

    #[test]
    fn allowlist_non_empty_rejects_others() {
        let cfg = WebConfig {
            url_allowlist: vec!["https://ok.example".into()],
            ..WebConfig::default()
        };
        assert!(!cfg.url_allowed("https://elsewhere.example/x"));
    }

    #[tokio::test]
    async fn fetch_returns_body_and_status() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/hello"))
            .respond_with(ResponseTemplate::new(200).set_body_string("hi world"))
            .mount(&server)
            .await;

        let tool = WebFetch::new(WebConfig::default());
        let ctx = ToolCtx::new();
        let url = format!("{}/hello", server.uri());
        let args = ToolArgs {
            positional: vec![Value::Str(url)],
            named: vec![],
        };
        let v = tool.call(args, &ctx).await.unwrap();
        let Value::Struct(f) = v else {
            panic!("expected struct");
        };
        assert!(matches!(
            f.iter().find(|(k, _)| k == "status").unwrap().1,
            Value::Int(200)
        ));
        assert!(matches!(
            &f.iter().find(|(k, _)| k == "body").unwrap().1,
            Value::Str(s) if s == "hi world"
        ));
        assert!(matches!(
            f.iter().find(|(k, _)| k == "truncated").unwrap().1,
            Value::Bool(false)
        ));
    }

    #[tokio::test]
    async fn fetch_truncates_when_over_max_bytes() {
        let server = MockServer::start().await;
        let body = "A".repeat(50);
        Mock::given(method("GET"))
            .and(path("/big"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let cfg = WebConfig {
            max_bytes: 10,
            ..WebConfig::default()
        };
        let tool = WebFetch::new(cfg);
        let ctx = ToolCtx::new();
        let args = ToolArgs {
            positional: vec![Value::Str(format!("{}/big", server.uri()))],
            named: vec![],
        };
        let v = tool.call(args, &ctx).await.unwrap();
        let Value::Struct(f) = v else {
            panic!("expected struct");
        };
        assert!(matches!(
            f.iter().find(|(k, _)| k == "truncated").unwrap().1,
            Value::Bool(true)
        ));
        let Value::Str(body) = &f.iter().find(|(k, _)| k == "body").unwrap().1 else {
            panic!("body not str");
        };
        assert!(body.contains("truncated at 10 bytes"));
        assert!(body.contains("full length 50"));
    }

    #[tokio::test]
    async fn fetch_rejects_when_url_denylisted() {
        let cfg = WebConfig {
            url_denylist: vec!["https://bad.example".into()],
            ..WebConfig::default()
        };
        let tool = WebFetch::new(cfg);
        let ctx = ToolCtx::new();
        let args = ToolArgs {
            positional: vec![Value::Str("https://bad.example/x".into())],
            named: vec![],
        };
        let err = tool.call(args, &ctx).await.err().unwrap();
        assert!(format!("{err}").contains("blocked by policy"));
    }
}
