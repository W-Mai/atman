use std::collections::HashMap;

#[derive(Debug, Default, Clone)]
pub struct ToolNaming {
    per_provider: HashMap<String, HashMap<String, String>>,
    reverse: HashMap<String, HashMap<String, String>>,
}

impl ToolNaming {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn map(
        &mut self,
        provider: impl Into<String>,
        flow_name: impl Into<String>,
        provider_native: impl Into<String>,
    ) {
        let provider = provider.into();
        let flow_name = flow_name.into();
        let native = provider_native.into();
        self.per_provider
            .entry(provider.clone())
            .or_default()
            .insert(flow_name.clone(), native.clone());
        self.reverse
            .entry(provider)
            .or_default()
            .insert(native, flow_name);
    }

    pub fn to_provider<'a>(&'a self, provider: &str, flow_name: &'a str) -> &'a str {
        self.per_provider
            .get(provider)
            .and_then(|m| m.get(flow_name))
            .map(|s| s.as_str())
            .unwrap_or(flow_name)
    }

    pub fn from_provider<'a>(&'a self, provider: &str, native_name: &'a str) -> &'a str {
        self.reverse
            .get(provider)
            .and_then(|m| m.get(native_name))
            .map(|s| s.as_str())
            .unwrap_or(native_name)
    }

    pub fn known_flow_names(&self, provider: &str) -> impl Iterator<Item = &str> {
        self.per_provider
            .get(provider)
            .into_iter()
            .flat_map(|m| m.keys().map(|s| s.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_provider_returns_mapped_name() {
        let mut n = ToolNaming::new();
        n.map("anthropic", "fs.read", "str_replace_based_edit_tool");
        assert_eq!(
            n.to_provider("anthropic", "fs.read"),
            "str_replace_based_edit_tool"
        );
    }

    #[test]
    fn to_provider_falls_back_to_flow_name_when_unmapped() {
        let n = ToolNaming::new();
        assert_eq!(n.to_provider("anthropic", "fs.read"), "fs.read");
    }

    #[test]
    fn from_provider_is_reverse_of_to_provider() {
        let mut n = ToolNaming::new();
        n.map("openai", "bash.exec", "run_bash");
        assert_eq!(n.to_provider("openai", "bash.exec"), "run_bash");
        assert_eq!(n.from_provider("openai", "run_bash"), "bash.exec");
    }

    #[test]
    fn from_provider_falls_back_to_native_name_when_unmapped() {
        let n = ToolNaming::new();
        assert_eq!(n.from_provider("openai", "unknown_tool"), "unknown_tool");
    }

    #[test]
    fn maps_are_per_provider() {
        let mut n = ToolNaming::new();
        n.map("anthropic", "fs.read", "str_replace_based_edit_tool");
        n.map("openai", "fs.read", "read_file");
        assert_eq!(
            n.to_provider("anthropic", "fs.read"),
            "str_replace_based_edit_tool"
        );
        assert_eq!(n.to_provider("openai", "fs.read"), "read_file");
    }

    #[test]
    fn known_flow_names_lists_flow_side_only() {
        let mut n = ToolNaming::new();
        n.map("anthropic", "fs.read", "str_replace_based_edit_tool");
        n.map("anthropic", "bash.exec", "bash");
        let mut names: Vec<_> = n.known_flow_names("anthropic").collect();
        names.sort();
        assert_eq!(names, vec!["bash.exec", "fs.read"]);
    }
}
