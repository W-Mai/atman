//! Portable descriptions of host tool signatures.

use alloc::{boxed::Box, string::String, vec::Vec};
use core::fmt;

use crate::ToolCallMode;

/// A value type exposed by a host binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeSpec {
    Any,
    Unit,
    Bool,
    Int { min: i128, max: i128 },
    Float { bits: u8 },
    String,
    List(Box<TypeSpec>),
    Option(Box<TypeSpec>),
    Struct(StructSpec),
    Enum(EnumSpec),
    Resource(ResourceSpec),
}

impl fmt::Display for TypeSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Any => formatter.write_str("any"),
            Self::Unit => formatter.write_str("unit"),
            Self::Bool => formatter.write_str("bool"),
            Self::Int { .. } => formatter.write_str("int"),
            Self::Float { .. } => formatter.write_str("float"),
            Self::String => formatter.write_str("string"),
            Self::List(item) => write!(formatter, "[{item}]"),
            Self::Option(item) => write!(formatter, "option<{item}>"),
            Self::Struct(spec) => formatter.write_str(&spec.name),
            Self::Enum(spec) => formatter.write_str(&spec.name),
            Self::Resource(spec) => formatter.write_str(spec.name.as_deref().unwrap_or("resource")),
        }
    }
}

/// One named field in a structured value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldSpec {
    pub name: String,
    pub ty: TypeSpec,
}

/// A named structured value and its fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructSpec {
    pub name: String,
    pub fields: Vec<FieldSpec>,
}

/// A named fieldless enum and its variants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumSpec {
    pub name: String,
    pub variants: Vec<String>,
}

/// An opaque nominal host resource.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceSpec {
    /// A nominal Rust resource name. `None` accepts any resource handle.
    pub name: Option<String>,
}

/// One parameter accepted by a host tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolParamSpec {
    pub name: String,
    pub position: usize,
    pub required: bool,
    pub ty: TypeSpec,
}

/// The portable signature and documentation for a host tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSpec {
    pub name: String,
    pub namespace: String,
    pub description: String,
    pub mode: ToolCallMode,
    pub params: Vec<ToolParamSpec>,
    pub result: TypeSpec,
}

/// One registered router entry and its optional portable signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogEntry {
    pub name: String,
    pub mode: ToolCallMode,
    pub spec: Option<ToolSpec>,
}

/// A stable-order projection of all registered host tools.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolCatalog {
    entries: Vec<CatalogEntry>,
}

impl ToolCatalog {
    pub fn new(entries: Vec<CatalogEntry>) -> Self {
        Self { entries }
    }

    pub fn lookup(&self, name: &str) -> Option<&CatalogEntry> {
        self.entries.iter().find(|entry| entry.name == name)
    }

    pub fn iter(&self) -> core::slice::Iter<'_, CatalogEntry> {
        self.entries.iter()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use alloc::{string::ToString, vec};

    use super::*;

    #[test]
    fn catalog_keeps_entries_without_specs() {
        let catalog = ToolCatalog::new(vec![CatalogEntry {
            name: "manual".to_string(),
            mode: ToolCallMode::Immediate,
            spec: None,
        }]);

        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog.lookup("manual"), catalog.iter().next());
        assert!(catalog.lookup("manual").unwrap().spec.is_none());
    }

    #[test]
    fn iteration_and_lookup_preserve_entry_order() {
        let catalog = ToolCatalog::new(vec![
            CatalogEntry {
                name: "zeta".to_string(),
                mode: ToolCallMode::Deferred,
                spec: None,
            },
            CatalogEntry {
                name: "alpha".to_string(),
                mode: ToolCallMode::Immediate,
                spec: None,
            },
        ]);

        let names: Vec<_> = catalog.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, ["zeta", "alpha"]);
        assert_eq!(catalog.lookup("alpha"), catalog.iter().nth(1));
        assert!(catalog.lookup("missing").is_none());
    }
}
