use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::cursor::{Cursor, CursorTable};
use crate::protocol::{EventRecord, SessionRecord, SessionState, TurnRecord, TurnState};
use crate::screen::{EpochReason, LIVE_ROW_LIMIT, ScreenStore, StablePage};
use anyhow::{Context, Result, bail};
use uuid::Uuid;

pub struct Store {
    state: RefCell<MemoryState>,
}

const OUTPUT_CHUNK_LIMIT: usize = 8192;
/// Lifecycle events retained per daemon before the oldest are evicted.
const EVENT_RETENTION: usize = 50_000;
/// Terminal results retained per Session.
const RESULT_RETENTION: usize = 128;
/// Retained result bodies per Session.
const RESULT_BODY_RETENTION: usize = 16 * 1024 * 1024;

#[derive(Default)]
struct MemoryState {
    sessions: HashMap<String, StoredSession>,
    /// Every public Session ID this daemon has published, including
    /// pre-rekey identities, mapped to the immutable internal Session UID.
    uid_index: HashMap<String, String>,
    provider_reservations: HashMap<String, String>,
    turns: HashMap<String, TurnRecord>,
    events: VecDeque<EventRecord>,
    /// Highest lifecycle sequence already evicted by retention.
    evicted_event_seq: i64,
    /// Highest execution sequence whose result was evicted, per Session UID.
    evicted_result_seq: HashMap<String, i64>,
    screens: HashMap<String, ScreenStore>,
    /// Observation positions and the watermarks behind them. Held here so
    /// minting a position is atomic with the state capture it describes.
    cursors: CursorTable,
    outputs: HashMap<String, VecDeque<OutputChunk>>,
    next_event_seq: i64,
    next_input_seq: i64,
    next_output_seq: i64,
}

struct StoredSession {
    record: SessionRecord,
    /// Immutable internal identity. Public Session IDs rotate when Claude
    /// reports a new provider session, so cursors bind to this instead.
    uid: String,
    terminal_rows: u16,
    terminal_cols: u16,
}

/// Live screen projection: a replaceable snapshot, never cursor history.
#[derive(Debug, Default)]
pub struct LiveScreen {
    pub epoch: u64,
    pub reset_reason: Option<&'static str>,
    pub rows: Vec<String>,
    pub truncated: bool,
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

struct FinishTurn<'a> {
    id: &'a str,
    provider_turn_id: Option<&'a str>,
    state: TurnState,
    final_message: Option<&'a str>,
    recovered: bool,
    error: Option<&'a str>,
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
        let uid = format!("su_{}", Uuid::new_v4().simple());
        state.uid_index.insert(session.id.to_owned(), uid.clone());
        state.screens.insert(uid.clone(), ScreenStore::new(24, 80));
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
                uid,
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
        // Retained events are append-only. Pre-bind events recorded against an
        // internal launch ID stay exactly as they were written and are never
        // published; the canonical timeline is materialized at bind time.
        let replaced = state.sessions.remove(to);
        let mut session = state.sessions.remove(from).context("session not found")?;
        if let Some(replaced) = replaced {
            session.record.created_at_ms = session
                .record
                .created_at_ms
                .min(replaced.record.created_at_ms);
            // Resuming a retained provider conversation continues one logical
            // Session. Adopt its immutable identity so cursors, screen
            // history, and retention floors survive the replacement process,
            // and retire the launch identity that was only ever a placeholder.
            let launch_uid = std::mem::replace(&mut session.uid, replaced.uid.clone());
            state.screens.remove(&launch_uid);
            state.cursors.forget(&launch_uid);
            state.evicted_result_seq.remove(&launch_uid);
            state
                .uid_index
                .retain(|_, uid| uid.as_str() != launch_uid.as_str());
            // A replacement PTY is still a new terminal generation.
            if let Some(screen) = state.screens.get_mut(&session.uid) {
                screen.restart();
            }
        }
        to.clone_into(&mut session.record.id);
        session.record.updated_at_ms = now_ms();
        // The pre-rekey ID stays resolvable for the daemon lifetime so a
        // concurrent cursor keeps addressing the same logical Session.
        state.uid_index.insert(to.to_owned(), session.uid.clone());
        state.uid_index.insert(from.to_owned(), session.uid.clone());
        state.sessions.insert(to.to_owned(), session);

        for turn in state.turns.values_mut() {
            if turn.session_id == from {
                to.clone_into(&mut turn.session_id);
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
        let mut state = self.state.borrow_mut();
        let Some(session) = state.sessions.get_mut(session_id) else {
            return;
        };
        session.terminal_rows = rows;
        session.terminal_cols = cols;
        session.record.updated_at_ms = now_ms();
        let uid = session.uid.clone();
        if let Some(screen) = state.screens.get_mut(&uid) {
            screen.resize(rows, cols);
        }
    }

    /// Start a new terminal generation for a replacement provider process.
    pub fn restart_screen(&self, session_id: &str) {
        let mut state = self.state.borrow_mut();
        let Some(uid) = state.sessions.get(session_id).map(|s| s.uid.clone()) else {
            return;
        };
        if let Some(screen) = state.screens.get_mut(&uid) {
            screen.restart();
        }
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
            final_text_recovered: false,
            transcript_path: None,
            transcript_offset: None,
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

    pub fn turn_for_execution(&self, session_id: &str, execution_seq: i64) -> Option<TurnRecord> {
        self.state
            .borrow()
            .turns
            .values()
            .find(|turn| turn.session_id == session_id && turn.execution_seq == execution_seq)
            .cloned()
    }

    /// Newest retained *terminal* result. An execution that is still running
    /// must not hide the answer the caller has not read yet.
    pub fn latest_terminal_turn(&self, session_id: &str) -> Option<TurnRecord> {
        self.state
            .borrow()
            .turns
            .values()
            .filter(|turn| turn.session_id == session_id && turn.state.is_terminal())
            .max_by_key(|turn| turn.execution_seq)
            .cloned()
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
        recovered: bool,
    ) -> Result<bool> {
        self.finish_turn(&FinishTurn {
            id,
            provider_turn_id,
            state: TurnState::Completed,
            final_message,
            recovered,
            error: None,
        })
    }

    pub fn finish_turn_if_matching(
        &self,
        id: &str,
        provider_turn_id: Option<&str>,
        state: TurnState,
        final_message: Option<&str>,
        error: Option<&str>,
    ) -> Result<bool> {
        self.finish_turn(&FinishTurn {
            id,
            provider_turn_id,
            state,
            final_message,
            recovered: false,
            error,
        })
    }

    /// Record the provider transcript boundary for an execution, so a later
    /// fallback can never read a previous turn's assistant message.
    pub fn set_turn_transcript(&self, id: &str, path: &str, offset: Option<u64>) {
        if let Some(turn) = self.state.borrow_mut().turns.get_mut(id) {
            turn.transcript_path = Some(path.to_owned());
            turn.transcript_offset = offset;
        }
    }

    fn finish_turn(&self, finish: &FinishTurn<'_>) -> Result<bool> {
        let FinishTurn {
            id,
            provider_turn_id,
            state,
            final_message,
            recovered,
            error,
        } = *finish;
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
        turn.final_text_recovered = recovered;
        turn.error = error.map(str::to_owned);
        turn.completed_at_ms = Some(now_ms());
        if let Some(session) = memory.sessions.get_mut(&session_id)
            && session.record.active_turn_id.as_deref() == Some(id)
        {
            session.record.active_turn_id = None;
            session.record.updated_at_ms = now_ms();
        }
        enforce_result_retention(&mut memory, &session_id);
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
        enforce_result_retention(&mut state, session_id);
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
        enforce_result_retention(&mut state, &session_id);
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
        let scope = session_id.and_then(|id| {
            state
                .sessions
                .get(id)
                .map(|session| session.uid.clone())
                .or_else(|| state.uid_index.get(id).cloned())
        });
        let turn = turn_id.and_then(|id| state.turns.get(id));
        let execution_seq = turn.map(|turn| turn.execution_seq);
        let result_status = turn
            .map(|turn| turn.state)
            .filter(|state| state.is_terminal());
        state.events.push_back(EventRecord {
            seq,
            session_uid: scope,
            session_id: session_id.map(str::to_owned),
            turn_id: turn_id.map(str::to_owned),
            kind: kind.to_owned(),
            retry_attempt,
            execution_seq,
            result_status,
        });
        while state.events.len() > EVENT_RETENTION {
            if let Some(evicted) = state.events.pop_front() {
                state.evicted_event_seq = state.evicted_event_seq.max(evicted.seq);
            }
        }
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
        let uid = state.sessions.get(session_id).map(|s| s.uid.clone());
        if let Some(screen) = uid.and_then(|uid| state.screens.get_mut(&uid)) {
            screen.feed(data);
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

    /// Events after `after`, optionally scoped to one immutable Session UID.
    /// Scoping by UID keeps pre-rekey events readable through the Session's
    /// current address without rewriting what was already published.
    pub fn read_events(&self, uid: Option<&str>, after: i64) -> Vec<EventRecord> {
        self.state
            .borrow()
            .events
            .iter()
            .filter(|event| {
                event.seq > after && uid.is_none_or(|uid| event.session_uid.as_deref() == Some(uid))
            })
            .cloned()
            .collect()
    }

    /// Take the next observation position for a scope.
    pub fn reserve_cursor(&self, scope: &str) -> u64 {
        self.state.borrow_mut().cursors.reserve(scope)
    }

    /// Bind a reserved position to the watermarks it stands for.
    pub fn store_cursor(&self, scope: &str, number: u64, cursor: Cursor) -> Result<()> {
        self.state.borrow_mut().cursors.store(scope, number, cursor)
    }

    pub fn resolve_cursor(&self, scope: &str, value: &str) -> Result<Cursor> {
        self.state.borrow().cursors.resolve(scope, value)
    }

    /// Highest lifecycle sequence already dropped by event retention.
    pub fn evicted_event_seq(&self) -> i64 {
        self.state.borrow().evicted_event_seq
    }

    pub fn latest_event_seq(&self) -> i64 {
        self.state.borrow().next_event_seq
    }

    /// Immutable internal identity for any public Session ID or alias,
    /// including identities this daemon has already rekeyed away from.
    pub fn session_uid(&self, selector: &str) -> Option<String> {
        let state = self.state.borrow();
        if let Some(session) = state.sessions.get(selector) {
            return Some(session.uid.clone());
        }
        if let Some(uid) = state.uid_index.get(selector) {
            return Some(uid.clone());
        }
        drop(state);
        self.get_session(selector).and_then(|session| {
            self.state
                .borrow()
                .sessions
                .get(&session.id)
                .map(|s| s.uid.clone())
        })
    }

    pub fn session_for_uid(&self, uid: &str) -> Option<SessionRecord> {
        self.state
            .borrow()
            .sessions
            .values()
            .find(|session| session.uid == uid)
            .map(|session| session.record.clone())
    }

    pub fn session_uids(&self) -> Vec<String> {
        let state = self.state.borrow();
        let mut uids = state
            .sessions
            .values()
            .map(|session| (session.record.created_at_ms, session.uid.clone()))
            .collect::<Vec<_>>();
        uids.sort_by_key(|(created, uid)| (std::cmp::Reverse(*created), uid.clone()));
        uids.into_iter().map(|(_, uid)| uid).collect()
    }

    /// Drop one retained result, as result retention eventually does.
    #[cfg(test)]
    pub fn evict_turn_for_test(&self, id: &str) {
        self.state.borrow_mut().turns.remove(id);
    }

    /// Highest execution sequence with a retained terminal result.
    pub fn latest_result_seq(&self, uid: &str) -> i64 {
        let state = self.state.borrow();
        let Some(session_id) = state
            .sessions
            .values()
            .find(|session| session.uid == uid)
            .map(|session| session.record.id.clone())
        else {
            return 0;
        };
        state
            .turns
            .values()
            .filter(|turn| turn.session_id == session_id && turn.state.is_terminal())
            .map(|turn| turn.execution_seq)
            .max()
            .unwrap_or(0)
    }

    /// Terminal results after `after`, oldest first.
    pub fn results_after(&self, uid: &str, after: i64, limit: usize) -> (Vec<TurnRecord>, bool) {
        let state = self.state.borrow();
        let Some(session_id) = state
            .sessions
            .values()
            .find(|session| session.uid == uid)
            .map(|session| session.record.id.clone())
        else {
            return (Vec::new(), false);
        };
        let mut results = state
            .turns
            .values()
            .filter(|turn| {
                turn.session_id == session_id
                    && turn.state.is_terminal()
                    && turn.execution_seq > after
            })
            .cloned()
            .collect::<Vec<_>>();
        results.sort_by_key(|turn| turn.execution_seq);
        let has_more = results.len() > limit;
        results.truncate(limit);
        (results, has_more)
    }

    /// Highest execution sequence whose retained result was evicted.
    pub fn evicted_result_seq(&self, uid: &str) -> i64 {
        self.state
            .borrow()
            .evicted_result_seq
            .get(uid)
            .copied()
            .unwrap_or(0)
    }

    pub fn screen_epoch(&self, uid: &str) -> u64 {
        self.state
            .borrow()
            .screens
            .get(uid)
            .map_or(0, ScreenStore::epoch)
    }

    pub fn stable_page(&self, uid: &str, after: u64, limit: usize) -> StablePage {
        self.state
            .borrow()
            .screens
            .get(uid)
            .map_or_else(StablePage::default, |screen| {
                screen.stable_page(after, limit)
            })
    }

    pub fn stable_tail(&self, uid: &str, limit: usize) -> StablePage {
        self.state
            .borrow()
            .screens
            .get(uid)
            .map_or_else(StablePage::default, |screen| screen.stable_tail(limit))
    }

    pub fn stable_head(&self, uid: &str) -> u64 {
        self.state
            .borrow()
            .screens
            .get(uid)
            .map_or(0, ScreenStore::head_row_id)
    }

    pub fn live_screen(&self, uid: &str, limit: usize) -> LiveScreen {
        let state = self.state.borrow();
        let Some(screen) = state.screens.get(uid) else {
            return LiveScreen::default();
        };
        let (rows, truncated) = screen.live_rows(limit.min(LIVE_ROW_LIMIT));
        LiveScreen {
            epoch: screen.epoch(),
            reset_reason: screen.last_reset_reason().map(EpochReason::as_str),
            rows,
            truncated,
        }
    }

    /// Rendered rows addressed by absolute row ID, live grid included.
    pub fn rendered_rows(&self, uid: &str, before: Option<u64>, lines: usize) -> RenderedRows {
        let state = self.state.borrow();
        let Some(screen) = state.screens.get(uid) else {
            return RenderedRows::default();
        };
        let head = screen.head_row_id();
        let (live, _) = screen.live_rows(usize::MAX);
        let live_len = u64::try_from(live.len()).unwrap_or(0);
        let (rows, cols) = screen.size();
        let end = before
            .unwrap_or(head + live_len + 1)
            .min(head + live_len + 1);
        let floor = screen.floor_row_id();
        let start = end
            .saturating_sub(u64::try_from(lines).unwrap_or(0))
            .max(floor);
        let mut selected = Vec::new();
        if start < end.min(head + 1) {
            let page = screen.stable_page(
                start - 1,
                usize::try_from(end.min(head + 1) - start).unwrap_or(0),
            );
            selected.extend(page.lines);
        }
        for (index, row) in live.into_iter().enumerate() {
            let id = head + u64::try_from(index).unwrap_or(0) + 1;
            if id >= start && id < end {
                selected.push(row);
            }
        }
        RenderedRows {
            rows,
            cols,
            lines: selected,
            truncated: start > floor || floor > 1,
            before: (start > floor).then_some(start),
        }
    }
}

#[derive(Debug, Default)]
pub struct RenderedRows {
    pub rows: u16,
    pub cols: u16,
    pub lines: Vec<String>,
    pub truncated: bool,
    pub before: Option<u64>,
}

/// Evict the oldest retained results once a Session exceeds either bound.
/// Eviction is recorded so a cursor that predates it reports a gap instead of
/// silently skipping a result.
fn enforce_result_retention(state: &mut MemoryState, session_id: &str) {
    let Some(uid) = state.sessions.get(session_id).map(|s| s.uid.clone()) else {
        return;
    };
    let mut terminal = state
        .turns
        .values()
        .filter(|turn| turn.session_id == session_id && turn.state.is_terminal())
        .map(|turn| {
            (
                turn.execution_seq,
                turn.id.clone(),
                turn.final_message.as_ref().map_or(0, String::len),
            )
        })
        .collect::<Vec<_>>();
    terminal.sort_by_key(|(seq, _, _)| *seq);
    let mut bytes = terminal.iter().map(|(_, _, len)| *len).sum::<usize>();
    let mut index = 0;
    while index + 1 < terminal.len()
        && (terminal.len() - index > RESULT_RETENTION || bytes > RESULT_BODY_RETENTION)
    {
        let (seq, id, len) = &terminal[index];
        state.turns.remove(id);
        bytes = bytes.saturating_sub(*len);
        let floor = state.evicted_result_seq.entry(uid.clone()).or_default();
        *floor = (*floor).max(*seq);
        index += 1;
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

    fn ready_store() -> Store {
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
        assert!(store.set_session_running("codex:thread-1", Some(42)));
        assert!(store.set_session_state("codex:thread-1", SessionState::Idle));
        store
    }

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
        let store = ready_store();
        store.record_event(Some("codex:thread-1"), None, "session.started");
        store.record_event(None, None, "runtime.started");
        let uid = store
            .session_uid("codex:thread-1")
            .unwrap_or_else(|| panic!("session uid missing"));

        let events = store.read_events(None, 0);
        assert_eq!(events.len(), 2);
        assert_eq!(store.read_events(Some(&uid), 0).len(), 1);
    }

    #[test]
    fn a_running_execution_never_hides_the_previous_result() {
        let mut store = ready_store();
        store
            .insert_turn("turn_1", "codex:thread-1", "first")
            .unwrap_or_else(|error| panic!("failed to insert turn: {error}"));
        assert!(store.mark_turn_started("turn_1", Some("p1")));
        assert!(
            store
                .complete_turn_if_matching("turn_1", Some("p1"), Some("first-answer"), false)
                .unwrap_or_else(|error| panic!("failed to complete turn: {error}"))
        );
        store
            .insert_turn("turn_2", "codex:thread-1", "second")
            .unwrap_or_else(|error| panic!("failed to insert turn: {error}"));
        assert!(store.mark_turn_started("turn_2", Some("p2")));

        assert_eq!(
            store
                .latest_turn("codex:thread-1")
                .map(|turn| turn.execution_seq),
            Some(2)
        );
        let retained = store
            .latest_terminal_turn("codex:thread-1")
            .unwrap_or_else(|| panic!("the previous result was hidden by the active turn"));
        assert_eq!(retained.execution_seq, 1);
        assert_eq!(retained.final_message.as_deref(), Some("first-answer"));
    }

    #[test]
    fn retained_events_are_never_rewritten_by_a_rekey() {
        let store = Store::new();
        store
            .insert_session(&NewSession {
                id: "internal:LAUNCH01",
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
        store.record_event(Some("internal:LAUNCH01"), None, "session.created");
        let launch_uid = store
            .session_uid("internal:LAUNCH01")
            .unwrap_or_else(|| panic!("launch uid missing"));
        let before = store.read_events(None, 0);

        store
            .rekey_session("internal:LAUNCH01", "claude:provider-1")
            .unwrap_or_else(|error| panic!("failed to promote: {error}"));
        store.record_event(Some("claude:provider-1"), None, "session.created");

        let after = store.read_events(None, 0);
        assert_eq!(
            after[0].session_id, before[0].session_id,
            "a retained event was rewritten"
        );
        assert_eq!(after[0].session_uid.as_deref(), Some(launch_uid.as_str()));
        assert_eq!(after[1].session_id.as_deref(), Some("claude:provider-1"));
        assert_eq!(after.len(), 2);
    }

    #[test]
    fn retained_events_survive_result_eviction_and_rekey_unchanged() {
        let mut store = ready_store();
        store
            .insert_turn("turn_1", "codex:thread-1", "hello")
            .unwrap_or_else(|error| panic!("failed to insert turn: {error}"));
        assert!(store.mark_turn_started("turn_1", Some("provider-turn")));
        assert!(
            store
                .complete_turn_if_matching("turn_1", Some("provider-turn"), Some("done"), false)
                .unwrap_or_else(|error| panic!("failed to complete turn: {error}"))
        );
        store.record_event(Some("codex:thread-1"), Some("turn_1"), "turn.completed");
        let uid = store
            .session_uid("codex:thread-1")
            .unwrap_or_else(|| panic!("session uid missing"));
        let before = store.read_events(Some(&uid), 0);
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].execution_seq, Some(1));
        assert_eq!(before[0].result_status, Some(TurnState::Completed));

        // The turn the event described is gone, and the public ID rotated.
        store.evict_turn_for_test("turn_1");
        store
            .rekey_session("codex:thread-1", "codex:thread-2")
            .unwrap_or_else(|error| panic!("failed to rekey: {error}"));

        let after = store.read_events(Some(&uid), 0);
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].execution_seq, Some(1));
        assert_eq!(after[0].result_status, Some(TurnState::Completed));
        assert_eq!(after[0].session_id.as_deref(), Some("codex:thread-1"));
        assert_eq!(
            store.session_uid("codex:thread-1").as_deref(),
            Some(uid.as_str()),
            "the pre-rekey ID must keep addressing the same logical Session"
        );
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
                .complete_turn_if_matching("turn_1", Some("provider-2"), Some("wrong"), false)
                .unwrap_or_else(|error| panic!("failed to reject stop: {error}"))
        );
        assert!(
            store
                .complete_turn_if_matching("turn_1", Some("provider-1"), Some("done"), false)
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
                .complete_turn_if_matching("turn_1", None, Some("done"), false)
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
        let uid = store
            .session_uid("codex:thread-1")
            .unwrap_or_else(|| panic!("session uid missing"));
        // The launch event keeps the identity it was written with; the
        // canonical timeline is recorded after binding.
        assert!(store.read_events(Some(&uid), 0).is_empty());
        store.record_event(Some("codex:thread-1"), None, "session.created");
        assert_eq!(store.read_events(Some(&uid), 0).len(), 1);
        assert_eq!(
            store.read_output_page("codex:thread-1", 0, 64).data,
            b"oldnew"
        );
    }
}
