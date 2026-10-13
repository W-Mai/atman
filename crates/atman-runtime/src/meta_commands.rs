pub struct MetaCommand {
    pub name: &'static str,
    pub desc: &'static str,
    pub usage: &'static str,
    pub aliases: &'static [&'static str],
}

pub const META_COMMANDS: &[MetaCommand] = &[
    MetaCommand {
        name: "panel",
        desc: "open the command palette (Ctrl+P)",
        usage: ":panel",
        aliases: &[],
    },
    MetaCommand {
        name: "providers",
        desc: "manage provider connections",
        usage: ":providers",
        aliases: &[],
    },
    MetaCommand {
        name: "models",
        desc: "manage model configurations",
        usage: ":models",
        aliases: &[],
    },
    MetaCommand {
        name: "aliases",
        desc: "manage model aliases",
        usage: ":aliases",
        aliases: &[],
    },
    MetaCommand {
        name: "mcp",
        desc: "manage MCP servers and tools",
        usage: ":mcp",
        aliases: &[],
    },
    MetaCommand {
        name: "memory",
        desc: "manage memory and rules",
        usage: ":memory",
        aliases: &[],
    },
    MetaCommand {
        name: "history",
        desc: "search session and project history",
        usage: ":history",
        aliases: &[],
    },
    MetaCommand {
        name: "formula",
        desc: "configure formula rendering",
        usage: ":formula",
        aliases: &[],
    },
    MetaCommand {
        name: "keybindings",
        desc: "open the keyboard shortcuts panel",
        usage: ":keybindings",
        aliases: &[],
    },
    MetaCommand {
        name: "new-session",
        desc: "start a new session in this directory",
        usage: ":new-session",
        aliases: &[],
    },
    MetaCommand {
        name: "move-session",
        desc: "choose a new working directory for this session",
        usage: ":move-session",
        aliases: &[],
    },
    MetaCommand {
        name: "delete-session",
        desc: "open the session list to choose a session to delete",
        usage: ":delete-session",
        aliases: &[],
    },
    MetaCommand {
        name: "name-session",
        desc: "generate a session name",
        usage: ":name-session",
        aliases: &[],
    },
    MetaCommand {
        name: "yank",
        desc: "enter transcript selection and copy mode",
        usage: ":yank",
        aliases: &[],
    },
    MetaCommand {
        name: "help",
        desc: "show this help",
        usage: ":help",
        aliases: &[],
    },
    MetaCommand {
        name: "exit",
        desc: "leave repl",
        usage: ":exit | :quit",
        aliases: &["quit"],
    },
    MetaCommand {
        name: "session",
        desc: "print current session id",
        usage: ":session",
        aliases: &[],
    },
    MetaCommand {
        name: "cost",
        desc: "cost summary hint",
        usage: ":cost",
        aliases: &[],
    },
    MetaCommand {
        name: "mode",
        desc: "switch trust mode (calm/steady/eager/reckless)",
        usage: ":mode",
        aliases: &[],
    },
    MetaCommand {
        name: "mode-theme",
        desc: "switch display theme (default/wuxia/animal/weather/drink)",
        usage: ":mode-theme",
        aliases: &[],
    },
    MetaCommand {
        name: "model",
        desc: "switch active model",
        usage: ":model",
        aliases: &[],
    },
    MetaCommand {
        name: "attach",
        desc: "attach file / list / clear",
        usage: ":attach <path> | :attach clear | :attach list",
        aliases: &[],
    },
    MetaCommand {
        name: "suggest",
        desc: "meta-LLM flow suggestion",
        usage: ":suggest",
        aliases: &[],
    },
    MetaCommand {
        name: "goal",
        desc: "manage goals and pause / resume agents",
        usage: ":goal | :goal set <text> | :goal pause | :goal resume | :goal clear",
        aliases: &[],
    },
    MetaCommand {
        name: "sessions",
        desc: "list recent sessions",
        usage: ":sessions",
        aliases: &[],
    },
    MetaCommand {
        name: "projects",
        desc: "open all projects",
        usage: ":projects",
        aliases: &[],
    },
    MetaCommand {
        name: "storage",
        desc: "set current project storage scope",
        usage: ":storage [global|local]",
        aliases: &[],
    },
    MetaCommand {
        name: "sidebar",
        desc: "toggle sidebar or set on / off / auto",
        usage: ":sidebar [on|off|auto]",
        aliases: &[],
    },
    MetaCommand {
        name: "todo",
        desc: "list / done <id> / cancel <id> / clear",
        usage: ":todo list | :todo done <id> | :todo cancel <id> | :todo clear",
        aliases: &[],
    },
    MetaCommand {
        name: "rename",
        desc: "set / clear session title",
        usage: ":rename <text> | :rename clear",
        aliases: &[],
    },
    MetaCommand {
        name: "copy",
        desc: "push to clipboard (OSC 52)",
        usage: ":copy last-message | :copy last-tool",
        aliases: &[],
    },
    MetaCommand {
        name: "compact",
        desc: "compact transcript now",
        usage: ":compact",
        aliases: &[],
    },
];

pub fn builtin_list() -> Vec<(&'static str, &'static str)> {
    META_COMMANDS.iter().map(|c| (c.name, c.desc)).collect()
}

pub fn help_lines() -> Vec<&'static str> {
    let mut lines: Vec<&'static str> = META_COMMANDS.iter().map(|c| c.usage).collect();
    lines.push("");
    lines.push("resume a prior session: exit, then run `atman --continue <session_id>`");
    lines.push("@./path or @/abs     — inline attach in bare input");
    lines
}

pub fn match_command(input: &str) -> Option<&'static MetaCommand> {
    let name = input.split_whitespace().next().unwrap_or(input);
    META_COMMANDS
        .iter()
        .find(|c| c.name == name || c.aliases.contains(&name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_commands_have_nonempty_fields() {
        for c in META_COMMANDS {
            assert!(!c.name.is_empty());
            assert!(!c.desc.is_empty());
            assert!(!c.usage.is_empty());
        }
    }

    #[test]
    fn match_by_name() {
        assert_eq!(match_command("help").unwrap().name, "help");
        assert_eq!(match_command("mode").unwrap().name, "mode");
        assert_eq!(match_command("mode eager").unwrap().name, "mode");
    }

    #[test]
    fn match_by_alias() {
        assert_eq!(match_command("quit").unwrap().name, "exit");
    }

    #[test]
    fn unknown_returns_none() {
        assert!(match_command("nonexistent").is_none());
    }

    #[test]
    fn builtin_list_matches_commands() {
        let list = builtin_list();
        assert_eq!(list.len(), META_COMMANDS.len());
        assert!(list.iter().any(|(n, _)| *n == "mode"));
        assert!(list.iter().any(|(n, _)| *n == "projects"));
        assert!(list.iter().any(|(n, _)| *n == "storage"));
    }

    #[test]
    fn help_lines_not_empty() {
        let lines = help_lines();
        assert!(!lines.is_empty());
        assert!(lines.iter().any(|l| l.contains("resume")));
    }
}
