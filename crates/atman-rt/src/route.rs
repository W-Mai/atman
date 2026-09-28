//! Route selection for a parsed Atman program.

use alloc::{format, string::String};

use crate::ast::File;

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

/// Selects the first matching route, then falls back to `default_route`.
pub fn resolve_route(file: &File, input: &str) -> Option<RouteMatch> {
    for route in &file.routes {
        if let Some(rest) = input.strip_prefix(&route.pattern) {
            return Some(RouteMatch {
                command: route.flow.name.clone(),
                args: rest.trim().into(),
            });
        }
    }
    file.default_route.as_ref().map(|route| RouteMatch {
        command: route.flow.name.clone(),
        args: input.trim().into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{DefaultRouteDecl, Ident, RouteDecl, Span};

    #[test]
    fn explicit_route_precedes_default() {
        let mut file = File::default();
        file.routes.push(RouteDecl {
            pattern: "hello".into(),
            flow: Ident::new("greet", Span::default()),
            span: Span::default(),
        });
        file.default_route = Some(DefaultRouteDecl {
            flow: Ident::new("agent", Span::default()),
            span: Span::default(),
        });

        assert_eq!(
            resolve_route(&file, "hello world"),
            Some(RouteMatch {
                command: "greet".into(),
                args: "world".into(),
            })
        );
        assert_eq!(
            resolve_route(&file, "other"),
            Some(RouteMatch {
                command: "agent".into(),
                args: "other".into(),
            })
        );
    }
}
