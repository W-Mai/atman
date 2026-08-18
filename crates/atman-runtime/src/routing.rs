use std::fmt;

use atman_dsl::ast::File;

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
}
