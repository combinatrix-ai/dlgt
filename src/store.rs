use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::cursor::{Cursor, CursorTable};
use crate::messages::{MessageBody, MessageLog, MessagePage};
use crate::protocol::{
    Approvals, ApprovalsMode, Auth, EventRecord, SessionRecord, SessionState, TurnRecord, TurnState,
};
use crate::requests::{
    Answer, AnswerClaim, Outcome, RequestDraft, RequestRecord, RequestTable, Resolution,
};
use anyhow::{Context, Result, bail};

use crate::error::{Code, WithCode, fail};
use serde_json::{Value, json};
use uuid::Uuid;

pub struct Store {
    state: RefCell<MemoryState>,
}

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
    cancel_controls: HashMap<String, CancelControls>,
    cancel_terminal_seen: HashSet<String>,
    events: VecDeque<EventRecord>,
    /// Highest lifecycle sequence already evicted by retention.
    evicted_event_seq: i64,
    /// Highest execution sequence whose result was evicted, per Session UID.
    evicted_result_seq: HashMap<String, i64>,
    /// Messages and Requests, per Session UID.
    messages: HashMap<String, MessageLog>,
    requests: HashMap<String, RequestTable>,
    steer_attempts: HashMap<String, SteerAttempt>,
    /// Observation positions and the watermarks behind them. Held here so
    /// minting a position is atomic with the state capture it describes.
    cursors: CursorTable,
    next_event_seq: i64,
}

struct StoredSession {
    record: SessionRecord,
    /// Identifies the provider process generation that currently owns this Session.
    runtime_generation: u64,
    /// Immutable internal identity. Public Session IDs rotate when Claude
    /// reports a new provider session, so cursors bind to this instead.
    uid: String,
    /// When the provider last emitted any event.
    last_provider_event_ms: Option<i64>,
    /// Diagnostics retained when the Session failed before it became ready.
    launch_failure: Option<Value>,
}

#[derive(Clone)]
struct SteerAttempt {
    session_uid: String,
    runtime_generation: u64,
    turn_id: String,
    execution_seq: i64,
}

#[derive(Clone, Copy)]
struct CancelControls {
    runtime_generation: u64,
    pending: usize,
}

struct FinishTurn<'a> {
    id: &'a str,
    provider_turn_id: Option<&'a str>,
    state: TurnState,
    final_message: Option<&'a str>,
    recovered: bool,
    error: Option<&'a Value>,
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
    pub approvals: ApprovalsMode,
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
            fail!(AliasInUse, "active session alias already exists");
        }
        let now = now_ms();
        let uid = format!("su_{}", Uuid::new_v4().simple());
        state.uid_index.insert(session.id.to_owned(), uid.clone());
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
                    approvals: Approvals::new(session.approvals),
                    auth: None,
                    auth_policy: None,
                    resolved_model: None,
                    active_turn_id: None,
                    pid: None,
                    created_at_ms: now,
                    updated_at_ms: now,
                },
                runtime_generation: 1,
                uid,
                last_provider_event_ms: None,
                launch_failure: None,
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

    pub fn runtime_generation(&self, id: &str) -> Option<u64> {
        self.state
            .borrow()
            .sessions
            .get(id)
            .map(|session| session.runtime_generation)
    }

    pub fn runtime_generation_is_current(&self, id: &str, generation: u64) -> bool {
        self.runtime_generation(id) == Some(generation)
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
            fail!(AliasInUse, "active session alias already exists");
        }
        // The old provider is about to be replaced. Withdraw its pending
        // Requests now so a delivery failure racing process shutdown can
        // settle only that old attempt, while a successful receipt can still
        // commit as delivered.
        close_requests(&mut state, id);
        let active_turn_id = state
            .sessions
            .get(id)
            .and_then(|session| session.record.active_turn_id.clone());
        let Some(session) = state.sessions.get_mut(id) else {
            return Ok(false);
        };
        session.record.state = SessionState::Restarting;
        session.record.updated_at_ms = now_ms();
        if let Some(turn_id) = active_turn_id {
            state.cancel_controls.remove(&turn_id);
            state.cancel_terminal_seen.remove(&turn_id);
        }
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
        let Some(generation) = session.runtime_generation.checked_add(1) else {
            return false;
        };
        session.runtime_generation = generation;
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

    pub fn set_launch_failure(&self, id: &str, failure: Value) {
        if let Some(session) = self.state.borrow_mut().sessions.get_mut(id) {
            session.launch_failure = Some(failure);
        }
    }

    pub fn launch_failure(&self, id: &str) -> Option<Value> {
        self.state
            .borrow()
            .sessions
            .get(id)
            .and_then(|session| session.launch_failure.clone())
    }

    pub fn rekey_session(&self, from: &str, to: &str) -> Result<()> {
        if from == to {
            return self
                .state
                .borrow()
                .sessions
                .contains_key(from)
                .then_some(())
                .context("session not found")
                .code(Code::NotFound);
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
        let mut session = state
            .sessions
            .remove(from)
            .context("session not found")
            .code(Code::NotFound)?;
        if let Some(replaced) = replaced {
            session.record.created_at_ms = session
                .record
                .created_at_ms
                .min(replaced.record.created_at_ms);
            // Resuming a retained provider conversation continues one logical
            // Session. Adopt its immutable identity so cursors, messages, and
            // retention floors survive the replacement process, and retire
            // the launch identity that was only ever a placeholder.
            let launch_uid = std::mem::replace(&mut session.uid, replaced.uid.clone());
            if let Some(launched) = state.messages.remove(&launch_uid) {
                state
                    .messages
                    .entry(session.uid.clone())
                    .or_default()
                    .absorb(launched);
            }
            state.requests.remove(&launch_uid);
            state.cursors.forget(&launch_uid);
            state.evicted_result_seq.remove(&launch_uid);
            state
                .uid_index
                .retain(|_, uid| uid.as_str() != launch_uid.as_str());
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
            fail!(NotFound, "session not found");
        };
        if session.record.active_turn_id.is_some() || session.record.state != SessionState::Idle {
            fail!(
                SessionBusy,
                "session already has an active turn or is not ready"
            );
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
            denied_requests: 0,
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
        if !matches!(turn.state, TurnState::Submitted | TurnState::Canceled) {
            return false;
        }
        if turn.state == TurnState::Submitted {
            turn.state = TurnState::Running;
        }
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
        error: Option<&Value>,
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
            fail!(
                InvalidArgument,
                "invalid terminal turn state {:?}",
                state.as_str()
            );
        }
        let mut memory = self.state.borrow_mut();
        let turn = memory
            .turns
            .get_mut(id)
            .context("turn not found")
            .code(Code::NotFound)?;
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
        turn.error = error.cloned();
        turn.completed_at_ms = Some(now_ms());
        if let Some(session) = memory.sessions.get_mut(&session_id)
            && session.record.active_turn_id.as_deref() == Some(id)
        {
            session.record.active_turn_id = None;
            session.record.updated_at_ms = now_ms();
        }
        close_requests(&mut memory, &session_id);
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
        turn.error = Some(json!({"code": "interrupted", "message": error}));
        turn.completed_at_ms = Some(now_ms());
        if let Some(session) = state.sessions.get_mut(session_id) {
            session.record.active_turn_id = None;
            session.record.updated_at_ms = now_ms();
        }
        close_requests(&mut state, session_id);
        enforce_result_retention(&mut state, session_id);
        Some(turn_id)
    }

    #[cfg(test)]
    pub fn cancel_turn(&mut self, id: &str) -> Result<bool> {
        let generation = self
            .runtime_generation_for_turn(id)
            .context("turn runtime generation is missing")?;
        self.cancel_turn_with_controls(id, generation, 0)
    }

    pub fn cancel_turn_with_controls(
        &mut self,
        id: &str,
        runtime_generation: u64,
        controls: usize,
    ) -> Result<bool> {
        let mut state = self.state.borrow_mut();
        let session_id = {
            let Some(turn) = state.turns.get(id) else {
                fail!(NotFound, "turn not found");
            };
            if !turn.state.is_active() {
                return Ok(false);
            }
            turn.session_id.clone()
        };
        if !state.sessions.get(&session_id).is_some_and(|session| {
            session.runtime_generation == runtime_generation
                && matches!(
                    session.record.state,
                    SessionState::Busy | SessionState::Blocked
                )
                && session.record.active_turn_id.as_deref() == Some(id)
        }) {
            return Ok(false);
        }
        let Some(turn) = state.turns.get_mut(id) else {
            return Ok(false);
        };
        turn.state = TurnState::Canceled;
        turn.completed_at_ms = Some(now_ms());
        state.cancel_controls.insert(
            id.to_owned(),
            CancelControls {
                runtime_generation,
                pending: controls,
            },
        );
        state.cancel_terminal_seen.remove(id);
        if let Some(session) = state.sessions.get_mut(&session_id)
            && session.record.active_turn_id.as_deref() == Some(id)
        {
            session.record.state = SessionState::Quiescing;
            session.record.updated_at_ms = now_ms();
        }
        close_requests(&mut state, &session_id);
        enforce_result_retention(&mut state, &session_id);
        Ok(true)
    }

    pub fn reserve_cancel_control(&self, id: &str) -> bool {
        let mut state = self.state.borrow_mut();
        let Some(turn) = state
            .turns
            .get(id)
            .filter(|turn| turn.state == TurnState::Canceled)
        else {
            return false;
        };
        let session_id = turn.session_id.clone();
        if !state.sessions.get(&session_id).is_some_and(|session| {
            session.record.state == SessionState::Quiescing
                && session.record.active_turn_id.as_deref() == Some(id)
                && state.cancel_controls.get(id).is_some_and(|controls| {
                    controls.runtime_generation == session.runtime_generation
                })
        }) {
            return false;
        }
        let Some(controls) = state.cancel_controls.get_mut(id) else {
            return false;
        };
        controls.pending = controls.pending.saturating_add(1);
        true
    }

    /// Settle one dispatched cancel control and report whether the provider
    /// has already terminalized the execution and all controls are complete.
    pub fn finish_cancel_control(&self, id: &str) -> bool {
        let mut state = self.state.borrow_mut();
        let current_generation = state
            .turns
            .get(id)
            .and_then(|turn| state.sessions.get(&turn.session_id))
            .map(|session| session.runtime_generation);
        if let Some(controls) = state.cancel_controls.get_mut(id)
            && current_generation == Some(controls.runtime_generation)
        {
            controls.pending = controls.pending.saturating_sub(1);
        }
        let settled = state.cancel_controls.get(id).is_some_and(|controls| {
            current_generation == Some(controls.runtime_generation) && controls.pending == 0
        }) && state.cancel_terminal_seen.contains(id);
        if settled {
            state.cancel_controls.remove(id);
            state.cancel_terminal_seen.remove(id);
        }
        settled
    }

    pub fn cancel_controls_pending(&self, id: &str) -> bool {
        self.state
            .borrow()
            .cancel_controls
            .get(id)
            .is_some_and(|controls| controls.pending > 0)
    }

    pub fn cancel_execution_complete(&self, id: &str) -> bool {
        let state = self.state.borrow();
        state
            .turns
            .get(id)
            .is_some_and(|turn| turn.state.is_terminal())
            && !state.cancel_controls.contains_key(id)
            && !state.cancel_terminal_seen.contains(id)
    }

    pub fn fail_canceled_turn(
        &self,
        id: &str,
        runtime_generation: u64,
        code: &str,
        message: &str,
    ) -> bool {
        let mut state = self.state.borrow_mut();
        let Some(turn) = state.turns.get(id) else {
            return false;
        };
        if turn.state != TurnState::Canceled {
            return false;
        }
        let session_id = turn.session_id.clone();
        if !state.sessions.get(&session_id).is_some_and(|session| {
            session.runtime_generation == runtime_generation
                && session.record.state == SessionState::Quiescing
                && session.record.active_turn_id.as_deref() == Some(id)
        }) || state
            .cancel_controls
            .get(id)
            .is_none_or(|controls| controls.runtime_generation != runtime_generation)
        {
            return false;
        }
        let Some(turn) = state.turns.get_mut(id) else {
            return false;
        };
        turn.state = TurnState::Failed;
        turn.error = Some(json!({"code": code, "message": message}));
        turn.completed_at_ms = Some(now_ms());
        state.cancel_controls.remove(id);
        state.cancel_terminal_seen.remove(id);
        if let Some(session) = state.sessions.get_mut(&session_id) {
            session.record.active_turn_id = None;
            session.record.state = SessionState::Failed;
            session.record.updated_at_ms = now_ms();
        }
        close_requests(&mut state, &session_id);
        enforce_result_retention(&mut state, &session_id);
        true
    }

    pub fn settle_canceled_turn(&self, id: &str, provider_turn_id: Option<&str>) -> Result<bool> {
        let mut state = self.state.borrow_mut();
        let session_id = {
            let turn = state
                .turns
                .get(id)
                .context("turn not found")
                .code(Code::NotFound)?;
            if turn.state != TurnState::Canceled
                || provider_turn_id.is_some()
                    && turn.provider_turn_id.is_some()
                    && turn.provider_turn_id.as_deref() != provider_turn_id
            {
                return Ok(false);
            }
            turn.session_id.clone()
        };
        let runtime_generation = state
            .sessions
            .get(&session_id)
            .map(|session| session.runtime_generation)
            .context("session not found")
            .code(Code::NotFound)?;
        if state
            .cancel_controls
            .get(id)
            .is_none_or(|controls| controls.runtime_generation != runtime_generation)
        {
            return Ok(false);
        }
        state.cancel_terminal_seen.insert(id.to_owned());
        if state
            .cancel_controls
            .get(id)
            .is_some_and(|controls| controls.pending > 0)
        {
            return Ok(false);
        }
        state.cancel_controls.remove(id);
        state.cancel_terminal_seen.remove(id);
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

    #[cfg(test)]
    fn runtime_generation_for_turn(&self, id: &str) -> Option<u64> {
        let state = self.state.borrow();
        let turn = state.turns.get(id)?;
        state
            .sessions
            .get(&turn.session_id)
            .map(|session| session.runtime_generation)
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
    pub fn store_cursor(&self, scope: &str, number: u64, cursor: Cursor) {
        self.state.borrow_mut().cursors.store(scope, number, cursor);
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

    pub fn message_head(&self, uid: &str) -> u64 {
        self.state
            .borrow()
            .messages
            .get(uid)
            .map_or(0, MessageLog::head)
    }

    pub fn messages_after(&self, uid: &str, after: u64, limit: usize) -> MessagePage {
        self.state
            .borrow()
            .messages
            .get(uid)
            .map_or_else(MessagePage::default, |log| log.page_after(after, limit))
    }

    pub fn messages_tail(&self, uid: &str, limit: usize) -> MessagePage {
        self.state
            .borrow()
            .messages
            .get(uid)
            .map_or_else(MessagePage::default, |log| log.tail(limit))
    }

    /// Record a message against the Session's active execution, if any. A
    /// refused approval is counted on that execution's result.
    pub fn append_message(&self, session_id: &str, body: MessageBody) -> Option<u64> {
        let mut state = self.state.borrow_mut();
        let session = state.sessions.get(session_id)?;
        let uid = session.uid.clone();
        let turn_id = session.record.active_turn_id.clone();
        let refuses = body.refuses();
        let execution_seq = turn_id.and_then(|id| {
            let turn = state.turns.get_mut(&id)?;
            if refuses {
                turn.denied_requests += 1;
            }
            Some(turn.execution_seq)
        });
        Some(
            state
                .messages
                .entry(uid)
                .or_default()
                .push(execution_seq, body),
        )
    }

    pub fn begin_steer_attempt(
        &self,
        session_id: &str,
        runtime_generation: u64,
        turn_id: &str,
        execution_seq: i64,
        attempt_id: &str,
        text: &str,
    ) -> bool {
        let mut state = self.state.borrow_mut();
        let Some(session) = state.sessions.get(session_id) else {
            return false;
        };
        if session.runtime_generation != runtime_generation
            || session.record.state != SessionState::Busy
            || session.record.active_turn_id.as_deref() != Some(turn_id)
            || !state
                .turns
                .get(turn_id)
                .is_some_and(|turn| turn.state.is_active() && turn.execution_seq == execution_seq)
            || state.steer_attempts.contains_key(attempt_id)
        {
            return false;
        }
        let uid = session.uid.clone();
        state.steer_attempts.insert(
            attempt_id.to_owned(),
            SteerAttempt {
                session_uid: uid.clone(),
                runtime_generation,
                turn_id: turn_id.to_owned(),
                execution_seq,
            },
        );
        state
            .messages
            .entry(uid)
            .or_default()
            .push(Some(execution_seq), MessageBody::steer(text, false, false));
        true
    }

    pub fn finish_steer_attempt(
        &self,
        session_id: &str,
        runtime_generation: u64,
        attempt_id: &str,
        text: &str,
        delivered: bool,
    ) -> bool {
        let mut state = self.state.borrow_mut();
        let Some(attempt) = state.steer_attempts.get(attempt_id).cloned() else {
            return false;
        };
        let Some(session) = state.sessions.get(session_id) else {
            state.steer_attempts.remove(attempt_id);
            return false;
        };
        let uid = session.uid.clone();
        let session_generation = session.runtime_generation;
        let session_state = session.record.state;
        let active_turn_id = session.record.active_turn_id.clone();
        let owned_attempt = attempt.session_uid == uid
            && attempt.runtime_generation == runtime_generation
            && state.turns.get(&attempt.turn_id).is_some_and(|turn| {
                turn.execution_seq == attempt.execution_seq && (delivered || turn.state.is_active())
            });
        let session_owns_generation = session_generation == runtime_generation;
        let execution_is_active = session_state == SessionState::Busy
            && active_turn_id.as_deref() == Some(attempt.turn_id.as_str());
        state.steer_attempts.remove(attempt_id);
        if !owned_attempt || !session_owns_generation || !delivered && !execution_is_active {
            return false;
        }
        state.messages.entry(uid).or_default().push(
            Some(attempt.execution_seq),
            MessageBody::steer(text, delivered, !delivered),
        );
        true
    }

    pub fn expire_steer_attempt(
        &self,
        session_id: &str,
        runtime_generation: u64,
        attempt_id: &str,
        text: &str,
    ) -> bool {
        let mut state = self.state.borrow_mut();
        let Some(attempt) = state.steer_attempts.get(attempt_id).cloned() else {
            return false;
        };
        let Some(session) = state.sessions.get(session_id) else {
            state.steer_attempts.remove(attempt_id);
            return false;
        };
        let uid = session.uid.clone();
        let session_generation = session.runtime_generation;
        let active_turn_id = session.record.active_turn_id.clone();
        let session_state = session.record.state;
        let owns_generation = attempt.session_uid == uid
            && attempt.runtime_generation == runtime_generation
            && attempt.runtime_generation == session_generation
            && active_turn_id.as_deref() == Some(attempt.turn_id.as_str())
            && matches!(session_state, SessionState::Busy | SessionState::Quiescing)
            && state
                .turns
                .get(&attempt.turn_id)
                .is_some_and(|turn| turn.execution_seq == attempt.execution_seq);
        state.steer_attempts.remove(attempt_id);
        if !owns_generation {
            return false;
        }
        state.messages.entry(uid).or_default().push(
            Some(attempt.execution_seq),
            MessageBody::steer(text, false, true),
        );
        true
    }

    /// Note that the provider emitted something, for `quiet_for_ms`.
    pub fn touch_provider(&self, session_id: &str) {
        if let Some(session) = self.state.borrow_mut().sessions.get_mut(session_id) {
            session.last_provider_event_ms = Some(now_ms());
        }
    }

    pub fn last_provider_event_ms(&self, session_id: &str) -> Option<i64> {
        self.state
            .borrow()
            .sessions
            .get(session_id)
            .and_then(|session| session.last_provider_event_ms)
    }

    /// Open a Request and block the Session on it. A Session that is not
    /// running an execution cannot be blocked, so the Request is not opened.
    pub fn open_request(&self, session_id: &str, draft: RequestDraft) -> Option<RequestRecord> {
        let (record, blocked_now) = {
            let mut state = self.state.borrow_mut();
            let session = state.sessions.get(session_id)?;
            if !matches!(
                session.record.state,
                SessionState::Busy | SessionState::Blocked
            ) {
                return None;
            }
            let uid = session.uid.clone();
            let blocked_now = session.record.state == SessionState::Busy;
            let execution_seq = session
                .record
                .active_turn_id
                .as_ref()
                .and_then(|id| state.turns.get(id))
                .map(|turn| turn.execution_seq);
            let record =
                state
                    .requests
                    .entry(uid)
                    .or_default()
                    .open(draft, execution_seq, now_ms());
            if let Some(session) = state.sessions.get_mut(session_id) {
                session.record.state = SessionState::Blocked;
                session.record.updated_at_ms = now_ms();
            }
            (record, blocked_now)
        };
        if blocked_now {
            let turn_id = self
                .get_session(session_id)
                .and_then(|session| session.active_turn_id);
            self.record_event(Some(session_id), turn_id.as_deref(), "session.blocked");
        }
        Some(record)
    }

    pub fn request(&self, session_id: &str, id: &str) -> Option<RequestRecord> {
        let state = self.state.borrow();
        let uid = &state.sessions.get(session_id)?.uid;
        state.requests.get(uid)?.get(id).cloned()
    }

    pub fn pending_requests(&self, uid: &str) -> Vec<RequestRecord> {
        self.state
            .borrow()
            .requests
            .get(uid)
            .map(RequestTable::pending)
            .unwrap_or_default()
    }

    pub fn begin_request_answer(
        &self,
        session_id: &str,
        id: &str,
        answer: &Answer,
    ) -> Option<AnswerClaim> {
        let mut state = self.state.borrow_mut();
        let uid = state.sessions.get(session_id)?.uid.clone();
        state.requests.get_mut(&uid)?.begin_answer(id, answer)
    }

    pub fn answer_attempt_status(
        &self,
        session_id: &str,
        id: &str,
        attempt_id: u64,
    ) -> Option<(crate::requests::AnswerAttemptState, Option<Resolution>)> {
        let state = self.state.borrow();
        let uid = &state.sessions.get(session_id)?.uid;
        state
            .requests
            .get(uid)?
            .answer_attempt_status(id, attempt_id)
    }

    pub fn answer_attempt_owns_execution(
        &self,
        session_id: &str,
        runtime_generation: u64,
        request_id: &str,
        attempt_id: u64,
        turn_id: &str,
    ) -> bool {
        let state = self.state.borrow();
        let Some(session) = state.sessions.get(session_id) else {
            return false;
        };
        if session.runtime_generation != runtime_generation
            || !matches!(
                session.record.state,
                SessionState::Busy | SessionState::Blocked
            )
            || session.record.active_turn_id.as_deref() != Some(turn_id)
        {
            return false;
        }
        let Some(turn) = state.turns.get(turn_id) else {
            return false;
        };
        if !turn.state.is_active() {
            return false;
        }
        let Some(request) = state
            .requests
            .get(&session.uid)
            .and_then(|table| table.get(request_id))
        else {
            return false;
        };
        request.answer_state == crate::requests::AnswerState::Sending
            && request.answer_attempt_id == Some(attempt_id)
            && request.execution_seq == Some(turn.execution_seq)
    }

    pub fn execution_is_current(
        &self,
        session_id: &str,
        runtime_generation: u64,
        turn_id: &str,
        execution_seq: i64,
    ) -> bool {
        let state = self.state.borrow();
        let Some(session) = state.sessions.get(session_id) else {
            return false;
        };
        let Some(turn) = state.turns.get(turn_id) else {
            return false;
        };
        session.record.state == SessionState::Busy
            && session.runtime_generation == runtime_generation
            && session.record.active_turn_id.as_deref() == Some(turn_id)
            && turn.state.is_active()
            && turn.execution_seq == execution_seq
    }

    pub fn execution_can_cancel(
        &self,
        session_id: &str,
        runtime_generation: u64,
        turn_id: &str,
        execution_seq: i64,
    ) -> bool {
        let state = self.state.borrow();
        let Some(session) = state.sessions.get(session_id) else {
            return false;
        };
        let Some(turn) = state.turns.get(turn_id) else {
            return false;
        };
        session.runtime_generation == runtime_generation
            && matches!(
                session.record.state,
                SessionState::Busy | SessionState::Blocked
            )
            && session.record.active_turn_id.as_deref() == Some(turn_id)
            && turn.state.is_active()
            && turn.execution_seq == execution_seq
    }

    pub fn release_answer_waiter(&mut self, session_id: &str, id: &str, attempt_id: u64) {
        let mut state = self.state.borrow_mut();
        let Some(uid) = state
            .sessions
            .get(session_id)
            .map(|session| session.uid.clone())
        else {
            return;
        };
        if let Some(table) = state.requests.get_mut(&uid) {
            table.release_answer_waiter(id, attempt_id);
        }
    }

    pub fn finish_request_answer(
        &self,
        session_id: &str,
        id: &str,
        attempt_id: u64,
        answer: &Answer,
        outcome: Outcome,
        receipt: Value,
    ) -> bool {
        let resolved = {
            let mut state = self.state.borrow_mut();
            let Some(uid) = state
                .sessions
                .get(session_id)
                .map(|session| session.uid.clone())
            else {
                return false;
            };
            state
                .requests
                .get_mut(&uid)
                .and_then(|table| {
                    table.deliver_with_receipt(
                        id,
                        attempt_id,
                        answer,
                        Resolution {
                            outcome,
                            answer: Some(answer.clone()),
                        },
                        receipt,
                    )
                })
                .is_some()
        };
        if resolved {
            self.unblock_if_settled(session_id);
        }
        resolved
    }

    pub fn retry_request_answer(
        &self,
        session_id: &str,
        id: &str,
        attempt_id: u64,
        answer: &Answer,
    ) -> bool {
        let mut state = self.state.borrow_mut();
        let Some(uid) = state
            .sessions
            .get(session_id)
            .map(|session| session.uid.clone())
        else {
            return false;
        };
        state
            .requests
            .get_mut(&uid)
            .is_some_and(|table| table.retry_answer(id, attempt_id, answer))
    }

    pub fn fail_request_answer(
        &self,
        session_id: &str,
        id: &str,
        attempt_id: u64,
        answer: &Answer,
    ) -> bool {
        let mut state = self.state.borrow_mut();
        let Some(uid) = state
            .sessions
            .get(session_id)
            .map(|session| session.uid.clone())
        else {
            return false;
        };
        state
            .requests
            .get_mut(&uid)
            .is_some_and(|table| table.fail_answer(id, attempt_id, answer))
    }

    /// Settle an old delivery failure only when the provider has already
    /// withdrawn the Request. This deliberately does not change Session or
    /// execution state, which may belong to a replacement runtime.
    pub fn fail_withdrawn_request_answer(
        &self,
        session_id: &str,
        id: &str,
        attempt_id: u64,
        answer: &Answer,
    ) -> bool {
        let mut state = self.state.borrow_mut();
        let Some(uid) = state
            .sessions
            .get(session_id)
            .map(|session| session.uid.clone())
        else {
            return false;
        };
        state
            .requests
            .get_mut(&uid)
            .is_some_and(|table| table.fail_withdrawn_answer(id, attempt_id, answer))
    }

    /// Close pending Requests as canceled: all of them, or the one the
    /// provider withdrew.
    pub fn cancel_requests(
        &self,
        session_id: &str,
        provider_key: Option<&str>,
    ) -> Vec<RequestRecord> {
        let closed = {
            let mut state = self.state.borrow_mut();
            let Some(uid) = state.sessions.get(session_id).map(|s| s.uid.clone()) else {
                return Vec::new();
            };
            state
                .requests
                .get_mut(&uid)
                .map(|table| table.cancel(provider_key))
                .unwrap_or_default()
        };
        if !closed.is_empty() {
            self.unblock_if_settled(session_id);
        }
        closed
    }

    fn unblock_if_settled(&self, session_id: &str) {
        let turn_id = {
            let mut state = self.state.borrow_mut();
            let Some(session) = state.sessions.get(session_id) else {
                return;
            };
            if session.record.state != SessionState::Blocked
                || state
                    .requests
                    .get(&session.uid)
                    .is_some_and(RequestTable::has_pending)
            {
                return;
            }
            let Some(session) = state.sessions.get_mut(session_id) else {
                return;
            };
            session.record.state = SessionState::Busy;
            session.record.updated_at_ms = now_ms();
            session.record.active_turn_id.clone()
        };
        self.record_event(Some(session_id), turn_id.as_deref(), "session.resumed");
    }

    /// Record the mode the provider says it runs, when it differs from the
    /// one the Session asked for.
    pub fn set_effective_approvals(
        &self,
        session_id: &str,
        effective: ApprovalsMode,
        reason: &str,
    ) -> bool {
        let mut state = self.state.borrow_mut();
        let Some(session) = state.sessions.get_mut(session_id) else {
            return false;
        };
        let approvals = &mut session.record.approvals;
        if approvals.effective == effective {
            return false;
        }
        approvals.effective = effective;
        approvals.reason = (effective != approvals.requested).then(|| reason.to_owned());
        session.record.updated_at_ms = now_ms();
        true
    }

    /// Returns whether the reported authentication is new information.
    pub fn set_auth(&self, session_id: &str, auth: Auth) -> bool {
        let mut state = self.state.borrow_mut();
        let Some(session) = state.sessions.get_mut(session_id) else {
            return false;
        };
        let changed = session.record.auth != Some(auth);
        session.record.auth = Some(auth);
        changed
    }

    pub fn set_auth_policy(&self, session_id: &str, auth_policy: crate::protocol::AuthPolicy) {
        if let Some(session) = self.state.borrow_mut().sessions.get_mut(session_id) {
            session.record.auth_policy = Some(auth_policy);
        }
    }

    pub fn set_resolved_model(&self, session_id: &str, model: &str) {
        if let Some(session) = self.state.borrow_mut().sessions.get_mut(session_id) {
            session.record.resolved_model = Some(model.to_owned());
        }
    }

    /// Replace the launch selection a restart or resume reuses.
    pub fn set_requested_approvals(&self, session_id: &str, requested: ApprovalsMode) {
        if let Some(session) = self.state.borrow_mut().sessions.get_mut(session_id) {
            session.record.approvals = Approvals::new(requested);
        }
    }

    pub fn set_turn_usage(&self, id: &str, usage: Value) {
        if let Some(turn) = self.state.borrow_mut().turns.get_mut(id) {
            turn.usage = Some(usage);
        }
    }

    /// Raise an execution's denial count to what the provider itself counted.
    pub fn raise_denied_requests(&self, id: &str, count: u32) {
        if let Some(turn) = self.state.borrow_mut().turns.get_mut(id) {
            turn.denied_requests = turn.denied_requests.max(count);
        }
    }
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

fn close_requests(state: &mut MemoryState, session_id: &str) {
    if let Some(uid) = state.sessions.get(session_id).map(|s| s.uid.clone())
        && let Some(table) = state.requests.get_mut(&uid)
    {
        table.cancel(None);
    }
}

fn set_terminal_session(state: &mut MemoryState, id: &str, terminal_state: SessionState) {
    close_requests(state, id);
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
    use crate::messages::{Decider, Decision, MessageBody};
    use crate::protocol::{ApprovalsMode, Auth, SessionState, TurnState};
    use crate::requests::{Answer, Outcome, ProviderRequest, RequestDraft, RequestKind};

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
                approvals: ApprovalsMode::Auto,
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
    fn retains_the_requested_approvals_mode() {
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
                approvals: ApprovalsMode::Ask,
            })
            .unwrap_or_else(|error| panic!("failed to insert session: {error}"));
        let session = store
            .get_session("claude:thread-1")
            .unwrap_or_else(|| panic!("session missing"));
        assert_eq!(session.approvals.requested, ApprovalsMode::Ask);
        assert_eq!(session.approvals.effective, ApprovalsMode::Ask);
        assert!(session.approvals.reason.is_none());
    }

    #[test]
    fn stale_delivery_failure_cannot_fail_a_replaced_execution() {
        let mut store = ready_store();
        let first = store
            .insert_turn("turn_1", "codex:thread-1", "first")
            .unwrap_or_else(|error| panic!("failed to insert first turn: {error}"));
        let generation = store
            .runtime_generation("codex:thread-1")
            .unwrap_or_else(|| panic!("runtime generation missing"));
        assert!(store.mark_turn_started("turn_1", Some("provider-1")));
        assert!(
            store
                .complete_turn_if_matching("turn_1", Some("provider-1"), Some("done"), false)
                .unwrap_or_else(|error| panic!("failed to complete first turn: {error}"))
        );
        assert!(
            store
                .begin_session_restart("codex:thread-1")
                .unwrap_or_else(|error| panic!("failed to begin restart: {error}"))
        );
        assert!(store.start_restarted_session("codex:thread-1"));
        mark_ready(&store, "codex:thread-1");
        store
            .insert_turn("turn_2", "codex:thread-1", "second")
            .unwrap_or_else(|error| panic!("failed to insert second turn: {error}"));
        assert!(store.mark_turn_started("turn_2", Some("provider-2")));
        assert!(store.set_session_state("codex:thread-1", SessionState::Busy));

        // A stale delivery callback may mutate only while its generation and
        // execution still own the Session.
        if store.execution_is_current("codex:thread-1", generation, &first.id, first.execution_seq)
        {
            store.set_session_failed("codex:thread-1");
        }

        let current_generation = store
            .runtime_generation("codex:thread-1")
            .unwrap_or_else(|| panic!("runtime generation missing after restart"));
        assert!(
            store.execution_is_current(
                "codex:thread-1",
                current_generation,
                "turn_2",
                store
                    .get_turn("turn_2")
                    .unwrap_or_else(|| panic!("second turn missing"))
                    .execution_seq,
            )
        );

        assert_eq!(
            store
                .get_session("codex:thread-1")
                .map(|session| session.state),
            Some(SessionState::Busy),
            "a stale callback changed the newer execution"
        );
        assert_eq!(
            store.get_turn("turn_2").map(|turn| turn.state),
            Some(TurnState::Running)
        );
    }

    #[test]
    fn retains_session_turn_and_event() {
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
                approvals: ApprovalsMode::Auto,
            })
            .unwrap_or_else(|error| panic!("failed to insert session: {error}"));
        mark_ready(&store, "codex:thread-1");
        let turn = store
            .insert_turn("turn_1", "codex:thread-1", "hello")
            .unwrap_or_else(|error| panic!("failed to insert turn: {error}"));
        assert_eq!(turn.state, TurnState::Submitted);
        assert_eq!(
            store.record_event(Some("codex:thread-1"), Some("turn_1"), "turn.submitted"),
            1
        );
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
                approvals: ApprovalsMode::Auto,
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
                approvals: ApprovalsMode::Auto,
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
                approvals: ApprovalsMode::Auto,
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
                approvals: ApprovalsMode::Auto,
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
                approvals: ApprovalsMode::Auto,
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
                    approvals: ApprovalsMode::Auto,
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
                approvals: ApprovalsMode::Auto,
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
                    approvals: ApprovalsMode::Auto,
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
                approvals: ApprovalsMode::Auto,
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
        assert_eq!(
            turn.error.as_ref().map(|error| error["message"].as_str()),
            Some(Some("agent exited"))
        );
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
                approvals: ApprovalsMode::Auto,
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
                approvals: ApprovalsMode::Auto,
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
                approvals: ApprovalsMode::Auto,
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
                approvals: ApprovalsMode::Auto,
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
                approvals: ApprovalsMode::Auto,
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
                approvals: ApprovalsMode::Auto,
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
                approvals: ApprovalsMode::Auto,
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
                approvals: ApprovalsMode::Auto,
            })
            .unwrap_or_else(|error| panic!("failed to insert old Session: {error}"));
        mark_ready(&store, "codex:thread-1");
        store.append_message("codex:thread-1", MessageBody::notice("old"));
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
                approvals: ApprovalsMode::Auto,
            })
            .unwrap_or_else(|error| panic!("failed to insert launch Session: {error}"));
        mark_ready(&store, "internal:ABC12345");
        let turn = store
            .insert_turn("turn_rekey", "internal:ABC12345", "hello")
            .unwrap_or_else(|error| panic!("failed to insert turn: {error}"));
        store.record_event(Some("internal:ABC12345"), Some(&turn.id), "turn.submitted");
        store.append_message("internal:ABC12345", MessageBody::notice("new"));

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
        let messages = store.messages_after(&uid, 0, 64).messages;
        assert_eq!(
            messages
                .iter()
                .map(|message| (message.seq, message.execution_seq, message.body.clone()))
                .collect::<Vec<_>>(),
            [
                (1, None, MessageBody::notice("old")),
                (2, Some(1), MessageBody::notice("new")),
            ],
            "the resumed Session continues one message history"
        );
    }

    fn approval_draft(provider_key: &str) -> RequestDraft {
        RequestDraft {
            kind: RequestKind::Approval,
            provider_key: provider_key.to_owned(),
            provider: ProviderRequest::CodexCommand,
            tool: "exec".to_owned(),
            summary: "touch a".to_owned(),
            input: serde_json::json!({"command": "touch a"}),
            cwd: None,
            reason: None,
            text: None,
            choices: Vec::new(),
        }
    }

    fn busy_store() -> Store {
        let mut store = ready_store();
        store
            .insert_turn("turn_1", "codex:thread-1", "hello")
            .unwrap_or_else(|error| panic!("failed to insert turn: {error}"));
        assert!(store.set_session_state("codex:thread-1", SessionState::Busy));
        store
    }

    fn state(store: &Store) -> SessionState {
        store
            .get_session("codex:thread-1")
            .unwrap_or_else(|| panic!("session missing"))
            .state
    }

    #[test]
    fn a_session_is_blocked_exactly_while_a_request_is_pending() {
        let store = busy_store();
        let first = store
            .open_request("codex:thread-1", approval_draft("p1"))
            .unwrap_or_else(|| panic!("request was not opened"));
        let second = store
            .open_request("codex:thread-1", approval_draft("p2"))
            .unwrap_or_else(|| panic!("request was not opened"));
        assert_eq!((first.id.as_str(), second.id.as_str()), ("r1", "r2"));
        assert_eq!(first.execution_seq, Some(1));
        assert_eq!(state(&store), SessionState::Blocked);

        let allowed = Answer::Allow;
        let Some(crate::requests::AnswerClaim::Started(first_attempt)) =
            store.begin_request_answer("codex:thread-1", "r1", &allowed)
        else {
            panic!("first answer attempt did not start");
        };
        assert!(store.finish_request_answer(
            "codex:thread-1",
            "r1",
            first_attempt,
            &allowed,
            Outcome::Allowed,
            serde_json::json!({"resolution": "allowed"}),
        ));
        assert_eq!(state(&store), SessionState::Blocked, "r2 is still pending");
        assert!(
            !store.finish_request_answer(
                "codex:thread-1",
                "r1",
                first_attempt,
                &allowed,
                Outcome::Allowed,
                serde_json::json!({"resolution": "allowed"}),
            ),
            "a Request is resolved once"
        );
        let Some(crate::requests::AnswerClaim::Started(second_attempt)) =
            store.begin_request_answer("codex:thread-1", "r2", &allowed)
        else {
            panic!("second answer attempt did not start");
        };
        assert!(store.finish_request_answer(
            "codex:thread-1",
            "r2",
            second_attempt,
            &allowed,
            Outcome::Allowed,
            serde_json::json!({"resolution": "allowed"}),
        ));
        assert_eq!(state(&store), SessionState::Busy);

        let uid = store
            .session_uid("codex:thread-1")
            .unwrap_or_else(|| panic!("session uid missing"));
        let kinds = store
            .read_events(Some(&uid), 0)
            .into_iter()
            .map(|event| event.kind)
            .collect::<Vec<_>>();
        assert_eq!(kinds, ["session.blocked", "session.resumed"]);
    }

    #[test]
    fn an_idle_session_cannot_be_blocked() {
        let store = ready_store();
        assert!(
            store
                .open_request("codex:thread-1", approval_draft("p1"))
                .is_none()
        );
        assert_eq!(state(&store), SessionState::Idle);
    }

    #[test]
    fn ending_the_execution_closes_its_pending_requests_as_canceled() {
        let mut store = busy_store();
        store.open_request("codex:thread-1", approval_draft("p1"));
        assert!(
            store
                .cancel_turn("turn_1")
                .unwrap_or_else(|error| panic!("failed to cancel: {error}"))
        );
        let uid = store
            .session_uid("codex:thread-1")
            .unwrap_or_else(|| panic!("session uid missing"));
        assert!(store.pending_requests(&uid).is_empty());
        assert_eq!(
            store
                .request("codex:thread-1", "r1")
                .and_then(|request| request.resolution)
                .map(|resolution| resolution.outcome),
            Some(Outcome::Canceled)
        );
        assert_eq!(state(&store), SessionState::Quiescing);
    }

    #[test]
    fn a_refused_approval_is_counted_on_the_running_execution() {
        let store = busy_store();
        for decision in [Decision::Approved, Decision::Denied, Decision::TimedOut] {
            store.append_message(
                "codex:thread-1",
                MessageBody::approval(decision, Decider::ProviderReviewer, None, None, "exec", "x"),
            );
        }
        assert_eq!(
            store.get_turn("turn_1").map(|turn| turn.denied_requests),
            Some(2)
        );
        store.raise_denied_requests("turn_1", 1);
        assert_eq!(
            store.get_turn("turn_1").map(|turn| turn.denied_requests),
            Some(2),
            "the provider's own count never lowers what dlgt observed"
        );
    }

    #[test]
    fn the_effective_approvals_mode_reports_why_it_differs() {
        let store = ready_store();
        assert!(store.set_effective_approvals(
            "codex:thread-1",
            ApprovalsMode::Ask,
            "provider reported permission mode default"
        ));
        let approvals = store
            .get_session("codex:thread-1")
            .unwrap_or_else(|| panic!("session missing"))
            .approvals;
        assert_eq!(approvals.requested, ApprovalsMode::Auto);
        assert_eq!(approvals.effective, ApprovalsMode::Ask);
        assert_eq!(
            approvals.reason.as_deref(),
            Some("provider reported permission mode default")
        );
        assert!(store.set_effective_approvals("codex:thread-1", ApprovalsMode::Auto, "back"));
        assert!(
            store
                .get_session("codex:thread-1")
                .is_some_and(|session| session.approvals.reason.is_none())
        );
        assert!(store.set_auth("codex:thread-1", Auth::Subscription));
        assert!(!store.set_auth("codex:thread-1", Auth::Subscription));
    }
}
