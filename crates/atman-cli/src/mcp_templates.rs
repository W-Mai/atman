//! Curated MCP server templates for `atman mcp add --template <name>`.

pub struct McpTemplate {
    pub name: &'static str,
    pub description: &'static str,
    pub command: &'static str,
    pub args: &'static [&'static str],
    pub env_keys: &'static [&'static str],
    pub path_placeholder: Option<&'static str>,
}

pub static TEMPLATES: &[McpTemplate] = &[
    McpTemplate {
        name: "filesystem",
        description: "Filesystem access (read/write/list)",
        command: "npx",
        args: &["-y", "@modelcontextprotocol/server-filesystem"],
        env_keys: &[],
        path_placeholder: Some("/path/to/dir"),
    },
    McpTemplate {
        name: "github",
        description: "GitHub API (repos, issues, PRs)",
        command: "npx",
        args: &["-y", "@modelcontextprotocol/server-github"],
        env_keys: &["GITHUB_PERSONAL_ACCESS_TOKEN"],
        path_placeholder: None,
    },
    McpTemplate {
        name: "gitlab",
        description: "GitLab API (repos, issues, MRs)",
        command: "npx",
        args: &["-y", "@modelcontextprotocol/server-gitlab"],
        env_keys: &["GITLAB_TOKEN"],
        path_placeholder: None,
    },
    McpTemplate {
        name: "sqlite",
        description: "SQLite database query",
        command: "npx",
        args: &["-y", "@modelcontextprotocol/server-sqlite"],
        env_keys: &[],
        path_placeholder: Some("/path/to/db.sqlite"),
    },
    McpTemplate {
        name: "fetch",
        description: "Web fetch (HTTP requests)",
        command: "npx",
        args: &["-y", "@modelcontextprotocol/server-fetch"],
        env_keys: &[],
        path_placeholder: None,
    },
    McpTemplate {
        name: "memory",
        description: "Persistent memory (knowledge graph)",
        command: "npx",
        args: &["-y", "@modelcontextprotocol/server-memory"],
        env_keys: &[],
        path_placeholder: None,
    },
    McpTemplate {
        name: "puppeteer",
        description: "Browser automation (Puppeteer)",
        command: "npx",
        args: &["-y", "@modelcontextprotocol/server-puppeteer"],
        env_keys: &[],
        path_placeholder: None,
    },
    McpTemplate {
        name: "brave-search",
        description: "Brave Search API",
        command: "npx",
        args: &["-y", "@modelcontextprotocol/server-brave-search"],
        env_keys: &["BRAVE_API_KEY"],
        path_placeholder: None,
    },
    McpTemplate {
        name: "google-maps",
        description: "Google Maps API",
        command: "npx",
        args: &["-y", "@modelcontextprotocol/server-google-maps"],
        env_keys: &["GOOGLE_MAPS_API_KEY"],
        path_placeholder: None,
    },
    McpTemplate {
        name: "sequential-thinking",
        description: "Sequential thinking / reasoning",
        command: "npx",
        args: &["-y", "@modelcontextprotocol/server-sequential-thinking"],
        env_keys: &[],
        path_placeholder: None,
    },
    McpTemplate {
        name: "time",
        description: "Time and timezone",
        command: "npx",
        args: &["-y", "@modelcontextprotocol/server-time"],
        env_keys: &[],
        path_placeholder: None,
    },
    McpTemplate {
        name: "everart",
        description: "EverArt image generation",
        command: "npx",
        args: &["-y", "@modelcontextprotocol/server-everart"],
        env_keys: &["EVERART_API_KEY"],
        path_placeholder: None,
    },
];

pub fn find(name: &str) -> Option<&'static McpTemplate> {
    TEMPLATES.iter().find(|t| t.name == name)
}
