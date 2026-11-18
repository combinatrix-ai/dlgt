//! Requests: input that only the caller can give. A Session is `blocked`
//! while it has at least one pending Request.

use anyhow::Result;

use crate::error::fail;
use serde_json::{Map, Value, json};

/// Pending Requests published per Session in one response.
pub const PUBLISHED_REQUEST_LIMIT: usize = 16;
/// Resolved Requests kept per Session so a repeated answer can be replayed.
const RESOLVED_RETENTION: usize = 256;
/// Leave room for the rest of a `fetch` response when publishing the minimum
/// answerable Request.
pub const MINIMUM_REQUEST_LIMIT: usize = 200 * 1024;
/// Hard compact-response limit documented by the CLI and RPC contracts.
pub const REQUEST_HARD_LIMIT: usize = 256 * 1024;
/// Largest provider tool input published verbatim.
const INPUT_LIMIT: usize = 2048;
const CHOICE_LIMIT: usize = 32;
const TOOL_LIMIT: usize = 1024;
const PATH_LIMIT: usize = 4096;
const SUMMARY_LIMIT: usize = 8192;
const TEXT_LIMIT: usize = 8192;
const REASON_LIMIT: usize = 8192;
const LABEL_LIMIT: usize = 512;

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
    Json(Value),
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
                crate::error::coded(
                    crate::error::Code::InvalidArgument,
                    "invalid answer: a text answer must carry its text",
                )
            }),
            Some("choice") => params
                .get("choice")
                .and_then(Value::as_u64)
                .and_then(|choice| usize::try_from(choice).ok())
                .filter(|choice| *choice > 0)
                .map(Self::Choice)
                .ok_or_else(|| {
                    crate::error::coded(
                        crate::error::Code::InvalidArgument,
                        "invalid answer: --choice is a 1-based number",
                    )
                }),
            Some("json") => params
                .get("json")
                .filter(|value| value.is_object())
                .cloned()
                .map(Self::Json)
                .ok_or_else(|| {
                    crate::error::coded(
                        crate::error::Code::InvalidArgument,
                        "invalid answer: --json must be a JSON object",
                    )
                }),
            _ => fail!(
                InvalidArgument,
                "invalid answer: use --allow, --deny, --text, --choice, or --json"
            ),
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
    /// `schema` is the provider's `requestedSchema`, published unchanged.
    CodexElicitation {
        schema: Value,
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
    pub answer_state: AnswerState,
    pub answer_attempt: Option<Answer>,
    pub answer_attempt_id: Option<u64>,
    attempts: Vec<AnswerAttempt>,
    provider_withdrawn: bool,
    pub answer_receipt: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnswerAttemptState {
    Sending,
    Retryable,
    Delivered,
    Failed,
}

#[derive(Debug, Clone)]
struct AnswerAttempt {
    id: u64,
    state: AnswerAttemptState,
    waiters: usize,
    resolution: Option<Resolution>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnswerState {
    Pending,
    Sending,
    Delivered,
    Failed,
}

impl AnswerState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Sending => "sending",
            Self::Delivered => "delivered",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnswerClaim {
    Started(u64),
    Waiting(u64),
    Replay(Resolution),
    Conflict,
    Failed,
}

impl RequestRecord {
    pub const fn pending(&self) -> bool {
        matches!(
            self.answer_state,
            AnswerState::Pending | AnswerState::Sending
        )
    }

    pub fn public(&self) -> Value {
        let draft = &self.draft;
        let mut value = Map::new();
        value.insert("id".to_owned(), json!(self.id));
        value.insert("kind".to_owned(), json!(draft.kind.as_str()));
        value.insert("answer_state".to_owned(), json!(self.answer_state.as_str()));
        if let Some(execution_seq) = self.execution_seq {
            value.insert("execution_seq".to_owned(), json!(execution_seq));
        }
        match draft.kind {
            RequestKind::Approval => {
                value.insert(
                    "tool".to_owned(),
                    json!(display_bounded(&draft.tool, TOOL_LIMIT)),
                );
                value.insert("summary".to_owned(), json!(public_summary(draft)));
                value.insert("input".to_owned(), bounded_input(&draft.input));
                if let Some(cwd) = &draft.cwd {
                    value.insert("cwd".to_owned(), json!(display_bounded(cwd, PATH_LIMIT)));
                }
                if let Some(reason) = &draft.reason {
                    value.insert(
                        "reason".to_owned(),
                        json!(display_bounded(reason, REASON_LIMIT)),
                    );
                }
            }
            RequestKind::Question => {
                value.insert(
                    "tool".to_owned(),
                    json!(display_bounded(&draft.tool, TOOL_LIMIT)),
                );
                let text = display_bounded(draft.text.as_deref().unwrap_or_default(), TEXT_LIMIT);
                value.insert("summary".to_owned(), json!(public_summary(draft)));
                value.insert("text".to_owned(), json!(text));
                if !draft.choices.is_empty() {
                    value.insert(
                        "choices".to_owned(),
                        json!(
                            draft
                                .choices
                                .iter()
                                .take(CHOICE_LIMIT)
                                .enumerate()
                                .map(|(index, choice)| {
                                    json!({"index": index + 1, "label": bounded_label(choice)})
                                })
                                .collect::<Vec<_>>()
                        ),
                    );
                }
                if let ProviderRequest::CodexElicitation { schema, form: true } = &draft.provider {
                    value.insert("schema".to_owned(), schema.clone());
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
            | (RequestKind::Question, Answer::Text(_) | Answer::Choice(_) | Answer::Json(_)) => {}
            (RequestKind::Approval, _) => {
                fail!(
                    InvalidArgument,
                    "invalid answer: Request {} is an approval; use --allow or --deny",
                    self.id
                )
            }
            (RequestKind::Question, _) => {
                fail!(
                    InvalidArgument,
                    "invalid answer: Request {} is a question; use --text or --choice",
                    self.id
                )
            }
        }
        let allow = matches!(answer, Answer::Allow);
        let outcome = match answer {
            Answer::Allow => Outcome::Allowed,
            Answer::Deny { .. } => Outcome::Denied,
            Answer::Text(_) | Answer::Choice(_) | Answer::Json(_) => Outcome::Answered,
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
            ProviderRequest::CodexElicitation { schema, form } => {
                if !form {
                    return Ok((outcome, json!({"action": "accept"})));
                }
                let content = match answer {
                    Answer::Json(Value::Object(object)) => Value::Object(object.clone()),
                    Answer::Text(text) => match serde_json::from_str::<Value>(text) {
                        Ok(Value::Object(object)) => Value::Object(object),
                        _ => fail!(
                            InvalidArgument,
                            "invalid answer: this form requires --json with a JSON object"
                        ),
                    },
                    _ => fail!(
                        InvalidArgument,
                        "invalid answer: this form requires --json with a JSON object"
                    ),
                };
                validate_form_answer(schema, &content)?;
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
                    crate::error::coded(
                        crate::error::Code::InvalidArgument,
                        format!(
                            "invalid answer: Request {} has {} choices",
                            self.id,
                            self.draft.choices.len()
                        ),
                    )
                })
            }
            Answer::Json(_) | Answer::Allow | Answer::Deny { .. } => {
                fail!(InvalidArgument, "invalid answer for a question")
            }
        }
    }
}

fn bounded_label(value: &str) -> String {
    let value = crate::messages::one_line(value).0;
    let value = crate::messages::bounded(&value).0;
    display_bounded(&value, LABEL_LIMIT)
}

fn display_bounded(value: &str, max_bytes: usize) -> String {
    const MARKER: &str = "… [truncated]";
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let prefix_limit = max_bytes.saturating_sub(MARKER.len());
    let mut end = prefix_limit.min(value.len());
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{MARKER}", &value[..end])
}

fn public_summary(draft: &RequestDraft) -> String {
    let source = match draft.kind {
        RequestKind::Approval => &draft.summary,
        RequestKind::Question => draft.text.as_deref().unwrap_or_default(),
    };
    if !source.trim().is_empty() {
        return display_bounded(source, SUMMARY_LIMIT);
    }
    if !draft.tool.trim().is_empty() {
        return display_bounded(&draft.tool, SUMMARY_LIMIT);
    }
    "Caller input required".to_owned()
}

/// dlgt checks only the answer's shape against the provider's schema; the
/// provider validates the values.
fn validate_form_answer(schema: &Value, content: &Value) -> Result<()> {
    for field in schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if content.get(field).is_none() {
            fail!(
                InvalidArgument,
                "invalid answer: required form field {field:?} is missing"
            );
        }
    }
    if let (Some(properties), Some(object)) = (
        schema.get("properties").and_then(Value::as_object),
        content.as_object(),
    ) && let Some(unknown) = object.keys().find(|key| !properties.contains_key(*key))
    {
        fail!(
            InvalidArgument,
            "invalid answer: form has no field {unknown:?}"
        );
    }
    Ok(())
}

impl RequestDraft {
    /// Whether the exact minimum answerable Request can be published without
    /// changing its schema or exceeding the reserved response budget.
    pub fn fits_minimum_request_budget(&self) -> bool {
        let record = RequestRecord {
            id: "r999999999999".to_owned(),
            execution_seq: Some(i64::MAX),
            created_at_ms: i64::MAX,
            draft: self.clone(),
            resolution: None,
            answer_state: AnswerState::Pending,
            answer_attempt: None,
            answer_attempt_id: None,
            attempts: Vec::new(),
            provider_withdrawn: false,
            answer_receipt: None,
        };
        serde_json::to_vec(&record.public()).is_ok_and(|encoded| {
            encoded.len() <= MINIMUM_REQUEST_LIMIT && encoded.len() <= REQUEST_HARD_LIMIT
        })
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

/// The integration regressions shrink retention to reach trimming without
/// hundreds of Requests. Release builds always keep the full limit.
fn resolved_retention() -> usize {
    if cfg!(debug_assertions)
        && let Some(limit) = std::env::var("DLGT_TEST_RESOLVED_RETENTION")
            .ok()
            .and_then(|value| value.parse().ok())
    {
        return limit;
    }
    RESOLVED_RETENTION
}

#[derive(Debug, Default)]
pub struct RequestTable {
    records: Vec<RequestRecord>,
    issued: u64,
    issued_attempts: u64,
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
            answer_state: AnswerState::Pending,
            answer_attempt: None,
            answer_attempt_id: None,
            attempts: Vec::new(),
            provider_withdrawn: false,
            answer_receipt: None,
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

    pub fn deliver_with_receipt(
        &mut self,
        id: &str,
        attempt_id: u64,
        answer: &Answer,
        resolution: Resolution,
        receipt: Value,
    ) -> Option<RequestRecord> {
        let record = self.records.iter_mut().find(|record| {
            record.id == id
                && record.answer_state == AnswerState::Sending
                && record.answer_attempt_id == Some(attempt_id)
                && record.answer_attempt.as_ref() == Some(answer)
        })?;
        let before = record.clone();
        record.resolution = Some(resolution.clone());
        record.answer_state = AnswerState::Delivered;
        record.answer_receipt = Some(receipt);
        if let Some(attempt) = record
            .attempts
            .iter_mut()
            .find(|attempt| attempt.id == attempt_id)
        {
            attempt.state = AnswerAttemptState::Delivered;
            attempt.resolution = Some(resolution);
        }
        self.trim();
        Some(before)
    }

    pub fn begin_answer(&mut self, id: &str, answer: &Answer) -> Option<AnswerClaim> {
        let record = self.records.iter_mut().find(|record| record.id == id)?;
        Some(match record.answer_state {
            AnswerState::Pending => {
                self.issued_attempts = self.issued_attempts.saturating_add(1);
                let attempt_id = self.issued_attempts;
                record.answer_state = AnswerState::Sending;
                record.answer_attempt = Some(answer.clone());
                record.answer_attempt_id = Some(attempt_id);
                record.attempts.push(AnswerAttempt {
                    id: attempt_id,
                    state: AnswerAttemptState::Sending,
                    waiters: 0,
                    resolution: None,
                });
                AnswerClaim::Started(attempt_id)
            }
            AnswerState::Sending if record.answer_attempt.as_ref() == Some(answer) => {
                let Some(attempt_id) = record.answer_attempt_id else {
                    return Some(AnswerClaim::Failed);
                };
                if let Some(attempt) = record.attempts.iter_mut().find(|a| a.id == attempt_id) {
                    attempt.waiters = attempt.waiters.saturating_add(1);
                }
                AnswerClaim::Waiting(attempt_id)
            }
            AnswerState::Delivered => record
                .resolution
                .as_ref()
                .filter(|resolution| resolution.answer.as_ref() == Some(answer))
                .cloned()
                .map_or(AnswerClaim::Conflict, AnswerClaim::Replay),
            AnswerState::Failed => AnswerClaim::Failed,
            AnswerState::Sending => AnswerClaim::Conflict,
        })
    }

    pub fn retry_answer(&mut self, id: &str, attempt_id: u64, answer: &Answer) -> bool {
        let Some(record) = self.records.iter_mut().find(|record| {
            record.id == id
                && record.answer_state == AnswerState::Sending
                && record.answer_attempt_id == Some(attempt_id)
                && record.answer_attempt.as_ref() == Some(answer)
        }) else {
            return false;
        };
        if let Some(attempt) = record.attempts.iter_mut().find(|a| a.id == attempt_id) {
            attempt.state = AnswerAttemptState::Retryable;
        }
        if record.provider_withdrawn {
            record.resolution = Some(Resolution {
                outcome: Outcome::Canceled,
                answer: None,
            });
            record.answer_state = AnswerState::Failed;
            record.answer_attempt = None;
        } else {
            record.answer_state = AnswerState::Pending;
        }
        true
    }

    pub fn fail_answer(&mut self, id: &str, attempt_id: u64, answer: &Answer) -> bool {
        let Some(record) = self.records.iter_mut().find(|record| {
            record.id == id
                && record.answer_state == AnswerState::Sending
                && record.answer_attempt_id == Some(attempt_id)
                && record.answer_attempt.as_ref() == Some(answer)
        }) else {
            return false;
        };
        record.answer_state = AnswerState::Failed;
        if let Some(attempt) = record.attempts.iter_mut().find(|a| a.id == attempt_id) {
            attempt.state = AnswerAttemptState::Failed;
        }
        true
    }

    pub fn fail_withdrawn_answer(&mut self, id: &str, attempt_id: u64, answer: &Answer) -> bool {
        let Some(record) = self.records.iter_mut().find(|record| {
            record.id == id
                && record.answer_state == AnswerState::Sending
                && record.answer_attempt_id == Some(attempt_id)
                && record.answer_attempt.as_ref() == Some(answer)
                && record.provider_withdrawn
        }) else {
            return false;
        };
        record.answer_state = AnswerState::Failed;
        record.resolution = Some(Resolution {
            outcome: Outcome::Canceled,
            answer: None,
        });
        if let Some(attempt) = record
            .attempts
            .iter_mut()
            .find(|attempt| attempt.id == attempt_id)
        {
            attempt.state = AnswerAttemptState::Failed;
        }
        true
    }

    pub fn answer_attempt_status(
        &self,
        id: &str,
        attempt_id: u64,
    ) -> Option<(AnswerAttemptState, Option<Resolution>)> {
        let record = self.records.iter().find(|record| record.id == id)?;
        let attempt = record
            .attempts
            .iter()
            .find(|attempt| attempt.id == attempt_id)?;
        Some((attempt.state, attempt.resolution.clone()))
    }

    pub fn release_answer_waiter(&mut self, id: &str, attempt_id: u64) {
        if let Some(record) = self.records.iter_mut().find(|record| record.id == id)
            && let Some(attempt) = record
                .attempts
                .iter_mut()
                .find(|attempt| attempt.id == attempt_id)
        {
            attempt.waiters = attempt.waiters.saturating_sub(1);
        }
        self.trim();
    }

    /// Close pending Requests as canceled: every one, or only the one the
    /// provider named.
    pub fn cancel(&mut self, provider_key: Option<&str>) -> Vec<RequestRecord> {
        let mut closed = Vec::new();
        for record in &mut self.records {
            if !record.pending() || provider_key.is_some_and(|key| record.draft.provider_key != key)
            {
                continue;
            }
            if record.answer_state == AnswerState::Sending {
                record.provider_withdrawn = true;
            } else {
                closed.push(record.clone());
                record.resolution = Some(Resolution {
                    outcome: Outcome::Canceled,
                    answer: None,
                });
                record.answer_state = AnswerState::Failed;
                record.answer_attempt = None;
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
        let mut excess = resolved.saturating_sub(resolved_retention());
        self.records.retain(|record| {
            // A duplicate still waiting on an attempt reads its outcome here.
            let awaited = record.attempts.iter().any(|attempt| attempt.waiters > 0);
            if excess > 0 && !record.pending() && !awaited {
                excess -= 1;
                return false;
            }
            true
        });
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{
        Answer, AnswerAttemptState, AnswerClaim, AnswerState, Outcome, ProviderRequest,
        RequestDraft, RequestKind, RequestRecord, RequestTable, Resolution,
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
        assert_eq!(public["summary"], "Which color?");
        assert_eq!(public["tool"], "AskUserQuestion");
        assert_eq!(
            public["choices"],
            json!([
                {"index": 1, "label": "Red"},
                {"index": 2, "label": "Blue"},
            ])
        );
    }

    #[test]
    fn answerable_requests_always_have_a_short_public_summary() {
        let mut draft = question(ProviderRequest::CodexElicitation {
            schema: Value::Null,
            form: true,
        });
        draft.text = Some(String::new());
        let record = RequestTable::default().open(draft, Some(1), 1);
        assert_eq!(record.public()["summary"], "AskUserQuestion");
        assert_eq!(record.public()["text"], "");
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
    fn form_schemas_are_published_verbatim_and_answers_checked_against_them() {
        let long_name = "required_".to_owned() + &"n".repeat(180);
        let mut properties = serde_json::Map::new();
        for index in 0..17 {
            properties.insert(format!("field_{index}"), json!({"type": "string"}));
        }
        properties.insert(long_name.clone(), json!({"type": "string"}));
        properties.insert(
            "email".to_owned(),
            json!({"type": "string", "format": "email"}),
        );
        properties.insert(
            "plan".to_owned(),
            json!({"type": "string", "oneOf": [
                {"const": "opaque-a17", "title": "Small"},
                {"const": "opaque-b93", "title": "Large"}
            ]}),
        );
        properties.insert(
            "tags".to_owned(),
            json!({"type": "array", "items": {"type": "string", "enum": ["x", "y"]}}),
        );
        let mut required = (0..17)
            .map(|index| format!("field_{index}"))
            .collect::<Vec<_>>();
        required.extend([long_name.clone(), "plan".to_owned()]);
        let schema = json!({"type": "object", "properties": properties, "required": required});
        let mut draft = question(ProviderRequest::CodexElicitation {
            schema: schema.clone(),
            form: true,
        });
        draft.choices.clear();
        let record = RequestTable::default().open(draft, Some(4), 1);

        assert_eq!(record.public()["schema"], schema);

        let mut answer = (0..17)
            .map(|index| (format!("field_{index}"), json!("v")))
            .collect::<serde_json::Map<_, _>>();
        answer.insert(long_name.clone(), json!("v"));
        assert!(
            record
                .reply(&Answer::Json(Value::Object(answer.clone())))
                .is_err()
        );
        answer.insert("plan".to_owned(), Value::Null);
        let (_, reply) = record
            .reply(&Answer::Json(Value::Object(answer.clone())))
            .unwrap_or_else(|error| panic!("a present null value was rejected: {error}"));
        assert_eq!(reply["content"]["plan"], Value::Null);
        answer.insert("field_0".to_owned(), json!(true));
        answer.insert("plan".to_owned(), json!("opaque-b93"));
        let (outcome, reply) = record
            .reply(&Answer::Json(Value::Object(answer.clone())))
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(outcome, Outcome::Answered);
        assert_eq!(
            reply,
            json!({"action": "accept", "content": Value::Object(answer.clone())}),
            "the answer must reach the provider exactly as submitted"
        );
        answer.remove("field_16");
        assert!(
            record
                .reply(&Answer::Json(Value::Object(answer.clone())))
                .is_err()
        );
        answer.insert("field_16".to_owned(), json!("v"));
        answer.insert("not_in_form".to_owned(), json!("v"));
        assert!(record.reply(&Answer::Json(Value::Object(answer))).is_err());
    }

    #[test]
    fn delivered_answers_are_trimmed_to_the_retention_limit_except_awaited_ones() {
        let mut table = RequestTable::default();
        let mut awaited = Vec::new();
        for index in 0..300 {
            let record = table.open(approval(ProviderRequest::CodexCommand), Some(1), 1);
            let Some(AnswerClaim::Started(attempt)) =
                table.begin_answer(&record.id, &Answer::Allow)
            else {
                panic!("answer was not started");
            };
            if index < 260 {
                assert_eq!(
                    table.begin_answer(&record.id, &Answer::Allow),
                    Some(AnswerClaim::Waiting(attempt))
                );
                awaited.push((record.id.clone(), attempt));
            }
            let resolution = Resolution {
                outcome: Outcome::Allowed,
                answer: Some(Answer::Allow),
            };
            assert!(
                table
                    .deliver_with_receipt(
                        &record.id,
                        attempt,
                        &Answer::Allow,
                        resolution,
                        json!({})
                    )
                    .is_some()
            );
        }
        assert!(
            awaited.iter().all(|(id, _)| table.get(id).is_some()),
            "an awaited attempt was trimmed"
        );
        assert_eq!(table.records.len(), 260);
        for (id, attempt) in &awaited {
            table.release_answer_waiter(id, *attempt);
        }
        assert_eq!(table.records.len(), super::RESOLVED_RETENTION);
    }

    #[test]
    fn every_public_request_fits_the_hard_budget_with_huge_tool_and_cwd() {
        let mut draft = approval(ProviderRequest::CodexCommand);
        draft.tool = "T".repeat(300_000);
        draft.cwd = Some("/".to_owned() + &"c".repeat(300_000));
        draft.summary = "S".repeat(300_000);
        draft.reason = Some("R".repeat(300_000));
        draft.input = json!({"command": "C".repeat(300_000)});
        let record = RequestTable::default().open(draft, Some(1), 1);
        let public = record.public();
        let encoded = serde_json::to_vec(&public).unwrap_or_default();

        assert!(
            encoded.len() <= super::REQUEST_HARD_LIMIT,
            "{} bytes",
            encoded.len()
        );
        for field in ["tool", "cwd", "summary", "reason"] {
            assert!(
                public[field]
                    .as_str()
                    .is_some_and(|value| value.len() < 300_000)
            );
            assert!(
                public[field].to_string().contains("truncated"),
                "{field} lacks a marker"
            );
        }
    }

    #[test]
    fn ambiguous_answer_delivery_does_not_make_the_request_retryable() {
        let answer = Answer::Allow;
        let mut table = RequestTable::default();
        table.open(approval(ProviderRequest::CodexCommand), None, 1);
        let Some(AnswerClaim::Started(attempt_id)) = table.begin_answer("r1", &answer) else {
            panic!("answer attempt did not start");
        };
        assert!(table.fail_answer("r1", attempt_id, &answer));
        assert!(!table.retry_answer("r1", attempt_id, &answer));
        assert_eq!(
            table.get("r1").map(|record| record.answer_state),
            Some(AnswerState::Failed)
        );
    }

    #[test]
    fn a_failed_stale_answer_only_closes_a_request_withdrawn_by_restart() {
        let answer = Answer::Allow;
        let mut table = RequestTable::default();
        table.open(approval(ProviderRequest::CodexCommand), None, 1);
        let Some(AnswerClaim::Started(attempt_id)) = table.begin_answer("r1", &answer) else {
            panic!("answer attempt did not start");
        };

        assert!(table.cancel(None).is_empty());
        assert!(table.fail_withdrawn_answer("r1", attempt_id, &answer));
        assert_eq!(
            table.get("r1").map(|record| record.answer_state),
            Some(AnswerState::Failed)
        );
        assert_eq!(
            table.answer_attempt_status("r1", attempt_id),
            Some((AnswerAttemptState::Failed, None))
        );
        assert_eq!(table.begin_answer("r1", &answer), Some(AnswerClaim::Failed));
    }

    #[test]
    fn delivered_answer_receipt_survives_provider_resolution_race() {
        let answer = Answer::Allow;
        let mut table = RequestTable::default();
        table.open(approval(ProviderRequest::CodexCommand), None, 1);
        let Some(AnswerClaim::Started(attempt_id)) = table.begin_answer("r1", &answer) else {
            panic!("answer attempt did not start");
        };

        assert!(table.cancel(Some("p1")).is_empty());
        assert!(
            table
                .deliver_with_receipt(
                    "r1",
                    attempt_id,
                    &answer,
                    Resolution {
                        outcome: Outcome::Allowed,
                        answer: Some(answer.clone())
                    },
                    json!({"delivered": true}),
                )
                .is_some()
        );
        let record = table
            .get("r1")
            .unwrap_or_else(|| panic!("request was removed"));
        assert_eq!(record.answer_state, AnswerState::Delivered);
        assert_eq!(record.answer_receipt, Some(json!({"delivered": true})));
    }

    #[test]
    fn json_answer_parsing_requires_an_object() {
        assert_eq!(
            Answer::parse(&json!({"decision": "json", "json": {"x": 1}}))
                .unwrap_or_else(|error| panic!("{error}")),
            Answer::Json(json!({"x": 1}))
        );
        assert!(Answer::parse(&json!({"decision": "json", "json": []})).is_err());
    }

    #[test]
    fn an_answer_is_replayable_only_after_provider_delivery() {
        let answer = Answer::Deny {
            message: Some("reason".to_owned()),
        };
        let mut table = RequestTable::default();
        table.open(approval(ProviderRequest::CodexCommand), None, 1);

        let Some(AnswerClaim::Started(first_attempt)) = table.begin_answer("r1", &answer) else {
            panic!("first attempt did not start");
        };
        assert_eq!(
            table.begin_answer("r1", &answer),
            Some(AnswerClaim::Waiting(first_attempt))
        );
        assert_eq!(
            table.begin_answer("r1", &Answer::Allow),
            Some(AnswerClaim::Conflict)
        );
        assert!(table.get("r1").is_some_and(RequestRecord::pending));

        assert!(table.retry_answer("r1", first_attempt, &answer));
        assert_eq!(
            table.answer_attempt_status("r1", first_attempt),
            Some((super::AnswerAttemptState::Retryable, None))
        );
        let Some(AnswerClaim::Started(second_attempt)) = table.begin_answer("r1", &answer) else {
            panic!("second attempt did not start");
        };
        assert_ne!(first_attempt, second_attempt);
        assert_eq!(
            table.begin_answer("r1", &answer),
            Some(AnswerClaim::Waiting(second_attempt))
        );

        let resolution = Resolution {
            outcome: Outcome::Denied,
            answer: Some(answer.clone()),
        };
        assert!(
            table
                .deliver_with_receipt(
                    "r1",
                    second_attempt,
                    &answer,
                    resolution.clone(),
                    json!({"delivered": true})
                )
                .is_some()
        );
        assert_eq!(
            table.begin_answer("r1", &answer),
            Some(AnswerClaim::Replay(resolution))
        );
        assert!(table.get("r1").is_some_and(|record| !record.pending()));
    }

    #[test]
    fn cancel_closes_only_pending_requests_and_can_target_one() {
        let mut table = RequestTable::default();
        let first = table.open(approval(ProviderRequest::CodexCommand), None, 1);
        let answer = Answer::Allow;
        let Some(AnswerClaim::Started(attempt_id)) = table.begin_answer(&first.id, &answer) else {
            panic!("first Request answer did not start");
        };
        assert!(
            table
                .deliver_with_receipt(
                    &first.id,
                    attempt_id,
                    &answer,
                    Resolution {
                        outcome: Outcome::Allowed,
                        answer: Some(answer.clone()),
                    },
                    json!({"delivered": true}),
                )
                .is_some()
        );
        let mut other = approval(ProviderRequest::CodexCommand);
        other.provider_key = "p9".to_owned();
        table.open(other, None, 1);
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
