use thiserror::Error;

#[derive(Debug, Clone, Error)]
pub enum RuntimeError {
    #[error("undefined variable: {0}")]
    UndefinedVar(String),

    #[error("undefined tool: {0}")]
    UndefinedTool(String),

    #[error("type mismatch: expected {expected}, got {actual}")]
    TypeMismatch { expected: String, actual: String },

    #[error("missing argument: {0}")]
    MissingArg(String),

    #[error("tool failed: {0}")]
    ToolFailed(String),

    #[error("cancelled: {0}")]
    Cancelled(String),

    #[error("aborted: {0}")]
    Aborted(String),

    #[error("redirect to flow `{0}`")]
    Redirect(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    Transient,
    Timeout,
    RateLimit,
    AuthFailed,
    ContentFilter,
    InvalidRequest,
    ProviderDown,
    ToolError,
    TypeMismatch,
    MissingArg,
    Cancelled,
    UserError,
    Internal,
}

impl ErrorKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ErrorKind::Transient => "transient",
            ErrorKind::Timeout => "timeout",
            ErrorKind::RateLimit => "rate_limit",
            ErrorKind::AuthFailed => "auth_failed",
            ErrorKind::ContentFilter => "content_filter",
            ErrorKind::InvalidRequest => "invalid_request",
            ErrorKind::ProviderDown => "provider_down",
            ErrorKind::ToolError => "tool_error",
            ErrorKind::TypeMismatch => "type_mismatch",
            ErrorKind::MissingArg => "missing_arg",
            ErrorKind::Cancelled => "cancelled",
            ErrorKind::UserError => "user_error",
            ErrorKind::Internal => "internal",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "transient" => ErrorKind::Transient,
            "timeout" => ErrorKind::Timeout,
            "rate_limit" => ErrorKind::RateLimit,
            "auth_failed" => ErrorKind::AuthFailed,
            "content_filter" => ErrorKind::ContentFilter,
            "invalid_request" => ErrorKind::InvalidRequest,
            "provider_down" => ErrorKind::ProviderDown,
            "tool_error" => ErrorKind::ToolError,
            "type_mismatch" => ErrorKind::TypeMismatch,
            "missing_arg" => ErrorKind::MissingArg,
            "cancelled" => ErrorKind::Cancelled,
            "user_error" => ErrorKind::UserError,
            "internal" => ErrorKind::Internal,
            _ => return None,
        })
    }
}

impl RuntimeError {
    pub fn kind(&self) -> ErrorKind {
        match self {
            RuntimeError::UndefinedVar(_) => ErrorKind::InvalidRequest,
            RuntimeError::UndefinedTool(_) => ErrorKind::InvalidRequest,
            RuntimeError::TypeMismatch { .. } => ErrorKind::TypeMismatch,
            RuntimeError::MissingArg(_) => ErrorKind::MissingArg,
            RuntimeError::Cancelled(_) => ErrorKind::Cancelled,
            RuntimeError::Aborted(_) => ErrorKind::UserError,
            RuntimeError::Redirect(_) => ErrorKind::Cancelled,
            RuntimeError::ToolFailed(msg) => classify_tool_failed(msg),
        }
    }
}

fn classify_tool_failed(msg: &str) -> ErrorKind {
    let m = msg.to_ascii_lowercase();
    if m.contains("timeout") || m.contains("timed out") {
        return ErrorKind::Timeout;
    }
    if m.contains("429") || m.contains("rate limit") || m.contains("rate-limit") {
        return ErrorKind::RateLimit;
    }
    if m.contains(" 401")
        || m.contains(" 403")
        || m.contains("unauthorized")
        || m.contains("forbidden")
    {
        return ErrorKind::AuthFailed;
    }
    if m.contains("content_filter")
        || m.contains("content filter")
        || m.contains("safety")
        || m.contains("policy violation")
        || m.contains("policy_violation")
    {
        return ErrorKind::ContentFilter;
    }
    if m.contains(" 500")
        || m.contains(" 502")
        || m.contains(" 503")
        || m.contains(" 504")
        || m.contains("upstream")
        || m.contains("bad gateway")
        || m.contains("service unavailable")
    {
        return ErrorKind::ProviderDown;
    }
    if m.contains("network")
        || m.contains("connection")
        || m.contains("connect ")
        || m.contains("reset by peer")
    {
        return ErrorKind::Transient;
    }
    if m.contains(" 400") || m.contains("bad request") || m.contains("invalid request") {
        return ErrorKind::InvalidRequest;
    }
    ErrorKind::ToolError
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_covers_common_provider_error_strings() {
        let cases: &[(&str, ErrorKind)] = &[
            ("openai net: request timed out", ErrorKind::Timeout),
            ("anthropic: 429 rate limit exceeded", ErrorKind::RateLimit),
            ("openai http 401: unauthorized", ErrorKind::AuthFailed),
            (
                "anthropic http 400: content_filter block",
                ErrorKind::ContentFilter,
            ),
            ("openai http 502 Bad Gateway", ErrorKind::ProviderDown),
            ("hyper: connection reset by peer", ErrorKind::Transient),
            (
                "openai http 400: bad request schema",
                ErrorKind::InvalidRequest,
            ),
            ("fs.read: no such file", ErrorKind::ToolError),
        ];
        for (msg, expected) in cases {
            let err = RuntimeError::ToolFailed(msg.to_string());
            assert_eq!(err.kind(), *expected, "input: {msg}");
        }
    }

    #[test]
    fn structural_variants_map_to_semantic_kinds() {
        assert_eq!(
            RuntimeError::TypeMismatch {
                expected: "int".into(),
                actual: "string".into()
            }
            .kind(),
            ErrorKind::TypeMismatch
        );
        assert_eq!(
            RuntimeError::MissingArg("model".into()).kind(),
            ErrorKind::MissingArg
        );
        assert_eq!(
            RuntimeError::Cancelled("user hit ctrl-c".into()).kind(),
            ErrorKind::Cancelled
        );
        assert_eq!(
            RuntimeError::Aborted("watch tripped".into()).kind(),
            ErrorKind::UserError
        );
        assert_eq!(
            RuntimeError::UndefinedTool("fs.nope".into()).kind(),
            ErrorKind::InvalidRequest
        );
    }

    #[test]
    fn error_kind_round_trips_through_names() {
        for k in [
            ErrorKind::Transient,
            ErrorKind::Timeout,
            ErrorKind::RateLimit,
            ErrorKind::AuthFailed,
            ErrorKind::ContentFilter,
            ErrorKind::InvalidRequest,
            ErrorKind::ProviderDown,
            ErrorKind::ToolError,
            ErrorKind::TypeMismatch,
            ErrorKind::MissingArg,
            ErrorKind::Cancelled,
            ErrorKind::UserError,
            ErrorKind::Internal,
        ] {
            assert_eq!(ErrorKind::from_name(k.as_str()), Some(k));
        }
        assert_eq!(ErrorKind::from_name("nope"), None);
    }
}
