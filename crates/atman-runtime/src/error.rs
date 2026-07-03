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
