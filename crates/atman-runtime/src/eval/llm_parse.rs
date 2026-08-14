//! Shared parsing utilities for LLM high-level tools (classify / extract / generate_branches).

use crate::value::Value;

/// Normalize a `dispatch_llm` return value into text.
///
/// - `Value::Err` → `None` (caller must propagate before calling this)
/// - `Value::Str(s)` → `Some(s)` if non-empty
/// - `Value::Message(m)` → `Some(text_concat)` if non-empty
/// - Other pre-parsed types (Bool/Int/Float/Struct/List) → `None`
pub fn llm_result_to_text(result: &Value) -> Option<String> {
    match result {
        Value::Str(s) if !s.is_empty() => Some(s.clone()),
        Value::Message(m) => {
            let text = m.text_concat();
            if text.is_empty() { None } else { Some(text) }
        }
        _ => None,
    }
}

/// Strip markdown code fences (```json ... ``` or ``` ... ```).
/// Returns the inner content, or the original text if no fences found.
fn strip_code_fences(text: &str) -> &str {
    let trimmed = text.trim();
    if !trimmed.starts_with("```") {
        return trimmed;
    }
    // Skip the opening fence line
    let after_open = &trimmed[3..];
    let newline_pos = match after_open.find('\n') {
        Some(p) => p,
        None => return trimmed, // malformed, no newline after fence
    };
    let inner = &after_open[newline_pos + 1..];
    // Find closing fence
    if let Some(end) = inner.find("```") {
        inner[..end].trim()
    } else {
        // No closing fence, return inner content
        inner.trim()
    }
}

/// Find the first complete JSON object in text using a brace-depth scanner.
/// Counts `{`/`}` while skipping string literals (handles `"` and escape sequences).
pub fn find_first_json_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape = false;
    for i in start..bytes.len() {
        let c = bytes[i] as char;
        if escape {
            escape = false;
            continue;
        }
        if in_string {
            match c {
                '\\' => escape = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Extract JSON from LLM output text.
/// Tries: strip code fences → full parse → brace-depth scan → fail.
pub fn extract_json_from_text(text: &str) -> Option<serde_json::Value> {
    let cleaned = strip_code_fences(text);
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(cleaned) {
        return Some(json);
    }
    if let Some(substring) = find_first_json_object(cleaned) {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(substring) {
            return Some(json);
        }
    }
    None
}

/// Parse a boolean from LLM output text.
/// Handles: yes/true/1/affirmative/是 → true; no/false/0/negative/否 → false
pub fn parse_bool_from_text(text: &str) -> Option<bool> {
    let first_word = text.split_whitespace().next()?.to_lowercase();
    match first_word.as_str() {
        "yes" | "true" | "1" | "affirmative" | "是" => Some(true),
        "no" | "false" | "0" | "negative" | "否" => Some(false),
        _ => None,
    }
}

/// Parse an explicit category label from the first output token.
/// Matches case-insensitively and tolerates surrounding markdown/punctuation, but
/// deliberately rejects explanatory prose. The caller can retry with a stricter
/// prompt instead of guessing from words that happen to occur inside a label.
pub fn parse_category_from_text(text: &str, categories: &[String]) -> Option<String> {
    let first_token = text
        .split_whitespace()
        .next()?
        .trim_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != '-');
    let normalized = first_token.to_lowercase();
    categories
        .iter()
        .find(|cat| cat.to_lowercase() == normalized)
        .cloned()
}

/// Parse a list of strings from LLM output text.
/// Tries JSON array first, then falls back to newline splitting with markdown cleanup.
pub fn parse_list_from_text(text: &str) -> Vec<String> {
    let cleaned = strip_code_fences(text);
    // Try JSON array
    if let Ok(serde_json::Value::Array(arr)) = serde_json::from_str::<serde_json::Value>(cleaned) {
        return arr
            .into_iter()
            .map(|v| match v {
                serde_json::Value::String(s) => s,
                other => other.to_string(),
            })
            .collect();
    }
    // Fallback: split by newlines, clean markdown markers
    cleaned
        .lines()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .map(|line| {
            // Strip leading markdown list markers: -, *, 1., 2., etc.
            let stripped = line.trim_start_matches(['-', '*']).trim_start();
            // Strip leading "N." patterns
            let stripped = if let Some(rest) = stripped.find('.').and_then(|pos| {
                let prefix = &stripped[..pos];
                if prefix.chars().all(|c| c.is_ascii_digit()) && !prefix.is_empty() {
                    Some(stripped[pos + 1..].trim_start())
                } else {
                    None
                }
            }) {
                rest
            } else {
                stripped
            };
            stripped.to_string()
        })
        .filter(|s| !s.is_empty())
        .collect()
}

/// Field definition parsed from the `fields` parameter of `llm.extract`.
pub struct FieldDef {
    pub name: String,
    pub ty: String, // "bool", "int", "float", "string", "list of string", "list of int"
    pub description: String,
}

/// Parse field definitions from a `Value::Struct` (the `fields` parameter).
/// Parse field definitions from a `Value::Struct` (the `fields` parameter).
/// Uses `--` annotation format: each field value is a `Value::Struct { type, desc }`.
pub fn parse_field_definitions(fields: &[(String, Value)]) -> Result<Vec<FieldDef>, String> {
    let mut defs = Vec::new();
    for (name, val) in fields {
        match val {
            Value::Struct(pairs)
                if pairs.len() == 2
                    && pairs.iter().any(|(k, _)| k == "type")
                    && pairs.iter().any(|(k, _)| k == "desc") =>
            {
                let ty = pairs
                    .iter()
                    .find(|(k, _)| k == "type")
                    .and_then(|(_, v)| match v {
                        Value::Str(s) => Some(s.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                let desc = pairs
                    .iter()
                    .find(|(k, _)| k == "desc")
                    .and_then(|(_, v)| match v {
                        Value::Str(s) => Some(s.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                defs.push(FieldDef {
                    name: name.clone(),
                    ty: ty.to_lowercase(),
                    description: desc,
                });
            }
            _ => {
                return Err(format!(
                    "field `{name}`: expected type annotation (e.g. `bool -- \"description\"`), got {}",
                    val.kind_name()
                ));
            }
        }
    }
    Ok(defs)
}

/// Coerce a Value to match a declared type.
/// Returns Err with a message if coercion is impossible.
pub fn coerce_field(value: Value, declared_type: &str) -> Result<Value, String> {
    match declared_type {
        "bool" => coerce_bool(value),
        "int" => coerce_int(value),
        "float" => coerce_float(value),
        "string" => coerce_string(value),
        "list of string" | "list of int" => coerce_list(value, declared_type),
        _ => Ok(value), // Unknown type: pass through
    }
}

fn coerce_bool(value: Value) -> Result<Value, String> {
    match value {
        Value::Bool(b) => Ok(Value::Bool(b)),
        Value::Int(n) => Ok(Value::Bool(n != 0)),
        Value::Float(f) => Ok(Value::Bool(f != 0.0)),
        Value::Unit => Ok(Value::Bool(false)),
        Value::Str(s) => {
            let lower = s.trim().to_lowercase();
            match lower.as_str() {
                "true" | "yes" | "1" | "affirmative" => Ok(Value::Bool(true)),
                "false" | "no" | "0" | "negative" => Ok(Value::Bool(false)),
                _ => Err(format!("cannot coerce \"{s}\" to bool")),
            }
        }
        other => Err(format!("cannot coerce {} to bool", other.kind_name())),
    }
}

fn coerce_int(value: Value) -> Result<Value, String> {
    match value {
        Value::Int(_) => Ok(value),
        Value::Float(f) => Ok(Value::Int(f as i64)),
        Value::Bool(b) => Ok(Value::Int(if b { 1 } else { 0 })),
        Value::Str(s) => {
            if let Ok(n) = s.trim().parse::<i64>() {
                return Ok(Value::Int(n));
            }
            // Try float string → truncate
            if let Ok(f) = s.trim().parse::<f64>() {
                return Ok(Value::Int(f as i64));
            }
            Err(format!("cannot coerce \"{s}\" to int"))
        }
        other => Err(format!("cannot coerce {} to int", other.kind_name())),
    }
}

fn coerce_float(value: Value) -> Result<Value, String> {
    match value {
        Value::Float(_) | Value::Int(_) => Ok(value),
        Value::Bool(b) => Ok(Value::Float(if b { 1.0 } else { 0.0 })),
        Value::Str(s) => s
            .trim()
            .parse::<f64>()
            .map(Value::Float)
            .map_err(|_| format!("cannot coerce \"{s}\" to float")),
        other => Err(format!("cannot coerce {} to float", other.kind_name())),
    }
}

fn coerce_string(value: Value) -> Result<Value, String> {
    match value {
        Value::Str(_) => Ok(value),
        Value::Int(n) => Ok(Value::Str(n.to_string())),
        Value::Float(f) => Ok(Value::Str(f.to_string())),
        Value::Bool(b) => Ok(Value::Str(b.to_string())),
        Value::Unit => Ok(Value::Str(String::new())),
        other => Err(format!("cannot coerce {} to string", other.kind_name())),
    }
}

fn coerce_list(value: Value, declared_type: &str) -> Result<Value, String> {
    match value {
        Value::List(_) => Ok(value),
        Value::Unit => Ok(Value::List(Vec::new())),
        Value::Str(s) if declared_type == "list of string" => Ok(Value::List(vec![Value::Str(s)])),
        other => Err(format!(
            "cannot coerce {} to {declared_type}",
            other.kind_name()
        )),
    }
}

/// Validate that a Struct contains all required fields.
pub fn validate_struct_fields(s: &[(String, Value)], required: &[&str]) -> Result<(), String> {
    for field_name in required {
        if !s.iter().any(|(k, _)| k == field_name) {
            return Err(format!("missing field '{field_name}'"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod category_tests {
    use super::parse_category_from_text;

    fn categories() -> Vec<String> {
        ["waiting_for_user", "lazy", "forgot_tools", "done"]
            .into_iter()
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn explanatory_tools_text_does_not_become_forgot_tools() {
        assert_eq!(
            parse_category_from_text(
                "The agent already used tools and is summarizing completed work.",
                &categories(),
            ),
            None
        );
    }

    #[test]
    fn explanatory_user_text_does_not_become_waiting_for_user() {
        assert_eq!(
            parse_category_from_text(
                "The user request has been answered and no decision remains.",
                &categories(),
            ),
            None
        );
    }

    #[test]
    fn explicit_label_allows_markdown_and_punctuation() {
        assert_eq!(
            parse_category_from_text("`done`. Nothing remains.", &categories()),
            Some("done".to_string())
        );
    }

    #[test]
    fn explicit_label_matches_unicode_case() {
        assert_eq!(
            parse_category_from_text("état", &["ÉTAT".to_string()]),
            Some("ÉTAT".to_string())
        );
    }
}
