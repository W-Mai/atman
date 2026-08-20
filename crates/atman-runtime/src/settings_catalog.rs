use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingSource {
    Default,
    Global,
    Project,
    Session,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activation {
    Immediate,
    NextSession,
    RestartRequired,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingDescriptor {
    pub key: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub source: SettingSource,
    pub activation: Activation,
    pub sensitive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingValue {
    pub descriptor: SettingDescriptor,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingMutationError {
    EmptyValue,
    UnsupportedKey(String),
}

impl fmt::Display for SettingMutationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyValue => write!(f, "setting value cannot be empty"),
            Self::UnsupportedKey(key) => write!(f, "unsupported setting: {key}"),
        }
    }
}

impl std::error::Error for SettingMutationError {}

pub fn catalog() -> Vec<SettingDescriptor> {
    vec![
        SettingDescriptor {
            key: "session.context_budget",
            title: "Context budget",
            description: "Maximum provider context window budget.",
            source: SettingSource::Global,
            activation: Activation::NextSession,
            sensitive: false,
        },
        SettingDescriptor {
            key: "tools.output.max_bytes",
            title: "Tool output bytes",
            description: "Maximum inline tool output before continuation is offered.",
            source: SettingSource::Global,
            activation: Activation::Immediate,
            sensitive: false,
        },
        SettingDescriptor {
            key: "trust.mode",
            title: "Trust mode",
            description: "Controls approval behavior for tool execution.",
            source: SettingSource::Project,
            activation: Activation::NextSession,
            sensitive: false,
        },
        SettingDescriptor {
            key: "provider.api_key",
            title: "Provider API key",
            description: "Credential used by the selected provider.",
            source: SettingSource::Global,
            activation: Activation::Immediate,
            sensitive: true,
        },
    ]
}

pub fn descriptor(key: &str) -> Option<SettingDescriptor> {
    catalog().into_iter().find(|item| item.key == key)
}

pub fn validate_mutation(key: &str, value: &str) -> Result<(), SettingMutationError> {
    if descriptor(key).is_none() {
        return Err(SettingMutationError::UnsupportedKey(key.to_owned()));
    }
    if value.trim().is_empty() {
        return Err(SettingMutationError::EmptyValue);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_exposes_source_activation_and_sensitive_metadata() {
        let item = descriptor("provider.api_key").unwrap();
        assert_eq!(item.source, SettingSource::Global);
        assert_eq!(item.activation, Activation::Immediate);
        assert!(item.sensitive);
    }

    #[test]
    fn mutation_validation_rejects_unknown_and_empty_values() {
        assert_eq!(
            validate_mutation("unknown", "x"),
            Err(SettingMutationError::UnsupportedKey("unknown".into()))
        );
        assert_eq!(
            validate_mutation("trust.mode", " "),
            Err(SettingMutationError::EmptyValue)
        );
    }
}
