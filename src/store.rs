use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::protocol::{EventRecord, SessionRecord, SessionState, TurnRecord, TurnState};
use anyhow::{Context, Result, bail};

pub struct Store {
    state: RefCell<MemoryState>,
}

const OUTPUT_CHUNK_LIMIT: usize = 8192;

#[derive(Default)]
struct MemoryState {
    sessions: HashMap<String, StoredSession>,
    provider_reservations: HashMap<String, String>,
    turns: HashMap<String, TurnRecord>,
    events: Vec<EventRecord>,
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
            existing.record.alias == session.alias && !existing.record.state.is_terminal()
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
                    state: SessionState::Starting,
                    model: session.model.map(str::to_owned),
                    effort: session.effort.map(str::to_owned),
                    harness_options: session.harness_options.to_vec(),
                    auto_approve: session.auto_approve,
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

    pub fn set_session_running(&self, id: &str, pid: Option<u32>) -> bool {
        let mut state = self.state.borrow_mut();
        let Some(session) = state.sessions.get_mut(id) else {
            return false;
        };
        if !matches!(
            session.record.state,
            SessionState::Starting | SessionState::Idle
        ) {
            return false;
        }
        if session.record.state == SessionState::Starting {
            session.record.state = SessionState::Running;
        }
        session.record.pid = pid;
        session.record.updated_at_ms = now_ms();
        true
    }

    pub fn begin_session_restart(&self, id: &str) -> Result<bool> {
        let mut state = self.state.borrow_mut();
        let Some(session) = state.sessions.get(id) else {
            return Ok(false);
        };
        if matches!(
            session.record.state,
            SessionState::Starting | SessionState::Stopping | SessionState::Restarting
        ) {
            return Ok(false);
        }
        let alias = session.record.alias.clone();
        if state.sessions.values().any(|candidate| {
            candidate.record.id != id
                && candidate.record.alias == alias
                && !candidate.record.state.is_terminal()
        }) {
            bail!("active session alias already exists");
        }
        let Some(session) = state.sessions.get_mut(id) else {
            return Ok(false);
        };
        session.record.state = SessionState::Restarting;
        session.record.updated_at_ms = now_ms();
        Ok(true)
    }

    pub fn finish_session_restart_stop(&self, id: &str) {
        if let Some(session) = self.state.borrow_mut().sessions.get_mut(id)
            && session.record.state == SessionState::Restarting
        {
            session.record.pid = None;
            session.record.active_turn_id = None;
            session.record.updated_at_ms = now_ms();
        }
    }

    pub fn start_restarted_session(&self, id: &str) -> bool {
        let mut state = self.state.borrow_mut();
        let Some(session) = state.sessions.get_mut(id) else {
            return false;
        };
        if session.record.state != SessionState::Restarting {
            return false;
        }
        session.record.state = SessionState::Starting;
        session.record.pid = None;
        session.record.active_turn_id = None;
        session.record.updated_at_ms = now_ms();
        true
    }

    pub fn set_session_state(&self, id: &str, state: SessionState) -> bool {
        let mut memory = self.state.borrow_mut();
        let Some(session) = memory.sessions.get_mut(id) else {
            return false;
        };
        if matches!(
            session.record.state,
            SessionState::Stopped
                | SessionState::Failed
                | SessionState::Stopping
                | SessionState::Restarting
        ) {
            return false;
        }
        session.record.state = state;
        session.record.updated_at_ms = now_ms();
        true
    }

    pub fn set_session_stopped(&self, id: &str) {
        set_terminal_session(&mut self.state.borrow_mut(), id, SessionState::Stopped);
    }

    pub fn set_session_failed(&self, id: &str) {
        set_terminal_session(&mut self.state.borrow_mut(), id, SessionState::Failed);
    }

    pub fn rekey_session(&self, from: &str, to: &str) -> Result<()> {
        if from == to {
            return self
                .state
                .borrow()
                .sessions
                .contains_key(from)
                .then_some(())
                .context("session not found");
        }

        let mut state = self.state.borrow_mut();
        if state
            .sessions
            .get(to)
            .is_some_and(|session| !session.record.state.is_terminal())
        {
            bail!("active session id already exists: {to}");
        }
        let replaced = state.sessions.remove(to);
        let mut session = state.sessions.remove(from).context("session not found")?;
        if let Some(replaced) = replaced {
            session.record.created_at_ms = session
                .record
                .created_at_ms
                .min(replaced.record.created_at_ms);
        }
        to.clone_into(&mut session.record.id);
        session.record.updated_at_ms = now_ms();
        state.sessions.insert(to.to_owned(), session);

        for turn in state.turns.values_mut() {
            if turn.session_id == from {
                to.clone_into(&mut turn.session_id);
            }
        }
        for event in &mut state.events {
            if event.session_id.as_deref() == Some(from) {
                event.session_id = Some(to.to_owned());
            }
        }
        let mut merged = state.outputs.remove(to).unwrap_or_default();
        if let Some(mut launched) = state.outputs.remove(from) {
            merged.append(&mut launched);
        }
        if !merged.is_empty() {
            merged.make_contiguous().sort_by_key(|chunk| chunk.seq);
            while merged.len() > OUTPUT_CHUNK_LIMIT {
                merged.pop_front();
            }
            state.outputs.insert(to.to_owned(), merged);
        }
        Ok(())
    }

    /// Reserve a provider conversation before launching a replacement runtime.
    /// This closes the check/launch race between concurrent `--resume` calls.
    pub fn reserve_provider_session(&self, provider_ref: &str, session_id: &str) -> bool {
        let mut state = self.state.borrow_mut();
        if state.provider_reservations.contains_key(provider_ref)
            || state
                .sessions
                .get(provider_ref)
                .is_some_and(|session| !session.record.state.is_terminal())
        {
            return false;
        }
        state
            .provider_reservations
            .insert(provider_ref.to_owned(), session_id.to_owned());
        true
    }

    pub fn release_provider_session(&self, provider_ref: &str, session_id: &str) {
        let mut state = self.state.borrow_mut();
        if state
            .provider_reservations
            .get(provider_ref)
            .map(String::as_str)
            == Some(session_id)
        {
            state.provider_reservations.remove(provider_ref);
        }
    }

    pub fn set_terminal_size(&self, session_id: &str, rows: u16, cols: u16) {
        if let Some(session) = self.state.borrow_mut().sessions.get_mut(session_id) {
            session.terminal_rows = rows;
            session.terminal_cols = cols;
            session.record.updated_at_ms = now_ms();
        }
    }

    pub fn terminal_size(&self, session_id: &str) -> Result<(u16, u16)> {
        self.state
            .borrow()
            .sessions
            .get(session_id)
            .map(|session| (session.terminal_rows, session.terminal_cols))
            .context("failed to read terminal size")
    }

    pub fn get_session(&self, selector: &str) -> Option<SessionRecord> {
        let state = self.state.borrow();
        if let Some(session) = state.sessions.get(selector) {
            return Some(session.record.clone());
        }
        state
            .sessions
            .values()
            .filter(|session| {
                session.record.alias == selector && !session.record.state.is_terminal()
            })
            .max_by_key(|session| session.record.created_at_ms)
            .map(|session| session.record.clone())
    }

    pub fn list_sessions(&self) -> Vec<SessionRecord> {
        let mut sessions = self
            .state
            .borrow()
            .sessions
            .values()
            .map(|session| session.record.clone())
            .collect::<Vec<_>>();
        sessions.sort_by_key(|session| std::cmp::Reverse(session.created_at_ms));
        sessions
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
        if session.record.active_turn_id.is_some() || session.record.state != SessionState::Idle {
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
            state: TurnState::Submitted,
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

    pub fn get_turn(&self, id: &str) -> Option<TurnRecord> {
        self.state.borrow().turns.get(id).cloned()
    }

    pub fn latest_turn(&self, session_id: &str) -> Option<TurnRecord> {
        self.state
            .borrow()
            .turns
            .values()
            .filter(|turn| turn.session_id == session_id)
            .max_by_key(|turn| turn.execution_seq)
            .cloned()
    }

    pub fn mark_turn_started(&self, id: &str, provider_turn_id: Option<&str>) -> bool {
        let mut state = self.state.borrow_mut();
        let Some(turn) = state.turns.get_mut(id) else {
            return false;
        };
        if turn.state != TurnState::Submitted {
            return false;
        }
        turn.state = TurnState::Running;
        if turn.provider_turn_id.is_none() {
            turn.provider_turn_id = provider_turn_id.map(str::to_owned);
        }
        turn.started_at_ms.get_or_insert_with(now_ms);
        true
    }

    pub fn complete_turn_if_matching(
        &self,
        id: &str,
        provider_turn_id: Option<&str>,
        final_message: Option<&str>,
    ) -> Result<bool> {
        self.finish_turn_if_matching(
            id,
            provider_turn_id,
            TurnState::Completed,
            final_message,
            None,
        )
    }

    pub fn finish_turn_if_matching(
        &self,
        id: &str,
        provider_turn_id: Option<&str>,
        state: TurnState,
        final_message: Option<&str>,
        error: Option<&str>,
    ) -> Result<bool> {
        if !state.is_provider_terminal() {
            bail!("invalid terminal turn state {:?}", state.as_str());
        }
        let mut memory = self.state.borrow_mut();
        let turn = memory.turns.get_mut(id).context("turn not found")?;
        if !turn.state.is_active()
            || provider_turn_id.is_some()
                && turn.provider_turn_id.is_some()
                && turn.provider_turn_id.as_deref() != provider_turn_id
        {
            return Ok(false);
        }
        let session_id = turn.session_id.clone();
        turn.state = state;
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

    pub fn interrupt_active_turn(&self, session_id: &str, error: &str) -> Option<String> {
        let mut state = self.state.borrow_mut();
        let session = state.sessions.get(session_id)?;
        let turn_id = session.record.active_turn_id.clone()?;
        let turn = state.turns.get_mut(&turn_id)?;
        if !turn.state.is_active() {
            return None;
        }
        turn.state = TurnState::Interrupted;
        turn.error = Some(error.to_owned());
        turn.completed_at_ms = Some(now_ms());
        if let Some(session) = state.sessions.get_mut(session_id) {
            session.record.active_turn_id = None;
            session.record.updated_at_ms = now_ms();
        }
        Some(turn_id)
    }

    pub fn cancel_turn(&mut self, id: &str) -> Result<bool> {
        let mut state = self.state.borrow_mut();
        let Some(turn) = state.turns.get_mut(id) else {
            bail!("turn not found");
        };
        if !turn.state.is_active() {
            return Ok(false);
        }
        turn.state = TurnState::Canceled;
        turn.completed_at_ms = Some(now_ms());
        let session_id = turn.session_id.clone();
        if let Some(session) = state.sessions.get_mut(&session_id)
            && session.record.active_turn_id.as_deref() == Some(id)
        {
            session.record.state = SessionState::Quiescing;
            session.record.updated_at_ms = now_ms();
        }
        Ok(true)
    }

    pub fn settle_canceled_turn(&self, id: &str, provider_turn_id: Option<&str>) -> Result<bool> {
        let mut state = self.state.borrow_mut();
        let turn = state.turns.get(id).context("turn not found")?;
        if turn.state != TurnState::Canceled
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
            || session.record.state != SessionState::Quiescing
        {
            return Ok(false);
        }
        session.record.active_turn_id = None;
        session.record.state = SessionState::Idle;
        session.record.updated_at_ms = now_ms();
        Ok(true)
    }

    pub fn record_event(&self, session_id: Option<&str>, turn_id: Option<&str>, kind: &str) -> i64 {
        self.record_event_with_retry_attempt(session_id, turn_id, kind, None)
    }

    pub fn record_provider_retry_event(
        &self,
        session_id: Option<&str>,
        turn_id: Option<&str>,
        attempt: u64,
    ) -> i64 {
        self.record_event_with_retry_attempt(
            session_id,
            turn_id,
            "provider.error.retrying",
            Some(attempt),
        )
    }

    fn record_event_with_retry_attempt(
        &self,
        session_id: Option<&str>,
        turn_id: Option<&str>,
        kind: &str,
        retry_attempt: Option<u64>,
    ) -> i64 {
        let mut state = self.state.borrow_mut();
        state.next_event_seq += 1;
        let seq = state.next_event_seq;
        state.events.push(EventRecord {
            seq,
            session_id: session_id.map(str::to_owned),
            turn_id: turn_id.map(str::to_owned),
            kind: kind.to_owned(),
            retry_attempt,
        });
        seq
    }

    /// Allocate an acknowledgement sequence without retaining input bytes or
    /// metadata. The sequence is emitted in the corresponding input event.
    pub fn allocate_input_sequence(&self) -> i64 {
        let mut state = self.state.borrow_mut();
        state.next_input_seq += 1;
        state.next_input_seq
    }

    pub fn record_output(&self, session_id: &str, data: &[u8]) {
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
    }

    pub fn read_output_page(&self, session_id: &str, after: i64, limit_bytes: usize) -> OutputPage {
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
        OutputPage {
            data: output,
            next_after,
            has_more,
        }
    }

    pub fn read_events(&self, session_id: Option<&str>, after: i64) -> Vec<EventRecord> {
        self.state
            .borrow()
            .events
            .iter()
            .filter(|event| {
                event.seq > after
                    && session_id.is_none_or(|id| event.session_id.as_deref() == Some(id))
            })
            .cloned()
            .collect()
    }
}

fn set_terminal_session(state: &mut MemoryState, id: &str, terminal_state: SessionState) {
    if let Some(session) = state.sessions.get_mut(id) {
        session.record.state = terminal_state;
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

#[cfg(test)]
mod tests {
    use super::{NewSession, Store};
    use crate::protocol::{SessionState, TurnState};

    fn mark_ready(store: &Store, session_id: &str) {
        assert!(store.set_session_running(session_id, Some(42)));
        assert!(store.set_session_state(session_id, SessionState::Idle));
    }

    #[test]
    fn retains_explicit_auto_approve_opt_out() {
        let store = Store::new();
        store
            .insert_session(&NewSession {
                id: "claude:thread-1",
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
            .get_session("claude:thread-1")
            .unwrap_or_else(|| panic!("session missing"));
        assert!(!session.auto_approve);
    }

    #[test]
    fn retains_session_turn_event_and_input_sequence() {
        let mut store = Store::new();
        store
            .insert_session(&NewSession {
                id: "codex:thread-1",
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
        mark_ready(&store, "codex:thread-1");
        let turn = store
            .insert_turn("turn_1", "codex:thread-1", "hello")
            .unwrap_or_else(|error| panic!("failed to insert turn: {error}"));
        assert_eq!(turn.state, TurnState::Submitted);
        store.record_event(Some("codex:thread-1"), Some("turn_1"), "turn.submitted");
        assert_eq!(store.allocate_input_sequence(), 1);
        assert_eq!(store.allocate_input_sequence(), 2);
    }

    #[test]
    fn canceled_turn_is_not_a_provider_finish_state() {
        let mut store = Store::new();
        store
            .insert_session(&NewSession {
                id: "codex:thread-1",
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
        mark_ready(&store, "codex:thread-1");
        store
            .insert_turn("turn_1", "codex:thread-1", "hello")
            .unwrap_or_else(|error| panic!("failed to insert turn: {error}"));
        assert!(store.mark_turn_started("turn_1", Some("provider-turn")));

        let error = store
            .finish_turn_if_matching(
                "turn_1",
                Some("provider-turn"),
                TurnState::Canceled,
                None,
                None,
            )
            .err()
            .unwrap_or_else(|| panic!("canceled provider finish unexpectedly succeeded"));
        assert_eq!(
            error.to_string(),
            "invalid terminal turn state \"canceled\""
        );
        assert_eq!(
            store
                .get_turn("turn_1")
                .unwrap_or_else(|| panic!("turn missing"))
                .state,
            TurnState::Running
        );
    }

    #[test]
    fn unscoped_event_reads_include_session_events() {
        let store = Store::new();
        store.record_event(Some("codex:thread-1"), None, "session.started");
        store.record_event(None, None, "runtime.started");

        let events = store.read_events(None, 0);
        assert_eq!(events.len(), 2);
        assert_eq!(store.read_events(Some("codex:thread-1"), 0).len(), 1);
    }

    #[test]
    fn provider_retry_events_keep_only_the_attempt_number() {
        let store = Store::new();
        store.record_provider_retry_event(Some("codex:thread-1"), None, 3);

        let event = store
            .read_events(None, 0)
            .pop()
            .unwrap_or_else(|| panic!("retry event missing"));
        assert_eq!(event.retry_attempt, Some(3));
        assert_eq!(event.kind, "provider.error.retrying");
    }

    #[test]
    fn stopped_session_releases_its_alias() {
        let store = Store::new();
        store
            .insert_session(&NewSession {
                id: "codex:old-session",
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
        mark_ready(&store, "codex:old-session");
        store.set_session_stopped("codex:old-session");
        store
            .insert_session(&NewSession {
                id: "codex:new-session",
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
            .get_session("codex:old-session")
            .unwrap_or_else(|| panic!("old session missing"));
        assert_eq!(archived.alias, "@worker");
        assert_eq!(
            store
                .get_session("@worker")
                .unwrap_or_else(|| panic!("new session missing"))
                .id,
            "codex:new-session"
        );
    }

    #[test]
    fn terminal_session_can_restart_without_losing_identity() {
        let store = Store::new();
        let harness_options = vec!["permission-mode=auto".to_owned()];
        store
            .insert_session(&NewSession {
                id: "claude:provider-thread",
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
        mark_ready(&store, "claude:provider-thread");
        store.set_session_stopped("claude:provider-thread");

        assert!(
            store
                .begin_session_restart("claude:provider-thread")
                .unwrap_or_else(|error| panic!("failed to restart session: {error}"))
        );
        let session = store
            .get_session("claude:provider-thread")
            .unwrap_or_else(|| panic!("session missing"));
        assert_eq!(session.state, SessionState::Restarting);
        assert_eq!(session.id, "claude:provider-thread");
        assert_eq!(session.alias, "@worker");
        assert_eq!(session.harness_options, harness_options);
        assert!(
            !store
                .begin_session_restart("claude:provider-thread")
                .unwrap_or(false)
        );
        assert!(store.start_restarted_session("claude:provider-thread"));
        assert_eq!(
            store
                .get_session("claude:provider-thread")
                .unwrap_or_else(|| panic!("restarted session missing"))
                .state,
            SessionState::Starting
        );
    }

    #[test]
    fn terminal_session_cannot_restart_after_its_alias_is_reused() {
        let store = Store::new();
        for id in ["codex:old-session", "codex:new-session"] {
            store
                .insert_session(&NewSession {
                    id,
                    alias: "@worker",
                    title: "worker",
                    agent: "claude",
                    cwd: "/tmp",
                    model: None,
                    effort: None,
                    harness_options: &[],
                    auto_approve: true,
                })
                .unwrap_or_else(|error| panic!("failed to insert {id}: {error}"));
            mark_ready(&store, id);
            if id == "codex:old-session" {
                store.set_session_stopped(id);
            }
        }
        let error = store
            .begin_session_restart("codex:old-session")
            .err()
            .unwrap_or_else(|| panic!("alias-conflicting restart unexpectedly succeeded"));
        assert!(
            error
                .to_string()
                .contains("active session alias already exists")
        );
    }

    #[test]
    fn active_session_can_enter_restart_without_releasing_its_alias() {
        let store = Store::new();
        store
            .insert_session(&NewSession {
                id: "codex:thread-1",
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
        mark_ready(&store, "codex:thread-1");

        assert!(
            store
                .begin_session_restart("codex:thread-1")
                .unwrap_or_else(|error| panic!("failed to begin restart: {error}"))
        );
        assert_eq!(
            store
                .get_session("@worker")
                .unwrap_or_else(|| panic!("reserved alias missing"))
                .state,
            SessionState::Restarting
        );
        assert!(!store.set_session_state("codex:thread-1", SessionState::Idle));
        assert_eq!(
            store
                .get_session("codex:thread-1")
                .unwrap_or_else(|| panic!("restarting session missing"))
                .state,
            SessionState::Restarting
        );
        assert!(
            store
                .insert_session(&NewSession {
                    id: "codex:session-2",
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
                id: "codex:thread-1",
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
        mark_ready(&store, "codex:thread-1");
        store
            .insert_turn("turn_1", "codex:thread-1", "hello")
            .unwrap_or_else(|error| panic!("failed to insert turn: {error}"));
        let interrupted = store.interrupt_active_turn("codex:thread-1", "agent exited");
        assert_eq!(interrupted.as_deref(), Some("turn_1"));
        let turn = store
            .get_turn("turn_1")
            .unwrap_or_else(|| panic!("turn missing"));
        assert_eq!(turn.state, TurnState::Interrupted);
        assert_eq!(turn.error.as_deref(), Some("agent exited"));
    }

    #[test]
    fn output_reads_are_paginated() {
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
                auto_approve: true,
            })
            .unwrap_or_else(|error| panic!("failed to insert session: {error}"));
        store.record_output("codex:thread-1", b"first");
        store.record_output("codex:thread-1", b"second");
        let first = store.read_output_page("codex:thread-1", 0, 5);
        assert_eq!(first.data, b"first");
        assert!(first.has_more);
        let second = store.read_output_page("codex:thread-1", first.next_after, 5);
        assert_eq!(second.data, b"second");
        assert!(!second.has_more);
    }

    #[test]
    fn only_one_turn_can_claim_a_session() {
        let mut store = Store::new();
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
                auto_approve: true,
            })
            .unwrap_or_else(|error| panic!("failed to insert session: {error}"));
        mark_ready(&store, "codex:thread-1");
        store
            .insert_turn("turn_1", "codex:thread-1", "first")
            .unwrap_or_else(|error| panic!("failed to insert first turn: {error}"));
        assert!(
            store
                .insert_turn("turn_2", "codex:thread-1", "second")
                .is_err()
        );
    }

    #[test]
    fn claude_cannot_claim_a_turn_before_session_start() {
        let mut store = Store::new();
        store
            .insert_session(&NewSession {
                id: "claude:session",
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
        assert!(store.set_session_running("claude:session", Some(42)));

        assert!(
            store
                .insert_turn("turn_1", "claude:session", "first")
                .is_err()
        );
    }

    #[test]
    fn stop_must_match_a_running_provider_turn() {
        let mut store = Store::new();
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
                auto_approve: true,
            })
            .unwrap_or_else(|error| panic!("failed to insert session: {error}"));
        mark_ready(&store, "codex:thread-1");
        store
            .insert_turn("turn_1", "codex:thread-1", "first")
            .unwrap_or_else(|error| panic!("failed to insert turn: {error}"));
        assert!(store.mark_turn_started("turn_1", Some("provider-1")));
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
                id: "codex:thread-1",
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
        mark_ready(&store, "codex:thread-1");
        store
            .insert_turn("turn_1", "codex:thread-1", "first")
            .unwrap_or_else(|error| panic!("failed to insert first turn: {error}"));
        assert!(store.mark_turn_started("turn_1", None));
        assert!(
            store
                .complete_turn_if_matching("turn_1", None, Some("done"))
                .unwrap_or_else(|error| panic!("failed to complete first turn: {error}"))
        );
        store
            .insert_turn("turn_2", "codex:thread-1", "second")
            .unwrap_or_else(|error| panic!("failed to insert second turn: {error}"));
        assert!(
            !store
                .cancel_turn("turn_1")
                .unwrap_or_else(|error| panic!("failed to reject late cancel: {error}"))
        );
        let session = store
            .get_session("codex:thread-1")
            .unwrap_or_else(|| panic!("session missing"));
        assert_eq!(session.active_turn_id.as_deref(), Some("turn_2"));
    }

    #[test]
    fn active_cancel_waits_for_provider_quiescence() {
        let mut store = Store::new();
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
                auto_approve: true,
            })
            .unwrap_or_else(|error| panic!("failed to insert session: {error}"));
        mark_ready(&store, "codex:thread-1");
        store
            .insert_turn("turn_1", "codex:thread-1", "first")
            .unwrap_or_else(|error| panic!("failed to insert turn: {error}"));
        assert!(store.mark_turn_started("turn_1", Some("provider-turn")));
        store.set_session_state("codex:thread-1", SessionState::Busy);
        assert!(
            store
                .cancel_turn("turn_1")
                .unwrap_or_else(|error| panic!("failed to cancel turn: {error}"))
        );
        let session = store
            .get_session("codex:thread-1")
            .unwrap_or_else(|| panic!("session missing"));
        assert_eq!(session.state, SessionState::Quiescing);
        assert_eq!(session.active_turn_id.as_deref(), Some("turn_1"));
        assert!(
            store
                .settle_canceled_turn("turn_1", Some("provider-turn"))
                .unwrap_or_else(|error| panic!("failed to settle canceled turn: {error}"))
        );
        let session = store
            .get_session("codex:thread-1")
            .unwrap_or_else(|| panic!("settled session missing"));
        assert_eq!(session.state, SessionState::Idle);
        assert!(session.active_turn_id.is_none());
    }

    #[test]
    fn terminal_session_cannot_be_resurrected() {
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
                auto_approve: true,
            })
            .unwrap_or_else(|error| panic!("failed to insert session: {error}"));
        store.set_session_stopped("codex:thread-1");
        assert!(!store.set_session_state("codex:thread-1", SessionState::Idle));
        let session = store
            .get_session("codex:thread-1")
            .unwrap_or_else(|| panic!("session missing"));
        assert_eq!(session.state, SessionState::Stopped);
    }

    #[test]
    fn provider_qualified_lookup_and_reservation_are_agent_scoped() {
        let store = Store::new();
        store
            .insert_session(&NewSession {
                id: "codex:shared-id",
                alias: "@codex",
                title: "codex",
                agent: "codex",
                cwd: "/tmp",
                model: None,
                effort: None,
                harness_options: &[],
                auto_approve: true,
            })
            .unwrap_or_else(|error| panic!("failed to insert session: {error}"));
        assert_eq!(
            store
                .get_session("codex:shared-id")
                .map(|session| session.id),
            Some("codex:shared-id".to_owned())
        );
        assert!(!store.reserve_provider_session("codex:shared-id", "codex:new-session"));
        assert!(store.reserve_provider_session("claude:shared-id", "claude:session"));
        store.release_provider_session("claude:shared-id", "claude:session");
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn launch_id_rekeys_every_retained_session_record() {
        let mut store = Store::new();
        store
            .insert_session(&NewSession {
                id: "codex:thread-1",
                alias: "@worker",
                title: "old generation",
                agent: "codex",
                cwd: "/tmp",
                model: None,
                effort: None,
                harness_options: &[],
                auto_approve: true,
            })
            .unwrap_or_else(|error| panic!("failed to insert old Session: {error}"));
        mark_ready(&store, "codex:thread-1");
        store.record_output("codex:thread-1", b"old");
        store.set_session_stopped("codex:thread-1");

        store
            .insert_session(&NewSession {
                id: "internal:ABC12345",
                alias: "@worker",
                title: "new generation",
                agent: "codex",
                cwd: "/tmp",
                model: None,
                effort: None,
                harness_options: &[],
                auto_approve: true,
            })
            .unwrap_or_else(|error| panic!("failed to insert launch Session: {error}"));
        mark_ready(&store, "internal:ABC12345");
        let turn = store
            .insert_turn("turn_rekey", "internal:ABC12345", "hello")
            .unwrap_or_else(|error| panic!("failed to insert turn: {error}"));
        store.record_event(Some("internal:ABC12345"), Some(&turn.id), "turn.submitted");
        store.record_output("internal:ABC12345", b"new");

        store
            .rekey_session("internal:ABC12345", "codex:thread-1")
            .unwrap_or_else(|error| panic!("failed to promote Session: {error}"));

        assert!(store.get_session("internal:ABC12345").is_none());
        assert_eq!(store.list_sessions().len(), 1);
        assert_eq!(
            store
                .get_turn("turn_rekey")
                .unwrap_or_else(|| panic!("turn missing"))
                .session_id,
            "codex:thread-1"
        );
        assert_eq!(store.read_events(Some("codex:thread-1"), 0).len(), 1);
        assert_eq!(
            store.read_output_page("codex:thread-1", 0, 64).data,
            b"oldnew"
        );
    }
}
