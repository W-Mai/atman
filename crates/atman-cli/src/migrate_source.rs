use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRef {
    pub id: String,
    pub title: String,
    pub created_ms: i64,
    pub source_tag: String,
    pub project_dir: Option<String>,
    pub raw_meta_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedMessage {
    pub role: MessageRole,
    pub text: String,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub created_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRole {
    User,
    Assistant,
    System,
    Tool,
}

impl MessageRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::System => "system",
            MessageRole::Tool => "tool",
        }
    }
}

pub trait MigrationSource {
    fn source_tag(&self) -> &'static str;
    fn discover_sessions(&self) -> Result<Vec<SessionRef>>;
    fn load_messages(&self, session_id: &str) -> Result<Vec<ImportedMessage>>;
}

pub struct KiroCliSource {
    sessions_root: PathBuf,
}

impl KiroCliSource {
    pub fn new(sessions_root: PathBuf) -> Self {
        Self { sessions_root }
    }

    pub fn default_root() -> Result<PathBuf> {
        let home = home_dir().context("no HOME directory available")?;
        Ok(home.join(".kiro").join("sessions").join("cli"))
    }
}

#[derive(Debug, Deserialize)]
struct KiroSessionMeta {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
}

#[derive(Debug, Deserialize)]
struct KiroTurn {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    data: serde_json::Value,
}

impl MigrationSource for KiroCliSource {
    fn source_tag(&self) -> &'static str {
        "kiro-cli"
    }

    fn discover_sessions(&self) -> Result<Vec<SessionRef>> {
        if !self.sessions_root.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in read_dir(&self.sessions_root)? {
            let e = entry?;
            let path = e.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("read {}", path.display()))?;
            let Ok(meta) = serde_json::from_str::<KiroSessionMeta>(&text) else {
                continue;
            };
            let id = match meta.session_id.clone() {
                Some(id) => id,
                None => path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string(),
            };
            let created_ms = meta
                .created_at
                .as_deref()
                .and_then(parse_rfc3339_ms)
                .unwrap_or(0);
            out.push(SessionRef {
                id: id.clone(),
                title: meta.title.unwrap_or(id),
                created_ms,
                source_tag: "kiro-cli".to_string(),
                project_dir: meta.cwd,
                raw_meta_path: path,
            });
        }
        out.sort_by_key(|s| std::cmp::Reverse(s.created_ms));
        Ok(out)
    }

    fn load_messages(&self, session_id: &str) -> Result<Vec<ImportedMessage>> {
        let jsonl = self.sessions_root.join(format!("{session_id}.jsonl"));
        if !jsonl.exists() {
            bail!(
                "no transcript for kiro session {session_id} at {}",
                jsonl.display()
            );
        }
        let text =
            std::fs::read_to_string(&jsonl).with_context(|| format!("read {}", jsonl.display()))?;
        let mut out = Vec::new();
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(turn) = serde_json::from_str::<KiroTurn>(trimmed) else {
                continue;
            };
            let role = match turn.kind.as_str() {
                "Prompt" => MessageRole::User,
                "AssistantMessage" => MessageRole::Assistant,
                "ToolResults" => MessageRole::Tool,
                _ => continue,
            };
            let created_ms = turn
                .data
                .get("meta")
                .and_then(|m| m.get("timestamp"))
                .and_then(|t| t.as_i64())
                .map(|s| s.saturating_mul(1000))
                .unwrap_or(0);
            let content = turn.data.get("content").and_then(|c| c.as_array());
            let Some(parts) = content else {
                continue;
            };
            let text = collect_kiro_parts(parts, role);
            if text.trim().is_empty() {
                continue;
            }
            out.push(ImportedMessage {
                role,
                text,
                agent: None,
                model: None,
                created_ms,
            });
        }
        Ok(out)
    }
}

fn parse_rfc3339_ms(s: &str) -> Option<i64> {
    let dt = chrono_like_parse(s)?;
    Some(dt)
}

fn chrono_like_parse(s: &str) -> Option<i64> {
    let trimmed = s.trim_end_matches('Z');
    let (date, rest) = trimmed.split_once('T')?;
    let (time_part, frac) = match rest.find('.') {
        Some(dot) => (&rest[..dot], &rest[dot..]),
        None => (rest, ""),
    };
    let date_parts: Vec<&str> = date.split('-').collect();
    let time_parts: Vec<&str> = time_part.split(':').collect();
    if date_parts.len() != 3 || time_parts.len() != 3 {
        return None;
    }
    let y: i32 = date_parts[0].parse().ok()?;
    let mo: u32 = date_parts[1].parse().ok()?;
    let d: u32 = date_parts[2].parse().ok()?;
    let h: u32 = time_parts[0].parse().ok()?;
    let mi: u32 = time_parts[1].parse().ok()?;
    let s: f64 = format!("{}{}", time_parts[2], frac).parse().ok()?;
    let secs =
        days_from_civil(y, mo, d) * 86_400 + i64::from(h) * 3600 + i64::from(mi) * 60 + s as i64;
    Some(secs.saturating_mul(1000))
}

fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = y - if m <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u32;
    let m = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * m + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    i64::from(era) * 146_097 + i64::from(doe) - 719_468
}

fn collect_kiro_parts(parts: &[serde_json::Value], role: MessageRole) -> String {
    let mut buf = String::new();
    for part in parts {
        let kind = part.get("kind").and_then(|k| k.as_str()).unwrap_or("");
        match kind {
            "text" => {
                if let Some(t) = part.get("data").and_then(|d| d.as_str())
                    && !t.is_empty()
                {
                    push_with_sep(&mut buf, t);
                }
            }
            "toolResult" if matches!(role, MessageRole::Tool) => {
                let Some(inner) = part
                    .get("data")
                    .and_then(|d| d.get("content"))
                    .and_then(|c| c.as_array())
                else {
                    continue;
                };
                for sub in inner {
                    if sub.get("kind").and_then(|k| k.as_str()) == Some("text")
                        && let Some(t) = sub.get("data").and_then(|d| d.as_str())
                    {
                        push_with_sep(&mut buf, t);
                    }
                }
            }
            _ => {}
        }
    }
    buf
}

fn push_with_sep(buf: &mut String, s: &str) {
    if !buf.is_empty() {
        buf.push_str("\n\n");
    }
    buf.push_str(s);
}

pub struct OpencodeSource {
    storage_root: PathBuf,
}

impl OpencodeSource {
    pub fn new(storage_root: PathBuf) -> Self {
        Self { storage_root }
    }

    pub fn default_root() -> Result<PathBuf> {
        let home = home_dir().context("no HOME directory available")?;
        Ok(home
            .join(".local")
            .join("share")
            .join("opencode")
            .join("storage"))
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[derive(Debug, Deserialize)]
struct OpencodeSessionMeta {
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    directory: Option<String>,
    #[serde(default)]
    time: Option<OpencodeTime>,
}

#[derive(Debug, Deserialize)]
struct OpencodeTime {
    #[serde(default)]
    created: i64,
    #[serde(default, rename = "updated")]
    _updated: i64,
}

#[derive(Debug, Deserialize)]
struct OpencodeMessageMeta {
    id: String,
    role: String,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    model: Option<OpencodeModel>,
    #[serde(default)]
    time: Option<OpencodeMsgTime>,
}

#[derive(Debug, Deserialize)]
struct OpencodeMsgTime {
    #[serde(default)]
    created: i64,
}

#[derive(Debug, Deserialize)]
struct OpencodeModel {
    #[serde(default)]
    #[serde(rename = "providerID")]
    provider_id: Option<String>,
    #[serde(default)]
    #[serde(rename = "modelID")]
    model_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpencodePart {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
}

impl MigrationSource for OpencodeSource {
    fn source_tag(&self) -> &'static str {
        "opencode"
    }

    fn discover_sessions(&self) -> Result<Vec<SessionRef>> {
        let session_root = self.storage_root.join("session");
        if !session_root.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for project_entry in read_dir(&session_root)? {
            let project_dir = project_entry?;
            if !project_dir.file_type()?.is_dir() {
                continue;
            }
            for sess_entry in read_dir(&project_dir.path())? {
                let sess = sess_entry?;
                let path = sess.path();
                if path.extension().and_then(|s| s.to_str()) != Some("json") {
                    continue;
                }
                let text = std::fs::read_to_string(&path)
                    .with_context(|| format!("read {}", path.display()))?;
                let meta: OpencodeSessionMeta = match serde_json::from_str(&text) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                out.push(SessionRef {
                    id: meta.id.clone(),
                    title: meta.title.unwrap_or_else(|| meta.id.clone()),
                    created_ms: meta.time.as_ref().map(|t| t.created).unwrap_or(0),
                    source_tag: "opencode".to_string(),
                    project_dir: meta.directory,
                    raw_meta_path: path,
                });
            }
        }
        out.sort_by_key(|s| std::cmp::Reverse(s.created_ms));
        Ok(out)
    }

    fn load_messages(&self, session_id: &str) -> Result<Vec<ImportedMessage>> {
        let msg_dir = self.storage_root.join("message").join(session_id);
        if !msg_dir.exists() {
            bail!("no messages directory for session {session_id}");
        }
        let mut metas: Vec<OpencodeMessageMeta> = Vec::new();
        for entry in read_dir(&msg_dir)? {
            let e = entry?;
            let path = e.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("read {}", path.display()))?;
            let Ok(meta) = serde_json::from_str::<OpencodeMessageMeta>(&text) else {
                continue;
            };
            metas.push(meta);
        }
        metas.sort_by(|a, b| {
            let a_ts = a.time.as_ref().map(|t| t.created).unwrap_or(0);
            let b_ts = b.time.as_ref().map(|t| t.created).unwrap_or(0);
            a_ts.cmp(&b_ts).then_with(|| a.id.cmp(&b.id))
        });

        let mut out = Vec::new();
        for meta in metas {
            let text = load_message_text(&self.storage_root, &meta.id)?;
            if text.trim().is_empty() {
                continue;
            }
            let role = parse_role(&meta.role);
            let created_ms = meta.time.as_ref().map(|t| t.created).unwrap_or(0);
            let model_str = meta.model.and_then(|m| match (m.provider_id, m.model_id) {
                (Some(p), Some(md)) => Some(format!("{p}/{md}")),
                (None, Some(md)) => Some(md),
                (Some(p), None) => Some(p),
                _ => None,
            });
            out.push(ImportedMessage {
                role,
                text,
                agent: meta.agent,
                model: model_str,
                created_ms,
            });
        }
        Ok(out)
    }
}

fn read_dir(path: &Path) -> Result<std::fs::ReadDir> {
    std::fs::read_dir(path).with_context(|| format!("read_dir {}", path.display()))
}

fn parse_role(role: &str) -> MessageRole {
    match role.to_ascii_lowercase().as_str() {
        "user" => MessageRole::User,
        "assistant" => MessageRole::Assistant,
        "system" => MessageRole::System,
        "tool" => MessageRole::Tool,
        _ => MessageRole::Assistant,
    }
}

fn load_message_text(storage_root: &Path, message_id: &str) -> Result<String> {
    let part_dir = storage_root.join("part").join(message_id);
    if !part_dir.exists() {
        return Ok(String::new());
    }
    let mut parts: Vec<(String, String)> = Vec::new();
    for entry in read_dir(&part_dir)? {
        let e = entry?;
        let path = e.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let file_name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        let text =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let Ok(part) = serde_json::from_str::<OpencodePart>(&text) else {
            continue;
        };
        if part.kind != "text" {
            continue;
        }
        if let Some(t) = part.text {
            parts.push((file_name, t));
        }
    }
    parts.sort_by(|a, b| a.0.cmp(&b.0));
    let joined: Vec<String> = parts.into_iter().map(|(_, t)| t).collect();
    Ok(joined.join("\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed_opencode_fixture(root: &Path) {
        let sess_dir = root.join("session/proj_hash");
        std::fs::create_dir_all(&sess_dir).unwrap();
        std::fs::write(
            sess_dir.join("ses_abc.json"),
            r#"{"id":"ses_abc","title":"chat one","directory":"/proj",
                "time":{"created":1000,"updated":2000}}"#,
        )
        .unwrap();
        std::fs::write(
            sess_dir.join("ses_def.json"),
            r#"{"id":"ses_def","title":"chat two","time":{"created":500,"updated":600}}"#,
        )
        .unwrap();

        let msg_dir = root.join("message/ses_abc");
        std::fs::create_dir_all(&msg_dir).unwrap();
        std::fs::write(
            msg_dir.join("msg_1.json"),
            r#"{"id":"msg_1","sessionID":"ses_abc","role":"user",
                "time":{"created":1001}}"#,
        )
        .unwrap();
        std::fs::write(
            msg_dir.join("msg_2.json"),
            r#"{"id":"msg_2","sessionID":"ses_abc","role":"assistant",
                "agent":"explore","model":{"providerID":"opencode","modelID":"big-pickle"},
                "time":{"created":1002}}"#,
        )
        .unwrap();

        let part_msg_1 = root.join("part/msg_1");
        std::fs::create_dir_all(&part_msg_1).unwrap();
        std::fs::write(
            part_msg_1.join("prt_1a.json"),
            r#"{"id":"prt_1a","messageID":"msg_1","type":"text","text":"hello agent"}"#,
        )
        .unwrap();

        let part_msg_2 = root.join("part/msg_2");
        std::fs::create_dir_all(&part_msg_2).unwrap();
        std::fs::write(
            part_msg_2.join("prt_2a.json"),
            r#"{"id":"prt_2a","messageID":"msg_2","type":"text","text":"first chunk"}"#,
        )
        .unwrap();
        std::fs::write(
            part_msg_2.join("prt_2b.json"),
            r#"{"id":"prt_2b","messageID":"msg_2","type":"text","text":"second chunk"}"#,
        )
        .unwrap();
        std::fs::write(
            part_msg_2.join("prt_2c.json"),
            r#"{"id":"prt_2c","messageID":"msg_2","type":"tool_use","text":"ignored"}"#,
        )
        .unwrap();
    }

    #[test]
    fn discover_sessions_sorted_newest_first() {
        let tmp = tempfile::tempdir().unwrap();
        seed_opencode_fixture(tmp.path());
        let src = OpencodeSource::new(tmp.path().to_path_buf());
        let sessions = src.discover_sessions().unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].id, "ses_abc");
        assert_eq!(sessions[0].title, "chat one");
        assert_eq!(sessions[0].project_dir.as_deref(), Some("/proj"));
        assert_eq!(sessions[1].id, "ses_def");
    }

    #[test]
    fn discover_returns_empty_when_storage_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let src = OpencodeSource::new(tmp.path().join("nope"));
        assert!(src.discover_sessions().unwrap().is_empty());
    }

    #[test]
    fn load_messages_orders_by_time_then_id_and_joins_text_parts() {
        let tmp = tempfile::tempdir().unwrap();
        seed_opencode_fixture(tmp.path());
        let src = OpencodeSource::new(tmp.path().to_path_buf());
        let messages = src.load_messages("ses_abc").unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, MessageRole::User);
        assert_eq!(messages[0].text, "hello agent");
        assert_eq!(messages[1].role, MessageRole::Assistant);
        assert_eq!(messages[1].text, "first chunk\n\nsecond chunk");
        assert_eq!(messages[1].agent.as_deref(), Some("explore"));
        assert_eq!(messages[1].model.as_deref(), Some("opencode/big-pickle"));
    }

    #[test]
    fn load_messages_missing_session_errors() {
        let tmp = tempfile::tempdir().unwrap();
        seed_opencode_fixture(tmp.path());
        let src = OpencodeSource::new(tmp.path().to_path_buf());
        let err = src.load_messages("ses_nope").unwrap_err();
        assert!(
            err.to_string().contains("no messages directory"),
            "want missing-dir error, got: {err}"
        );
    }

    fn seed_kiro_fixture(root: &Path) {
        std::fs::create_dir_all(root).unwrap();
        std::fs::write(
            root.join("aaa.json"),
            r#"{"session_id":"aaa","cwd":"/proj","created_at":"2026-04-09T18:52:46.845470Z",
                "title":"newer chat"}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("bbb.json"),
            r#"{"session_id":"bbb","cwd":"/proj","created_at":"2026-01-01T00:00:00.000000Z",
                "title":"older chat"}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("aaa.jsonl"),
            r#"{"version":"v1","kind":"Prompt","data":{"content":[{"kind":"text","data":"hello"}],"meta":{"timestamp":1000}}}
{"version":"v1","kind":"AssistantMessage","data":{"content":[{"kind":"text","data":"greetings"},{"kind":"toolUse","data":{"toolUseId":"tu1"}}]}}
{"version":"v1","kind":"ToolResults","data":{"content":[{"kind":"toolResult","data":{"content":[{"kind":"text","data":"read some files"}]}}]}}
{"version":"v1","kind":"Unknown","data":{"content":[{"kind":"text","data":"ignore me"}]}}
"#,
        )
        .unwrap();
    }

    #[test]
    fn kiro_discover_sessions_sorted_newest_first() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cli");
        seed_kiro_fixture(&root);
        let src = KiroCliSource::new(root);
        let sessions = src.discover_sessions().unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].id, "aaa");
        assert_eq!(sessions[0].title, "newer chat");
        assert!(sessions[0].created_ms > sessions[1].created_ms);
    }

    #[test]
    fn kiro_load_messages_maps_kinds_to_roles() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cli");
        seed_kiro_fixture(&root);
        let src = KiroCliSource::new(root);
        let msgs = src.load_messages("aaa").unwrap();
        assert_eq!(
            msgs.len(),
            3,
            "want prompt/assistant/toolresult only: {msgs:?}"
        );
        assert_eq!(msgs[0].role, MessageRole::User);
        assert_eq!(msgs[0].text, "hello");
        assert_eq!(msgs[0].created_ms, 1_000_000);
        assert_eq!(msgs[1].role, MessageRole::Assistant);
        assert_eq!(msgs[1].text, "greetings");
        assert_eq!(msgs[2].role, MessageRole::Tool);
        assert_eq!(msgs[2].text, "read some files");
    }

    #[test]
    fn kiro_missing_transcript_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cli");
        seed_kiro_fixture(&root);
        let src = KiroCliSource::new(root);
        let err = src.load_messages("nope").unwrap_err();
        assert!(
            err.to_string().contains("no transcript"),
            "want missing hint: {err}"
        );
    }

    #[test]
    fn empty_or_missing_parts_dir_drops_the_message() {
        let tmp = tempfile::tempdir().unwrap();
        seed_opencode_fixture(tmp.path());
        let extra_msg_dir = tmp.path().join("message/ses_abc");
        std::fs::write(
            extra_msg_dir.join("msg_3.json"),
            r#"{"id":"msg_3","sessionID":"ses_abc","role":"user","time":{"created":1003}}"#,
        )
        .unwrap();
        let src = OpencodeSource::new(tmp.path().to_path_buf());
        let messages = src.load_messages("ses_abc").unwrap();
        assert_eq!(messages.len(), 2, "no-part message should be dropped");
    }
}
