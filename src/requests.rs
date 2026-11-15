//! Requests: input that only the caller can give. A Session is `blocked`
//! while it has at least one pending Request.

use anyhow::{Result, bail};
use serde_json::{Map, Value, json};

use crate::messages::{bounded, one_line};

/// Pending Requests published per Session in one response.
pub const PUBLISHED_REQUEST_LIMIT: usize = 16;
/// Resolved Requests kept per Session so a repeated answer can be replayed.
const RESOLVED_RETENTION: usize = 256;
/// Largest provider tool input published verbatim.
const INPUT_LIMIT: usize = 2048;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    Approval,
    Question,
}

impl RequestKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Approval => "approval",
            Self::Question => "question",
        }
    }
}

/// What the caller said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    Allow,
    Deny { message: Option<String> },
    Text(String),
    Choice(usize),
}

impl Answer {
    pub fn parse(params: &Value) -> Result<Self> {
        let text = |name: &str| params.get(name).and_then(Value::as_str).map(str::to_owned);
        match params.get("decision").and_then(Value::as_str) {
            Some("allow") => Ok(Self::Allow),
            Some("deny") => Ok(Self::Deny {
                message: text("message"),
            }),
            Some("text") => text("text").map(Self::Text).ok_or_else(|| {
                anyhow::anyhow!("invalid answer: a text answer must carry its text")
            }),
            Some("choice") => params
                .get("choice")
                .and_then(Value::as_u64)
                .and_then(|choice| usize::try_from(choice).ok())
                .filter(|choice| *choice > 0)
                .map(Self::Choice)
                .ok_or_else(|| anyhow::anyhow!("invalid answer: --choice is a 1-based number")),
            _ => bail!("invalid answer: use --allow, --deny, --text, or --choice"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Allowed,
    Denied,
    Answered,
    Canceled,
}

impl Outcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allowed => "allowed",
            Self::Denied => "denied",
            Self::Answered => "answered",
            Self::Canceled => "canceled",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    pub outcome: Outcome,
    /// Absent when the Request was closed by cancel, stop, or restart.
    pub answer: Option<Answer>,
}

/// How a Request is answered on its provider's own channel.
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderRequest {
    /// Claude `can_use_tool` for an ordinary tool.
    ClaudeTool {
        input: Value,
    },
    /// Claude `can_use_tool` for `AskUserQuestion`.
    ClaudeQuestion {
        input: Value,
        questions: Vec<String>,
    },
    CodexCommand,
    CodexFileChange,
    CodexPermissions {
        permissions: Value,
    },
    CodexUserInput {
        question_ids: Vec<String>,
    },
    CodexElicitation {
        property: Option<String>,
        form: bool,
    },
}

/// A Request as the provider adapter describes it, before it has an ID.
#[derive(Debug, Clone)]
pub struct RequestDraft {
    pub kind: RequestKind,
    /// The provider's own ID for the request, used to match its later
    /// cancellation or resolution.
    pub provider_key: String,
    pub provider: ProviderRequest,
    pub tool: String,
    pub summary: String,
    pub input: Value,
    pub cwd: Option<String>,
    pub reason: Option<String>,
    pub text: Option<String>,
    pub choices: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RequestRecord {
    /// Short per-Session ordinal: `r1`, `r2`, ...
    pub id: String,
    pub execution_seq: Option<i64>,
    pub created_at_ms: i64,
    pub draft: RequestDraft,
    pub resolution: Option<Resolution>,
}

impl RequestRecord {
    pub const fn pending(&self) -> bool {
        self.resolution.is_none()
    }

    pub fn public(&self) -> Value {
        let draft = &self.draft;
        let mut value = Map::new();
        value.insert("id".to_owned(), json!(self.id));
        value.insert("kind".to_owned(), json!(draft.kind.as_str()));
        if let Some(execution_seq) = self.execution_seq {
            value.insert("execution_seq".to_owned(), json!(execution_seq));
        }
        match draft.kind {
            RequestKind::Approval => {
                value.insert("tool".to_owned(), json!(draft.tool));
                value.insert("summary".to_owned(), json!(one_line(&draft.summary).0));
                value.insert("input".to_owned(), bounded_input(&draft.input));
                if let Some(cwd) = &draft.cwd {
                    value.insert("cwd".to_owned(), json!(cwd));
                }
                if let Some(reason) = &draft.reason {
                    value.insert("reason".to_owned(), json!(bounded(reason).0));
                }
            }
            RequestKind::Question => {
                value.insert(
                    "text".to_owned(),
                    json!(bounded(draft.text.as_deref().unwrap_or_default()).0),
                );
                if !draft.choices.is_empty() {
                    value.insert("choices".to_owned(), json!(draft.choices));
                }
            }
        }
        value.insert("created_at_ms".to_owned(), json!(self.created_at_ms));
        Value::Object(value)
    }

    /// Check an answer against this Request and encode it for the provider.
    pub fn reply(&self, answer: &Answer) -> Result<(Outcome, Value)> {
        let draft = &self.draft;
        match (draft.kind, answer) {
            (RequestKind::Approval, Answer::Allow | Answer::Deny { .. })
            | (RequestKind::Question, Answer::Text(_) | Answer::Choice(_)) => {}
            (RequestKind::Approval, _) => {
                bail!(
                    "invalid answer: Request {} is an approval; use --allow or --deny",
                    self.id
                )
            }
            (RequestKind::Question, _) => {
                bail!(
                    "invalid answer: Request {} is a question; use --text or --choice",
                    self.id
                )
            }
        }
        let allow = matches!(answer, Answer::Allow);
        let outcome = match answer {
            Answer::Allow => Outcome::Allowed,
            Answer::Deny { .. } => Outcome::Denied,
            Answer::Text(_) | Answer::Choice(_) => Outcome::Answered,
        };
        let payload = match &draft.provider {
            ProviderRequest::ClaudeTool { input } => match answer {
                Answer::Deny { message } => json!({
                    "behavior": "deny",
                    "message": message.as_deref().unwrap_or("Denied by the caller."),
                }),
                _ => json!({"behavior": "allow", "updatedInput": input}),
            },
            ProviderRequest::ClaudeQuestion { input, questions } => {
                let reply = self.answer_text(answer)?;
                let answers = questions
                    .iter()
                    .map(|question| (question.clone(), json!(reply)))
                    .collect::<Map<_, _>>();
                let mut updated = input.clone();
                if let Some(object) = updated.as_object_mut() {
                    object.insert("answers".to_owned(), Value::Object(answers));
                }
                json!({"behavior": "allow", "updatedInput": updated})
            }
            ProviderRequest::CodexCommand | ProviderRequest::CodexFileChange => {
                json!({"decision": if allow { "accept" } else { "decline" }})
            }
            ProviderRequest::CodexPermissions { permissions } => json!({
                "permissions": if allow { permissions.clone() } else { json!({}) },
                "scope": "turn",
            }),
            ProviderRequest::CodexUserInput { question_ids } => {
                let reply = self.answer_text(answer)?;
                let answers = question_ids
                    .iter()
                    .map(|id| (id.clone(), json!({"answers": [reply]})))
                    .collect::<Map<_, _>>();
                json!({"answers": answers})
            }
            ProviderRequest::CodexElicitation { property, form } => {
                let reply = self.answer_text(answer)?;
                if !form {
                    return Ok((outcome, json!({"action": "accept"})));
                }
                let content = match serde_json::from_str::<Value>(&reply) {
                    Ok(Value::Object(object)) => Value::Object(object),
                    _ => match property {
                        Some(property) => json!({property.as_str(): reply}),
                        None => bail!(
                            "invalid answer: this form has several fields; pass --text as a JSON object"
                        ),
                    },
                };
                json!({"action": "accept", "content": content})
            }
        };
        Ok((outcome, payload))
    }

    fn answer_text(&self, answer: &Answer) -> Result<String> {
        match answer {
            Answer::Text(text) => Ok(text.clone()),
            Answer::Choice(choice) => {
                self.draft.choices.get(choice - 1).cloned().ok_or_else(|| {
                    anyhow::anyhow!(
                        "invalid answer: Request {} has {} choices",
                        self.id,
                        self.draft.choices.len()
                    )
                })
            }
            Answer::Allow | Answer::Deny { .. } => bail!("invalid answer for a question"),
        }
    }
}

fn bounded_input(input: &Value) -> Value {
    let encoded = input.to_string();
    if encoded.len() <= INPUT_LIMIT {
        return input.clone();
    }
    let mut end = INPUT_LIMIT / 2;
    while end > 0 && !encoded.is_char_boundary(end) {
        end -= 1;
    }
    json!({"truncated": true, "preview": &encoded[..end]})
}

#[derive(Debug, Default)]
pub struct RequestTable {
    records: Vec<RequestRecord>,
    issued: u64,
}

impl RequestTable {
    pub fn open(
        &mut self,
        draft: RequestDraft,
        execution_seq: Option<i64>,
        now_ms: i64,
    ) -> RequestRecord {
        self.issued += 1;
        let record = RequestRecord {
            id: format!("r{}", self.issued),
            execution_seq,
            created_at_ms: now_ms,
            draft,
            resolution: None,
        };
        self.records.push(record.clone());
        record
    }

    pub fn get(&self, id: &str) -> Option<&RequestRecord> {
        self.records.iter().find(|record| record.id == id)
    }

    pub fn pending(&self) -> Vec<RequestRecord> {
        self.records
            .iter()
            .filter(|record| record.pending())
            .cloned()
            .collect()
    }

    pub fn has_pending(&self) -> bool {
        self.records.iter().any(RequestRecord::pending)
    }

    /// Resolve one pending Request. Returns it as it was before resolution.
    pub fn resolve(&mut self, id: &str, resolution: Resolution) -> Option<RequestRecord> {
        let record = self
            .records
            .iter_mut()
            .find(|record| record.id == id && record.pending())?;
        let before = record.clone();
        record.resolution = Some(resolution);
        self.trim();
        Some(before)
    }

    /// Close pending Requests as canceled: every one, or only the one the
    /// provider named.
    pub fn cancel(&mut self, provider_key: Option<&str>) -> Vec<RequestRecord> {
        let mut closed = Vec::new();
        for record in &mut self.records {
            if record.pending() && provider_key.is_none_or(|key| record.draft.provider_key == key) {
                closed.push(record.clone());
                record.resolution = Some(Resolution {
                    outcome: Outcome::Canceled,
                    answer: None,
                });
            }
        }
        self.trim();
        closed
    }

    fn trim(&mut self) {
        let resolved = self
            .records
            .iter()
            .filter(|record| !record.pending())
            .count();
        let mut excess = resolved.saturating_sub(RESOLVED_RETENTION);
        self.records.retain(|record| {
            if excess > 0 && !record.pending() {
                excess -= 1;
                return false;
            }
            true
        });
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        Answer, Outcome, ProviderRequest, RequestDraft, RequestKind, RequestTable, Resolution,
    };

    fn approval(provider: ProviderRequest) -> RequestDraft {
        RequestDraft {
            kind: RequestKind::Approval,
            provider_key: "p1".to_owned(),
            provider,
            tool: "Bash".to_owned(),
            summary: "git push --force origin main".to_owned(),
            input: json!({"command": "git push --force origin main"}),
            cwd: Some("/work/repo".to_owned()),
            reason: None,
            text: None,
            choices: Vec::new(),
        }
    }

    fn question(provider: ProviderRequest) -> RequestDraft {
        RequestDraft {
            kind: RequestKind::Question,
            provider_key: "p2".to_owned(),
            provider,
            tool: "AskUserQuestion".to_owned(),
            summary: String::new(),
            input: json!({}),
            cwd: None,
            reason: None,
            text: Some("Which color?".to_owned()),
            choices: vec!["Red".to_owned(), "Blue".to_owned()],
        }
    }

    #[test]
    fn requests_get_short_ordinals_and_publish_their_kind() {
        let mut table = RequestTable::default();
        let first = table.open(approval(ProviderRequest::CodexCommand), Some(7), 1);
        let second = table.open(
            question(ProviderRequest::CodexUserInput {
                question_ids: vec!["q".to_owned()],
            }),
            Some(7),
            2,
        );
        assert_eq!((first.id.as_str(), second.id.as_str()), ("r1", "r2"));
        let public = first.public();
        assert_eq!(public["kind"], "approval");
        assert_eq!(public["tool"], "Bash");
        assert_eq!(public["input"]["command"], "git push --force origin main");
        assert_eq!(public["execution_seq"], 7);
        let public = second.public();
        assert_eq!(public["text"], "Which color?");
        assert_eq!(public["choices"], json!(["Red", "Blue"]));
        assert!(public.get("tool").is_none());
    }

    #[test]
    fn the_wrong_answer_kind_is_rejected() {
        let mut table = RequestTable::default();
        let approval = table.open(approval(ProviderRequest::CodexCommand), None, 1);
        let question = table.open(
            question(ProviderRequest::CodexUserInput {
                question_ids: vec!["q".to_owned()],
            }),
            None,
            1,
        );
        assert!(approval.reply(&Answer::Text("x".to_owned())).is_err());
        assert!(question.reply(&Answer::Allow).is_err());
        assert!(question.reply(&Answer::Choice(3)).is_err());
    }

    #[test]
    fn claude_answers_use_the_permission_prompt_shapes() {
        let mut table = RequestTable::default();
        let input = json!({"command": "touch a"});
        let record = table.open(
            approval(ProviderRequest::ClaudeTool {
                input: input.clone(),
            }),
            None,
            1,
        );
        let (outcome, allow) = record
            .reply(&Answer::Allow)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(outcome, Outcome::Allowed);
        assert_eq!(allow, json!({"behavior": "allow", "updatedInput": input}));
        let (_, deny) = record
            .reply(&Answer::Deny {
                message: Some("no".to_owned()),
            })
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(deny, json!({"behavior": "deny", "message": "no"}));

        let asked = json!({"questions": [{"question": "Which color?"}]});
        let record = table.open(
            question(ProviderRequest::ClaudeQuestion {
                input: asked,
                questions: vec!["Which color?".to_owned()],
            }),
            None,
            1,
        );
        let (outcome, reply) = record
            .reply(&Answer::Choice(2))
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(outcome, Outcome::Answered);
        assert_eq!(reply["updatedInput"]["answers"]["Which color?"], "Blue");
    }

    #[test]
    fn codex_answers_use_the_app_server_shapes() {
        let mut table = RequestTable::default();
        let command = table.open(approval(ProviderRequest::CodexCommand), None, 1);
        assert_eq!(
            command
                .reply(&Answer::Allow)
                .unwrap_or_else(|e| panic!("{e}"))
                .1,
            json!({"decision": "accept"})
        );
        assert_eq!(
            command
                .reply(&Answer::Deny { message: None })
                .unwrap_or_else(|e| panic!("{e}"))
                .1,
            json!({"decision": "decline"})
        );
        let permissions = table.open(
            approval(ProviderRequest::CodexPermissions {
                permissions: json!({"network": {"enabled": true}}),
            }),
            None,
            1,
        );
        assert_eq!(
            permissions
                .reply(&Answer::Deny { message: None })
                .unwrap_or_else(|e| panic!("{e}"))
                .1["permissions"],
            json!({})
        );
        let asked = table.open(
            question(ProviderRequest::CodexUserInput {
                question_ids: vec!["color".to_owned()],
            }),
            None,
            1,
        );
        assert_eq!(
            asked
                .reply(&Answer::Text("green".to_owned()))
                .unwrap_or_else(|e| panic!("{e}"))
                .1,
            json!({"answers": {"color": {"answers": ["green"]}}})
        );
    }

    #[test]
    fn cancel_closes_only_pending_requests_and_can_target_one() {
        let mut table = RequestTable::default();
        let first = table.open(approval(ProviderRequest::CodexCommand), None, 1);
        let mut other = approval(ProviderRequest::CodexCommand);
        other.provider_key = "p9".to_owned();
        table.open(other, None, 1);
        table.resolve(
            &first.id,
            Resolution {
                outcome: Outcome::Allowed,
                answer: Some(Answer::Allow),
            },
        );
        assert!(table.cancel(Some("p1")).is_empty(), "already resolved");
        assert_eq!(table.cancel(None).len(), 1);
        assert!(!table.has_pending());
        assert_eq!(
            table
                .get("r2")
                .and_then(|r| r.resolution.clone())
                .map(|r| r.outcome),
            Some(Outcome::Canceled)
        );
    }
}
