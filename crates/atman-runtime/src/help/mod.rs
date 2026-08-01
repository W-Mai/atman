//! Built-in help system — docstring-driven, version-synced.
//!
//! Dynamic topics (`tools`, `cli`, `config`, `mcp`) are generated at runtime
//! from `ToolRegistry`, `META_COMMANDS`, and `DocumentedFields::FIELD_DOCS`,
//! so they can never drift from the code. Static topics (`dsl`, `features`)
//! are embedded markdown, test-guarded for key terms.

use crate::mcp::{McpServerConfig, TransportKind};
use crate::meta_commands::META_COMMANDS;
use crate::safety::SafetyConfig;
use crate::storage::StorageConfig;
use crate::tool::{ToolCtx, ToolRegistry};
use crate::trust::TrustConfig;
use documented::{DocumentedFields, DocumentedVariants};

pub struct HelpTopic {
    pub id: &'static str,
    pub title: &'static str,
}

pub const TOPICS: &[HelpTopic] = &[
    HelpTopic {
        id: "index",
        title: "Topic Index",
    },
    HelpTopic {
        id: "tools",
        title: "Built-in Tools",
    },
    HelpTopic {
        id: "cli",
        title: "CLI Meta Commands",
    },
    HelpTopic {
        id: "config",
        title: "Configuration Reference",
    },
    HelpTopic {
        id: "mcp",
        title: "MCP Setup",
    },
    HelpTopic {
        id: "dsl",
        title: "Atman DSL Syntax",
    },
    HelpTopic {
        id: "features",
        title: "Feature Overview",
    },
];

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Render help content for a topic. Returns `None` if the topic is unknown.
pub fn topic_content(topic: &str, ctx: &ToolCtx) -> Option<String> {
    match topic {
        "index" => Some(render_index()),
        "tools" => ctx
            .registry
            .as_ref()
            .map(|r| render_tools(r))
            .or_else(|| Some(render_tools_offline())),
        "cli" => Some(render_cli()),
        "config" => Some(render_config()),
        "mcp" => Some(render_mcp()),
        "dsl" => Some(include_str!("content/dsl.md").to_string()),
        "features" => Some(include_str!("content/features.md").to_string()),
        _ => None,
    }
}

fn render_index() -> String {
    let mut out = String::new();
    out.push_str(&format!("# Atman {} — Help Index\n\n", version()));
    out.push_str("Call `help.show(topic: \"...\")` with one of:\n\n");
    for t in TOPICS {
        out.push_str(&format!("- **{}** — {}\n", t.id, t.title));
    }
    out
}

fn render_tools(registry: &ToolRegistry) -> String {
    let mut tools: Vec<_> = registry.iter();
    tools.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = String::from("# Built-in Tools\n\n");
    out.push_str("| Tier | Name | Description |\n|------|------|-------------|\n");
    for (name, tool) in &tools {
        let tier = format!("{:?}", tool.tier());
        let desc = tool
            .description()
            .unwrap_or("")
            .lines()
            .next()
            .unwrap_or("");
        out.push_str(&format!("| {} | `{}` | {} |\n", tier, name, desc));
    }
    out.push_str(&format!("\n_{} tools registered._\n", tools.len()));
    out
}

fn render_tools_offline() -> String {
    String::from("# Built-in Tools\n\nTool registry not available in this context.\n")
}

fn render_cli() -> String {
    let mut out = String::from("# CLI Meta Commands (REPL `:`commands)\n\n");
    out.push_str("| Command | Aliases | Description |\n|---------|---------|-------------|\n");
    for cmd in META_COMMANDS {
        let aliases = if cmd.aliases.is_empty() {
            "—".to_string()
        } else {
            cmd.aliases
                .iter()
                .map(|a| format!("`:{a}`"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        out.push_str(&format!(
            "| `:{}` | {} | {} |\n",
            cmd.name, aliases, cmd.desc
        ));
    }
    out.push_str("\n```\n");
    for cmd in META_COMMANDS {
        out.push_str(&format!("{}\n", cmd.usage));
    }
    out.push_str("```\n");
    out
}

fn render_config() -> String {
    let mut out = String::from("# Configuration Reference\n\n");
    out.push_str("Config file: `~/.config/atman/config.toml`\n\n");

    out.push_str("## [safety]\n\n");
    out.push_str("| Field | Type | Description |\n|-------|------|-------------|\n");
    for (field, ty) in [
        ("enabled", "bool"),
        ("mode", "SafetyMode"),
        ("auto_rewrite", "bool"),
    ] {
        let doc = SafetyConfig::get_field_docs(field).unwrap_or("");
        out.push_str(&format!("| `{}` | {} | {} |\n", field, ty, doc));
    }

    out.push_str("\n## [storage]\n\n");
    out.push_str("| Field | Type | Description |\n|-------|------|-------------|\n");
    let doc = StorageConfig::get_field_docs("scope").unwrap_or("");
    out.push_str(&format!("| `scope` | Option<StorageScope> | {} |\n", doc));

    out.push_str("\n## [trust]\n\n");
    out.push_str("| Field | Type | Description |\n|-------|------|-------------|\n");
    for (field, ty) in [
        ("mode", "TrustMode"),
        ("theme", "Theme"),
        ("outside", "OutsideBehavior"),
    ] {
        let doc = TrustConfig::get_field_docs(field).unwrap_or("");
        out.push_str(&format!("| `{}` | {} | {} |\n", field, ty, doc));
    }

    out.push_str("\n## [compaction]\n\n");
    out.push_str(
        "| Field | Type | Default | Description |\n|-------|------|---------|-------------|\n",
    );
    out.push_str("| `review` | string | `\"manual-only\"` | When to show compaction review modal: `always` / `manual-only` / `never` |\n");

    out.push_str("\n## [registry]\n\n");
    out.push_str(
        "| Field | Type | Default | Description |\n|-------|------|---------|-------------|\n",
    );
    out.push_str("| `auto_snapshot` | bool | `false` | Auto flow snapshot on every run |\n");

    out.push_str("\n## [suggest]\n\n");
    out.push_str(
        "| Field | Type | Default | Description |\n|-------|------|---------|-------------|\n",
    );
    out.push_str("| `model` | string | `\"gpt-4o-mini\"` | Model for flow suggestions |\n");

    out.push_str("\n## [interjection]\n\n");
    out.push_str(
        "| Field | Type | Default | Description |\n|-------|------|---------|-------------|\n",
    );
    out.push_str("| `classifier` | string | — | Model for interjection classification |\n");

    out
}

fn render_mcp() -> String {
    let mut out = String::from("# MCP Setup\n\n");
    out.push_str("Config file: `~/.config/atman/mcp_servers.json`\n\n");
    out.push_str("## McpServerConfig Fields\n\n");
    out.push_str("| Field | Type | Description |\n|-------|------|-------------|\n");
    let fields = [
        ("name", "String"),
        ("transport", "TransportKind"),
        ("command", "String"),
        ("args", "Vec<String>"),
        ("env", "Vec<(String, String)>"),
        ("url", "Option<String>"),
        ("auth_token", "Option<String>"),
        ("headers", "Vec<(String, String)>"),
        ("tier", "Tier"),
        ("timeout_ms", "u64"),
        ("disabled", "bool"),
    ];
    for (field, ty) in &fields {
        let doc = McpServerConfig::get_field_docs(field).unwrap_or("");
        out.push_str(&format!("| `{}` | {} | {} |\n", field, ty, doc));
    }

    out.push_str("\n## TransportKind\n\n");
    out.push_str("| Variant | Description |\n|---------|-------------|\n");
    for v in [
        TransportKind::Stdio,
        TransportKind::Http,
        TransportKind::Sse,
    ] {
        let doc = v.get_variant_docs();
        out.push_str(&format!("| `{:?}` | {} |\n", v, doc));
    }

    out.push_str("\n## Tier\n\n");
    out.push_str(
        "| Tier | Approval Level | Description |\n|------|---------------|-------------|\n",
    );
    for (tier, level, desc) in [
        ("Zero", "Auto", "Read-only, safe — no approval needed"),
        ("One", "Approve", "Requires user approval"),
        ("Two", "Approve", "File writes and mutations"),
        ("Three", "Dangerous", "Network operations, push"),
        ("Four", "Dangerous", "Shell, terminal — most dangerous"),
    ] {
        out.push_str(&format!("| `{}` | {} | {} |\n", tier, level, desc));
    }

    out.push_str("\n## Example\n\n");
    out.push_str("```json\n");
    out.push_str("{\n  \"mcpServers\": {\n    \"filesystem\": {\n");
    out.push_str("      \"command\": \"npx\",\n      \"args\": [\"-y\", \"@modelcontextprotocol/server-filesystem\", \"/tmp\"],\n");
    out.push_str("      \"tier\": \"Two\",\n      \"timeout_ms\": 30000\n    }\n  }\n}\n");
    out.push_str("```\n");

    out.push_str("\nIn a flow, include MCP tools with `\"mcp.*\"` in the `tools` list:\n\n");
    out.push_str("```\nllm {\n    tools: [fs.read, \"mcp.*\"]\n}\n```\n");
    out
}
