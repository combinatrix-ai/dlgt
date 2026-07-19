use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
#[cfg(test)]
use base64::Engine as _;
use serde_json::Value;

#[cfg(test)]
use crate::protocol::InputRecord;
use crate::protocol::{EventRecord, SessionRecord, TurnRecord};

pub struct Store {
    state: RefCell<MemoryState>,
}

const OUTPUT_CHUNK_LIMIT: usize = 8192;

#[derive(Default)]
struct MemoryState {
    sessions: HashMap<String, StoredSession>,
    turns: HashMap<String, TurnRecord>,
    events: Vec<EventRecord>,
    inputs: Vec<StoredInput>,
    outputs: HashMap<String, VecDeque<OutputChunk>>,
    next_event_seq: i64,
    next_input_seq: i64,
    next_output_seq: i64,
}

struct StoredSession {
    record: SessionRecord,
    terminal_rows: u16,
    terminal_cols: u16,
}

#[cfg_attr(not(test), allow(dead_code))]
struct StoredInput {
    seq: i64,
    session_id: String,
    turn_id: Option<String>,
    source: String,
    data: Vec<u8>,
    display: String,
    created_at_ms: i64,
}

struct OutputChunk {
    seq: i64,
    data: Vec<u8>,
}

pub struct OutputPage {
    pub data: Vec<u8>,
    pub next_after: i64,
    pub has_more: bool,
}

pub struct NewSession<'a> {
    pub id: &'a str,
    pub alias: &'a str,
    pub title: &'a str,
    pub agent: &'a str,
    pub cwd: &'a str,
    pub model: Option<&'a str>,
    pub effort: Option<&'a str>,
    pub harness_options: &'a [String],
    pub auto_approve: bool,
}

#[allow(
    clippy::assigning_clones,
    clippy::unnecessary_wraps,
    clippy::unused_self
)]
impl Store {
    pub fn new() -> Self {
        Self {
            state: RefCell::new(MemoryState::default()),
        }
    }

    pub fn insert_session(&self, session: &NewSession<'_>) -> Result<()> {
        let mut state = self.state.borrow_mut();
        if state.sessions.contains_key(session.id) {
            bail!("session id already exists");
        }
        if state.sessions.values().any(|existing| {
            existing.record.alias == session.alias && !terminal_session(&existing.record)
        }) {
            bail!("active session alias already exists");
        }
        let now = now_ms();
        state.sessions.insert(
            session.id.to_owned(),
            StoredSession {
                record: SessionRecord {
                    id: session.id.to_owned(),
                    alias: session.alias.to_owned(),
                    title: session.title.to_owned(),
                    agent: session.agent.to_owned(),
                    cwd: session.cwd.to_owned(),
                    state: "starting".to_owned(),
                    model: session.model.map(str::to_owned),
                    effort: session.effort.map(str::to_owned),
                    harness_options: session.harness_options.to_vec(),
                    auto_approve: session.auto_approve,
                    provider_session_id: None,
                    active_turn_id: None,
                    pid: None,
                    created_at_ms: now,
                    updated_at_ms: now,
                },
                terminal_rows: 24,
                terminal_cols: 80,
            },
        );
        Ok(())
    }

    pub fn set_session_running(&self, id: &str, pid: Option<u32>) -> Result<bool> {
        let mut state = self.state.borrow_mut();
        let Some(session) = state.sessions.get_mut(id) else {
            return Ok(false);
        };
        if !matches!(session.record.state.as_str(), "starting" | "idle") {
            return Ok(false);
        }
        if session.record.state == "starting" {
            session.record.state = "running".to_owned();
        }
        session.record.pid = pid;
        session.record.updated_at_ms = now_ms();
        Ok(true)
    }

    pub fn begin_session_restart(&self, id: &str) -> Result<bool> {
        let mut state = self.state.borrow_mut();
        let Some(session) = state.sessions.get_mut(id) else {
            return Ok(false);
        };
        if matches!(
            session.record.state.as_str(),
            "starting" | "stopping" | "restarting"
        ) {
            return Ok(false);
        }
        session.record.state = "restarting".to_owned();
        session.record.updated_at_ms = now_ms();
        Ok(true)
    }

    pub fn finish_session_restart_stop(&self, id: &str) -> Result<()> {
        if let Some(session) = self.state.borrow_mut().sessions.get_mut(id)
            && session.record.state == "restarting"
        {
            session.record.pid = None;
            session.record.active_turn_id = None;
            session.record.updated_at_ms = now_ms();
        }
        Ok(())
    }

    pub fn start_restarted_session(&self, id: &str) -> Result<bool> {
        let mut state = self.state.borrow_mut();
        let Some(session) = state.sessions.get_mut(id) else {
            return Ok(false);
        };
        if session.record.state != "restarting" {
            return Ok(false);
        }
        session.record.state = "starting".to_owned();
        session.record.pid = None;
        session.record.active_turn_id = None;
        session.record.updated_at_ms = now_ms();
        Ok(true)
    }

    pub fn set_session_state(&self, id: &str, state: &str) -> Result<bool> {
        let mut memory = self.state.borrow_mut();
        let Some(session) = memory.sessions.get_mut(id) else {
            return Ok(false);
        };
        if matches!(
            session.record.state.as_str(),
            "stopped" | "failed" | "stopping" | "restarting"
        ) {
            return Ok(false);
        }
        session.record.state = state.to_owned();
        session.record.updated_at_ms = now_ms();
        Ok(true)
    }

    pub fn set_session_stopped(&self, id: &str) -> Result<()> {
        set_terminal_session(&mut self.state.borrow_mut(), id, "stopped");
        Ok(())
    }

    pub fn set_session_failed(&self, id: &str) -> Result<()> {
        set_terminal_session(&mut self.state.borrow_mut(), id, "failed");
        Ok(())
    }

    pub fn set_session_provider_id(&self, id: &str, provider_id: &str) -> Result<()> {
        if let Some(session) = self.state.borrow_mut().sessions.get_mut(id) {
            session.record.provider_session_id = Some(provider_id.to_owned());
            session.record.updated_at_ms = now_ms();
        }
        Ok(())
    }

    pub fn set_terminal_size(&self, session_id: &str, rows: u16, cols: u16) -> Result<()> {
        if let Some(session) = self.state.borrow_mut().sessions.get_mut(session_id) {
            session.terminal_rows = rows;
            session.terminal_cols = cols;
            session.record.updated_at_ms = now_ms();
        }
        Ok(())
    }

    pub fn terminal_size(&self, session_id: &str) -> Result<(u16, u16)> {
        self.state
            .borrow()
            .sessions
            .get(session_id)
            .map(|session| (session.terminal_rows, session.terminal_cols))
            .context("failed to read terminal size")
    }

    pub fn get_session(&self, selector: &str) -> Result<Option<SessionRecord>> {
        let state = self.state.borrow();
        if let Some(session) = state.sessions.get(selector) {
            return Ok(Some(session.record.clone()));
        }
        Ok(state
            .sessions
            .values()
            .filter(|session| {
                session.record.alias == selector && !terminal_session(&session.record)
            })
            .max_by_key(|session| session.record.created_at_ms)
            .map(|session| session.record.clone()))
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionRecord>> {
        let mut sessions = self
            .state
            .borrow()
            .sessions
            .values()
            .map(|session| session.record.clone())
            .collect::<Vec<_>>();
        sessions.sort_by_key(|session| std::cmp::Reverse(session.created_at_ms));
        Ok(sessions)
    }

    pub fn insert_turn(&mut self, id: &str, session_id: &str, prompt: &str) -> Result<TurnRecord> {
        let mut state = self.state.borrow_mut();
        if state.turns.contains_key(id) {
            bail!("turn id already exists");
        }
        let now = now_ms();
        let Some(session) = state.sessions.get(session_id) else {
            bail!("session not found");
        };
        if session.record.active_turn_id.is_some() || session.record.state != "idle" {
            bail!("session already has an active turn or is not ready");
        }
        let execution_seq = state
            .turns
            .values()
            .filter(|turn| turn.session_id == session_id)
            .map(|turn| turn.execution_seq)
            .max()
            .unwrap_or(0)
            + 1;
        let turn = TurnRecord {
            id: id.to_owned(),
            session_id: session_id.to_owned(),
            execution_seq,
            prompt: prompt.to_owned(),
            state: "submitted".to_owned(),
            provider_turn_id: None,
            final_message: None,
            error: None,
            created_at_ms: now,
            started_at_ms: None,
            completed_at_ms: None,
            usage: None,
        };
        state.turns.insert(id.to_owned(), turn.clone());
        if let Some(session) = state.sessions.get_mut(session_id) {
            session.record.active_turn_id = Some(id.to_owned());
            session.record.updated_at_ms = now;
        }
        Ok(turn)
    }

    pub fn get_turn(&self, id: &str) -> Result<Option<TurnRecord>> {
        Ok(self.state.borrow().turns.get(id).cloned())
    }

    pub fn latest_turn(&self, session_id: &str) -> Result<Option<TurnRecord>> {
        Ok(self
            .state
            .borrow()
            .turns
            .values()
            .filter(|turn| turn.session_id == session_id)
            .max_by_key(|turn| turn.execution_seq)
            .cloned())
    }

    pub fn mark_turn_started(&self, id: &str, provider_turn_id: Option<&str>) -> Result<bool> {
        let mut state = self.state.borrow_mut();
        let Some(turn) = state.turns.get_mut(id) else {
            return Ok(false);
        };
        if turn.state != "submitted" {
            return Ok(false);
        }
        turn.state = "running".to_owned();
        if turn.provider_turn_id.is_none() {
            turn.provider_turn_id = provider_turn_id.map(str::to_owned);
        }
        turn.started_at_ms.get_or_insert_with(now_ms);
        Ok(true)
    }

    pub fn complete_turn_if_matching(
        &self,
        id: &str,
        provider_turn_id: Option<&str>,
        final_message: Option<&str>,
    ) -> Result<bool> {
        self.finish_turn_if_matching(id, provider_turn_id, "completed", final_message, None)
    }

    pub fn finish_turn_if_matching(
        &self,
        id: &str,
        provider_turn_id: Option<&str>,
        state: &str,
        final_message: Option<&str>,
        error: Option<&str>,
    ) -> Result<bool> {
        if !matches!(state, "completed" | "failed" | "interrupted") {
            bail!("invalid terminal turn state {state:?}");
        }
        let mut memory = self.state.borrow_mut();
        let turn = memory.turns.get_mut(id).context("turn not found")?;
        if !matches!(turn.state.as_str(), "submitted" | "running")
            || provider_turn_id.is_some()
                && turn.provider_turn_id.is_some()
                && turn.provider_turn_id.as_deref() != provider_turn_id
        {
            return Ok(false);
        }
        let session_id = turn.session_id.clone();
        turn.state = state.to_owned();
        if turn.provider_turn_id.is_none() {
            turn.provider_turn_id = provider_turn_id.map(str::to_owned);
        }
        turn.final_message = final_message.map(str::to_owned);
        turn.error = error.map(str::to_owned);
        turn.completed_at_ms = Some(now_ms());
        if let Some(session) = memory.sessions.get_mut(&session_id)
            && session.record.active_turn_id.as_deref() == Some(id)
        {
            session.record.active_turn_id = None;
            session.record.updated_at_ms = now_ms();
        }
        Ok(true)
    }

    pub fn interrupt_active_turn(&self, session_id: &str, error: &str) -> Result<Option<String>> {
        let mut state = self.state.borrow_mut();
        let Some(session) = state.sessions.get(session_id) else {
            return Ok(None);
        };
        let Some(turn_id) = session.record.active_turn_id.clone() else {
            return Ok(None);
        };
        let Some(turn) = state.turns.get_mut(&turn_id) else {
            return Ok(None);
        };
        if !matches!(turn.state.as_str(), "submitted" | "running") {
            return Ok(None);
        }
        turn.state = "interrupted".to_owned();
        turn.error = Some(error.to_owned());
        turn.completed_at_ms = Some(now_ms());
        if let Some(session) = state.sessions.get_mut(session_id) {
            session.record.active_turn_id = None;
            session.record.updated_at_ms = now_ms();
        }
        Ok(Some(turn_id))
    }

    pub fn cancel_turn(&mut self, id: &str) -> Result<bool> {
        let mut state = self.state.borrow_mut();
        let Some(turn) = state.turns.get_mut(id) else {
            bail!("turn not found");
        };
        if !matches!(turn.state.as_str(), "submitted" | "running") {
            return Ok(false);
        }
        turn.state = "canceled".to_owned();
        turn.completed_at_ms = Some(now_ms());
        let session_id = turn.session_id.clone();
        if let Some(session) = state.sessions.get_mut(&session_id)
            && session.record.active_turn_id.as_deref() == Some(id)
        {
            session.record.state = "quiescing".to_owned();
            session.record.updated_at_ms = now_ms();
        }
        Ok(true)
    }

    pub fn settle_canceled_turn(&self, id: &str, provider_turn_id: Option<&str>) -> Result<bool> {
        let mut state = self.state.borrow_mut();
        let turn = state.turns.get(id).context("turn not found")?;
        if turn.state != "canceled"
            || provider_turn_id.is_some()
                && turn.provider_turn_id.is_some()
                && turn.provider_turn_id.as_deref() != provider_turn_id
        {
            return Ok(false);
        }
        let session_id = turn.session_id.clone();
        let Some(session) = state.sessions.get_mut(&session_id) else {
            return Ok(false);
        };
        if session.record.active_turn_id.as_deref() != Some(id)
            || session.record.state != "quiescing"
        {
            return Ok(false);
        }
        session.record.active_turn_id = None;
        session.record.state = "idle".to_owned();
        session.record.updated_at_ms = now_ms();
        Ok(true)
    }

    pub fn record_event(
        &self,
        session_id: Option<&str>,
        turn_id: Option<&str>,
        kind: &str,
        payload: &Value,
    ) -> Result<i64> {
        let mut state = self.state.borrow_mut();
        state.next_event_seq += 1;
        let seq = state.next_event_seq;
        state.events.push(EventRecord {
            seq,
            session_id: session_id.map(str::to_owned),
            turn_id: turn_id.map(str::to_owned),
            kind: kind.to_owned(),
            payload: payload.clone(),
            created_at_ms: now_ms(),
        });
        Ok(seq)
    }

    pub fn record_input(
        &self,
        session_id: &str,
        turn_id: Option<&str>,
        source: &str,
        data: &[u8],
    ) -> Result<i64> {
        let mut state = self.state.borrow_mut();
        state.next_input_seq += 1;
        let seq = state.next_input_seq;
        state.inputs.push(StoredInput {
            seq,
            session_id: session_id.to_owned(),
            turn_id: turn_id.map(str::to_owned),
            source: source.to_owned(),
            data: data.to_vec(),
            display: display_bytes(data),
            created_at_ms: now_ms(),
        });
        Ok(seq)
    }

    pub fn record_output(&self, session_id: &str, data: &[u8]) -> Result<()> {
        let mut state = self.state.borrow_mut();
        state.next_output_seq += 1;
        let seq = state.next_output_seq;
        let chunks = state.outputs.entry(session_id.to_owned()).or_default();
        chunks.push_back(OutputChunk {
            seq,
            data: data.to_vec(),
        });
        while chunks.len() > OUTPUT_CHUNK_LIMIT {
            chunks.pop_front();
        }
        Ok(())
    }

    pub fn read_output_page(
        &self,
        session_id: &str,
        after: i64,
        limit_bytes: usize,
    ) -> Result<OutputPage> {
        let state = self.state.borrow();
        let chunks = state.outputs.get(session_id);
        let mut output = Vec::new();
        let mut next_after = after;
        for chunk in chunks
            .into_iter()
            .flatten()
            .filter(|chunk| chunk.seq > after)
        {
            if !output.is_empty() && output.len().saturating_add(chunk.data.len()) > limit_bytes {
                break;
            }
            output.extend(&chunk.data);
            next_after = chunk.seq;
            if output.len() >= limit_bytes {
                break;
            }
        }
        let has_more =
            chunks.is_some_and(|chunks| chunks.iter().any(|chunk| chunk.seq > next_after));
        Ok(OutputPage {
            data: output,
            next_after,
            has_more,
        })
    }

    #[cfg(test)]
    pub fn read_inputs(&self, session_id: &str, after: i64) -> Result<Vec<InputRecord>> {
        Ok(self
            .state
            .borrow()
            .inputs
            .iter()
            .filter(|input| input.session_id == session_id && input.seq > after)
            .map(|input| InputRecord {
                seq: input.seq,
                session_id: input.session_id.clone(),
                turn_id: input.turn_id.clone(),
                source: input.source.clone(),
                data_base64: base64::engine::general_purpose::STANDARD.encode(&input.data),
                display: input.display.clone(),
                byte_len: input.data.len(),
                created_at_ms: input.created_at_ms,
            })
            .collect())
    }

    pub fn read_events(&self, session_id: Option<&str>, after: i64) -> Result<Vec<EventRecord>> {
        Ok(self
            .state
            .borrow()
            .events
            .iter()
            .filter(|event| {
                event.seq > after
                    && session_id.is_none_or(|id| event.session_id.as_deref() == Some(id))
            })
            .cloned()
            .collect())
    }
}

fn terminal_session(session: &SessionRecord) -> bool {
    matches!(session.state.as_str(), "stopped" | "failed")
}

fn set_terminal_session(state: &mut MemoryState, id: &str, terminal_state: &str) {
    if let Some(session) = state.sessions.get_mut(id) {
        terminal_state.clone_into(&mut session.record.state);
        session.record.pid = None;
        session.record.active_turn_id = None;
        session.record.updated_at_ms = now_ms();
    }
}

pub fn now_ms() -> i64 {
    let milliseconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0_u128, |duration| duration.as_millis());
    i64::try_from(milliseconds).unwrap_or(i64::MAX)
}

pub fn display_bytes(data: &[u8]) -> String {
    let mut display = String::new();
    for &byte in data {
        match byte {
            b'\n' => display.push_str("\\n"),
            b'\r' => display.push_str("\\r"),
            b'\t' => display.push_str("\\t"),
            0x20..=0x7e => display.push(char::from(byte)),
            _ => {
                use std::fmt::Write as _;
                let _ = write!(display, "\\x{byte:02x}");
            }
        }
    }
    display
}

#[cfg(test)]
mod tests {
    use super::{NewSession, Store, display_bytes};
    use serde_json::json;

    fn mark_ready(store: &Store, session_id: &str) {
        assert!(
            store
                .set_session_running(session_id, Some(42))
                .unwrap_or_else(|error| panic!("failed to start session: {error}"))
        );
        assert!(
            store
                .set_session_state(session_id, "idle")
                .unwrap_or_else(|error| panic!("failed to mark session ready: {error}"))
        );
    }

    #[test]
    fn retains_explicit_auto_approve_opt_out() {
        let store = Store::new();
        store
            .insert_session(&NewSession {
                id: "ses_1",
                alias: "@worker",
                title: "worker",
                agent: "claude",
                cwd: "/tmp",
                model: None,
                effort: None,
                harness_options: &[],
                auto_approve: false,
            })
            .unwrap_or_else(|error| panic!("failed to insert session: {error}"));
        let session = store
            .get_session("ses_1")
            .unwrap_or_else(|error| panic!("failed to read session: {error}"))
            .unwrap_or_else(|| panic!("session missing"));
        assert!(!session.auto_approve);
    }

    #[test]
    fn retains_session_turn_event_and_input() {
        let mut store = Store::new();
        store
            .insert_session(&NewSession {
                id: "ses_1",
                alias: "@test",
                title: "test",
                agent: "codex",
                cwd: "/tmp",
                model: None,
                effort: None,
                harness_options: &[],
                auto_approve: true,
            })
            .unwrap_or_else(|error| panic!("failed to insert session: {error}"));
        mark_ready(&store, "ses_1");
        let turn = store
            .insert_turn("turn_1", "ses_1", "hello")
            .unwrap_or_else(|error| panic!("failed to insert turn: {error}"));
        assert_eq!(turn.state, "submitted");
        store
            .record_event(Some("ses_1"), Some("turn_1"), "turn.submitted", &json!({}))
            .unwrap_or_else(|error| panic!("failed to record event: {error}"));
        store
            .record_input("ses_1", Some("turn_1"), "api", b"hello\r")
            .unwrap_or_else(|error| panic!("failed to record input: {error}"));
        assert_eq!(
            store
                .read_inputs("ses_1", 0)
                .unwrap_or_else(|error| panic!("failed to read inputs: {error}"))[0]
                .display,
            "hello\\r"
        );
    }

    #[test]
    fn unscoped_event_reads_include_session_events() {
        let store = Store::new();
        store
            .record_event(Some("ses_1"), None, "session.started", &json!({}))
            .unwrap_or_else(|error| panic!("failed to record session event: {error}"));
        store
            .record_event(None, None, "runtime.started", &json!({}))
            .unwrap_or_else(|error| panic!("failed to record global event: {error}"));

        let events = store
            .read_events(None, 0)
            .unwrap_or_else(|error| panic!("failed to read all events: {error}"));
        assert_eq!(events.len(), 2);
        assert_eq!(
            store
                .read_events(Some("ses_1"), 0)
                .unwrap_or_else(|error| panic!("failed to read session events: {error}"))
                .len(),
            1
        );
    }

    #[test]
    fn formats_control_bytes_without_losing_data() {
        assert_eq!(display_bytes(b"a\n\x1b"), "a\\n\\x1b");
    }

    #[test]
    fn stopped_session_releases_its_alias() {
        let store = Store::new();
        store
            .insert_session(&NewSession {
                id: "ses_old",
                alias: "@worker",
                title: "worker",
                agent: "codex",
                cwd: "/tmp",
                model: None,
                effort: None,
                harness_options: &[],
                auto_approve: true,
            })
            .unwrap_or_else(|error| panic!("failed to insert session: {error}"));
        mark_ready(&store, "ses_old");
        store
            .set_session_stopped("ses_old")
            .unwrap_or_else(|error| panic!("failed to stop session: {error}"));
        store
            .insert_session(&NewSession {
                id: "ses_new",
                alias: "@worker",
                title: "worker",
                agent: "claude",
                cwd: "/tmp",
                model: None,
                effort: None,
                harness_options: &[],
                auto_approve: true,
            })
            .unwrap_or_else(|error| panic!("failed to reuse alias: {error}"));
        let archived = store
            .get_session("ses_old")
            .unwrap_or_else(|error| panic!("failed to read old session: {error}"))
            .unwrap_or_else(|| panic!("old session missing"));
        assert_eq!(archived.alias, "@worker");
        assert_eq!(
            store
                .get_session("@worker")
                .unwrap_or_else(|error| panic!("failed to read new session: {error}"))
                .unwrap_or_else(|| panic!("new session missing"))
                .id,
            "ses_new"
        );
    }

    #[test]
    fn terminal_session_can_restart_without_losing_identity() {
        let store = Store::new();
        let harness_options = vec!["permission-mode=auto".to_owned()];
        store
            .insert_session(&NewSession {
                id: "ses_1",
                alias: "@worker",
                title: "worker",
                agent: "claude",
                cwd: "/tmp",
                model: Some("gpt-test"),
                effort: Some("high"),
                harness_options: &harness_options,
                auto_approve: true,
            })
            .unwrap_or_else(|error| panic!("failed to insert session: {error}"));
        mark_ready(&store, "ses_1");
        store
            .set_session_provider_id("ses_1", "provider-thread")
            .unwrap_or_else(|error| panic!("failed to bind provider: {error}"));
        store
            .set_session_stopped("ses_1")
            .unwrap_or_else(|error| panic!("failed to stop session: {error}"));

        assert!(
            store
                .begin_session_restart("ses_1")
                .unwrap_or_else(|error| panic!("failed to restart session: {error}"))
        );
        let session = store
            .get_session("ses_1")
            .unwrap_or_else(|error| panic!("failed to read session: {error}"))
            .unwrap_or_else(|| panic!("session missing"));
        assert_eq!(session.state, "restarting");
        assert_eq!(session.id, "ses_1");
        assert_eq!(session.alias, "@worker");
        assert_eq!(session.harness_options, harness_options);
        assert_eq!(
            session.provider_session_id.as_deref(),
            Some("provider-thread")
        );
        assert!(!store.begin_session_restart("ses_1").unwrap_or(false));
        assert!(
            store
                .start_restarted_session("ses_1")
                .unwrap_or_else(|error| panic!("failed to start restarted session: {error}"))
        );
        assert_eq!(
            store
                .get_session("ses_1")
                .unwrap_or_else(|error| panic!("failed to read restarted session: {error}"))
                .unwrap_or_else(|| panic!("restarted session missing"))
                .state,
            "starting"
        );
    }

    #[test]
    fn active_session_can_enter_restart_without_releasing_its_alias() {
        let store = Store::new();
        store
            .insert_session(&NewSession {
                id: "ses_1",
                alias: "@worker",
                title: "worker",
                agent: "claude",
                cwd: "/tmp",
                model: None,
                effort: None,
                harness_options: &[],
                auto_approve: true,
            })
            .unwrap_or_else(|error| panic!("failed to insert session: {error}"));
        mark_ready(&store, "ses_1");

        assert!(
            store
                .begin_session_restart("ses_1")
                .unwrap_or_else(|error| panic!("failed to begin restart: {error}"))
        );
        assert_eq!(
            store
                .get_session("@worker")
                .unwrap_or_else(|error| panic!("failed to resolve reserved alias: {error}"))
                .unwrap_or_else(|| panic!("reserved alias missing"))
                .state,
            "restarting"
        );
        assert!(!store.set_session_state("ses_1", "idle").unwrap_or(true));
        assert_eq!(
            store
                .get_session("ses_1")
                .unwrap_or_else(|error| panic!("failed to reread restarting session: {error}"))
                .unwrap_or_else(|| panic!("restarting session missing"))
                .state,
            "restarting"
        );
        assert!(
            store
                .insert_session(&NewSession {
                    id: "ses_2",
                    alias: "@worker",
                    title: "other",
                    agent: "codex",
                    cwd: "/tmp",
                    model: None,
                    effort: None,
                    harness_options: &[],
                    auto_approve: true,
                })
                .is_err()
        );
    }

    #[test]
    fn process_exit_interrupts_active_turn() {
        let mut store = Store::new();
        store
            .insert_session(&NewSession {
                id: "ses_1",
                alias: "@worker",
                title: "worker",
                agent: "codex",
                cwd: "/tmp",
                model: None,
                effort: None,
                harness_options: &[],
                auto_approve: true,
            })
            .unwrap_or_else(|error| panic!("failed to insert session: {error}"));
        mark_ready(&store, "ses_1");
        store
            .insert_turn("turn_1", "ses_1", "hello")
            .unwrap_or_else(|error| panic!("failed to insert turn: {error}"));
        let interrupted = store
            .interrupt_active_turn("ses_1", "agent exited")
            .unwrap_or_else(|error| panic!("failed to interrupt turn: {error}"));
        assert_eq!(interrupted.as_deref(), Some("turn_1"));
        let turn = store
            .get_turn("turn_1")
            .unwrap_or_else(|error| panic!("failed to read turn: {error}"))
            .unwrap_or_else(|| panic!("turn missing"));
        assert_eq!(turn.state, "interrupted");
        assert_eq!(turn.error.as_deref(), Some("agent exited"));
    }

    #[test]
    fn output_reads_are_paginated() {
        let store = Store::new();
        store
            .insert_session(&NewSession {
                id: "ses_1",
                alias: "@worker",
                title: "worker",
                agent: "codex",
                cwd: "/tmp",
                model: None,
                effort: None,
                harness_options: &[],
                auto_approve: true,
            })
            .unwrap_or_else(|error| panic!("failed to insert session: {error}"));
        store
            .record_output("ses_1", b"first")
            .unwrap_or_else(|error| panic!("failed to write first output: {error}"));
        store
            .record_output("ses_1", b"second")
            .unwrap_or_else(|error| panic!("failed to write second output: {error}"));
        let first = store
            .read_output_page("ses_1", 0, 5)
            .unwrap_or_else(|error| panic!("failed to read first page: {error}"));
        assert_eq!(first.data, b"first");
        assert!(first.has_more);
        let second = store
            .read_output_page("ses_1", first.next_after, 5)
            .unwrap_or_else(|error| panic!("failed to read second page: {error}"));
        assert_eq!(second.data, b"second");
        assert!(!second.has_more);
    }

    #[test]
    fn only_one_turn_can_claim_a_session() {
        let mut store = Store::new();
        store
            .insert_session(&NewSession {
                id: "ses_1",
                alias: "@worker",
                title: "worker",
                agent: "codex",
                cwd: "/tmp",
                model: None,
                effort: None,
                harness_options: &[],
                auto_approve: true,
            })
            .unwrap_or_else(|error| panic!("failed to insert session: {error}"));
        mark_ready(&store, "ses_1");
        store
            .insert_turn("turn_1", "ses_1", "first")
            .unwrap_or_else(|error| panic!("failed to insert first turn: {error}"));
        assert!(store.insert_turn("turn_2", "ses_1", "second").is_err());
    }

    #[test]
    fn claude_cannot_claim_a_turn_before_session_start() {
        let mut store = Store::new();
        store
            .insert_session(&NewSession {
                id: "ses_claude",
                alias: "@claude",
                title: "claude",
                agent: "claude",
                cwd: "/tmp",
                model: None,
                effort: None,
                harness_options: &[],
                auto_approve: true,
            })
            .unwrap_or_else(|error| panic!("failed to insert session: {error}"));
        assert!(
            store
                .set_session_running("ses_claude", Some(42))
                .unwrap_or_else(|error| panic!("failed to start session: {error}"))
        );

        assert!(store.insert_turn("turn_1", "ses_claude", "first").is_err());
    }

    #[test]
    fn stop_must_match_a_running_provider_turn() {
        let mut store = Store::new();
        store
            .insert_session(&NewSession {
                id: "ses_1",
                alias: "@worker",
                title: "worker",
                agent: "codex",
                cwd: "/tmp",
                model: None,
                effort: None,
                harness_options: &[],
                auto_approve: true,
            })
            .unwrap_or_else(|error| panic!("failed to insert session: {error}"));
        mark_ready(&store, "ses_1");
        store
            .insert_turn("turn_1", "ses_1", "first")
            .unwrap_or_else(|error| panic!("failed to insert turn: {error}"));
        assert!(
            store
                .mark_turn_started("turn_1", Some("provider-1"))
                .unwrap_or_else(|error| panic!("failed to start turn: {error}"))
        );
        assert!(
            !store
                .complete_turn_if_matching("turn_1", Some("provider-2"), Some("wrong"))
                .unwrap_or_else(|error| panic!("failed to reject stop: {error}"))
        );
        assert!(
            store
                .complete_turn_if_matching("turn_1", Some("provider-1"), Some("done"))
                .unwrap_or_else(|error| panic!("failed to complete turn: {error}"))
        );
    }

    #[test]
    fn late_cancel_cannot_clear_a_newer_active_turn() {
        let mut store = Store::new();
        store
            .insert_session(&NewSession {
                id: "ses_1",
                alias: "@worker",
                title: "worker",
                agent: "codex",
                cwd: "/tmp",
                model: None,
                effort: None,
                harness_options: &[],
                auto_approve: true,
            })
            .unwrap_or_else(|error| panic!("failed to insert session: {error}"));
        mark_ready(&store, "ses_1");
        store
            .insert_turn("turn_1", "ses_1", "first")
            .unwrap_or_else(|error| panic!("failed to insert first turn: {error}"));
        assert!(
            store
                .mark_turn_started("turn_1", None)
                .unwrap_or_else(|error| panic!("failed to start first turn: {error}"))
        );
        assert!(
            store
                .complete_turn_if_matching("turn_1", None, Some("done"))
                .unwrap_or_else(|error| panic!("failed to complete first turn: {error}"))
        );
        store
            .insert_turn("turn_2", "ses_1", "second")
            .unwrap_or_else(|error| panic!("failed to insert second turn: {error}"));
        assert!(
            !store
                .cancel_turn("turn_1")
                .unwrap_or_else(|error| panic!("failed to reject late cancel: {error}"))
        );
        let session = store
            .get_session("ses_1")
            .unwrap_or_else(|error| panic!("failed to read session: {error}"))
            .unwrap_or_else(|| panic!("session missing"));
        assert_eq!(session.active_turn_id.as_deref(), Some("turn_2"));
    }

    #[test]
    fn active_cancel_waits_for_provider_quiescence() {
        let mut store = Store::new();
        store
            .insert_session(&NewSession {
                id: "ses_1",
                alias: "@worker",
                title: "worker",
                agent: "codex",
                cwd: "/tmp",
                model: None,
                effort: None,
                harness_options: &[],
                auto_approve: true,
            })
            .unwrap_or_else(|error| panic!("failed to insert session: {error}"));
        mark_ready(&store, "ses_1");
        store
            .insert_turn("turn_1", "ses_1", "first")
            .unwrap_or_else(|error| panic!("failed to insert turn: {error}"));
        assert!(
            store
                .mark_turn_started("turn_1", Some("provider-turn"))
                .unwrap_or_else(|error| panic!("failed to start turn: {error}"))
        );
        store
            .set_session_state("ses_1", "busy")
            .unwrap_or_else(|error| panic!("failed to mark session busy: {error}"));
        assert!(
            store
                .cancel_turn("turn_1")
                .unwrap_or_else(|error| panic!("failed to cancel turn: {error}"))
        );
        let session = store
            .get_session("ses_1")
            .unwrap_or_else(|error| panic!("failed to read session: {error}"))
            .unwrap_or_else(|| panic!("session missing"));
        assert_eq!(session.state, "quiescing");
        assert_eq!(session.active_turn_id.as_deref(), Some("turn_1"));
        assert!(
            store
                .settle_canceled_turn("turn_1", Some("provider-turn"))
                .unwrap_or_else(|error| panic!("failed to settle canceled turn: {error}"))
        );
        let session = store
            .get_session("ses_1")
            .unwrap_or_else(|error| panic!("failed to read settled session: {error}"))
            .unwrap_or_else(|| panic!("settled session missing"));
        assert_eq!(session.state, "idle");
        assert!(session.active_turn_id.is_none());
    }

    #[test]
    fn terminal_session_cannot_be_resurrected() {
        let store = Store::new();
        store
            .insert_session(&NewSession {
                id: "ses_1",
                alias: "@worker",
                title: "worker",
                agent: "codex",
                cwd: "/tmp",
                model: None,
                effort: None,
                harness_options: &[],
                auto_approve: true,
            })
            .unwrap_or_else(|error| panic!("failed to insert session: {error}"));
        store
            .set_session_stopped("ses_1")
            .unwrap_or_else(|error| panic!("failed to stop session: {error}"));
        assert!(
            !store
                .set_session_state("ses_1", "idle")
                .unwrap_or_else(|error| panic!("failed to reject transition: {error}"))
        );
        let session = store
            .get_session("ses_1")
            .unwrap_or_else(|error| panic!("failed to read session: {error}"))
            .unwrap_or_else(|| panic!("session missing"));
        assert_eq!(session.state, "stopped");
    }
}
