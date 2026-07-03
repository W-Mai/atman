use crate::error::RuntimeError;
use crate::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::Value;

pub struct ShellQuote;

impl Tool for ShellQuote {
    fn name(&self) -> &str {
        "shell_quote"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let s = extract_string(&args, "s", 0)?;
            Ok(Value::Str(shell_quote(&s)))
        })
    }
}

pub fn shell_quote(s: &str) -> String {
    // POSIX-safe: wrap in single quotes, escape any internal ' as '\''.
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

pub struct ComposeEmailPreview;

impl Tool for ComposeEmailPreview {
    fn name(&self) -> &str {
        "compose_email_preview"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let subject = extract_string(&args, "subject", 0)?;
            let body = extract_string(&args, "body", 1)?;
            let to = extract_string_list(&args, "to", 2)?;
            Ok(Value::Str(compose_email_preview(&subject, &body, &to)))
        })
    }
}

pub fn compose_email_preview(subject: &str, body: &str, to: &[String]) -> String {
    format!("To: {}\nSubject: {subject}\n---\n{body}", to.join(", "))
}

fn extract_string(args: &ToolArgs, name: &str, pos: usize) -> Result<String, RuntimeError> {
    let value = match args.named(name) {
        Some(v) => v,
        None => args.positional(pos)?,
    };
    match value {
        Value::Str(s) => Ok(s.clone()),
        other => Err(RuntimeError::TypeMismatch {
            expected: "string".into(),
            actual: other.kind_name().into(),
        }),
    }
}

fn extract_string_list(
    args: &ToolArgs,
    name: &str,
    pos: usize,
) -> Result<Vec<String>, RuntimeError> {
    let value = match args.named(name) {
        Some(v) => v,
        None => args.positional(pos)?,
    };
    match value {
        Value::List(items) => items
            .iter()
            .map(|v| match v {
                Value::Str(s) => Ok(s.clone()),
                other => Err(RuntimeError::TypeMismatch {
                    expected: "list of string".into(),
                    actual: other.kind_name().into(),
                }),
            })
            .collect(),
        other => Err(RuntimeError::TypeMismatch {
            expected: "list".into(),
            actual: other.kind_name().into(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_wraps_and_escapes() {
        assert_eq!(shell_quote("hello"), "'hello'");
        assert_eq!(shell_quote("It's fine"), "'It'\\''s fine'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("a'b'c"), "'a'\\''b'\\''c'");
    }

    #[test]
    fn compose_email_preview_formats_headers() {
        let preview = compose_email_preview(
            "Deploy status",
            "See attached",
            &["a@x.com".into(), "b@x.com".into()],
        );
        assert_eq!(
            preview,
            "To: a@x.com, b@x.com\nSubject: Deploy status\n---\nSee attached"
        );
    }
}
