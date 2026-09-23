use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuoteSnapshot {
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserInputPresentation {
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quote: Option<QuoteSnapshot>,
}

impl UserInputPresentation {
    pub fn model_text(&self) -> String {
        let Some(quote) = &self.quote else {
            return self.prompt.clone();
        };
        let quoted = quote
            .text
            .lines()
            .map(|line| {
                if line.is_empty() {
                    ">".to_owned()
                } else {
                    format!("> {line}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        format!("{quoted}\n\n{}", self.prompt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_only_and_multiline_model_text() {
        let presentation = UserInputPresentation {
            prompt: String::new(),
            quote: Some(QuoteSnapshot {
                text: "one\n\ntwo".into(),
            }),
        };
        assert_eq!(presentation.model_text(), "> one\n>\n> two\n\n");
    }
}
