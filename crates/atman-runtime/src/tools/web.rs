use std::sync::Arc;

use crate::error::RuntimeError;
use crate::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::Value;

#[derive(Debug, Clone)]
pub struct WebConfig {
    pub max_bytes: usize,
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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "provider", rename_all = "lowercase")]
pub enum SearchConfig {
    #[serde(rename = "none")]
    None,
    Tavily {
        #[serde(default)]
        api_key: Option<String>,
        #[serde(default)]
        base_url: Option<String>,
        #[serde(default)]
        max_results: Option<usize>,
    },
    Searxng {
        #[serde(default)]
        base_url: Option<String>,
        #[serde(default)]
        max_results: Option<usize>,
    },
    Parallel {
        api_key: String,
        #[serde(default)]
        base_url: Option<String>,
        #[serde(default)]
        max_results: Option<usize>,
    },
    Brave {
        api_key: String,
        #[serde(default)]
        base_url: Option<String>,
    },
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self::Tavily {
            api_key: None,
            base_url: None,
            max_results: None,
        }
    }
}

fn tavily_default_endpoint() -> String {
    "https://api.tavily.com".into()
}
fn parallel_default_endpoint() -> String {
    "https://api.parallel.ai".into()
}
fn brave_default_endpoint() -> String {
    "https://api.search.brave.com/res/v1/web/search".into()
}
fn default_max_results() -> usize {
    8
}

impl SearchConfig {
    pub fn provider_name(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Tavily { .. } => "tavily",
            Self::Searxng { .. } => "searxng",
            Self::Parallel { .. } => "parallel",
            Self::Brave { .. } => "brave",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub content: Option<String>,
    pub published_date: Option<String>,
    pub score: Option<f64>,
}

impl SearchResult {
    pub fn into_value(self) -> Value {
        let mut fields: Vec<(String, Value)> = vec![
            ("title".into(), Value::Str(self.title)),
            ("url".into(), Value::Str(self.url)),
            ("snippet".into(), Value::Str(self.snippet)),
        ];
        if let Some(c) = self.content {
            fields.push(("content".into(), Value::Str(c)));
        }
        if let Some(d) = self.published_date {
            fields.push(("published_date".into(), Value::Str(d)));
        }
        if let Some(s) = self.score {
            fields.push(("score".into(), Value::Float(s)));
        }
        Value::Struct(fields)
    }
}

pub trait SearchProvider: Send + Sync {
    fn name(&self) -> &'static str;
    fn call<'a>(&'a self, query: &'a str) -> BoxFut<'a, Result<Vec<SearchResult>, RuntimeError>>;
}

pub fn build_search_provider(cfg: &SearchConfig) -> Option<Arc<dyn SearchProvider>> {
    match cfg {
        SearchConfig::None => None,
        SearchConfig::Tavily {
            api_key,
            base_url,
            max_results,
        } => Some(Arc::new(TavilySearch::new(
            api_key.clone(),
            base_url.clone().unwrap_or_else(tavily_default_endpoint),
            max_results.unwrap_or_else(default_max_results),
        ))),
        SearchConfig::Searxng {
            base_url,
            max_results,
        } => Some(Arc::new(SearxngSearch::new(
            base_url
                .clone()
                .unwrap_or_else(|| "http://localhost:8080".into()),
            max_results.unwrap_or_else(default_max_results),
        ))),
        SearchConfig::Parallel {
            api_key,
            base_url,
            max_results,
        } => Some(Arc::new(ParallelSearch::new(
            api_key.clone(),
            base_url.clone().unwrap_or_else(parallel_default_endpoint),
            max_results.unwrap_or_else(default_max_results),
        ))),
        SearchConfig::Brave { api_key, base_url } => Some(Arc::new(BraveSearch::with_endpoint(
            api_key.clone(),
            base_url.clone().unwrap_or_else(brave_default_endpoint),
        ))),
    }
}

pub struct TavilySearch {
    api_key: Option<String>,
    base_url: String,
    max_results: usize,
    client: reqwest::Client,
}

impl TavilySearch {
    pub fn new(api_key: Option<String>, base_url: String, max_results: usize) -> Self {
        Self {
            api_key,
            base_url,
            max_results: max_results.max(1),
            client: reqwest::Client::new(),
        }
    }
}

impl SearchProvider for TavilySearch {
    fn name(&self) -> &'static str {
        "tavily"
    }

    fn call<'a>(&'a self, query: &'a str) -> BoxFut<'a, Result<Vec<SearchResult>, RuntimeError>> {
        Box::pin(async move {
            let body = serde_json::json!({
                "query": query,
                "max_results": self.max_results,
                "search_depth": "basic",
            });
            let mut req = self
                .client
                .post(format!("{}/search", self.base_url))
                .header("Content-Type", "application/json")
                .json(&body);
            if let Some(key) = &self.api_key {
                req = req.header("Authorization", format!("Bearer {key}"));
            } else {
                req = req.header("X-Tavily-Access-Mode", "keyless");
            }
            let resp = req
                .send()
                .await
                .map_err(|e| RuntimeError::ToolFailed(format!("web.search: tavily: {e}")))?;
            let status = resp.status();
            let json: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| RuntimeError::ToolFailed(format!("web.search: tavily decode: {e}")))?;
            if !status.is_success() {
                return Err(RuntimeError::ToolFailed(format!(
                    "web.search: tavily returned {status}: {json}"
                )));
            }
            let items = json
                .get("results")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            Ok(items
                .into_iter()
                .map(|item| SearchResult {
                    title: item
                        .get("title")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .into(),
                    url: item
                        .get("url")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .into(),
                    snippet: item
                        .get("content")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .into(),
                    content: item
                        .get("raw_content")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    published_date: None,
                    score: item.get("score").and_then(|v| v.as_f64()),
                })
                .collect())
        })
    }
}

pub struct SearxngSearch {
    base_url: String,
    max_results: usize,
    client: reqwest::Client,
}

impl SearxngSearch {
    pub fn new(base_url: String, max_results: usize) -> Self {
        Self {
            base_url,
            max_results: max_results.max(1),
            client: reqwest::Client::new(),
        }
    }
}

impl SearchProvider for SearxngSearch {
    fn name(&self) -> &'static str {
        "searxng"
    }

    fn call<'a>(&'a self, query: &'a str) -> BoxFut<'a, Result<Vec<SearchResult>, RuntimeError>> {
        Box::pin(async move {
            let resp = self
                .client
                .get(format!("{}/search", self.base_url.trim_end_matches('/')))
                .query(&[("q", query), ("format", "json")])
                .send()
                .await
                .map_err(|e| RuntimeError::ToolFailed(format!("web.search: searxng: {e}")))?;
            let status = resp.status();
            let json: serde_json::Value = resp.json().await.map_err(|e| {
                RuntimeError::ToolFailed(format!("web.search: searxng decode: {e}"))
            })?;
            if !status.is_success() {
                return Err(RuntimeError::ToolFailed(format!(
                    "web.search: searxng returned {status}: {json}"
                )));
            }
            let items = json
                .get("results")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            Ok(items
                .into_iter()
                .take(self.max_results)
                .map(|item| SearchResult {
                    title: item
                        .get("title")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .into(),
                    url: item
                        .get("url")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .into(),
                    snippet: item
                        .get("content")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .into(),
                    content: None,
                    published_date: None,
                    score: None,
                })
                .collect())
        })
    }
}

pub struct ParallelSearch {
    api_key: String,
    base_url: String,
    max_results: usize,
    client: reqwest::Client,
}

impl ParallelSearch {
    pub fn new(api_key: String, base_url: String, max_results: usize) -> Self {
        Self {
            api_key,
            base_url,
            max_results: max_results.max(1),
            client: reqwest::Client::new(),
        }
    }
}

impl SearchProvider for ParallelSearch {
    fn name(&self) -> &'static str {
        "parallel"
    }

    fn call<'a>(&'a self, query: &'a str) -> BoxFut<'a, Result<Vec<SearchResult>, RuntimeError>> {
        Box::pin(async move {
            let body = serde_json::json!({
                "objective": query,
                "search_queries": [query],
            });
            let resp = self
                .client
                .post(format!("{}/v1/search", self.base_url.trim_end_matches('/')))
                .header("Content-Type", "application/json")
                .header("x-api-key", &self.api_key)
                .json(&body)
                .send()
                .await
                .map_err(|e| RuntimeError::ToolFailed(format!("web.search: parallel: {e}")))?;
            let status = resp.status();
            let json: serde_json::Value = resp.json().await.map_err(|e| {
                RuntimeError::ToolFailed(format!("web.search: parallel decode: {e}"))
            })?;
            if !status.is_success() {
                return Err(RuntimeError::ToolFailed(format!(
                    "web.search: parallel returned {status}: {json}"
                )));
            }
            let items = json
                .get("results")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            Ok(items
                .into_iter()
                .take(self.max_results)
                .map(|item| {
                    let excerpts = item
                        .get("excerpts")
                        .and_then(|v| v.as_array())
                        .and_then(|arr| {
                            arr.iter()
                                .filter_map(|e| e.as_str())
                                .collect::<Vec<_>>()
                                .join("\n\n")
                                .into()
                        })
                        .filter(|s: &String| !s.is_empty());
                    SearchResult {
                        title: item
                            .get("title")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .into(),
                        url: item
                            .get("url")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .into(),
                        snippet: excerpts.clone().unwrap_or_default(),
                        content: excerpts,
                        published_date: item
                            .get("publish_date")
                            .and_then(|v| v.as_str())
                            .map(String::from),
                        score: None,
                    }
                })
                .collect())
        })
    }
}

pub struct BraveSearch {
    api_key: String,
    endpoint: String,
    client: reqwest::Client,
}

impl BraveSearch {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_endpoint(api_key, "https://api.search.brave.com/res/v1/web/search")
    }

    pub fn with_endpoint(api_key: impl Into<String>, endpoint: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            endpoint: endpoint.into(),
            client: reqwest::Client::new(),
        }
    }
}

impl SearchProvider for BraveSearch {
    fn name(&self) -> &'static str {
        "brave"
    }

    fn call<'a>(&'a self, query: &'a str) -> BoxFut<'a, Result<Vec<SearchResult>, RuntimeError>> {
        Box::pin(async move {
            let resp = self
                .client
                .get(&self.endpoint)
                .query(&[("q", query)])
                .header("X-Subscription-Token", &self.api_key)
                .header("Accept", "application/json")
                .send()
                .await
                .map_err(|e| RuntimeError::ToolFailed(format!("web.search: {e}")))?;
            let status = resp.status();
            let json: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| RuntimeError::ToolFailed(format!("web.search decode: {e}")))?;
            if !status.is_success() {
                return Err(RuntimeError::ToolFailed(format!(
                    "web.search: brave returned {status}: {json}"
                )));
            }
            let items = json
                .pointer("/web/results")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            Ok(items
                .into_iter()
                .map(|item| SearchResult {
                    title: item
                        .get("title")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .into(),
                    url: item
                        .get("url")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .into(),
                    snippet: item
                        .get("description")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .into(),
                    content: None,
                    published_date: None,
                    score: None,
                })
                .collect())
        })
    }
}

pub struct WebSearch {
    provider: Arc<dyn SearchProvider>,
}

impl WebSearch {
    pub fn new(provider: Arc<dyn SearchProvider>) -> Self {
        Self { provider }
    }
}

impl Tool for WebSearch {
    fn name(&self) -> &str {
        "web.search"
    }

    fn tier(&self) -> Tier {
        Tier::Three
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Search the web with the configured search provider and return result titles, URLs, snippets, and optional metadata. Use it when you need current external information or candidate pages to fetch.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Search query text."}
            },
            "required": ["query"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let query = extract_string(&args, "query", 0)?;
            let results = self.provider.call(&query).await?;
            Ok(Value::List(
                results.into_iter().map(SearchResult::into_value).collect(),
            ))
        })
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

    fn description(&self) -> Option<&str> {
        Some(
            "Fetch a URL subject to configured allow/deny policy and return status, content type, body, and truncation status. HTML responses are converted to markdown for easier reading.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "HTTP or HTTPS URL to fetch."}
            },
            "required": ["url"]
        })
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
            let content_type = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string();
            let bytes = resp
                .bytes()
                .await
                .map_err(|e| RuntimeError::ToolFailed(format!("web.fetch read body: {e}")))?;
            let truncated = bytes.len() > self.config.max_bytes;
            let raw = if truncated {
                let slice = &bytes[..self.config.max_bytes];
                String::from_utf8_lossy(slice).into_owned()
            } else {
                String::from_utf8_lossy(&bytes).into_owned()
            };
            let is_html = content_type.contains("text/html");
            let body = if is_html {
                match htmd::convert(&raw) {
                    Ok(md) => md,
                    Err(_) => raw,
                }
            } else {
                raw
            };
            let body = if truncated {
                format!(
                    "{}\n[atman: truncated at {} bytes; full length {}]",
                    body,
                    self.config.max_bytes,
                    bytes.len()
                )
            } else {
                body
            };
            Ok(Value::Struct(vec![
                ("status".into(), Value::Int(status)),
                ("body".into(), Value::Str(body)),
                ("truncated".into(), Value::Bool(truncated)),
                ("content_type".into(), Value::Str(content_type)),
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

    #[tokio::test]
    async fn brave_search_maps_results_and_forwards_api_key() {
        let server = MockServer::start().await;
        let body = serde_json::json!({
            "web": {
                "results": [
                    {"title": "atman flow DSL", "url": "https://example.com/a", "description": "flow-driven code agent"},
                    {"title": "runtime notes", "url": "https://example.com/b", "description": "watch supervisor"}
                ]
            }
        });
        Mock::given(method("GET"))
            .and(path("/web/search"))
            .and(wiremock::matchers::header(
                "X-Subscription-Token",
                "secret-key",
            ))
            .and(wiremock::matchers::query_param("q", "atman"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(&server)
            .await;

        let provider = Arc::new(BraveSearch::with_endpoint(
            "secret-key",
            format!("{}/web/search", server.uri()),
        ));
        let tool = WebSearch::new(provider);
        let ctx = ToolCtx::new();
        let args = ToolArgs {
            positional: vec![Value::Str("atman".into())],
            named: vec![],
        };
        let v = tool.call(args, &ctx).await.unwrap();
        let Value::List(items) = v else {
            panic!("expected list");
        };
        assert_eq!(items.len(), 2);
        let Value::Struct(first) = &items[0] else {
            panic!("first not struct");
        };
        assert!(
            matches!(&first.iter().find(|(k, _)| k == "title").unwrap().1, Value::Str(s) if s == "atman flow DSL")
        );
        assert!(
            matches!(&first.iter().find(|(k, _)| k == "url").unwrap().1, Value::Str(s) if s == "https://example.com/a")
        );
    }

    #[tokio::test]
    async fn brave_search_surfaces_http_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/web/search"))
            .respond_with(
                ResponseTemplate::new(429)
                    .set_body_json(serde_json::json!({"error": "rate limit"})),
            )
            .mount(&server)
            .await;

        let provider = Arc::new(BraveSearch::with_endpoint(
            "k",
            format!("{}/web/search", server.uri()),
        ));
        let tool = WebSearch::new(provider);
        let ctx = ToolCtx::new();
        let args = ToolArgs {
            positional: vec![Value::Str("x".into())],
            named: vec![],
        };
        let err = tool.call(args, &ctx).await.err().unwrap();
        let msg = format!("{err}");
        assert!(
            msg.contains("429") || msg.contains("rate limit"),
            "msg: {msg}"
        );
    }

    #[tokio::test]
    async fn tavily_search_keyless_sends_keyless_header() {
        let server = MockServer::start().await;
        let body = serde_json::json!({
            "results": [
                {"title": "Rust async", "url": "https://example.com/a", "content": "tokio primer", "score": 0.9}
            ]
        });
        Mock::given(method("POST"))
            .and(path("/search"))
            .and(wiremock::matchers::header(
                "X-Tavily-Access-Mode",
                "keyless",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(&server)
            .await;

        let provider = TavilySearch::new(None, server.uri(), 5);
        let results = provider.call("rust async").await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Rust async");
        assert_eq!(results[0].url, "https://example.com/a");
        assert_eq!(results[0].snippet, "tokio primer");
        assert_eq!(results[0].score, Some(0.9));
    }

    #[tokio::test]
    async fn tavily_search_keyed_sends_bearer() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/search"))
            .and(wiremock::matchers::header(
                "Authorization",
                "Bearer tvly-secret",
            ))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"results": []})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let provider = TavilySearch::new(Some("tvly-secret".into()), server.uri(), 5);
        provider.call("q").await.unwrap();
    }

    #[tokio::test]
    async fn searxng_search_maps_results() {
        let server = MockServer::start().await;
        let body = serde_json::json!({
            "results": [
                {"title": "SearXNG", "url": "https://example.com/s", "content": "meta search"},
                {"title": "second", "url": "https://example.com/b", "content": "more"}
            ]
        });
        Mock::given(method("GET"))
            .and(path("/search"))
            .and(wiremock::matchers::query_param("format", "json"))
            .and(wiremock::matchers::query_param("q", "test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let provider = SearxngSearch::new(server.uri(), 1);
        let results = provider.call("test").await.unwrap();
        assert_eq!(results.len(), 1, "max_results should cap");
        assert_eq!(results[0].title, "SearXNG");
    }

    #[tokio::test]
    async fn parallel_search_maps_excerpts_to_content() {
        let server = MockServer::start().await;
        let body = serde_json::json!({
            "results": [
                {"title": "Parallel", "url": "https://example.com/p", "excerpts": ["line one", "line two"], "publish_date": "2026-01-01"}
            ]
        });
        Mock::given(method("POST"))
            .and(path("/v1/search"))
            .and(wiremock::matchers::header("x-api-key", "par-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let provider = ParallelSearch::new("par-key".into(), server.uri(), 5);
        let results = provider.call("objective").await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Parallel");
        assert!(results[0].content.as_deref().unwrap().contains("line one"));
        assert!(results[0].content.as_deref().unwrap().contains("line two"));
        assert_eq!(results[0].published_date.as_deref(), Some("2026-01-01"));
    }

    #[tokio::test]
    async fn parallel_search_surfaces_http_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/search"))
            .respond_with(
                ResponseTemplate::new(401).set_body_json(serde_json::json!({"message": "no key"})),
            )
            .mount(&server)
            .await;
        let provider = ParallelSearch::new("k".into(), server.uri(), 5);
        let err = provider.call("q").await.err().unwrap();
        assert!(format!("{err}").contains("401"), "{}", err);
    }

    #[tokio::test]
    async fn fetch_converts_html_to_markdown() {
        let server = MockServer::start().await;
        let html = "<html><body><h1>Title</h1><p>hello <a href=\"x\">link</a></p></body></html>";
        Mock::given(method("GET"))
            .and(path("/page"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(html.as_bytes())
                    .insert_header("content-type", "text/html; charset=utf-8"),
            )
            .mount(&server)
            .await;

        let tool = WebFetch::new(WebConfig::default());
        let ctx = ToolCtx::new();
        let args = ToolArgs {
            positional: vec![Value::Str(format!("{}/page", server.uri()))],
            named: vec![],
        };
        let v = tool.call(args, &ctx).await.unwrap();
        let Value::Struct(f) = v else {
            panic!("expected struct")
        };
        let Value::Str(body) = &f.iter().find(|(k, _)| k == "body").unwrap().1 else {
            panic!("body not str");
        };
        assert!(body.contains("# Title"), "h1 → markdown heading: {body}");
        assert!(body.contains("hello"), "paragraph text preserved: {body}");
        assert!(!body.contains("<html>"), "html tags stripped: {body}");
    }

    #[tokio::test]
    async fn fetch_returns_raw_for_non_html() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/json"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string(r#"{"key":"value"}"#),
            )
            .mount(&server)
            .await;

        let tool = WebFetch::new(WebConfig::default());
        let ctx = ToolCtx::new();
        let args = ToolArgs {
            positional: vec![Value::Str(format!("{}/json", server.uri()))],
            named: vec![],
        };
        let v = tool.call(args, &ctx).await.unwrap();
        let Value::Struct(f) = v else {
            panic!("expected struct")
        };
        let Value::Str(body) = &f.iter().find(|(k, _)| k == "body").unwrap().1 else {
            panic!("body not str");
        };
        assert_eq!(
            body, r#"{"key":"value"}"#,
            "json returned raw, no conversion"
        );
    }

    #[test]
    fn build_search_provider_returns_none_for_disabled() {
        assert!(build_search_provider(&SearchConfig::None).is_none());
    }

    #[test]
    fn build_search_provider_tavily() {
        let cfg = SearchConfig::Tavily {
            api_key: None,
            base_url: Some("https://api.tavily.com".into()),
            max_results: Some(5),
        };
        let p = build_search_provider(&cfg).unwrap();
        assert_eq!(p.name(), "tavily");
    }

    #[test]
    fn build_search_provider_searxng() {
        let cfg = SearchConfig::Searxng {
            base_url: Some("http://localhost:8080".into()),
            max_results: Some(10),
        };
        let p = build_search_provider(&cfg).unwrap();
        assert_eq!(p.name(), "searxng");
    }

    #[test]
    fn build_search_provider_tavily_uses_defaults_when_fields_missing() {
        let cfg = SearchConfig::Tavily {
            api_key: None,
            base_url: None,
            max_results: None,
        };
        let p = build_search_provider(&cfg).unwrap();
        assert_eq!(p.name(), "tavily");
    }

    #[test]
    fn default_search_config_is_tavily_keyless() {
        let cfg = SearchConfig::default();
        assert!(matches!(cfg, SearchConfig::Tavily { api_key: None, .. }));
        assert!(build_search_provider(&cfg).is_some());
    }

    #[test]
    fn search_config_deserializes_minimal_tavily() {
        let toml = r#"
[web.search]
provider = "tavily"
"#;
        #[derive(serde::Deserialize)]
        struct W {
            #[serde(default)]
            web: WebWrap,
        }
        #[derive(serde::Deserialize, Default)]
        struct WebWrap {
            #[serde(default)]
            search: Option<SearchConfig>,
        }
        let w: W = toml::from_str(toml).unwrap();
        match w.web.search {
            Some(SearchConfig::Tavily {
                api_key: None,
                base_url: None,
                max_results: None,
            }) => {}
            other => panic!("expected minimal Tavily, got {other:?}"),
        }
    }

    #[test]
    fn search_config_deserializes_none() {
        let toml = r#"
[web.search]
provider = "none"
"#;
        #[derive(serde::Deserialize)]
        struct W {
            #[serde(default)]
            web: WebWrap,
        }
        #[derive(serde::Deserialize, Default)]
        struct WebWrap {
            #[serde(default)]
            search: Option<SearchConfig>,
        }
        let w: W = toml::from_str(toml).unwrap();
        assert!(matches!(w.web.search, Some(SearchConfig::None)));
    }
}
