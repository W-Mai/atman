//! Installed flow command loading and typed argument decoding.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use atman_rt::ast::{FlowDecl, TypeExpr};

use crate::source_program::{LinkedProgram, SourceRoots, load_program};
use crate::{Value, ValueJson};

/// Captured command source, selected entry, typed arguments and source directory.
pub type ResolvedCommand = (LinkedProgram, String, Vec<(String, Value)>, Option<PathBuf>);

/// Preserve whether input is authored message data or explicit argument syntax.
pub enum CommandArguments<'a> {
    Text(&'a str),
    Syntax(&'a str),
}

/// Resolve an explicit installed-flow command with the existing argument grammar.
pub fn resolve_slash_command(
    line: &str,
    cfg: &Path,
    project_root: Option<&Path>,
) -> Result<ResolvedCommand> {
    let trimmed_line = line.trim();
    let (name_full, rest_raw) = match trimmed_line.split_once(char::is_whitespace) {
        Some((n, r)) => (n, r.trim_start()),
        None => (trimmed_line, ""),
    };
    if name_full.is_empty() {
        bail!("empty slash command");
    }
    let name = name_full.strip_prefix('/').unwrap_or(name_full);
    resolve_command(name, CommandArguments::Syntax(rest_raw), cfg, project_root)
}

/// Load the installed source and bind arguments without changing their input identity.
pub fn resolve_command(
    name: &str,
    arguments: CommandArguments<'_>,
    cfg: &Path,
    project_root: Option<&Path>,
) -> Result<ResolvedCommand> {
    if name == "agent" || name == "spec" {
        crate::templates::ensure_managed_agent_at(cfg)?;
    }
    let path = crate::tools::flow_source::resolve_installed_command(name, Some(cfg), project_root)
        .map(|source| source.path)
        .ok_or_else(|| {
            let project_location = project_root
                .map(|root| root.join(".atman/commands").display().to_string())
                .unwrap_or_else(|| "<no project root>".into());
            anyhow::anyhow!(
                "no such command: {name} (looked in {project_location} and {})",
                cfg.join("commands").display()
            )
        })?;
    let program = load_program(
        &path,
        &SourceRoots {
            project_root: project_root.map(Path::to_path_buf),
            config_dir: Some(cfg.to_path_buf()),
        },
    )?;
    let parsed = program.entry_file();
    if parsed.flows.is_empty() {
        bail!("{} declares no flows", path.display());
    }
    let flow = parsed
        .flows
        .iter()
        .find(|f| f.name.name == name)
        .or_else(|| {
            if parsed.flows.len() == 1 {
                parsed.flows.first()
            } else {
                None
            }
        })
        .ok_or_else(|| {
            let names: Vec<&str> = parsed.flows.iter().map(|f| f.name.name.as_str()).collect();
            anyhow::anyhow!(
                "{} has {} flows but none is named `{name}` — declare a `flow {name}(...)` entry or invoke one of: {}",
                path.display(),
                parsed.flows.len(),
                names.join(", ")
            )
        })?;
    let flow_name = flow.name.name.clone();
    let params: Vec<String> = flow.params.iter().map(|p| p.name.name.clone()).collect();

    let (rest_raw, routed_text) = match arguments {
        CommandArguments::Text(text) => (text, true),
        CommandArguments::Syntax(text) => (text, false),
    };
    let message = crate::validate::invocation_message_parameter(flow)?
        .or_else(|| (params.len() == 1).then(|| params[0].as_str()));
    let named = rest_raw.split_whitespace().next().is_some_and(|first| {
        first
            .split_once('=')
            .is_some_and(|(name, _)| params.iter().any(|param| param == name))
    });
    let mut kv: Vec<(String, Value)> = Vec::new();
    // Routed messages are data. Explicit commands opt into named syntax only
    // through a declared leading key; punctuation elsewhere cannot select it.
    if !rest_raw.is_empty()
        && (routed_text || !named)
        && let Some(message) = message
    {
        kv.push((
            message.to_owned(),
            decode_flow_argument(flow, message, rest_raw)?,
        ));
        let source_dir = path.parent().map(|p| p.to_path_buf());
        return Ok((program, flow_name, kv, source_dir));
    }

    let mut positional_index = 0usize;
    for (key, value) in split_quoted_args(rest_raw)? {
        if let Some(name) = key {
            kv.push((name.clone(), decode_flow_argument(flow, &name, &value)?));
        } else {
            while positional_index < params.len()
                && kv.iter().any(|(name, _)| name == &params[positional_index])
            {
                positional_index += 1;
            }
            let Some(parameter) = params.get(positional_index) else {
                bail!(
                    "too many arguments for /{name}; expected at most {}",
                    params.len()
                );
            };
            kv.push((
                parameter.clone(),
                decode_flow_argument(flow, parameter, &value)?,
            ));
            positional_index += 1;
        }
    }
    let source_dir = path.parent().map(|p| p.to_path_buf());
    Ok((program, flow_name, kv, source_dir))
}

fn split_quoted_args(input: &str) -> Result<Vec<(Option<String>, String)>> {
    let mut out = Vec::new();
    let mut name = None;
    let mut cur = String::new();
    let mut started = false;
    let mut chars = input.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;
    while let Some(c) = chars.next() {
        match c {
            '"' if !in_single => {
                in_double = !in_double;
                started = true;
            }
            '\'' if !in_double => {
                in_single = !in_single;
                started = true;
            }
            '\\' if in_double => {
                if let Some(next @ ('"' | '\\')) = chars.peek().copied() {
                    cur.push(next);
                    chars.next();
                } else {
                    cur.push(c);
                }
            }
            '=' if !in_single && !in_double && name.is_none() && !cur.is_empty() => {
                name = Some(std::mem::take(&mut cur));
            }
            c if c.is_whitespace() && !in_single && !in_double => {
                if started {
                    out.push((name.take(), std::mem::take(&mut cur)));
                    started = false;
                }
            }
            c => {
                cur.push(c);
                started = true;
            }
        }
    }
    if in_single || in_double {
        bail!("unterminated quote in command arguments");
    }
    if started {
        out.push((name, cur));
    }
    Ok(out)
}

/// Decode one argument using its declared parameter type.
pub fn decode_flow_argument(flow: &FlowDecl, name: &str, raw: &str) -> Result<Value> {
    let Some(param) = flow.params.iter().find(|param| param.name.name == name) else {
        return Ok(Value::Str(raw.to_string()));
    };
    decode_flow_argument_type(raw, &param.ty)
        .with_context(|| format!("invalid value for flow argument `{name}`"))
}

fn decode_flow_argument_type(raw: &str, ty: &TypeExpr) -> Result<Value> {
    match ty {
        TypeExpr::Named(name) if name.name.eq_ignore_ascii_case("int") => raw
            .parse::<i64>()
            .map(Value::Int)
            .with_context(|| format!("expected int, got `{raw}`")),
        TypeExpr::Named(name) if name.name.eq_ignore_ascii_case("float") => raw
            .parse::<f64>()
            .map(Value::Float)
            .with_context(|| format!("expected float, got `{raw}`")),
        TypeExpr::Named(name) if name.name.eq_ignore_ascii_case("bool") => raw
            .parse::<bool>()
            .map(Value::Bool)
            .with_context(|| format!("expected bool, got `{raw}`")),
        TypeExpr::Named(name) if name.name.eq_ignore_ascii_case("unit") => match raw {
            "" | "()" | "null" => Ok(Value::Unit),
            _ => bail!("expected unit as `()`, `null`, or an empty value, got `{raw}`"),
        },
        TypeExpr::Named(name) if name.name.eq_ignore_ascii_case("string") => {
            Ok(Value::Str(raw.to_string()))
        }
        TypeExpr::Named(name) if name.name.eq_ignore_ascii_case("path") => {
            Ok(Value::Host(crate::AtmanPayload::Path(PathBuf::from(raw))))
        }
        TypeExpr::List(_) => decode_json_list_argument(raw),
        TypeExpr::Struct(_) => decode_json_struct_argument(raw),
        TypeExpr::Named(name) if name.name.eq_ignore_ascii_case("list") => {
            decode_json_list_argument(raw)
        }
        TypeExpr::Named(name) if name.name.eq_ignore_ascii_case("struct") => {
            decode_json_struct_argument(raw)
        }
        TypeExpr::Named(name)
            if name.name.eq_ignore_ascii_case("value")
                || name.name.eq_ignore_ascii_case("any")
                || name
                    .name
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_uppercase) =>
        {
            Ok(decode_json_argument(raw).unwrap_or_else(|| Value::Str(raw.to_string())))
        }
        TypeExpr::Named(_) => Ok(Value::Str(raw.to_string())),
    }
}

fn decode_json_argument(raw: &str) -> Option<Value> {
    serde_json::from_str(raw).ok().map(Value::from_json)
}

fn decode_json_list_argument(raw: &str) -> Result<Value> {
    let value: serde_json::Value =
        serde_json::from_str(raw).with_context(|| format!("expected a JSON array, got `{raw}`"))?;
    if !value.is_array() {
        bail!("expected a JSON array, got `{raw}`");
    }
    Ok(Value::from_json(value))
}

fn decode_json_struct_argument(raw: &str) -> Result<Value> {
    let value: serde_json::Value = serde_json::from_str(raw)
        .with_context(|| format!("expected a JSON object, got `{raw}`"))?;
    if !value.is_object() {
        bail!("expected a JSON object, got `{raw}`");
    }
    Ok(Value::from_json(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_command_prefers_project_source_over_user_source() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config");
        let project = root.path().join("project");
        let user_commands = config.join("commands");
        let project_commands = project.join(".atman/commands");
        std::fs::create_dir_all(&user_commands).unwrap();
        std::fs::create_dir_all(&project_commands).unwrap();
        std::fs::write(
            user_commands.join("review.at"),
            "flow user_entry() { return \"user\" }\n",
        )
        .unwrap();
        std::fs::write(
            project_commands.join("review.at"),
            "flow review(input: string) { return input }\n",
        )
        .unwrap();

        let (_, flow_name, args, source_dir) =
            resolve_slash_command("/review inspect this", &config, Some(&project)).unwrap();

        assert_eq!(flow_name, "review");
        assert_eq!(args.len(), 1);
        assert_eq!(args[0].0, "input");
        assert!(matches!(&args[0].1, Value::Str(value) if value == "inspect this"));
        assert_eq!(source_dir, Some(project_commands));
    }

    #[test]
    fn slash_command_decodes_positional_and_named_arguments() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config");
        let project = root.path().join("project");
        let commands = project.join(".atman/commands");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::create_dir_all(&commands).unwrap();
        std::fs::write(
            commands.join("inspect.at"),
            "flow inspect(count: int, ready: bool, file: path, payload: value) -> value { return payload }\n",
        )
        .unwrap();

        let (_, _, args, _) = resolve_slash_command(
            r#"/inspect 7 ready=true file=src/main.rs payload='{"id":3}'"#,
            &config,
            Some(&project),
        )
        .unwrap();

        assert!(matches!(&args[0].1, Value::Int(7)));
        assert!(matches!(&args[1].1, Value::Bool(true)));
        assert!(matches!(
            &args[2].1,
            Value::Host(crate::AtmanPayload::Path(path))
                if path == Path::new("src/main.rs")
        ));
        assert!(
            matches!(&args[3].1, Value::Struct(fields) if matches!(&fields[..], [(name, Value::Int(3))] if name == "id"))
        );
    }

    #[test]
    fn text_commands_keep_message_data_separate_from_argument_syntax() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config");
        let project = root.path().join("project");
        let commands = project.join(".atman/commands");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::create_dir_all(&commands).unwrap();
        std::fs::write(
            commands.join("review.at"),
            "flow review(input: string) -> string { return input }\n",
        )
        .unwrap();

        for text in [
            "inspect   this carefully",
            "关联仓库 https://code.example/repo?ref=main&view=tree 和 another repo",
            "代码 x = \"y\"; don't remove the quotes",
            "first\n\n  key=value\n  path=C:\\new\\folder\nlast",
            "未闭合 \"引号和 \\ 末尾",
            "中文 👩🏽‍💻 e\u{301} a==b x=y",
        ] {
            let (_, _, args, _) =
                resolve_slash_command(&format!("/review {text}"), &config, Some(&project)).unwrap();
            assert!(matches!(&args[..], [(name, Value::Str(value))]
                if name == "input" && value == text));
        }
        std::fs::write(
            commands.join("review.at"),
            "flow review(limit: int = 2, input: string) -> string { contract { invocation { user_message: input } } return input }\n",
        ).unwrap();
        for text in [
            "input=literal data a=b",
            "limit=not-an-option",
            "first\n  x=1\nlast",
        ] {
            let (_, _, args, _) = resolve_command(
                "review",
                CommandArguments::Text(text),
                &config,
                Some(&project),
            )
            .unwrap();
            assert!(matches!(&args[..], [(name, Value::Str(value))]
                if name == "input" && value == text));
        }
    }

    #[test]
    fn spec_slash_command_accepts_a_freeform_request() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config");
        let (_, flow_name, args, _) =
            resolve_slash_command("/spec discuss project goals", &config, None).unwrap();
        assert_eq!(flow_name, "spec");
        assert!(matches!(
            &args[..],
            [(name, Value::Str(value))]
                if name == "user_prompt" && value == "discuss project goals"
        ));
    }
}
