//! Claude transport: one long-lived `claude -p` process per Session, driven
//! over stream-json on stdin and stdout.
//!
//! User turns are JSON lines written to stdin. Lifecycle comes from the
//! events Claude writes back: the replay of a user message is the proof the
//! model read it, and a `result` ends a provider turn. Approvals and
//! questions arrive as `can_use_tool` control requests on the same channel.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt as _;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::config::AuthMismatch;
use crate::messages::{Decider, Decision, MessageBody, one_line};
use crate::protocol::{ApprovalsMode, Auth, AuthPolicy, SessionState, TurnState};
use crate::provider::CommandSpec;
use crate::reaper::{Reaper, Registration};
use crate::requests::{ProviderRequest, RequestDraft, RequestKind};
use crate::store::Store;

const STDERR_TAIL_LIMIT: usize = 4096;
const STDERR_DRAIN_TIMEOUT: Duration = Duration::from_millis(500);
/// Longest one-line outcome kept for a tool result.
const RESULT_SUMMARY_LIMIT: usize = 240;
const WRITER_QUEUE_CAPACITY: usize = 32;
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

pub type EventHandler = Arc<dyn Fn(Value) + Send + Sync>;
pub type ExitHandler = Arc<dyn Fn(Option<i32>) + Send + Sync>;

type ControlWaiters = Arc<Mutex<HashMap<String, Sender<Value>>>>;

pub struct ClaudeProcess {
    pid: u32,
    writer: SyncSender<WriteCommand>,
    controls: ControlWaiters,
    next_control: AtomicU64,
    stderr_tail: Arc<Mutex<String>>,
    /// Closes when the stderr reader has seen end of file.
    stderr_done: Mutex<mpsc::Receiver<()>>,
    _reaper_registration: Registration,
}

struct WriteCommand {
    line: Vec<u8>,
    written: Sender<std::result::Result<(), String>>,
}

impl ClaudeProcess {
    #[allow(clippy::too_many_lines)]
    pub fn spawn(
        spec: &CommandSpec,
        on_event: EventHandler,
        on_exit: ExitHandler,
        reaper: &Arc<Reaper>,
    ) -> Result<Arc<Self>> {
        let mut child = Command::new(&spec.program)
            .args(&spec.args)
            .current_dir(&spec.cwd)
            .env_clear()
            .envs(&spec.environment)
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to start {}", spec.program.display()))?;
        let pid = child.id();
        let registration = reaper.watch(pid)?;
        let mut stdin = child.stdin.take().context("Claude process had no stdin")?;
        let stdout = child
            .stdout
            .take()
            .context("Claude process had no stdout")?;
        let stderr = child
            .stderr
            .take()
            .context("Claude process had no stderr")?;
        let controls: ControlWaiters = Arc::new(Mutex::new(HashMap::new()));
        let stderr_tail = Arc::new(Mutex::new(String::new()));
        let closed = Arc::new(AtomicBool::new(false));

        let (writer, write_rx) = mpsc::sync_channel::<WriteCommand>(WRITER_QUEUE_CAPACITY);
        let writer_closed = Arc::clone(&closed);
        std::thread::Builder::new()
            .name("dlgt-claude-stdin".to_owned())
            .spawn(move || {
                for command in write_rx {
                    let result = stdin
                        .write_all(&command.line)
                        .and_then(|()| stdin.flush())
                        .map_err(|_| "failed to write to the Claude process".to_owned());
                    let failed = result.is_err();
                    let _ = command.written.send(result);
                    if failed {
                        writer_closed.store(true, Ordering::Release);
                        if let Ok(pid) = i32::try_from(pid) {
                            // SAFETY: the provider owns a process group led by this pid.
                            unsafe { libc::kill(-pid, libc::SIGKILL) };
                        }
                        break;
                    }
                }
            })
            .context("failed to start Claude stdin writer thread")?;

        let (drained, drained_signal) = mpsc::channel::<()>();
        let exit_waiters = Arc::clone(&controls);
        let exit_closed = Arc::clone(&closed);
        let exit_watch =
            crate::reaper::watch_exit(child, "dlgt-claude-exit", drained_signal, move |status| {
                if let Ok(mut waiters) = exit_waiters.lock() {
                    waiters.clear();
                }
                exit_closed.store(true, Ordering::Release);
                on_exit(status.and_then(|status| status.code()));
            })?;

        let tail = Arc::clone(&stderr_tail);
        let stderr_stop = Arc::clone(&exit_watch.stop_output);
        let (stderr_closed, stderr_done) = mpsc::channel::<()>();
        std::thread::Builder::new()
            .name("dlgt-claude-stderr".to_owned())
            .spawn(move || {
                retain_tail(
                    crate::reaper::StoppableOutput::new(stderr, stderr_stop),
                    &tail,
                );
                drop(stderr_closed);
            })
            .context("failed to start Claude stderr thread")?;

        let waiters = Arc::clone(&controls);
        std::thread::Builder::new()
            .name("dlgt-claude-events".to_owned())
            .spawn(move || {
                let stdout = crate::reaper::StoppableOutput::new(
                    stdout,
                    Arc::clone(&exit_watch.stop_output),
                );
                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    let Ok(event) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    if event.get("type").and_then(Value::as_str) == Some("control_response") {
                        let response = event.get("response").cloned().unwrap_or(Value::Null);
                        let waiter = response
                            .get("request_id")
                            .and_then(Value::as_str)
                            .and_then(|id| waiters.lock().ok()?.remove(id));
                        if let Some(waiter) = waiter {
                            let _ = waiter.send(response);
                        }
                        continue;
                    }
                    on_event(event);
                }
                drop(drained);
                crate::reaper::output_closed(pid, &exit_watch.exited);
            })
            .context("failed to start Claude event thread")?;

        Ok(Arc::new(Self {
            pid,
            writer,
            controls,
            next_control: AtomicU64::new(1),
            stderr_tail,
            stderr_done: Mutex::new(stderr_done),
            _reaper_registration: registration,
        }))
    }

    pub const fn pid(&self) -> u32 {
        self.pid
    }

    pub fn stderr_tail(&self) -> String {
        self.stderr_tail
            .lock()
            .map(|tail| tail.trim().to_owned())
            .unwrap_or_default()
    }

    /// The stderr tail of a process that has exited or is about to: waits
    /// briefly for the reader to drain what the process wrote last.
    pub fn final_stderr_tail(&self) -> String {
        if let Ok(done) = self.stderr_done.lock() {
            let _ = done.recv_timeout(STDERR_DRAIN_TIMEOUT);
        }
        self.stderr_tail()
    }

    /// Write one user message. `uuid` comes back on the replay, which is how
    /// the message is matched when the model reads it.
    pub fn send_user(&self, uuid: &str, text: &str) -> Result<()> {
        self.write_line(&json!({
            "type": "user",
            "uuid": uuid,
            "session_id": "",
            "parent_tool_use_id": null,
            "message": {"role": "user", "content": text},
        }))
    }

    /// Write one user message, waiting at most `window` for the write. Past
    /// the window the write continues in the background with the usual
    /// timeout, which stops the process if it expires, and `Ok(false)` says
    /// delivery is still in progress.
    pub fn send_user_within(
        self: &Arc<Self>,
        uuid: &str,
        text: &str,
        window: Duration,
    ) -> Result<bool> {
        let mut line = serde_json::to_vec(&json!({
            "type": "user",
            "uuid": uuid,
            "session_id": "",
            "parent_tool_use_id": null,
            "message": {"role": "user", "content": text},
        }))?;
        line.push(b'\n');
        let (written, result) = mpsc::channel();
        self.enqueue(WriteCommand { line, written })?;
        match result.recv_timeout(window) {
            Ok(Ok(())) => Ok(true),
            Ok(Err(message)) => Err(anyhow!(message)),
            Err(_) => {
                let process = Arc::downgrade(self);
                let remaining = WRITE_TIMEOUT.saturating_sub(window);
                std::thread::Builder::new()
                    .name("dlgt-claude-write-watch".to_owned())
                    .spawn(move || {
                        if !matches!(result.recv_timeout(remaining), Ok(Ok(())))
                            && let Some(process) = process.upgrade()
                        {
                            process.force_stop();
                        }
                    })
                    .context("failed to start Claude write watcher")?;
                Ok(false)
            }
        }
    }

    /// Send a control request and wait for Claude's response to it. Must not
    /// be called from the event thread, which is what delivers the response.
    pub fn control(&self, request: &Value, timeout: Duration) -> Result<Value> {
        let id = format!("dlgt_{}", self.next_control.fetch_add(1, Ordering::Relaxed));
        let (sender, receiver) = mpsc::channel();
        self.controls
            .lock()
            .map_err(|_| anyhow!("Claude control lock poisoned"))?
            .insert(id.clone(), sender);
        let subtype = request
            .get("subtype")
            .and_then(Value::as_str)
            .unwrap_or("control")
            .to_owned();
        let deadline = Instant::now() + timeout;
        if let Err(error) = self.write_line_until(
            &json!({"type": "control_request", "request_id": id, "request": request}),
            deadline,
        ) {
            if let Ok(mut controls) = self.controls.lock() {
                controls.remove(&id);
            }
            return Err(error);
        }
        let response = receiver
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| {
                if let Ok(mut controls) = self.controls.lock() {
                    controls.remove(&id);
                }
                anyhow!("Claude did not answer the {subtype} control request")
            })?;
        if response.get("subtype").and_then(Value::as_str) != Some("success") {
            bail!(
                "Claude rejected the {subtype} control request: {}",
                response
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error")
            );
        }
        Ok(response.get("response").cloned().unwrap_or(Value::Null))
    }

    /// Send a control request without waiting, for use on the event thread.
    pub fn control_nowait(&self, request: &Value) -> Result<()> {
        let id = format!("dlgt_{}", self.next_control.fetch_add(1, Ordering::Relaxed));
        self.write_line_nowait(
            &json!({"type": "control_request", "request_id": id, "request": request}),
        )
    }

    /// Tell Claude a control request will not be answered.
    pub fn refuse(&self, request_id: &str, reason: &str) -> Result<()> {
        self.write_line_nowait(&json!({
            "type": "control_response",
            "response": {"subtype": "error", "request_id": request_id, "error": reason},
        }))
    }

    pub fn respond_delivery(
        &self,
        request_id: &str,
        response: &Value,
    ) -> std::result::Result<(), crate::provider::DeliveryFailure> {
        let mut line = serde_json::to_vec(&json!({
            "type": "control_response",
            "response": {"subtype": "success", "request_id": request_id, "response": response},
        }))
        .map_err(|error| crate::provider::DeliveryFailure::not_handed(error.to_string()))?;
        line.push(b'\n');
        let (written, result) = mpsc::channel();
        if let Err(error) = self.enqueue(WriteCommand { line, written }) {
            return Err(crate::provider::DeliveryFailure::not_handed(
                error.to_string(),
            ));
        }
        match result.recv_timeout(WRITE_TIMEOUT) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(message)) => Err(crate::provider::DeliveryFailure::ambiguous(message)),
            Err(_) => {
                self.force_stop();
                Err(crate::provider::DeliveryFailure::ambiguous(
                    "timed out writing to the Claude process",
                ))
            }
        }
    }

    fn write_line(&self, value: &Value) -> Result<()> {
        self.write_line_until(value, Instant::now() + WRITE_TIMEOUT)
    }

    fn write_line_until(&self, value: &Value, deadline: Instant) -> Result<()> {
        let mut line = serde_json::to_vec(value)?;
        line.push(b'\n');
        let (written, result) = mpsc::channel();
        self.enqueue(WriteCommand { line, written })?;
        match result.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(message)) => Err(anyhow!(message)),
            Err(_) => {
                self.force_stop();
                Err(anyhow!("timed out writing to the Claude process"))
            }
        }
    }

    fn write_line_nowait(&self, value: &Value) -> Result<()> {
        let mut line = serde_json::to_vec(value)?;
        line.push(b'\n');
        let (written, _result) = mpsc::channel();
        self.enqueue(WriteCommand { line, written })
    }

    fn enqueue(&self, command: WriteCommand) -> Result<()> {
        match self.writer.try_send(command) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                self.force_stop();
                Err(anyhow!("Claude stdin writer queue is full"))
            }
            Err(TrySendError::Disconnected(_)) => {
                self.force_stop();
                Err(anyhow!("Claude process stdin is closed"))
            }
        }
    }

    pub fn stop(&self) {
        self.signal(libc::SIGTERM);
    }

    pub fn force_stop(&self) {
        self.signal(libc::SIGKILL);
    }

    fn signal(&self, signal: i32) {
        if let Ok(pid) = i32::try_from(self.pid) {
            // SAFETY: kill has no memory-safety preconditions. The process is
            // spawned as the leader of its own process group.
            unsafe {
                libc::kill(-pid, signal);
            }
        }
    }
}

impl Drop for ClaudeProcess {
    fn drop(&mut self) {
        self.force_stop();
    }
}

fn retain_tail(mut stream: impl Read, tail: &Mutex<String>) {
    let mut buffer = [0_u8; 4096];
    while let Ok(read) = stream.read(&mut buffer) {
        if read == 0 {
            break;
        }
        let Ok(mut tail) = tail.lock() else { break };
        tail.push_str(&String::from_utf8_lossy(&buffer[..read]));
        if tail.len() > STDERR_TAIL_LIMIT {
            let mut cut = tail.len() - STDERR_TAIL_LIMIT;
            while !tail.is_char_boundary(cut) {
                cut += 1;
            }
            tail.drain(..cut);
        }
    }
}

/// A user message written to stdin that the model has not read yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unread {
    pub uuid: String,
    /// The text of a steer; `None` for the execution's own prompt.
    pub steer: Option<String>,
    /// Claude dequeued the message to run it as a provider turn.
    started: bool,
}

/// What dlgt knows about the provider turns that make up one execution.
///
/// An execution is finished only when a `result` has arrived and every
/// message written for it has been replayed: a steer written during text
/// generation runs as a provider turn of its own after the current result,
/// and that turn still belongs to the execution that accepted the steer.
#[derive(Debug, Default)]
pub struct ClaudeTurns {
    unread: Vec<Unread>,
    /// A provider turn is running and its `result` has not arrived.
    turn_open: bool,
    /// Claude has started this execution's own prompt. A `result` before
    /// that belongs to a turn dlgt did not start and cannot end this one.
    engaged: bool,
    /// The first provider auth report was checked before accepting a turn.
    auth_checked: bool,
    result_texts: Vec<String>,
    last_assistant: Option<String>,
    failure: Option<String>,
    usage: Option<Value>,
    denials: u32,
    tools: HashMap<String, String>,
}

/// Something only the process owner can do in response to an event.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// The provider reports a different session ID than the one launched.
    Rekey(String),
    /// Claude reported API-key authentication under the subscription policy.
    AuthMismatch,
    /// A canceled execution's leftover message started a provider turn.
    Interrupt { turn_id: String },
    /// A control request nobody can answer: an unsupported kind, or a
    /// permission request from a turn no execution owns. Left unanswered it
    /// would hold the provider forever.
    Refuse {
        request_id: String,
        reason: &'static str,
    },
}

impl ClaudeTurns {
    pub fn begin(&mut self, uuid: &str) {
        // `system/init` is a process-level event. Keep its result across turns
        // accepted by this long-lived provider process.
        let auth_checked = self.auth_checked;
        *self = Self::default();
        self.auth_checked = auth_checked;
        self.turn_open = true;
        self.unread.push(Unread {
            uuid: uuid.to_owned(),
            steer: None,
            started: false,
        });
    }

    pub fn add_steer(&mut self, uuid: &str, text: &str) {
        self.unread.push(Unread {
            uuid: uuid.to_owned(),
            steer: Some(text.to_owned()),
            started: false,
        });
    }

    pub fn unread(&self) -> Vec<Unread> {
        self.unread.clone()
    }

    /// Drop a message the provider confirmed it will never read.
    pub fn discard(
        &mut self,
        store: &Store,
        session_id: &str,
        runtime_generation: u64,
        uuid: &str,
    ) {
        let Some(unread) = self.take(uuid) else {
            return;
        };
        match unread.steer {
            Some(text) => {
                store.expire_steer_attempt(session_id, runtime_generation, uuid, &text);
            }
            // A prompt canceled while still queued never runs, so no result
            // will follow it.
            None if !unread.started => self.turn_open = false,
            None => {}
        }
    }

    pub fn discard_without_record(&mut self, uuid: &str) {
        let Some(unread) = self.take(uuid) else {
            return;
        };
        if unread.steer.is_none() && !unread.started {
            self.turn_open = false;
        }
    }

    fn take(&mut self, uuid: &str) -> Option<Unread> {
        let index = self.unread.iter().position(|unread| unread.uuid == uuid)?;
        Some(self.unread.remove(index))
    }

    /// Terminalize the active execution once nothing is left to wait for.
    pub fn settle(&mut self, store: &Store, session_id: &str) -> Result<()> {
        if self.turn_open || !self.unread.is_empty() {
            return Ok(());
        }
        let Some(turn_id) = store
            .get_session(session_id)
            .and_then(|session| session.active_turn_id)
        else {
            return Ok(());
        };
        let Some(turn) = store.get_turn(&turn_id) else {
            return Ok(());
        };
        if store.cancel_controls_pending(&turn_id) {
            return Ok(());
        }
        if let Some(usage) = self.usage.take() {
            store.set_turn_usage(&turn_id, usage);
        }
        store.raise_denied_requests(&turn_id, self.denials);
        let text = self.result_texts.join("\n\n");
        let recovered = text.is_empty() && self.last_assistant.is_some();
        let text = if recovered {
            self.last_assistant.clone().unwrap_or_default()
        } else {
            text
        };
        // The lifecycle records the result event; only a report that matched
        // no execution is recorded here, as a diagnostic.
        let unmatched = if turn.state == TurnState::Canceled {
            store.settle_canceled_turn(&turn_id, None)?;
            None
        } else if let Some(failure) = self.failure.take() {
            let error = json!({"code": "provider_error", "message": failure});
            (!store.finish_turn_if_matching(
                &turn_id,
                None,
                TurnState::Failed,
                Some(&text),
                Some(&error),
            )?)
            .then_some("provider.failure_unmatched")
        } else {
            (!store.complete_turn_if_matching(&turn_id, None, Some(&text), recovered)?)
                .then_some("provider.stop_unmatched")
        };
        if let Some(kind) = unmatched {
            store.record_event(Some(session_id), Some(&turn_id), kind);
        }
        let auth_checked = self.auth_checked;
        *self = Self::default();
        self.auth_checked = auth_checked;
        Ok(())
    }
}

/// Apply one Claude stream-json event to the Session.
#[allow(clippy::too_many_lines)]
pub fn apply_event(
    store: &Store,
    session_id: &str,
    runtime_generation: u64,
    turns: &mut ClaudeTurns,
    event: &Value,
) -> Result<Vec<Action>> {
    let Some(session) = store.get_session(session_id) else {
        return Ok(Vec::new());
    };
    store.touch_provider(session_id);
    let text = |name: &str| event.get(name).and_then(Value::as_str);
    let mut actions = Vec::new();
    match (text("type"), text("subtype")) {
        (Some("system"), Some("init")) => {
            if let Some(provider_id) = text("session_id")
                && session.id != format!("claude:{provider_id}")
                && !session.id.starts_with("internal:")
            {
                actions.push(Action::Rekey(provider_id.to_owned()));
            }
            if let Some(model) = text("model") {
                store.set_resolved_model(session_id, model);
            }
            if let Some(source) = text("apiKeySource") {
                let auth = if source == "none" {
                    Auth::Subscription
                } else {
                    Auth::ApiKey
                };
                if store.set_auth(session_id, auth) && auth == Auth::ApiKey {
                    store.append_message(
                        session_id,
                        MessageBody::notice(&format!(
                            "Claude authenticated with an API key ({source}), not the \
                             subscription login; this Session is billed to that key"
                        )),
                    );
                }
            }
            apply_permission_mode(store, session_id, text("permissionMode"));
            turns.auth_checked = true;
            let auth_mismatch = session.auth_policy.unwrap_or_default() == AuthPolicy::Subscription
                && text("apiKeySource") != Some("none");
            if auth_mismatch {
                let error = json!({"code": "AUTH_MISMATCH", "message": AuthMismatch::default().to_string()});
                if let Some(turn_id) = session.active_turn_id.as_deref() {
                    store.finish_turn_if_matching(
                        turn_id,
                        None,
                        TurnState::Failed,
                        None,
                        Some(&error),
                    )?;
                }
                // The daemon stops this process; the Session stops with it.
                let _ = store.begin_stop(session_id, runtime_generation, false);
                actions.push(Action::AuthMismatch);
                return Ok(actions);
            }
            if turns.engaged {
                confirm_started(store, session_id);
            }
        }
        (Some("system"), Some("status")) => {
            apply_permission_mode(store, session_id, text("permissionMode"));
        }
        (Some("system"), Some("permission_denied")) => {
            let by = if session.approvals.effective == ApprovalsMode::Auto {
                Decider::ProviderReviewer
            } else {
                Decider::ProviderPolicy
            };
            store.append_message(
                session_id,
                MessageBody::approval(
                    Decision::Denied,
                    by,
                    None,
                    None,
                    text("tool_name").unwrap_or("tool"),
                    text("message").unwrap_or_default(),
                ),
            );
        }
        (Some("system"), Some("api_retry")) => {
            let attempt = event.get("attempt").and_then(Value::as_u64).unwrap_or(1);
            store.record_provider_retry_event(
                Some(session_id),
                session.active_turn_id.as_deref(),
                attempt,
            );
        }
        (Some("assistant"), _) => apply_assistant(store, session_id, turns, event),
        (Some("user"), _) if event.get("isReplay").and_then(Value::as_bool) == Some(true) => {
            let Some(read) = text("uuid").and_then(|uuid| turns.take(uuid)) else {
                return Ok(actions);
            };
            turns.turn_open = true;
            turns.engaged = true;
            let turn_id = session.active_turn_id.as_deref();
            let canceled = turn_id
                .and_then(|id| store.get_turn(id))
                .is_some_and(|turn| turn.state == TurnState::Canceled);
            if canceled
                && let Some(turn_id) = turn_id
                && store.reserve_cancel_control(turn_id)
            {
                actions.push(Action::Interrupt {
                    turn_id: turn_id.to_owned(),
                });
            }
            match read.steer {
                Some(steer) => {
                    store.finish_steer_attempt(
                        session_id,
                        runtime_generation,
                        &read.uuid,
                        &steer,
                        true,
                    );
                }
                None => confirm_started(store, session_id),
            }
        }
        (Some("user"), _) => apply_tool_results(store, session_id, turns, event),
        (Some("command_lifecycle"), _) => {
            let Some(uuid) = text("command_uuid") else {
                return Ok(actions);
            };
            match text("state") {
                Some("started") => {
                    if let Some(unread) = turns.unread.iter_mut().find(|u| u.uuid == uuid) {
                        unread.started = true;
                        turns.turn_open = true;
                        turns.engaged = true;
                        // Claude dequeued the prompt and began the turn: that
                        // is its acceptance, ahead of the model reading it.
                        let subscription_not_checked = session.auth_policy.unwrap_or_default()
                            == AuthPolicy::Subscription
                            && !turns.auth_checked;
                        if unread.steer.is_none() && !subscription_not_checked {
                            confirm_started(store, session_id);
                        }
                    }
                }
                Some("cancelled") => {
                    turns.discard(store, session_id, runtime_generation, uuid);
                    turns.settle(store, session_id)?;
                }
                _ => {}
            }
        }
        (Some("result"), _) if !turns.engaged => {
            store.record_event(
                Some(session_id),
                session.active_turn_id.as_deref(),
                "provider.stop_unmatched",
            );
        }
        (Some("result"), subtype) => {
            turns.turn_open = false;
            if let Some(result) = text("result").filter(|result| !result.is_empty()) {
                turns.result_texts.push(result.to_owned());
            }
            let failed = event.get("is_error").and_then(Value::as_bool) == Some(true)
                || subtype.is_some_and(|subtype| subtype != "success");
            if failed && turns.failure.is_none() {
                turns.failure = Some(format!(
                    "Claude turn ended with {}{}",
                    subtype.unwrap_or("an error"),
                    text("result")
                        .filter(|result| !result.is_empty())
                        .map(|result| format!(": {}", one_line(result).0))
                        .unwrap_or_default()
                ));
            }
            // Token counts only: the per-iteration breakdown is unbounded.
            if let Some(usage) = event.get("usage").and_then(Value::as_object) {
                turns.usage = Some(Value::Object(
                    usage
                        .iter()
                        .filter(|(_, value)| value.is_number())
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect(),
                ));
            }
            turns.denials += event
                .get("permission_denials")
                .and_then(Value::as_array)
                .map_or(0, |denials| {
                    u32::try_from(denials.len()).unwrap_or(u32::MAX)
                });
            turns.settle(store, session_id)?;
        }
        (Some("control_request"), _) => {
            let Some(provider_key) = text("request_id") else {
                return Ok(actions);
            };
            let request = event.get("request").unwrap_or(&Value::Null);
            let refuse = |reason| Action::Refuse {
                request_id: provider_key.to_owned(),
                reason,
            };
            if request.get("subtype").and_then(Value::as_str) != Some("can_use_tool") {
                actions.push(refuse("dlgt does not handle this control request"));
            } else if !matches!(session.state, SessionState::Busy | SessionState::Blocked)
                || session
                    .active_turn_id
                    .as_deref()
                    .and_then(|turn_id| store.get_turn(turn_id))
                    .is_none_or(|turn| turn.state != TurnState::Running)
            {
                store.append_message(
                    session_id,
                    MessageBody::notice(
                        "Claude asked for permission outside any execution; the request was refused",
                    ),
                );
                actions.push(refuse("no caller is waiting on this turn"));
            } else {
                store.open_request(
                    session_id,
                    tool_request(&session.cwd, provider_key, request),
                );
            }
        }
        (Some("control_cancel_request"), _) => {
            if let Some(provider_key) = text("request_id") {
                store.cancel_requests(session_id, Some(provider_key));
            }
        }
        _ => {}
    }
    Ok(actions)
}

fn confirm_started(store: &Store, session_id: &str) {
    if let Some(turn_id) = store
        .get_session(session_id)
        .and_then(|session| session.active_turn_id)
    {
        store.mark_turn_started(&turn_id, None);
    }
}

pub fn apply_permission_mode(store: &Store, session_id: &str, mode: Option<&str>) {
    let Some(mode) = mode else { return };
    let effective = match mode {
        "auto" => ApprovalsMode::Auto,
        "bypassPermissions" => ApprovalsMode::Bypass,
        _ => ApprovalsMode::Ask,
    };
    let reason = format!("provider reported permission mode {mode}");
    if store.set_effective_approvals(session_id, effective, &reason)
        && store
            .get_session(session_id)
            .is_some_and(|session| session.approvals.requested != effective)
    {
        store.append_message(
            session_id,
            MessageBody::notice(&format!(
                "approvals run as {}: {reason}",
                effective.as_str()
            )),
        );
    }
}

fn content_blocks(event: &Value) -> impl Iterator<Item = &Value> {
    event
        .pointer("/message/content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

fn apply_assistant(store: &Store, session_id: &str, turns: &mut ClaudeTurns, event: &Value) {
    let top_level = event.get("parent_tool_use_id").is_none_or(Value::is_null);
    for block in content_blocks(event) {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                let text = block
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if text.trim().is_empty() {
                    continue;
                }
                if top_level {
                    turns.last_assistant = Some(text.to_owned());
                }
                store.append_message(session_id, MessageBody::assistant(text));
            }
            Some("tool_use") => {
                let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
                if let Some(id) = block.get("id").and_then(Value::as_str) {
                    turns.tools.insert(id.to_owned(), name.to_owned());
                }
                let input = block.get("input").unwrap_or(&Value::Null);
                store.append_message(
                    session_id,
                    MessageBody::tool_call(name, &input_summary(input)),
                );
            }
            _ => {}
        }
    }
}

fn apply_tool_results(store: &Store, session_id: &str, turns: &mut ClaudeTurns, event: &Value) {
    for block in content_blocks(event) {
        if block.get("type").and_then(Value::as_str) != Some("tool_result") {
            continue;
        }
        let tool = block
            .get("tool_use_id")
            .and_then(Value::as_str)
            .and_then(|id| turns.tools.remove(id))
            .unwrap_or_else(|| "tool".to_owned());
        let ok = block.get("is_error").and_then(Value::as_bool) != Some(true);
        let output = match block.get("content") {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Array(parts)) => parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        store.append_message(
            session_id,
            MessageBody::tool_result(&tool, ok, &brief(&output)),
        );
    }
}

/// The one field of a tool input that says what the call does.
pub fn input_summary(input: &Value) -> String {
    const KEYS: [&str; 8] = [
        "command",
        "file_path",
        "path",
        "pattern",
        "url",
        "query",
        "prompt",
        "description",
    ];
    KEYS.iter()
        .find_map(|key| input.get(key).and_then(Value::as_str))
        .or_else(|| {
            input
                .pointer("/questions/0/question")
                .and_then(Value::as_str)
        })
        .map_or_else(
            || {
                if input.is_null() {
                    String::new()
                } else {
                    input.to_string()
                }
            },
            str::to_owned,
        )
}

/// First line of a tool's output, short enough to scan.
pub fn brief(output: &str) -> String {
    let line = output
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    let mut end = line.len().min(RESULT_SUMMARY_LIMIT);
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    line[..end].to_owned()
}

fn tool_request(cwd: &str, provider_key: &str, request: &Value) -> RequestDraft {
    let tool = request
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("tool");
    let input = request.get("input").cloned().unwrap_or(Value::Null);
    let questions = input
        .get("questions")
        .and_then(Value::as_array)
        .filter(|_| tool == "AskUserQuestion")
        .cloned()
        .unwrap_or_default();
    if !questions.is_empty() {
        let texts = questions
            .iter()
            .filter_map(|question| question.get("question").and_then(Value::as_str))
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let choices = if let [question] = questions.as_slice() {
            question
                .get("options")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|option| option.get("label").and_then(Value::as_str))
                .map(str::to_owned)
                .collect()
        } else {
            Vec::new()
        };
        return RequestDraft {
            kind: RequestKind::Question,
            provider_key: provider_key.to_owned(),
            provider: ProviderRequest::ClaudeQuestion {
                input: input.clone(),
                questions: texts.clone(),
            },
            tool: tool.to_owned(),
            summary: String::new(),
            input,
            cwd: None,
            reason: None,
            text: Some(texts.join("\n")),
            choices,
        };
    }
    RequestDraft {
        kind: RequestKind::Approval,
        provider_key: provider_key.to_owned(),
        provider: ProviderRequest::ClaudeTool {
            input: input.clone(),
        },
        tool: tool.to_owned(),
        summary: input_summary(&input),
        input,
        cwd: Some(cwd.to_owned()),
        reason: ["decision_reason", "description"]
            .iter()
            .find_map(|key| request.get(key).and_then(Value::as_str))
            .map(str::to_owned),
        text: None,
        choices: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{Action, ClaudeTurns, apply_event};
    use crate::protocol::{ApprovalsMode, Auth, AuthPolicy, SessionState, TurnRecord, TurnState};
    use crate::store::{NewSession, Store};

    const SESSION: &str = "claude:s1";

    fn busy_session(approvals: ApprovalsMode) -> (Store, ClaudeTurns) {
        let store = Store::new();
        store
            .insert_session(&NewSession {
                id: SESSION,
                alias: "@worker",
                title: "worker",
                agent: "claude",
                cwd: "/work",
                model: None,
                effort: None,
                harness_options: &[],
                approvals,
            })
            .unwrap_or_else(|error| panic!("failed to insert session: {error}"));
        assert!(store.set_session_running(SESSION, Some(42)));
        assert!(store.set_session_state(SESSION, SessionState::Idle));
        store
            .insert_turn("turn_1", SESSION, "prompt")
            .unwrap_or_else(|error| panic!("failed to insert turn: {error}"));
        assert!(store.set_session_state(SESSION, SessionState::Busy));
        let mut turns = ClaudeTurns::default();
        turns.begin("u-prompt");
        (store, turns)
    }

    fn apply(store: &Store, turns: &mut ClaudeTurns, event: &Value) -> Vec<Action> {
        let generation = store
            .runtime_generation(SESSION)
            .unwrap_or_else(|| panic!("runtime generation missing"));
        apply_event(store, SESSION, generation, turns, event)
            .unwrap_or_else(|error| panic!("failed to apply {event}: {error}"))
    }

    fn queue_steer(store: &Store, turns: &mut ClaudeTurns, uuid: &str, text: &str) {
        let session = store
            .get_session(SESSION)
            .unwrap_or_else(|| panic!("session missing"));
        let turn_id = session
            .active_turn_id
            .unwrap_or_else(|| panic!("active turn missing"));
        let turn = store
            .get_turn(&turn_id)
            .unwrap_or_else(|| panic!("turn missing"));
        let generation = store
            .runtime_generation(SESSION)
            .unwrap_or_else(|| panic!("runtime generation missing"));
        assert!(store.begin_steer_attempt(
            SESSION,
            generation,
            &turn_id,
            turn.execution_seq,
            uuid,
            text,
        ));
        turns.add_steer(uuid, text);
    }

    fn replay(uuid: &str) -> Value {
        json!({"type": "user", "uuid": uuid, "isReplay": true,
               "message": {"role": "user", "content": "x"}})
    }

    fn lifecycle(uuid: &str, state: &str) -> Value {
        json!({"type": "command_lifecycle", "command_uuid": uuid, "state": state})
    }

    fn result(text: &str) -> Value {
        json!({"type": "result", "subtype": "success", "is_error": false, "result": text,
               "usage": {"input_tokens": 1}, "permission_denials": []})
    }

    fn interrupted() -> Value {
        json!({"type": "result", "subtype": "error_during_execution", "is_error": true,
               "result": null, "permission_denials": []})
    }

    fn assistant(text: &str) -> Value {
        json!({"type": "assistant", "parent_tool_use_id": null,
               "message": {"content": [{"type": "text", "text": text}]}})
    }

    fn state(store: &Store) -> SessionState {
        store
            .get_session(SESSION)
            .unwrap_or_else(|| panic!("session missing"))
            .state
    }

    fn turn(store: &Store) -> TurnRecord {
        store
            .get_turn("turn_1")
            .unwrap_or_else(|| panic!("turn missing"))
    }

    fn messages(store: &Store) -> Vec<Value> {
        let uid = store
            .session_uid(SESSION)
            .unwrap_or_else(|| panic!("session uid missing"));
        store
            .messages_after(&uid, 0, 64)
            .messages
            .iter()
            .map(|message| serde_json::to_value(message).unwrap_or_default())
            .collect()
    }

    #[test]
    fn a_turn_is_confirmed_when_claude_starts_it_and_ends_on_its_result() {
        let (store, mut turns) = busy_session(ApprovalsMode::Ask);
        apply(&store, &mut turns, &lifecycle("u-prompt", "queued"));
        assert!(turn(&store).started_at_ms.is_none());
        apply(&store, &mut turns, &lifecycle("u-prompt", "started"));
        assert_eq!(turn(&store).state, TurnState::Submitted);
        apply(
            &store,
            &mut turns,
            &json!({"type": "system", "subtype": "init", "session_id": "s1",
                    "apiKeySource": "none"}),
        );
        assert_eq!(turn(&store).state, TurnState::Running);
        apply(&store, &mut turns, &replay("u-prompt"));
        apply(
            &store,
            &mut turns,
            &json!({"type": "assistant", "parent_tool_use_id": null, "message": {"content": [
                {"type": "thinking", "thinking": "hidden"},
                {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "cargo test"}},
            ]}}),
        );
        apply(
            &store,
            &mut turns,
            &json!({"type": "user", "message": {"content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "42 passed\nfinished"},
            ]}}),
        );
        apply(&store, &mut turns, &assistant("Found one race."));
        assert_eq!(state(&store), SessionState::Busy);
        apply(&store, &mut turns, &result("Found one race."));

        let turn = turn(&store);
        assert_eq!(turn.state, TurnState::Completed);
        assert_eq!(turn.final_message.as_deref(), Some("Found one race."));
        assert!(!turn.final_text_recovered);
        assert_eq!(turn.usage, Some(json!({"input_tokens": 1})));
        assert_eq!(state(&store), SessionState::Idle);
        let messages = messages(&store);
        assert_eq!(
            messages
                .iter()
                .map(|message| message["kind"].as_str().unwrap_or_default())
                .collect::<Vec<_>>(),
            ["tool_call", "tool_result", "assistant"],
            "reasoning is never a message"
        );
        assert_eq!(messages[0]["tool"], "Bash");
        assert_eq!(messages[0]["summary"], "cargo test");
        assert_eq!(messages[1]["tool"], "Bash");
        assert_eq!(messages[1]["ok"], true);
        assert_eq!(messages[1]["summary"], "42 passed");
    }

    #[test]
    fn a_steer_read_after_a_tool_call_stays_inside_the_same_provider_turn() {
        let (store, mut turns) = busy_session(ApprovalsMode::Ask);
        apply(&store, &mut turns, &replay("u-prompt"));
        queue_steer(&store, &mut turns, "u-steer", "also say BANANA");
        apply(&store, &mut turns, &lifecycle("u-steer", "queued"));
        apply(&store, &mut turns, &replay("u-steer"));
        assert_eq!(state(&store), SessionState::Busy);
        apply(&store, &mut turns, &result("DONE BANANA"));

        assert_eq!(turn(&store).state, TurnState::Completed);
        assert_eq!(turn(&store).final_message.as_deref(), Some("DONE BANANA"));
        let messages = messages(&store);
        assert_eq!(messages[0]["kind"], "steer");
        assert_eq!(messages[0]["delivered"], false);
        assert_eq!(messages[1]["delivered"], true);
        assert_eq!(messages[1]["text"], "also say BANANA");
    }

    #[test]
    fn an_execution_stays_busy_until_every_accepted_steer_has_run() {
        let (store, mut turns) = busy_session(ApprovalsMode::Ask);
        apply(&store, &mut turns, &replay("u-prompt"));
        queue_steer(&store, &mut turns, "u-steer", "then say PAPAYA");
        // The model was only generating text, so the steer is not read
        // before this turn's result.
        apply(&store, &mut turns, &result("STORY_END"));
        assert_eq!(
            state(&store),
            SessionState::Busy,
            "a result with an unread steer does not end the execution"
        );
        assert_eq!(turn(&store).state, TurnState::Running);

        apply(&store, &mut turns, &lifecycle("u-steer", "started"));
        apply(&store, &mut turns, &replay("u-steer"));
        assert_eq!(state(&store), SessionState::Busy);
        apply(&store, &mut turns, &result("PAPAYA"));

        let turn = turn(&store);
        assert_eq!(turn.state, TurnState::Completed);
        assert_eq!(
            turn.final_message.as_deref(),
            Some("STORY_END\n\nPAPAYA"),
            "the continuation belongs to the execution that accepted the steer"
        );
        assert_eq!(state(&store), SessionState::Idle);
        assert_eq!(turn.execution_seq, 1);
    }

    #[test]
    fn a_canceled_execution_discards_its_unread_steer_and_settles_on_the_result() {
        let (mut store, mut turns) = busy_session(ApprovalsMode::Ask);
        apply(&store, &mut turns, &replay("u-prompt"));
        queue_steer(&store, &mut turns, "u-steer", "extra");
        assert!(
            store
                .cancel_turn("turn_1")
                .unwrap_or_else(|error| panic!("failed to cancel: {error}"))
        );
        // Claude confirms it dropped the queued message, then the interrupted
        // turn reports its result.
        apply(&store, &mut turns, &lifecycle("u-steer", "cancelled"));
        assert_eq!(state(&store), SessionState::Quiescing);
        assert!(turns.unread().is_empty());
        apply(&store, &mut turns, &interrupted());

        assert_eq!(turn(&store).state, TurnState::Canceled);
        assert_eq!(state(&store), SessionState::Idle);
        let messages = messages(&store);
        assert_eq!(messages[0]["kind"], "steer");
        assert_eq!(messages[0]["delivered"], false);
        assert_eq!(messages[1]["delivered"], false);
        assert_eq!(messages[1]["expired"], true);
    }

    #[test]
    fn a_leftover_message_that_starts_a_turn_after_a_cancel_is_interrupted_again() {
        let (mut store, mut turns) = busy_session(ApprovalsMode::Ask);
        apply(&store, &mut turns, &replay("u-prompt"));
        queue_steer(&store, &mut turns, "u-steer", "extra");
        let generation = store
            .runtime_generation(SESSION)
            .unwrap_or_else(|| panic!("runtime generation missing"));
        assert!(
            store
                .cancel_turn_with_controls("turn_1", generation, 1)
                .unwrap_or_else(|error| panic!("failed to cancel: {error}"))
        );
        apply(&store, &mut turns, &interrupted());
        assert_eq!(
            state(&store),
            SessionState::Quiescing,
            "the Session is not idle while a written message can still start a turn"
        );
        assert_eq!(
            apply(&store, &mut turns, &replay("u-steer")),
            [Action::Interrupt {
                turn_id: "turn_1".to_owned()
            }]
        );
        apply(&store, &mut turns, &interrupted());
        assert_eq!(state(&store), SessionState::Quiescing);
        assert!(!store.finish_cancel_control("turn_1"));
        assert!(!store.finish_cancel_control("turn_1"));
        turns
            .settle(&store, SESSION)
            .unwrap_or_else(|error| panic!("failed to settle cancel: {error}"));
        assert_eq!(state(&store), SessionState::Idle);
        assert_eq!(turn(&store).state, TurnState::Canceled);
    }

    #[test]
    fn a_prompt_canceled_while_still_queued_settles_without_a_result() {
        let (mut store, mut turns) = busy_session(ApprovalsMode::Ask);
        assert!(
            store
                .cancel_turn("turn_1")
                .unwrap_or_else(|error| panic!("failed to cancel: {error}"))
        );
        apply(&store, &mut turns, &lifecycle("u-prompt", "cancelled"));
        assert_eq!(state(&store), SessionState::Idle);
        assert_eq!(turn(&store).state, TurnState::Canceled);
    }

    #[test]
    fn a_result_from_a_turn_dlgt_did_not_start_cannot_end_a_new_execution() {
        let (store, mut turns) = busy_session(ApprovalsMode::Ask);
        apply(&store, &mut turns, &lifecycle("u-prompt", "queued"));
        // A provider turn that was already running reports its result while
        // this execution's prompt is still waiting to start.
        apply(&store, &mut turns, &result("someone else's answer"));
        assert_eq!(state(&store), SessionState::Busy);
        assert_eq!(turn(&store).state, TurnState::Submitted);

        apply(&store, &mut turns, &lifecycle("u-prompt", "started"));
        apply(&store, &mut turns, &replay("u-prompt"));
        apply(&store, &mut turns, &result("the real answer"));
        assert_eq!(
            turn(&store).final_message.as_deref(),
            Some("the real answer")
        );
        assert_eq!(state(&store), SessionState::Idle);
    }

    #[test]
    fn an_error_result_fails_the_execution_and_keeps_the_session_usable() {
        let (store, mut turns) = busy_session(ApprovalsMode::Ask);
        apply(&store, &mut turns, &replay("u-prompt"));
        apply(
            &store,
            &mut turns,
            &json!({"type": "result", "subtype": "success", "is_error": true,
                    "result": "API Error: overloaded"}),
        );
        let turn = turn(&store);
        assert_eq!(turn.state, TurnState::Failed);
        assert!(turn.error.as_ref().is_some_and(|error| {
            error["message"]
                .as_str()
                .is_some_and(|message| message.contains("API Error: overloaded"))
        }));
        assert_eq!(state(&store), SessionState::Idle);
    }

    #[test]
    fn a_result_without_text_recovers_the_last_assistant_message() {
        let (store, mut turns) = busy_session(ApprovalsMode::Ask);
        apply(&store, &mut turns, &replay("u-prompt"));
        apply(&store, &mut turns, &assistant("the streamed answer"));
        apply(&store, &mut turns, &result(""));
        let turn = turn(&store);
        assert_eq!(turn.state, TurnState::Completed);
        assert_eq!(turn.final_message.as_deref(), Some("the streamed answer"));
        assert!(turn.final_text_recovered);
    }

    #[test]
    fn a_permission_request_blocks_until_answered_or_withdrawn() {
        let (store, mut turns) = busy_session(ApprovalsMode::Ask);
        apply(&store, &mut turns, &replay("u-prompt"));
        apply(
            &store,
            &mut turns,
            &json!({"type": "control_request", "request_id": "p1", "request": {
                "subtype": "can_use_tool", "tool_name": "Bash",
                "input": {"command": "touch probe.txt", "description": "Create a file"},
                "description": "Create a file"}}),
        );
        apply(
            &store,
            &mut turns,
            &json!({"type": "control_request", "request_id": "p2", "request": {
                "subtype": "can_use_tool", "tool_name": "AskUserQuestion",
                "requires_user_interaction": true,
                "input": {"questions": [{"question": "Which color?", "header": "Color",
                    "options": [{"label": "Red"}, {"label": "Blue"}], "multiSelect": false}]}}}),
        );
        assert_eq!(state(&store), SessionState::Blocked);
        let uid = store
            .session_uid(SESSION)
            .unwrap_or_else(|| panic!("session uid missing"));
        let pending = store.pending_requests(&uid);
        let approval = pending[0].public();
        assert_eq!(approval["kind"], "approval");
        assert_eq!(approval["tool"], "Bash");
        assert_eq!(approval["summary"], "touch probe.txt");
        assert_eq!(approval["cwd"], "/work");
        assert_eq!(approval["reason"], "Create a file");
        let question = pending[1].public();
        assert_eq!(question["kind"], "question");
        assert_eq!(question["text"], "Which color?");
        assert_eq!(
            question["choices"],
            json!([
                {"index": 1, "label": "Red"},
                {"index": 2, "label": "Blue"},
            ])
        );

        for withdrawn in ["p1", "p2"] {
            apply(
                &store,
                &mut turns,
                &json!({"type": "control_cancel_request", "request_id": withdrawn}),
            );
        }
        assert!(store.pending_requests(&uid).is_empty());
        assert_eq!(state(&store), SessionState::Busy);
    }

    #[test]
    fn a_control_request_nobody_can_answer_is_refused_instead_of_left_hanging() {
        let (store, mut turns) = busy_session(ApprovalsMode::Ask);
        assert_eq!(
            apply(
                &store,
                &mut turns,
                &json!({"type": "control_request", "request_id": "h1",
                        "request": {"subtype": "hook_callback"}}),
            ),
            [Action::Refuse {
                request_id: "h1".to_owned(),
                reason: "dlgt does not handle this control request",
            }]
        );
        assert_eq!(state(&store), SessionState::Busy);

        apply(&store, &mut turns, &replay("u-prompt"));
        apply(&store, &mut turns, &result("done"));
        let orphan = apply(
            &store,
            &mut turns,
            &json!({"type": "control_request", "request_id": "p9", "request": {
                "subtype": "can_use_tool", "tool_name": "Bash", "input": {"command": "ls"}}}),
        );
        assert!(matches!(
            orphan.as_slice(),
            [Action::Refuse { request_id, .. }] if request_id == "p9"
        ));
        assert_eq!(
            state(&store),
            SessionState::Idle,
            "an idle Session is never blocked"
        );
    }

    #[test]
    fn the_mode_and_login_claude_reports_are_recorded_and_announced() {
        let (store, mut turns) = busy_session(ApprovalsMode::Auto);
        store.set_auth_policy(SESSION, AuthPolicy::Any);
        let actions = apply(
            &store,
            &mut turns,
            &json!({"type": "system", "subtype": "init", "session_id": "s1",
                    "model": "claude-haiku-4-5", "permissionMode": "auto",
                    "apiKeySource": "ANTHROPIC_API_KEY"}),
        );
        assert!(actions.is_empty());
        apply(
            &store,
            &mut turns,
            &json!({"type": "system", "subtype": "status", "permissionMode": "default"}),
        );
        let session = store
            .get_session(SESSION)
            .unwrap_or_else(|| panic!("session missing"));
        assert_eq!(session.auth, Some(Auth::ApiKey));
        assert_eq!(session.resolved_model.as_deref(), Some("claude-haiku-4-5"));
        assert_eq!(session.approvals.requested, ApprovalsMode::Auto);
        assert_eq!(session.approvals.effective, ApprovalsMode::Ask);
        assert_eq!(
            session.approvals.reason.as_deref(),
            Some("provider reported permission mode default")
        );
        let notices = messages(&store);
        assert_eq!(notices.len(), 2);
        assert!(notices.iter().all(|notice| notice["kind"] == "notice"));
        assert!(
            notices[0]["text"]
                .as_str()
                .is_some_and(|text| text.contains("API key"))
        );
    }

    #[test]
    fn subscription_auth_mismatch_fails_and_stops_before_confirmation() {
        let (store, mut turns) = busy_session(ApprovalsMode::Auto);
        apply(&store, &mut turns, &lifecycle("u-prompt", "started"));
        assert_eq!(turn(&store).state, TurnState::Submitted);
        let actions = apply(
            &store,
            &mut turns,
            &json!({"type": "system", "subtype": "init", "session_id": "s1",
                    "apiKeySource": "apiKeyHelper"}),
        );
        assert_eq!(actions, [Action::AuthMismatch]);
        assert_eq!(turn(&store).state, TurnState::Failed);
        assert_eq!(
            turn(&store)
                .error
                .as_ref()
                .map(|error| error["code"].as_str()),
            Some(Some("AUTH_MISMATCH"))
        );
        let session = store
            .get_session(SESSION)
            .unwrap_or_else(|| panic!("session missing"));
        // The process is still alive until the daemon stops it.
        assert_eq!(session.state, SessionState::Stopping);
        assert_eq!(session.auth, Some(Auth::ApiKey));
        assert_eq!(session.auth_policy, None);
        assert!(
            messages(&store)
                .iter()
                .any(|message| message["kind"] == "notice")
        );
    }

    #[test]
    fn subscription_auth_mismatch_when_init_omits_api_key_source() {
        let (store, mut turns) = busy_session(ApprovalsMode::Auto);
        store.set_auth_policy(SESSION, AuthPolicy::Subscription);
        let actions = apply(
            &store,
            &mut turns,
            &json!({"type": "system", "subtype": "init", "session_id": "s1",
                    "model": "claude-test", "permissionMode": "auto"}),
        );
        assert_eq!(actions, [Action::AuthMismatch]);
        assert_eq!(
            turn(&store)
                .error
                .as_ref()
                .map(|error| error["code"].as_str()),
            Some(Some("AUTH_MISMATCH"))
        );
        assert_eq!(
            store.get_session(SESSION).and_then(|session| session.auth),
            None
        );
    }

    #[test]
    fn process_auth_mode_and_model_survive_turn_settlement() {
        let (store, mut turns) = busy_session(ApprovalsMode::Ask);
        apply(
            &store,
            &mut turns,
            &json!({"type": "system", "subtype": "init", "session_id": "s1",
                    "model": "claude-test", "permissionMode": "auto", "apiKeySource": "none"}),
        );
        apply(&store, &mut turns, &replay("u-prompt"));
        apply(&store, &mut turns, &result("first"));
        assert!(turns.auth_checked);
        let session = store
            .get_session(SESSION)
            .unwrap_or_else(|| panic!("session missing"));
        assert_eq!(session.auth, Some(Auth::Subscription));
        assert_eq!(session.resolved_model.as_deref(), Some("claude-test"));
        assert_eq!(session.approvals.effective, ApprovalsMode::Auto);

        store
            .insert_turn("turn_2", SESSION, "second")
            .unwrap_or_else(|error| panic!("failed to insert second turn: {error}"));
        assert!(store.set_session_state(SESSION, SessionState::Busy));
        turns.begin("u-second");
        assert!(turns.auth_checked);
        apply(&store, &mut turns, &replay("u-second"));
        apply(&store, &mut turns, &result("second"));
        let session = store
            .get_session(SESSION)
            .unwrap_or_else(|| panic!("session missing"));
        assert_eq!(session.auth, Some(Auth::Subscription));
        assert_eq!(session.resolved_model.as_deref(), Some("claude-test"));
        assert_eq!(session.approvals.effective, ApprovalsMode::Auto);
    }

    #[test]
    fn a_subscription_login_is_reported_without_a_notice() {
        let (store, mut turns) = busy_session(ApprovalsMode::Ask);
        apply(
            &store,
            &mut turns,
            &json!({"type": "system", "subtype": "init", "session_id": "s1",
                    "permissionMode": "default", "apiKeySource": "none"}),
        );
        assert_eq!(
            store.get_session(SESSION).and_then(|session| session.auth),
            Some(Auth::Subscription)
        );
        assert!(messages(&store).is_empty());
    }

    #[test]
    fn a_provider_side_denial_is_an_approval_message_and_is_counted() {
        let (store, mut turns) = busy_session(ApprovalsMode::Ask);
        apply(&store, &mut turns, &replay("u-prompt"));
        apply(
            &store,
            &mut turns,
            &json!({"type": "system", "subtype": "permission_denied", "tool_name": "Bash",
                    "message": "touch needs approval"}),
        );
        apply(
            &store,
            &mut turns,
            &json!({"type": "result", "subtype": "success", "is_error": false, "result": "I could not.",
                    "permission_denials": [{"tool_name": "Bash"}]}),
        );
        let denial = &messages(&store)[0];
        assert_eq!(denial["kind"], "approval");
        assert_eq!(denial["decision"], "denied");
        assert_eq!(denial["by"], "provider_policy");
        let turn = turn(&store);
        assert_eq!(turn.state, TurnState::Completed);
        assert_eq!(
            turn.denied_requests, 1,
            "a completed result still says a needed tool was denied"
        );
    }

    #[test]
    fn a_rotated_provider_session_id_asks_for_a_rekey() {
        let (store, mut turns) = busy_session(ApprovalsMode::Ask);
        store.set_auth_policy(SESSION, AuthPolicy::Any);
        assert_eq!(
            apply(
                &store,
                &mut turns,
                &json!({"type": "system", "subtype": "init", "session_id": "s2"}),
            ),
            [Action::Rekey("s2".to_owned())]
        );
    }
}
