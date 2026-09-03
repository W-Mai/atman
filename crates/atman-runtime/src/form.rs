use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::event::FlowRunId;

// FormKind is what the caller asks the user for. Kept as a tagged enum
// so DSL calls, event replay, and daemon rendezvous can all round-trip
// through the same JSON shape.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FormKind {
    Confirm {
        prompt: String,
    },
    SingleSelect {
        prompt: String,
        options: Vec<String>,
    },
    MultiSelect {
        prompt: String,
        options: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max: Option<usize>,
    },
    Text {
        prompt: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
        #[serde(default)]
        multiline: bool,
    },
}

impl FormKind {
    pub fn prompt(&self) -> &str {
        match self {
            Self::Confirm { prompt }
            | Self::SingleSelect { prompt, .. }
            | Self::MultiSelect { prompt, .. }
            | Self::Text { prompt, .. } => prompt,
        }
    }

    pub fn discriminator(&self) -> &'static str {
        match self {
            Self::Confirm { .. } => "confirm",
            Self::SingleSelect { .. } => "single_select",
            Self::MultiSelect { .. } => "multi_select",
            Self::Text { .. } => "text",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FormQuestion {
    pub id: String,
    #[serde(flatten)]
    pub kind: FormKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CompositeForm {
    pub questions: Vec<FormQuestion>,
}

impl CompositeForm {
    pub fn validate_submission(&self, submission: &FormSubmission) -> Result<(), String> {
        let FormSubmission::Submitted { answers } = submission else {
            return Ok(());
        };
        if answers.len() != self.questions.len() {
            return Err(format!(
                "expected {} form answers, received {}",
                self.questions.len(),
                answers.len()
            ));
        }
        for (question, answer) in self.questions.iter().zip(answers) {
            validate_answer(question, answer)?;
        }
        Ok(())
    }
}

fn validate_answer(question: &FormQuestion, answer: &FormAnswer) -> Result<(), String> {
    if matches!(answer, FormAnswer::Cancelled) {
        return Ok(());
    }
    match (&question.kind, answer) {
        (FormKind::Confirm { .. }, FormAnswer::Confirmed { .. })
        | (FormKind::Text { .. }, FormAnswer::TextEntered { .. }) => Ok(()),
        (FormKind::SingleSelect { options, .. }, FormAnswer::Selected { index, label }) => {
            validate_option(&question.id, options, *index, label)
        }
        (
            FormKind::MultiSelect {
                options, min, max, ..
            },
            FormAnswer::MultiSelected { indices, labels },
        ) => {
            if indices.len() != labels.len() {
                return Err(format!(
                    "question `{}` received mismatched multi-select indices and labels",
                    question.id
                ));
            }
            if min.is_some_and(|min| indices.len() < min)
                || max.is_some_and(|max| indices.len() > max)
            {
                return Err(format!(
                    "question `{}` received an invalid number of selections",
                    question.id
                ));
            }
            let mut unique = HashSet::with_capacity(indices.len());
            for (index, label) in indices.iter().zip(labels) {
                if !unique.insert(*index) {
                    return Err(format!(
                        "question `{}` received duplicate selection index {index}",
                        question.id
                    ));
                }
                validate_option(&question.id, options, *index, label)?;
            }
            Ok(())
        }
        _ => Err(format!(
            "answer kind does not match question `{}` ({})",
            question.id,
            question.kind.discriminator()
        )),
    }
}

fn validate_option(
    question_id: &str,
    options: &[String],
    index: usize,
    label: &str,
) -> Result<(), String> {
    match options.get(index) {
        Some(option) if option == label => Ok(()),
        Some(_) => Err(format!(
            "question `{question_id}` received a label that does not match selection index {index}"
        )),
        None => Err(format!(
            "question `{question_id}` received out-of-range selection index {index}"
        )),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum FormSubmission {
    Submitted { answers: Vec<FormAnswer> },
    Rejected,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PendingForm {
    pub form_id: String,
    pub run_id: FlowRunId,
    pub tool_use_id: String,
    pub form: CompositeForm,
    pub kind: FormKind,
    pub emitted_at: DateTime<Utc>,
}

// FormAnswer stays tagged so a `Cancelled` response is a first-class
// choice, not a magic error code. DSL code inspects `answer.kind` first.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FormAnswer {
    Confirmed {
        value: bool,
    },
    Selected {
        index: usize,
        label: String,
    },
    MultiSelected {
        indices: Vec<usize>,
        labels: Vec<String>,
    },
    TextEntered {
        text: String,
    },
    Cancelled,
}

impl FormAnswer {
    pub fn discriminator(&self) -> &'static str {
        match self {
            Self::Confirmed { .. } => "confirmed",
            Self::Selected { .. } => "selected",
            Self::MultiSelected { .. } => "multi_selected",
            Self::TextEntered { .. } => "text_entered",
            Self::Cancelled => "cancelled",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_kind_serializes_with_tag() {
        let k = FormKind::SingleSelect {
            prompt: "pick".into(),
            options: vec!["a".into(), "b".into()],
        };
        let s = serde_json::to_string(&k).unwrap();
        assert!(s.contains(r#""kind":"single_select""#));
        assert!(s.contains(r#""prompt":"pick""#));
    }

    #[test]
    fn form_kind_round_trip_confirm() {
        let k = FormKind::Confirm {
            prompt: "sure?".into(),
        };
        let s = serde_json::to_string(&k).unwrap();
        let back: FormKind = serde_json::from_str(&s).unwrap();
        assert_eq!(back, k);
    }

    #[test]
    fn form_kind_round_trip_multi_select_omits_empty_bounds() {
        let k = FormKind::MultiSelect {
            prompt: "tags".into(),
            options: vec!["a".into()],
            min: None,
            max: Some(2),
        };
        let s = serde_json::to_string(&k).unwrap();
        assert!(!s.contains("\"min\""));
        assert!(s.contains("\"max\":2"));
        let back: FormKind = serde_json::from_str(&s).unwrap();
        assert_eq!(back, k);
    }

    #[test]
    fn form_answer_cancelled_serializes_as_tag_only() {
        let a = FormAnswer::Cancelled;
        let s = serde_json::to_string(&a).unwrap();
        assert_eq!(s, r#"{"kind":"cancelled"}"#);
    }

    #[test]
    fn form_answer_round_trip_multi_selected() {
        let a = FormAnswer::MultiSelected {
            indices: vec![0, 2],
            labels: vec!["a".into(), "c".into()],
        };
        let s = serde_json::to_string(&a).unwrap();
        let back: FormAnswer = serde_json::from_str(&s).unwrap();
        assert_eq!(back, a);
    }

    #[test]
    fn discriminators_are_stable() {
        assert_eq!(
            FormKind::Text {
                prompt: "".into(),
                placeholder: None,
                multiline: false,
            }
            .discriminator(),
            "text"
        );
        assert_eq!(FormAnswer::Cancelled.discriminator(), "cancelled");
    }

    #[test]
    fn composite_form_rejects_spoofed_selection_labels() {
        let form = CompositeForm {
            questions: vec![FormQuestion {
                id: "target".into(),
                kind: FormKind::SingleSelect {
                    prompt: "Pick".into(),
                    options: vec!["safe".into()],
                },
            }],
        };
        let result = form.validate_submission(&FormSubmission::Submitted {
            answers: vec![FormAnswer::Selected {
                index: 0,
                label: "different".into(),
            }],
        });
        assert!(result.is_err());
    }

    #[test]
    fn composite_form_accepts_matching_multi_selection() {
        let form = CompositeForm {
            questions: vec![FormQuestion {
                id: "targets".into(),
                kind: FormKind::MultiSelect {
                    prompt: "Pick".into(),
                    options: vec!["a".into(), "b".into()],
                    min: Some(1),
                    max: Some(2),
                },
            }],
        };
        assert!(
            form.validate_submission(&FormSubmission::Submitted {
                answers: vec![FormAnswer::MultiSelected {
                    indices: vec![0, 1],
                    labels: vec!["a".into(), "b".into()],
                }],
            })
            .is_ok()
        );
    }
}
