//! Codex lifecycle: how app-server notifications and server requests become
//! Session state, messages, and Requests.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::claude::{brief, input_summary};
use crate::codex::final_agent_message_text;
use crate::messages::{Decider, Decision, MessageBody};
use crate::protocol::{SessionRecord, SessionState, TurnState};
use crate::requests::{ProviderRequest, RequestDraft, RequestKind};
use crate::store::Store;

/// What dlgt remembers about thread items, so a later approval request or
/// completion can say which tool it is about.
#[derive(Debug, Default)]
pub struct CodexItems {
    summaries: HashMap<String, (String, String)>,
}

impl CodexItems {
    /// The message one item notification stands for, if any.
    fn message(&mut self, completed: bool, item: &Value) -> Option<MessageBody> {
        let text = |name: &str| item.get(name).and_then(Value::as_str);
        let id = text("id")?;
        let kind = text("type")?;
        if kind == "agentMessage" {
            return completed
                .then(|| text("text"))
                .flatten()
                .filter(|text| !text.trim().is_empty())
                .map(MessageBody::assistant);
        }
        let (tool, summary) = match kind {
            "commandExecution" => ("exec".to_owned(), text("command")?.to_owned()),
            "fileChange" => (
                "apply_patch".to_owned(),
                item.get("changes")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|change| change.get("path").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            "mcpToolCall" => (
                format!(
                    "{}.{}",
                    text("server").unwrap_or("mcp"),
                    text("tool").unwrap_or("tool")
                ),
                item.get("arguments").map(input_summary).unwrap_or_default(),
            ),
            "dynamicToolCall" => (
                text("tool").unwrap_or("tool").to_owned(),
                item.get("arguments").map(input_summary).unwrap_or_default(),
            ),
            "webSearch" => (
                "web_search".to_owned(),
                text("query").unwrap_or_default().to_owned(),
            ),
            "collabAgentToolCall" => (
                text("tool").unwrap_or("agent").to_owned(),
                text("prompt").unwrap_or_default().to_owned(),
            ),
            _ => return None,
        };
        if !completed {
            self.summaries
                .insert(id.to_owned(), (tool.clone(), summary.clone()));
            return Some(MessageBody::tool_call(&tool, &summary));
        }
        self.summaries.remove(id);
        let status = text("status").unwrap_or("completed");
        let exit_code = item.get("exitCode").and_then(Value::as_i64);
        let ok = !matches!(status, "failed" | "declined")
            && exit_code.is_none_or(|code| code == 0)
            && item.get("error").is_none_or(Value::is_null)
            && item.get("success").and_then(Value::as_bool) != Some(false);
        let output = brief(text("aggregatedOutput").unwrap_or_default());
        let outcome = if !output.is_empty() {
            output
        } else if let Some(code) = exit_code {
            format!("exit {code}")
        } else {
            status.to_owned()
        };
        Some(MessageBody::tool_result(&tool, ok, &outcome))
    }

    /// The Request one server request stands for.
    #[allow(clippy::too_many_lines)]
    fn request(&self, session_cwd: &str, request: &Value) -> Option<RequestDraft> {
        let method = request.get("method").and_then(Value::as_str)?;
        let provider_key = request.get("id")?.to_string();
        let params = request.get("params").unwrap_or(&Value::Null);
        let text = |name: &str| params.get(name).and_then(Value::as_str);
        let item = text("itemId").and_then(|id| self.summaries.get(id));
        let approval = |provider, tool: &str, summary: String, input: Value| RequestDraft {
            kind: RequestKind::Approval,
            provider_key: provider_key.clone(),
            provider,
            tool: tool.to_owned(),
            summary,
            input,
            cwd: Some(text("cwd").unwrap_or(session_cwd).to_owned()),
            reason: text("reason").map(str::to_owned),
            text: None,
            choices: Vec::new(),
        };
        let question = |provider, tool: &str, asked: String, choices: Vec<String>| RequestDraft {
            kind: RequestKind::Question,
            provider_key: provider_key.clone(),
            provider,
            tool: tool.to_owned(),
            summary: String::new(),
            input: Value::Null,
            cwd: None,
            reason: None,
            text: Some(asked),
            choices,
        };
        Some(match method {
            "item/commandExecution/requestApproval" => {
                let command = text("command")
                    .map(str::to_owned)
                    .or_else(|| item.map(|(_, summary)| summary.clone()))
                    .unwrap_or_default();
                approval(
                    ProviderRequest::CodexCommand,
                    "exec",
                    command.clone(),
                    json!({"command": command}),
                )
            }
            "item/fileChange/requestApproval" => {
                let files = item.map(|(_, summary)| summary.clone()).unwrap_or_default();
                approval(
                    ProviderRequest::CodexFileChange,
                    "apply_patch",
                    files.clone(),
                    json!({"files": files, "grant_root": params.get("grantRoot")}),
                )
            }
            "item/permissions/requestApproval" => {
                let permissions = params.get("permissions").cloned().unwrap_or(Value::Null);
                approval(
                    ProviderRequest::CodexPermissions {
                        permissions: permissions.clone(),
                    },
                    "permissions",
                    permissions.to_string(),
                    permissions,
                )
            }
            "item/tool/requestUserInput" => {
                let questions = params
                    .get("questions")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let field = |question: &Value, name: &str| {
                    question
                        .get(name)
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned()
                };
                let choices = if let [question] = questions.as_slice() {
                    question
                        .get("options")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .map(|option| field(option, "label"))
                        .collect()
                } else {
                    Vec::new()
                };
                question(
                    ProviderRequest::CodexUserInput {
                        question_ids: questions.iter().map(|q| field(q, "id")).collect(),
                    },
                    "request_user_input",
                    questions
                        .iter()
                        .map(|q| field(q, "question"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    choices,
                )
            }
            "mcpServer/elicitation/request" => {
                let form = text("mode") != Some("url");
                let mut asked = text("message").unwrap_or_default().to_owned();
                if let Some(url) = text("url") {
                    asked = format!("{asked}\n{url}");
                }
                question(
                    ProviderRequest::CodexElicitation {
                        schema: params
                            .get("requestedSchema")
                            .cloned()
                            .unwrap_or(Value::Null),
                        form,
                    },
                    text("serverName").unwrap_or("mcp"),
                    asked,
                    Vec::new(),
                )
            }
            _ => return None,
        })
    }
}

fn review_action(action: &Value) -> (&'static str, String) {
    let text = |name: &str| action.get(name).and_then(Value::as_str).unwrap_or_default();
    let joined = |name: &str| {
        action
            .get(name)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" ")
    };
    match action.get("type").and_then(Value::as_str) {
        Some("command") => ("exec", text("command").to_owned()),
        Some("execve") => ("exec", joined("argv")),
        Some("writeStdin") => ("exec", text("stdin").to_owned()),
        Some("applyPatch") => ("apply_patch", joined("files")),
        _ => ("tool", action.to_string()),
    }
}

fn notice(store: &Store, session_id: &str, text: Option<&Value>) {
    if let Some(text) = text.and_then(Value::as_str).filter(|text| !text.is_empty()) {
        store.append_message(session_id, MessageBody::notice(text));
    }
}

/// Apply one Codex app-server notification to the Session.
#[allow(clippy::too_many_lines)]
pub fn apply_notification(
    store: &mut Store,
    session_id: &str,
    items: &mut CodexItems,
    message: &Value,
) -> Result<bool> {
    let method = message
        .get("method")
        .and_then(Value::as_str)
        .context("Codex notification has no method")?;
    let params = message.get("params").unwrap_or(&Value::Null);
    let session = store
        .get_session(session_id)
        .context("session disappeared while handling Codex notification")?;
    store.touch_provider(session_id);
    match method {
        "thread/started" => {
            let _thread_id = params
                .pointer("/thread/id")
                .and_then(Value::as_str)
                .context("Codex thread/started had no thread id")?;
            store.record_event(Some(session_id), None, "provider.thread_started");
        }
        "turn/started" => {
            if !codex_thread_matches(&session, params)? {
                return Ok(true);
            }
            let provider_turn_id = params
                .pointer("/turn/id")
                .and_then(Value::as_str)
                .context("Codex turn/started had no turn id")?;
            let current = store
                .get_session(session_id)
                .context("session disappeared while starting Codex turn")?;
            let turn_id = if let Some(turn_id) = current.active_turn_id {
                turn_id
            } else {
                let turn_id = format!("turn_{}", Uuid::new_v4().simple());
                let prompt = codex_turn_prompt(params);
                store.insert_turn(&turn_id, session_id, &prompt)?;
                store.record_event(Some(session_id), Some(&turn_id), "turn.submitted");
                turn_id
            };
            if store.mark_turn_started(&turn_id, Some(provider_turn_id)) {
                if store
                    .get_turn(&turn_id)
                    .is_some_and(|turn| turn.state != TurnState::Canceled)
                {
                    store.set_session_state(session_id, SessionState::Busy);
                }
                store.record_event(Some(session_id), Some(&turn_id), "turn.started");
            }
        }
        "error" => {
            if !codex_thread_matches(&session, params)? {
                return Ok(true);
            }
            let provider_turn_id = params.get("turnId").and_then(Value::as_str);
            let turn_id = local_turn_for_provider(store, session_id, provider_turn_id)?;
            let will_retry = params
                .get("willRetry")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if will_retry {
                let attempt = params.get("attempt").and_then(Value::as_u64).unwrap_or(1);
                store.record_provider_retry_event(Some(session_id), turn_id.as_deref(), attempt);
            } else {
                store.record_event(Some(session_id), turn_id.as_deref(), "provider.error");
                notice(store, session_id, params.pointer("/error/message"));
            }
        }
        "turn/completed" => {
            if !codex_thread_matches(&session, params)? {
                return Ok(true);
            }
            let provider_turn_id = params
                .pointer("/turn/id")
                .and_then(Value::as_str)
                .context("Codex turn/completed had no turn id")?;
            let status = params
                .pointer("/turn/status")
                .and_then(Value::as_str)
                .context("Codex turn/completed had no status")?;
            let turn_state = match status {
                "completed" => TurnState::Completed,
                "failed" => TurnState::Failed,
                "interrupted" => TurnState::Interrupted,
                _ => bail!("invalid Codex terminal turn status {status:?}"),
            };
            let Some(turn_id) = local_turn_for_provider(store, session_id, Some(provider_turn_id))?
            else {
                store.record_event(Some(session_id), None, "provider.completion_unmatched");
                return Ok(true);
            };
            let final_message = codex_final_message(params);
            let error = (turn_state != TurnState::Completed)
                .then(|| sanitize_codex_error(params.pointer("/turn/error")));
            let completed = store.finish_turn_if_matching(
                &turn_id,
                Some(provider_turn_id),
                turn_state,
                final_message.as_deref(),
                error.as_ref(),
            )?;
            if completed {
                store.set_session_state(session_id, SessionState::Idle);
            }
            let quiesced =
                !completed && store.settle_canceled_turn(&turn_id, Some(provider_turn_id))?;
            let event_kind = if completed {
                match turn_state {
                    TurnState::Completed => "turn.completed",
                    TurnState::Failed => "turn.failed",
                    TurnState::Interrupted => "turn.interrupted",
                    _ => unreachable!(),
                }
            } else if quiesced {
                "provider.quiesced"
            } else {
                "provider.completion_unmatched"
            };
            store.record_event(Some(session_id), Some(&turn_id), event_kind);
        }
        "item/started" | "item/completed" => {
            if codex_thread_matches(&session, params)?
                && let Some(item) = params.get("item")
                && let Some(body) = items.message(method == "item/completed", item)
            {
                store.append_message(session_id, body);
            }
        }
        "item/autoApprovalReview/completed" => {
            if !codex_thread_matches(&session, params)? {
                return Ok(true);
            }
            let decision = match params.pointer("/review/status").and_then(Value::as_str) {
                Some("approved") => Decision::Approved,
                Some("denied") => Decision::Denied,
                Some("timedOut") => Decision::TimedOut,
                Some("aborted") => Decision::Aborted,
                _ => return Ok(true),
            };
            let action = params.get("action").unwrap_or(&Value::Null);
            let (tool, summary) = review_action(action);
            store.append_message(
                session_id,
                MessageBody::approval(
                    decision,
                    Decider::ProviderReviewer,
                    params.pointer("/review/riskLevel").and_then(Value::as_str),
                    None,
                    tool,
                    &summary,
                ),
            );
        }
        "dlgt/server/request" => {
            let Some(draft) = items.request(&session.cwd, params) else {
                return Ok(false);
            };
            if !codex_server_request_matches(store, &session, params) {
                return Ok(false);
            }
            if !draft.fits_minimum_request_budget() {
                store.append_message(
                    session_id,
                    MessageBody::notice(
                        "Codex form was declined because its exact schema exceeds dlgt's Request size limit",
                    ),
                );
                return Ok(false);
            }
            return Ok(store.open_request(session_id, draft).is_some());
        }
        "serverRequest/resolved" => {
            if let Some(id) = params.get("requestId") {
                store.cancel_requests(session_id, Some(&id.to_string()));
            }
        }
        "dlgt/server/unsupported" => {
            let method = params.get("method").and_then(Value::as_str).unwrap_or("?");
            store.append_message(
                session_id,
                MessageBody::notice(&format!(
                    "Codex sent a server request dlgt cannot answer ({method}); it was refused"
                )),
            );
        }
        "warning" | "guardianWarning" | "configWarning" => {
            notice(
                store,
                session_id,
                params.get("message").or_else(|| params.get("summary")),
            );
        }
        "dlgt/protocol/error" => {
            store.record_event(
                Some(session_id),
                session.active_turn_id.as_deref(),
                "provider.protocol_error",
            );
        }
        _ => {}
    }
    Ok(true)
}

fn codex_server_request_matches(store: &Store, session: &SessionRecord, params: &Value) -> bool {
    let Some(provider_turn_id) = params
        .get("params")
        .and_then(|params| params.get("turnId"))
        .and_then(Value::as_str)
    else {
        return false;
    };
    let Some(inner) = params.get("params") else {
        return false;
    };
    if !codex_thread_matches(session, inner).is_ok_and(|matches| matches)
        || !matches!(session.state, SessionState::Busy | SessionState::Blocked)
    {
        return false;
    }
    let Some(turn_id) = session.active_turn_id.as_deref() else {
        return false;
    };
    store.get_turn(turn_id).is_some_and(|turn| {
        turn.state == TurnState::Running
            && turn.provider_turn_id.as_deref() == Some(provider_turn_id)
    })
}

fn codex_thread_matches(session: &SessionRecord, params: &Value) -> Result<bool> {
    let thread_id = params
        .get("threadId")
        .and_then(Value::as_str)
        .context("Codex notification had no threadId")?;
    Ok(session
        .id
        .strip_prefix("codex:")
        .is_some_and(|provider_id| provider_id == thread_id))
}

fn local_turn_for_provider(
    store: &Store,
    session_id: &str,
    provider_turn_id: Option<&str>,
) -> Result<Option<String>> {
    let Some(turn_id) = store
        .get_session(session_id)
        .and_then(|value| value.active_turn_id)
    else {
        return Ok(None);
    };
    let turn = store.get_turn(&turn_id).context("active turn not found")?;
    if provider_turn_id.is_some()
        && turn.provider_turn_id.is_some()
        && turn.provider_turn_id.as_deref() != provider_turn_id
    {
        return Ok(None);
    }
    Ok(Some(turn_id))
}

fn codex_turn_prompt(params: &Value) -> String {
    params
        .pointer("/turn/items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|item| item.get("type").and_then(Value::as_str) == Some("userMessage"))
        .and_then(|item| item.get("content"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|input| input.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|input| input.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

fn codex_final_message(params: &Value) -> Option<String> {
    params
        .get("turn")
        .and_then(final_agent_message_text)
        .map(str::to_owned)
}

fn sanitize_codex_error(error: Option<&Value>) -> Value {
    let Some(error) = error.filter(|value| !value.is_null()) else {
        return Value::Null;
    };
    json!({
        "code": error.get("codexErrorInfo").and_then(Value::as_str).unwrap_or("provider_error"),
        "message": sanitize_message(error.get("message").and_then(Value::as_str).unwrap_or("Codex turn failed")),
    })
}

fn sanitize_message(message: &str) -> String {
    message.chars().take(4_096).collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{CodexItems, apply_notification};
    use crate::protocol::{ApprovalsMode, SessionState, TurnState};
    use crate::store::{NewSession, Store};

    fn ready_store(_agent: &str) -> Store {
        let store = Store::new();
        store
            .insert_session(&NewSession {
                id: "codex:thread-1",
                alias: "@worker",
                title: "worker",
                agent: "codex",
                cwd: "/tmp",
                model: None,
                effort: None,
                harness_options: &[],
                approvals: ApprovalsMode::Ask,
            })
            .unwrap_or_else(|error| panic!("failed to insert session: {error}"));
        assert!(store.set_session_running("codex:thread-1", Some(42)));
        assert!(store.set_session_state("codex:thread-1", SessionState::Idle));
        store
    }

    fn apply_codex_notification(
        store: &mut Store,
        session_id: &str,
        message: &serde_json::Value,
    ) -> anyhow::Result<()> {
        apply_notification(store, session_id, &mut CodexItems::default(), message).map(|_| ())
    }

    fn busy_store() -> (Store, CodexItems) {
        let mut store = ready_store("codex");
        store
            .insert_turn("turn_1", "codex:thread-1", "hello")
            .unwrap_or_else(|error| panic!("failed to insert turn: {error}"));
        let mut items = CodexItems::default();
        apply_notification(
            &mut store,
            "codex:thread-1",
            &mut items,
            &json!({
                "method": "turn/started",
                "params": {"threadId": "thread-1", "turn": {"id": "provider-turn", "items": []}},
            }),
        )
        .unwrap_or_else(|error| panic!("failed to start Codex turn: {error}"));
        (store, items)
    }

    fn messages(store: &Store) -> Vec<serde_json::Value> {
        let uid = store
            .session_uid("codex:thread-1")
            .unwrap_or_else(|| panic!("session uid missing"));
        store
            .messages_after(&uid, 0, 64)
            .messages
            .iter()
            .map(|message| serde_json::to_value(message).unwrap_or_default())
            .collect()
    }

    fn state(store: &Store) -> Option<SessionState> {
        store
            .get_session("codex:thread-1")
            .map(|session| session.state)
    }

    #[test]
    fn item_notifications_become_ordered_messages() {
        let (mut store, mut items) = busy_store();
        for (method, item) in [
            (
                "item/started",
                json!({"type": "commandExecution", "id": "i1", "command": "cargo test"}),
            ),
            (
                "item/completed",
                json!({"type": "commandExecution", "id": "i1", "command": "cargo test",
                       "status": "completed", "exitCode": 0, "aggregatedOutput": "42 passed\n"}),
            ),
            (
                "item/completed",
                json!({"type": "reasoning", "id": "i2", "summary": []}),
            ),
            (
                "item/completed",
                json!({"type": "agentMessage", "id": "i3", "text": "Found one race."}),
            ),
        ] {
            apply_notification(
                &mut store,
                "codex:thread-1",
                &mut items,
                &json!({"method": method, "params": {"threadId": "thread-1", "item": item}}),
            )
            .unwrap_or_else(|error| panic!("failed to apply item: {error}"));
        }
        let messages = messages(&store);
        assert_eq!(messages.len(), 3, "reasoning is not a message");
        assert_eq!(messages[0]["kind"], "tool_call");
        assert_eq!(messages[0]["tool"], "exec");
        assert_eq!(messages[0]["summary"], "cargo test");
        assert_eq!(messages[1]["kind"], "tool_result");
        assert_eq!(messages[1]["ok"], true);
        assert_eq!(messages[1]["summary"], "42 passed");
        assert_eq!(messages[2]["kind"], "assistant");
        assert_eq!(messages[2]["execution_seq"], 1);
    }

    #[test]
    fn an_approval_request_blocks_the_session_until_the_provider_resolves_it() {
        let (mut store, mut items) = busy_store();
        apply_notification(
            &mut store,
            "codex:thread-1",
            &mut items,
            &json!({
                "method": "dlgt/server/request",
                "params": {
                    "method": "item/commandExecution/requestApproval",
                    "id": 7,
                    "params": {"threadId": "thread-1", "turnId": "provider-turn", "itemId": "i1",
                               "command": "touch outside/probe.txt", "cwd": "/work",
                               "reason": "writes outside the workspace"},
                },
            }),
        )
        .unwrap_or_else(|error| panic!("failed to apply server request: {error}"));
        let uid = store
            .session_uid("codex:thread-1")
            .unwrap_or_else(|| panic!("session uid missing"));
        let pending = store.pending_requests(&uid);
        assert_eq!(pending.len(), 1);
        let public = pending[0].public();
        assert_eq!(public["id"], "r1");
        assert_eq!(public["kind"], "approval");
        assert_eq!(public["tool"], "exec");
        assert_eq!(public["summary"], "touch outside/probe.txt");
        assert_eq!(public["cwd"], "/work");
        assert_eq!(public["reason"], "writes outside the workspace");
        assert_eq!(state(&store), Some(SessionState::Blocked));

        apply_notification(
            &mut store,
            "codex:thread-1",
            &mut items,
            &json!({"method": "serverRequest/resolved",
                    "params": {"threadId": "thread-1", "requestId": 7}}),
        )
        .unwrap_or_else(|error| panic!("failed to apply resolution: {error}"));
        assert!(store.pending_requests(&uid).is_empty());
        assert_eq!(
            state(&store),
            Some(SessionState::Busy),
            "a withdrawn Request unblocks the Session without an answer"
        );
    }

    #[test]
    fn idle_canceling_and_stale_codex_requests_are_refused() {
        let mut idle = ready_store("codex");
        let mut items = CodexItems::default();
        let request = json!({
            "method": "dlgt/server/request",
            "params": {"method": "item/commandExecution/requestApproval", "id": 1,
                "params": {"threadId": "thread-1", "turnId": "provider-turn", "command": "echo x"}}
        });
        assert!(
            !apply_notification(&mut idle, "codex:thread-1", &mut items, &request)
                .unwrap_or_else(|error| panic!("failed to apply idle request: {error}"))
        );

        let (mut busy, mut items) = busy_store();
        let mut stale = request.clone();
        stale["params"]["params"]["turnId"] = json!("other-turn");
        assert!(
            !apply_notification(&mut busy, "codex:thread-1", &mut items, &stale)
                .unwrap_or_else(|error| panic!("failed to apply stale request: {error}"))
        );
        assert!(busy.cancel_turn("turn_1").unwrap_or(false));
        assert!(
            !apply_notification(&mut busy, "codex:thread-1", &mut items, &request)
                .unwrap_or_else(|error| panic!("failed to apply canceling request: {error}"))
        );
    }

    #[test]
    fn a_user_input_request_is_a_question_with_choices() {
        let (mut store, mut items) = busy_store();
        apply_notification(
            &mut store,
            "codex:thread-1",
            &mut items,
            &json!({
                "method": "dlgt/server/request",
                "params": {
                    "method": "item/tool/requestUserInput",
                    "id": "q-1",
                    "params": {"threadId": "thread-1", "turnId": "provider-turn", "itemId": "i9",
                               "isBlocking": true,
                               "questions": [{"id": "color", "header": "Color",
                                              "question": "Red or blue?",
                                              "options": [{"label": "Red", "description": ""},
                                                          {"label": "Blue", "description": ""}]}]},
                },
            }),
        )
        .unwrap_or_else(|error| panic!("failed to apply server request: {error}"));
        let uid = store
            .session_uid("codex:thread-1")
            .unwrap_or_else(|| panic!("session uid missing"));
        let public = store.pending_requests(&uid)[0].public();
        assert_eq!(public["kind"], "question");
        assert_eq!(public["summary"], "Red or blue?");
        assert_eq!(public["tool"], "request_user_input");
        assert_eq!(
            public["choices"],
            json!([
                {"index": 1, "label": "Red"},
                {"index": 2, "label": "Blue"},
            ])
        );
    }

    #[test]
    fn mcp_elicitation_publishes_the_providers_schema_unchanged() {
        let (mut store, mut items) = busy_store();
        apply_notification(
            &mut store,
            "codex:thread-1",
            &mut items,
            &json!({
                "method": "dlgt/server/request",
                "params": {
                    "method": "mcpServer/elicitation/request",
                    "id": "form-1",
                    "params": {
                        "threadId": "thread-1", "turnId": "provider-turn",
                        "mode": "form", "serverName": "profile", "message": "Create profile",
                        "requestedSchema": {
                            "type": "object", "required": ["name", "color"],
                            "properties": {
                                "name": {"type": "string", "maxLength": 40},
                                "color": {"type": "string", "enum": ["red", "blue"]}
                            }
                        }
                    }
                }
            }),
        )
        .unwrap_or_else(|error| panic!("failed to apply form request: {error}"));
        let uid = store
            .session_uid("codex:thread-1")
            .unwrap_or_else(|| panic!("session uid missing"));
        let public = store.pending_requests(&uid)[0].public();
        assert_eq!(
            public["schema"],
            json!({
                "type": "object", "required": ["name", "color"],
                "properties": {
                    "name": {"type": "string", "maxLength": 40},
                    "color": {"type": "string", "enum": ["red", "blue"]}
                }
            })
        );
    }

    #[test]
    fn an_unpublishable_form_is_declined_with_a_notice_instead_of_changed_schema() {
        let (mut store, mut items) = busy_store();
        let field_name = "required_field_".to_owned() + &"x".repeat(300_000);
        let mut properties = serde_json::Map::new();
        properties.insert(field_name.clone(), json!({"type": "string"}));
        let notification = json!({
            "method": "dlgt/server/request",
            "params": {
                "method": "mcpServer/elicitation/request",
                "id": "too-large-form",
                "params": {
                    "threadId": "thread-1", "turnId": "provider-turn",
                    "mode": "form", "serverName": "profile", "message": "Create profile",
                    "requestedSchema": {
                        "type": "object", "required": [field_name], "properties": properties
                    }
                }
            }
        });

        let accepted = apply_notification(&mut store, "codex:thread-1", &mut items, &notification)
            .unwrap_or_else(|error| panic!("failed to handle oversized form: {error}"));
        let uid = store
            .session_uid("codex:thread-1")
            .unwrap_or_else(|| panic!("session uid missing"));

        assert!(!accepted, "provider was told dlgt accepted a changed form");
        assert!(store.pending_requests(&uid).is_empty());
        assert!(messages(&store).iter().any(|message| {
            message["kind"] == "notice" && message.to_string().contains("form")
        }));
    }

    #[test]
    fn an_auto_review_is_the_providers_decision_and_a_denial_is_counted() {
        let (mut store, mut items) = busy_store();
        for (status, risk) in [("approved", "low"), ("denied", "high")] {
            apply_notification(
                &mut store,
                "codex:thread-1",
                &mut items,
                &json!({
                    "method": "item/autoApprovalReview/completed",
                    "params": {
                        "threadId": "thread-1", "turnId": "provider-turn", "reviewId": "rv",
                        "review": {"status": status, "riskLevel": risk, "rationale": "because"},
                        "action": {"type": "command", "command": "sudo -n true",
                                   "cwd": "/work", "source": "shell"},
                    },
                }),
            )
            .unwrap_or_else(|error| panic!("failed to apply review: {error}"));
        }
        let messages = messages(&store);
        assert_eq!(messages[0]["kind"], "approval");
        assert_eq!(messages[0]["decision"], "approved");
        assert_eq!(messages[0]["by"], "provider_reviewer");
        assert_eq!(messages[0]["risk"], "low");
        assert_eq!(messages[0]["summary"], "sudo -n true");
        assert_eq!(messages[1]["decision"], "denied");
        assert_eq!(
            store.get_turn("turn_1").map(|turn| turn.denied_requests),
            Some(1)
        );
        assert_eq!(
            state(&store),
            Some(SessionState::Busy),
            "a provider review never blocks the Session"
        );
    }

    #[test]
    fn codex_retry_error_does_not_finish_before_authoritative_completion() {
        let mut store = ready_store("codex");
        store
            .insert_turn("turn_1", "codex:thread-1", "hello")
            .unwrap_or_else(|error| panic!("failed to insert turn: {error}"));
        apply_codex_notification(
            &mut store,
            "codex:thread-1",
            &json!({
                "method": "turn/started",
                "params": {"threadId": "thread-1", "turn": {"id": "provider-turn", "items": []}},
            }),
        )
        .unwrap_or_else(|error| panic!("failed to start Codex turn: {error}"));
        apply_codex_notification(
            &mut store,
            "codex:thread-1",
            &json!({
                "method": "error",
                "params": {
                    "threadId": "thread-1",
                    "turnId": "provider-turn",
                    "willRetry": true,
                    "error": {"message": "temporary", "codexErrorInfo": "serverOverloaded", "additionalDetails": "secret"},
                },
            }),
        )
        .unwrap_or_else(|error| panic!("failed to apply retry error: {error}"));
        assert_eq!(
            store
                .get_turn("turn_1")
                .unwrap_or_else(|| panic!("turn missing"))
                .state,
            TurnState::Running
        );
        apply_codex_notification(
            &mut store,
            "codex:thread-1",
            &json!({
                "method": "turn/completed",
                "params": {
                    "threadId": "thread-1",
                    "turn": {
                        "id": "provider-turn",
                        "status": "failed",
                        "items": [],
                        "error": {"message": "bad request", "codexErrorInfo": "badRequest", "additionalDetails": "secret"},
                    },
                },
            }),
        )
        .unwrap_or_else(|error| panic!("failed to finish Codex turn: {error}"));
        let turn = store
            .get_turn("turn_1")
            .unwrap_or_else(|| panic!("turn missing"));
        assert_eq!(turn.state, TurnState::Failed);
        let error = turn.error.unwrap_or_else(|| panic!("turn error missing"));
        assert_eq!(error["code"], "badRequest");
        assert_eq!(error["message"], "bad request");
        assert!(!error.to_string().contains("secret"));
        let uid = store
            .session_uid("codex:thread-1")
            .unwrap_or_else(|| panic!("session uid missing"));
        let events = store.read_events(Some(&uid), 0);
        assert!(
            events
                .iter()
                .any(|event| event.kind == "provider.error.retrying")
        );
        assert!(events.iter().any(|event| event.kind == "turn.failed"));
    }

    #[test]
    fn codex_terminal_event_quiesces_without_resurrecting_a_canceled_turn() {
        let mut store = ready_store("codex");
        store
            .insert_turn("turn_1", "codex:thread-1", "hello")
            .unwrap_or_else(|error| panic!("failed to insert turn: {error}"));
        assert!(store.mark_turn_started("turn_1", Some("provider-turn")));
        store.set_session_state("codex:thread-1", SessionState::Busy);
        assert!(
            store
                .cancel_turn("turn_1")
                .unwrap_or_else(|error| panic!("failed to cancel turn: {error}"))
        );

        apply_codex_notification(
            &mut store,
            "codex:thread-1",
            &json!({
                "method": "turn/completed",
                "params": {
                    "threadId": "thread-1",
                    "turn": {
                        "id": "provider-turn",
                        "status": "interrupted",
                        "items": [],
                        "error": null,
                    },
                },
            }),
        )
        .unwrap_or_else(|error| panic!("failed to quiesce canceled turn: {error}"));

        let turn = store
            .get_turn("turn_1")
            .unwrap_or_else(|| panic!("turn missing"));
        assert_eq!(turn.state, TurnState::Canceled);
        let session = store
            .get_session("codex:thread-1")
            .unwrap_or_else(|| panic!("session missing"));
        assert_eq!(session.state, SessionState::Idle);
        assert!(session.active_turn_id.is_none());
        let uid = store
            .session_uid("codex:thread-1")
            .unwrap_or_else(|| panic!("session uid missing"));
        assert!(
            store
                .read_events(Some(&uid), 0)
                .iter()
                .any(|event| event.kind == "provider.quiesced")
        );
    }
}
