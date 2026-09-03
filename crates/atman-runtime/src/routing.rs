use std::fmt;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use atman_dsl::ast::File;

use crate::Value;
use crate::config_hub::{ConfigError, ConfigHub};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteMatch {
    pub command: String,
    pub args: String,
}

impl RouteMatch {
    pub fn slash_call(&self) -> String {
        if self.args.is_empty() {
            format!("/{}", self.command)
        } else {
            format!("/{} {}", self.command, self.args)
        }
    }
}

#[derive(Debug)]
pub enum RouteLoadError {
    Config(ConfigError),
    Parse(String),
}

impl fmt::Display for RouteLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(error) => write!(f, "load routes.at: {error}"),
            Self::Parse(error) => write!(f, "parse routes.at: {error}"),
        }
    }
}

impl std::error::Error for RouteLoadError {}

#[derive(Debug)]
pub struct RouteProgram {
    file: Option<File>,
}

impl RouteProgram {
    pub fn load(hub: &ConfigHub) -> Result<Self, RouteLoadError> {
        let file = hub
            .load_routes_source()
            .map_err(RouteLoadError::Config)?
            .map(|source| {
                atman_dsl::parse::parse_file(&source)
                    .map_err(|error| RouteLoadError::Parse(error.to_string()))
            })
            .transpose()?;
        Ok(Self { file })
    }

    pub fn resolve(&self, input: &str) -> Option<RouteMatch> {
        let file = self.file.as_ref()?;
        for route in &file.routes {
            if let Some(rest) = input.strip_prefix(&route.pattern) {
                return Some(RouteMatch {
                    command: route.flow.name.clone(),
                    args: rest.trim().to_string(),
                });
            }
        }
        file.default_route.as_ref().map(|route| RouteMatch {
            command: route.flow.name.clone(),
            args: input.trim().to_string(),
        })
    }
}

#[derive(Debug)]
pub struct ResolvedCommand {
    pub file: File,
    pub flow_name: String,
    pub args: Vec<(String, Value)>,
    pub source_dir: Option<PathBuf>,
    pub path: PathBuf,
}

pub fn resolve_command_call(hub: &ConfigHub, line: &str) -> Result<ResolvedCommand> {
    let trimmed_line = line.trim();
    let (name_full, rest_raw) = match trimmed_line.split_once(char::is_whitespace) {
        Some((name, rest)) => (name, rest.trim_start()),
        None => (trimmed_line, ""),
    };
    if name_full.is_empty() {
        bail!("empty slash command");
    }
    let name = name_full.strip_prefix('/').unwrap_or(name_full);
    if name.is_empty()
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
    {
        bail!("invalid command name: {name}");
    }
    if name == "agent" {
        crate::templates::ensure_managed_agent_at(hub.config_dir())?;
    }
    let path = hub.config_dir().join("commands").join(format!("{name}.at"));
    if !path.exists() {
        bail!("no such command: {name} (looked for {})", path.display());
    }
    let source =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let file = atman_dsl::parse::parse_file(&source)
        .with_context(|| format!("parsing {}", path.display()))?;
    if file.flows.is_empty() {
        bail!("{} declares no flows", path.display());
    }
    let flow = file
        .flows
        .iter()
        .find(|flow| flow.name.name == name)
        .or_else(|| (file.flows.len() == 1).then(|| &file.flows[0]))
        .ok_or_else(|| {
            let names = file
                .flows
                .iter()
                .map(|flow| flow.name.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::anyhow!(
                "{} has {} flows but none is named `{name}` — declare a `flow {name}(...)` entry or invoke one of: {names}",
                path.display(),
                file.flows.len()
            )
        })?;
    let flow_name = flow.name.name.clone();
    let params = flow
        .params
        .iter()
        .map(|param| param.name.name.clone())
        .collect::<Vec<_>>();
    let args = bind_command_args(&params, rest_raw);
    let source_dir = path.parent().map(std::path::Path::to_path_buf);
    Ok(ResolvedCommand {
        file,
        flow_name,
        args,
        source_dir,
        path,
    })
}

fn bind_command_args(params: &[String], raw: &str) -> Vec<(String, Value)> {
    let tokens = split_quoted_args(raw);
    if params.len() == 1
        && !raw.is_empty()
        && !tokens
            .iter()
            .any(|token| token.contains('=') && !token.starts_with('='))
    {
        return vec![(params[0].clone(), Value::Str(raw.to_owned()))];
    }

    let mut args = Vec::new();
    let mut positional_index = 0usize;
    for token in tokens {
        if let Some((key, value)) = token.split_once('=') {
            args.push((key.to_owned(), Value::Str(value.to_owned())));
        } else if positional_index < params.len() {
            args.push((params[positional_index].clone(), Value::Str(token)));
            positional_index += 1;
        } else {
            args.push((format!("_extra{positional_index}"), Value::Str(token)));
            positional_index += 1;
        }
    }
    args
}

fn split_quoted_args(input: &str) -> Vec<String> {
    let mut output = Vec::new();
    let mut current = String::new();
    let mut chars = input.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;
    while let Some(character) = chars.next() {
        match character {
            '"' if !in_single => in_double = !in_double,
            '\'' if !in_double => in_single = !in_single,
            '\\' if in_double => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            character if character.is_whitespace() && !in_single && !in_double => {
                if !current.is_empty() {
                    output.push(std::mem::take(&mut current));
                }
            }
            character => current.push(character),
        }
    }
    if !current.is_empty() || in_single || in_double {
        output.push(current);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hub_with(source: Option<&str>) -> (tempfile::TempDir, ConfigHub) {
        let dir = tempfile::tempdir().unwrap();
        if let Some(source) = source {
            std::fs::write(dir.path().join("routes.at"), source).unwrap();
        }
        let hub = ConfigHub::from_config_dir(dir.path());
        (dir, hub)
    }

    #[test]
    fn resolves_explicit_route_before_default() {
        let (_dir, hub) = hub_with(Some(
            "route \"hello\" { flow: greet }\ndefault_route { flow: agent }\n",
        ));
        let route = RouteProgram::load(&hub)
            .unwrap()
            .resolve("hello world")
            .unwrap();
        assert_eq!(route.command, "greet");
        assert_eq!(route.args, "world");
    }

    #[test]
    fn resolves_default_route() {
        let (_dir, hub) = hub_with(Some("default_route { flow: agent }\n"));
        let route = RouteProgram::load(&hub)
            .unwrap()
            .resolve("  question  ")
            .unwrap();
        assert_eq!(route.command, "agent");
        assert_eq!(route.args, "question");
    }

    #[test]
    fn missing_or_unmatched_routes_are_none() {
        let (_missing_dir, missing_hub) = hub_with(None);
        assert!(
            RouteProgram::load(&missing_hub)
                .unwrap()
                .resolve("hello")
                .is_none()
        );

        let (_unmatched_dir, unmatched_hub) = hub_with(Some("route \"hello\" { flow: greet }\n"));
        assert!(
            RouteProgram::load(&unmatched_hub)
                .unwrap()
                .resolve("bye")
                .is_none()
        );
    }

    #[test]
    fn unreadable_routes_surface_config_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("routes.at")).unwrap();
        let hub = ConfigHub::from_config_dir(dir.path());
        let error = RouteProgram::load(&hub).unwrap_err().to_string();
        assert!(error.contains("load routes.at"), "error: {error}");
    }

    #[test]
    fn invalid_routes_surface_parse_error() {
        let (_dir, hub) = hub_with(Some("route invalid"));
        let error = RouteProgram::load(&hub).unwrap_err().to_string();
        assert!(error.contains("parse routes.at"), "error: {error}");
    }

    #[test]
    fn routes_toml_is_ignored() {
        let (dir, hub) = hub_with(None);
        std::fs::write(dir.path().join("routes.toml"), "\"!\" -> echo\n").unwrap();
        assert!(
            RouteProgram::load(&hub)
                .unwrap()
                .resolve("!hello")
                .is_none()
        );
    }

    #[test]
    fn command_resolution_rejects_path_traversal() {
        let (_dir, hub) = hub_with(None);
        let error = resolve_command_call(&hub, "/../../outside")
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid command name"), "{error}");
    }
}
