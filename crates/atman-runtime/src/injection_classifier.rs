use std::sync::Arc;
use std::time::Duration;

use regex::Regex;

use crate::injection::InjectionLevel;
use crate::tool::BoxFut;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassifierSource {
    Prefix,
    Rule,
    Llm,
    Default,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classification {
    pub level: InjectionLevel,
    pub redirect_target: Option<String>,
    pub source: ClassifierSource,
}

pub trait InjectionClassifier: Send + Sync {
    fn classify<'a>(&'a self, text: &'a str) -> BoxFut<'a, Classification>;
    fn kind(&self) -> &'static str;
}

pub struct RuleClassifier {
    l4: Regex,
    l3: Regex,
    l2: Regex,
}

impl Default for RuleClassifier {
    fn default() -> Self {
        Self {
            l4: Regex::new(r"^(?i)\s*(停|停下|停止|stop|abort|halt|kill|终止)[\s!.。！]*$")
                .unwrap(),
            l3: Regex::new(r"(?i)^\s*(换成|切换到|redirect to|switch to)\s+(\S+)").unwrap(),
            l2: Regex::new(r"(别用|不要|不能|改成|错了|应该|请用|do not|don't use|stop using)")
                .unwrap(),
        }
    }
}

impl RuleClassifier {
    fn classify_sync(&self, text: &str) -> Classification {
        let trimmed = text.trim();
        if self.l4.is_match(trimmed) {
            return Classification {
                level: InjectionLevel::L4HardStop,
                redirect_target: None,
                source: ClassifierSource::Rule,
            };
        }
        if let Some(caps) = self.l3.captures(trimmed) {
            let target = caps.get(2).map(|m| m.as_str().to_string());
            return Classification {
                level: InjectionLevel::L3Redirect,
                redirect_target: target,
                source: ClassifierSource::Rule,
            };
        }
        if self.l2.is_match(trimmed) {
            return Classification {
                level: InjectionLevel::L2CourseCorrect,
                redirect_target: None,
                source: ClassifierSource::Rule,
            };
        }
        Classification {
            level: InjectionLevel::L1Nudge,
            redirect_target: None,
            source: ClassifierSource::Default,
        }
    }
}

impl InjectionClassifier for RuleClassifier {
    fn classify<'a>(&'a self, text: &'a str) -> BoxFut<'a, Classification> {
        let out = self.classify_sync(text);
        Box::pin(async move { out })
    }
    fn kind(&self) -> &'static str {
        "rule"
    }
}

pub struct LlmClassifier {
    provider: Arc<dyn crate::provider::Provider>,
    model: String,
}

impl LlmClassifier {
    pub fn new(provider: Arc<dyn crate::provider::Provider>, model: impl Into<String>) -> Self {
        Self {
            provider,
            model: model.into(),
        }
    }
}

impl InjectionClassifier for LlmClassifier {
    fn classify<'a>(&'a self, text: &'a str) -> BoxFut<'a, Classification> {
        Box::pin(async move {
            let prompt = format!(
                "You are classifying a user interruption sent to a running AI agent. Reply STRICTLY as one line of JSON, no prose.\n\n\
                 Levels:\n\
                 - L1: minor nudge (add info, remind, small hint)\n\
                 - L2: course-correct (wrong approach mid-stream, forbidden pattern)\n\
                 - L3: redirect (switch flows entirely, name a target if user gave one)\n\
                 - L4: hard stop (kill flow immediately)\n\n\
                 Reply shape: {{\"level\": \"L1\"|\"L2\"|\"L3\"|\"L4\", \"redirect_target\": null | \"<flow_name>\"}}\n\n\
                 User interruption: {text}"
            );
            let req = crate::provider::LlmRequest {
                model: self.model.clone(),
                messages: vec![crate::provider::user_text_message(prompt)],
                system: None,
                input: crate::value::Value::Unit,
                schema: None,
                cache_prompt: false,
                tools: Vec::new(),
            };
            let am = match self.provider.call(req).await {
                Ok(am) => am,
                Err(_) => return default_l1(ClassifierSource::Default),
            };
            let body = am.message.text_concat();
            let Some(parsed) =
                extract_json(&body).and_then(|s| serde_json::from_str::<ClassifyJson>(&s).ok())
            else {
                return default_l1(ClassifierSource::Default);
            };
            Classification {
                level: parse_level(&parsed.level),
                redirect_target: parsed.redirect_target,
                source: ClassifierSource::Llm,
            }
        })
    }
    fn kind(&self) -> &'static str {
        "llm"
    }
}

#[derive(serde::Deserialize)]
struct ClassifyJson {
    level: String,
    #[serde(default)]
    redirect_target: Option<String>,
}

fn parse_level(s: &str) -> InjectionLevel {
    match s.trim().to_ascii_lowercase().as_str() {
        "l4" => InjectionLevel::L4HardStop,
        "l3" => InjectionLevel::L3Redirect,
        "l2" => InjectionLevel::L2CourseCorrect,
        _ => InjectionLevel::L1Nudge,
    }
}

fn extract_json(body: &str) -> Option<String> {
    let start = body.find('{')?;
    let end = body.rfind('}')?;
    if end < start {
        return None;
    }
    Some(body[start..=end].to_string())
}

fn default_l1(source: ClassifierSource) -> Classification {
    Classification {
        level: InjectionLevel::L1Nudge,
        redirect_target: None,
        source,
    }
}

pub struct ComposedClassifier {
    rule: RuleClassifier,
    llm: Option<Arc<dyn InjectionClassifier>>,
    llm_timeout: Duration,
}

impl ComposedClassifier {
    pub fn new(rule: RuleClassifier) -> Self {
        Self {
            rule,
            llm: None,
            llm_timeout: Duration::from_secs(3),
        }
    }

    pub fn with_llm(mut self, llm: Arc<dyn InjectionClassifier>, timeout: Duration) -> Self {
        self.llm = Some(llm);
        self.llm_timeout = timeout;
        self
    }
}

impl InjectionClassifier for ComposedClassifier {
    fn classify<'a>(&'a self, text: &'a str) -> BoxFut<'a, Classification> {
        Box::pin(async move {
            let rule_hit = self.rule.classify_sync(text);
            if rule_hit.level != InjectionLevel::L1Nudge {
                return rule_hit;
            }
            let Some(llm) = self.llm.as_ref() else {
                return rule_hit;
            };
            match tokio::time::timeout(self.llm_timeout, llm.classify(text)).await {
                Ok(cls) => cls,
                Err(_) => rule_hit,
            }
        })
    }
    fn kind(&self) -> &'static str {
        "composed"
    }
}

pub fn source_tag(source: ClassifierSource) -> &'static str {
    match source {
        ClassifierSource::Prefix => "prefix",
        ClassifierSource::Rule => "rule",
        ClassifierSource::Llm => "llm",
        ClassifierSource::Default => "default",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rule_matches_l4_stop_variants() {
        let c = RuleClassifier::default();
        for text in ["停", "停下!", "stop", "STOP", "abort.", "kill"] {
            let out = c.classify(text).await;
            assert_eq!(out.level, InjectionLevel::L4HardStop, "text: {text}");
            assert_eq!(out.source, ClassifierSource::Rule);
        }
    }

    #[tokio::test]
    async fn rule_matches_l3_redirect_and_extracts_target() {
        let c = RuleClassifier::default();
        let out = c.classify("换成 review_code").await;
        assert_eq!(out.level, InjectionLevel::L3Redirect);
        assert_eq!(out.redirect_target.as_deref(), Some("review_code"));
        let out = c.classify("switch to hello").await;
        assert_eq!(out.level, InjectionLevel::L3Redirect);
        assert_eq!(out.redirect_target.as_deref(), Some("hello"));
    }

    #[tokio::test]
    async fn rule_matches_l2_course_correct_hints() {
        let c = RuleClassifier::default();
        for text in [
            "别用 as any",
            "不要写 unsafe",
            "改成 async",
            "错了 应该用 tokio",
            "do not use blocking",
        ] {
            let out = c.classify(text).await;
            assert_eq!(out.level, InjectionLevel::L2CourseCorrect, "text: {text}");
        }
    }

    #[tokio::test]
    async fn rule_falls_back_to_l1_nudge_default() {
        let c = RuleClassifier::default();
        let out = c.classify("记得加一句测试").await;
        assert_eq!(out.level, InjectionLevel::L1Nudge);
        assert_eq!(out.source, ClassifierSource::Default);
    }

    #[tokio::test]
    async fn composed_returns_rule_hit_when_matched() {
        let c = ComposedClassifier::new(RuleClassifier::default());
        let out = c.classify("停").await;
        assert_eq!(out.level, InjectionLevel::L4HardStop);
    }
}
