//! Codex transport: one `codex app-server` process per Session, driven with
//! JSON-RPC over its stdio. There is no terminal and no remote TUI.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{ChildStdin, Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde_json::{Map, Value, json};

use crate::provider::codex_program;
use crate::reaper::{Reaper, Registration};

const REQUEST_TIMEOUT: Duration = Duration::from_mins(1);
const RESUME_TIMEOUT: Duration = Duration::from_secs(10);
const TURN_RECONCILE_INTERVAL: Duration = Duration::from_millis(250);
const TURN_RECONCILE_READ_TIMEOUT: Duration = Duration::from_secs(5);
const STDERR_TAIL_LIMIT: usize = 4096;
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// Server requests that become Requests for the caller to answer. Anything
/// else is refused on the spot, because nobody would ever answer it.
const CALLER_REQUESTS: [&str; 5] = [
    "item/commandExecution/requestApproval",
    "item/fileChange/requestApproval",
    "item/permissions/requestApproval",
    "item/tool/requestUserInput",
    "mcpServer/elicitation/request",
];

type NotificationHandler = Arc<dyn Fn(Value) -> bool + Send + Sync>;
type Pending = Arc<Mutex<HashMap<u64, Sender<Result<Value, String>>>>>;

#[derive(Debug)]
enum WriteCommand {
    Message(Vec<u8>, Option<Sender<Result<(), String>>>),
    Close,
}

pub struct CodexConnection {
    pid: u32,
    writer: Sender<WriteCommand>,
    closed: Arc<AtomicBool>,
    pending: Pending,
    next_id: AtomicU64,
    handler: NotificationHandler,
    terminal_turns: Arc<Mutex<HashSet<String>>>,
    stderr_tail: Arc<Mutex<String>>,
    _reaper_registration: Registration,
}

impl CodexConnection {
    #[allow(clippy::too_many_lines)]
    pub fn connect(
        handler: NotificationHandler,
        environment: Option<&HashMap<String, String>>,
        cwd: Option<&Path>,
        args: &[String],
        startup_timeout: Duration,
        reaper: &Arc<Reaper>,
    ) -> Result<Arc<Self>> {
        let mut command = Command::new(codex_program());
        if let Some(environment) = environment {
            command.env_clear().envs(environment);
        }
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        let mut child = command
            .args(args)
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed to start the managed Codex app-server")?;
        let pid = child.id();
        let reaper_registration = reaper.watch(pid)?;
        let stdin = child
            .stdin
            .take()
            .context("Codex app-server stdin was unavailable")?;
        let stdout = child
            .stdout
            .take()
            .context("Codex app-server child was unavailable")?;
        let stderr = child
            .stderr
            .take()
            .context("Codex app-server child was unavailable")?;
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let terminal_turns = Arc::new(Mutex::new(HashSet::new()));
        let stderr_tail = Arc::new(Mutex::new(String::new()));
        let closed = Arc::new(AtomicBool::new(false));
        let writer = spawn_writer(stdin, Arc::clone(&closed), pid)?;

        let (drained, drained_signal) = std::sync::mpsc::channel::<()>();
        let exit_handler = Arc::clone(&handler);
        let exit_pending = Arc::clone(&pending);
        let exit_closed = Arc::clone(&closed);
        let exit_watch =
            crate::reaper::watch_exit(child, "dlgt-codex-exit", drained_signal, move |status| {
                let reason = status.map_or_else(
                    || "Codex app-server closed the connection".to_owned(),
                    |status| format!("Codex app-server exited: {status}"),
                );
                fail_pending(&exit_pending, &reason);
                exit_closed.store(true, Ordering::Release);
                exit_handler(transport_closed(&reason));
            })?;

        let tail = Arc::clone(&stderr_tail);
        let stderr_stop = Arc::clone(&exit_watch.stop_output);
        std::thread::Builder::new()
            .name("dlgt-codex-stderr".to_owned())
            .spawn(move || {
                retain_tail(
                    crate::reaper::StoppableOutput::new(stderr, stderr_stop),
                    &tail,
                );
            })
            .context("failed to start Codex stderr thread")?;

        let io_handler = Arc::clone(&handler);
        let io_pending = Arc::clone(&pending);
        let io_terminal_turns = Arc::clone(&terminal_turns);
        let io_writer = writer.clone();
        std::thread::Builder::new()
            .name("dlgt-codex-app-server".to_owned())
            .spawn(move || {
                let stdout = crate::reaper::StoppableOutput::new(
                    stdout,
                    Arc::clone(&exit_watch.stop_output),
                );
                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    if line.trim().is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<Value>(&line) {
                        Ok(message) => dispatch_message(
                            &io_writer,
                            &io_pending,
                            &io_handler,
                            &io_terminal_turns,
                            message,
                        ),
                        Err(error) => {
                            io_handler(json!({
                                "method": "dlgt/protocol/error",
                                "params": {"message": error.to_string()},
                            }));
                        }
                    }
                }
                drop(drained);
                crate::reaper::output_closed(pid, &exit_watch.exited);
            })
            .context("failed to start Codex app-server I/O thread")?;

        let connection = Arc::new(Self {
            pid,
            writer,
            closed,
            pending,
            next_id: AtomicU64::new(1),
            handler,
            terminal_turns,
            stderr_tail,
            _reaper_registration: reaper_registration,
        });
        connection.initialize(startup_timeout)?;
        Ok(connection)
    }

    fn initialize(&self, startup_timeout: Duration) -> Result<()> {
        self.request_within(
            "initialize",
            json!({
                "clientInfo": {
                    "name": "dlgt",
                    "title": "dlgt",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "capabilities": {
                    "experimentalApi": true,
                    "requestAttestation": false,
                },
            }),
            startup_timeout,
        )?;
        self.notify("initialized", json!({}))
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

    /// Start a new thread and return its ID together with what the provider
    /// resolved the launch selection to.
    pub fn start_thread(&self, params: Value, timeout: Duration) -> Result<(String, Value)> {
        let response = self.request_within("thread/start", params, timeout)?;
        let thread_id = response
            .pointer("/thread/id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .context("Codex thread/start response had no thread id")?;
        Ok((thread_id, response))
    }

    /// Load a stored thread into this app-server.
    pub fn resume_thread(&self, params: &Value, timeout: Duration) -> Result<Value> {
        let deadline = std::time::Instant::now() + RESUME_TIMEOUT.min(timeout);
        loop {
            match self.request_within("thread/resume", params.clone(), timeout) {
                Ok(response) => return Ok(response),
                Err(error)
                    if is_rollout_not_ready(&error.to_string())
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(error) => return Err(error),
            }
        }
    }

    pub fn start_turn(&self, thread_id: &str, prompt: &str) -> Result<String> {
        let response = self.request(
            "turn/start",
            json!({"threadId": thread_id, "input": [text_input(prompt)]}),
        )?;
        response
            .pointer("/turn/id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .context("Codex turn/start response had no turn id")
    }

    /// Add input to the turn that is already running.
    pub fn steer_turn(&self, thread_id: &str, turn_id: &str, text: &str) -> Result<()> {
        self.request(
            "turn/steer",
            json!({
                "threadId": thread_id,
                "expectedTurnId": turn_id,
                "input": [text_input(text)],
            }),
        )?;
        Ok(())
    }

    pub fn set_thread_name(&self, thread_id: &str, name: &str) -> Result<()> {
        self.request("thread/name/set", thread_name_set_params(thread_id, name))?;
        Ok(())
    }

    pub fn list_models(&self, include_hidden: bool) -> Result<Value> {
        let mut data = Vec::new();
        let mut cursor = Value::Null;
        loop {
            let response = self.request(
                "model/list",
                json!({"cursor":cursor,"limit":100,"includeHidden":include_hidden}),
            )?;
            data.extend(
                response
                    .get("data")
                    .and_then(Value::as_array)
                    .cloned()
                    .context("Codex model/list response had no data array")?,
            );
            cursor = response.get("nextCursor").cloned().unwrap_or(Value::Null);
            if cursor.is_null() {
                break;
            }
        }
        Ok(json!({"data":data,"nextCursor":null}))
    }

    pub fn interrupt_turn(&self, thread_id: &str, turn_id: &str) -> Result<()> {
        self.request(
            "turn/interrupt",
            json!({"threadId": thread_id, "turnId": turn_id}),
        )?;
        Ok(())
    }

    pub fn respond_delivery(
        &self,
        id: &Value,
        result: &Value,
    ) -> std::result::Result<(), crate::provider::DeliveryFailure> {
        let line = serialize_message(&json!({"id": id, "result": result}))
            .map_err(|error| crate::provider::DeliveryFailure::not_handed(error.to_string()))?;
        let (sender, receiver) = mpsc::channel();
        if self
            .writer
            .send(WriteCommand::Message(line, Some(sender)))
            .is_err()
        {
            return Err(crate::provider::DeliveryFailure::not_handed(
                "Codex app-server writer is closed",
            ));
        }
        match receiver.recv_timeout(WRITE_TIMEOUT) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(message)) => {
                self.force_stop();
                Err(crate::provider::DeliveryFailure::ambiguous(message))
            }
            Err(_) => {
                self.force_stop();
                Err(crate::provider::DeliveryFailure::ambiguous(
                    "timed out writing to the Codex app-server",
                ))
            }
        }
    }

    pub fn watch_turn(self: &Arc<Self>, thread_id: &str, turn_id: &str) -> Result<()> {
        let connection = Arc::downgrade(self);
        let thread_id = thread_id.to_owned();
        let turn_id = turn_id.to_owned();
        std::thread::Builder::new()
            .name("dlgt-codex-turn-watch".to_owned())
            .spawn(move || {
                let mut pending_terminal_status = None;
                loop {
                    let Some(connection) = connection.upgrade() else {
                        return;
                    };
                    // The turn's own terminal notification already settled
                    // it; reconciling it again is only load.
                    if is_terminal(&connection.terminal_turns, &turn_id) {
                        return;
                    }
                    // Bounded so a worker notices its turn settled soon after,
                    // even when a read goes unanswered.
                    let response = connection.request_within(
                        "thread/read",
                        json!({"threadId": thread_id, "includeTurns": true}),
                        TURN_RECONCILE_READ_TIMEOUT,
                    );
                    match response {
                        Ok(response) => {
                            if let Some(turn) = terminal_turn(&response, &turn_id) {
                                if terminal_reconciliation_ready(
                                    &mut pending_terminal_status,
                                    &turn,
                                ) {
                                    dispatch_notification(
                                        &connection.handler,
                                        &connection.terminal_turns,
                                        json!({
                                            "method": "turn/completed",
                                            "params": {
                                                "threadId": thread_id,
                                                "turn": turn,
                                                "source": "thread/read",
                                            },
                                        }),
                                    );
                                    return;
                                }
                            } else {
                                pending_terminal_status = None;
                            }
                        }
                        Err(_) if connection.closed() => return,
                        Err(_) => pending_terminal_status = None,
                    }
                    drop(connection);
                    std::thread::sleep(TURN_RECONCILE_INTERVAL);
                }
            })
            .context("failed to start Codex turn reconciliation thread")?;
        Ok(())
    }

    pub fn stop(&self) {
        self.close_writer();
        self.signal(libc::SIGTERM);
    }

    pub fn force_stop(&self) {
        self.close_writer();
        self.signal(libc::SIGKILL);
    }

    fn close_writer(&self) {
        let _ = self.writer.send(WriteCommand::Close);
        self.closed.store(true, Ordering::Release);
    }

    fn closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    fn signal(&self, signal: i32) {
        if let Ok(pid) = i32::try_from(self.pid) {
            // SAFETY: kill has no memory-safety preconditions. The app-server
            // is spawned as the leader of its own process group.
            unsafe {
                libc::kill(-pid, signal);
            }
        }
    }

    fn request(&self, method: &'static str, params: Value) -> Result<Value> {
        self.request_within(method, params, REQUEST_TIMEOUT)
    }

    fn request_within(
        &self,
        method: &'static str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::channel();
        self.pending
            .lock()
            .map_err(|_| anyhow!("Codex request lock poisoned"))?
            .insert(id, sender);
        let message = Value::Object(Map::from_iter([
            ("id".to_owned(), json!(id)),
            ("method".to_owned(), json!(method)),
            ("params".to_owned(), params),
        ]));
        if let Err(error) = write_message(&self.writer, &message) {
            if let Ok(mut pending) = self.pending.lock() {
                pending.remove(&id);
            }
            return Err(error);
        }
        let response = receiver.recv_timeout(timeout);
        if response.is_err()
            && let Ok(mut pending) = self.pending.lock()
        {
            // A reply that never came must not keep its slot forever.
            pending.remove(&id);
        }
        response
            .with_context(|| format!("timed out waiting for Codex {method}"))?
            .map_err(|message| anyhow!("Codex {method} failed: {message}"))
    }

    fn notify(&self, method: &'static str, params: Value) -> Result<()> {
        write_message(
            &self.writer,
            &Value::Object(Map::from_iter([
                ("method".to_owned(), json!(method)),
                ("params".to_owned(), params),
            ])),
        )
    }
}

impl Drop for CodexConnection {
    fn drop(&mut self) {
        self.force_stop();
    }
}

fn text_input(text: &str) -> Value {
    json!({"type": "text", "text": text, "text_elements": []})
}

fn serialize_message(message: &Value) -> Result<Vec<u8>> {
    let mut line = serde_json::to_vec(message)?;
    line.push(b'\n');
    Ok(line)
}

fn queue_message(writer: &Sender<WriteCommand>, message: &Value) -> Result<()> {
    writer
        .send(WriteCommand::Message(serialize_message(message)?, None))
        .context("Codex app-server writer closed")
}

fn write_message(writer: &Sender<WriteCommand>, message: &Value) -> Result<()> {
    let line = serialize_message(message)?;
    let (sender, receiver) = mpsc::channel();
    writer
        .send(WriteCommand::Message(line, Some(sender)))
        .context("Codex app-server writer closed")?;
    receiver
        .recv_timeout(WRITE_TIMEOUT)
        .context("timed out writing to the Codex app-server")?
        .map_err(|message| anyhow!(message))
}

fn spawn_writer(
    stdin: ChildStdin,
    closed: Arc<AtomicBool>,
    pid: u32,
) -> Result<Sender<WriteCommand>> {
    let (writer, receiver) = mpsc::channel();
    std::thread::Builder::new()
        .name("dlgt-codex-writer".to_owned())
        .spawn(move || writer_loop(stdin, receiver, &closed, pid))
        .context("failed to start Codex writer thread")?;
    Ok(writer)
}

fn writer_loop(
    mut stdin: ChildStdin,
    receiver: Receiver<WriteCommand>,
    closed: &AtomicBool,
    pid: u32,
) {
    for command in receiver {
        match command {
            WriteCommand::Close => break,
            WriteCommand::Message(line, acknowledgment) => {
                let result = stdin
                    .write_all(&line)
                    .and_then(|()| stdin.flush())
                    .map_err(|error| format!("Codex app-server connection closed: {error}"));
                if let Some(acknowledgment) = acknowledgment {
                    let _ = acknowledgment.send(result.clone());
                }
                if result.is_err() {
                    closed.store(true, Ordering::Release);
                    if let Ok(pid) = i32::try_from(pid) {
                        // SAFETY: kill has no memory-safety preconditions; the
                        // app-server owns a dedicated process group.
                        unsafe { libc::kill(-pid, libc::SIGKILL) };
                    }
                    return;
                }
            }
        }
    }
    closed.store(true, Ordering::Release);
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

pub(crate) fn thread_name_set_params(thread_id: &str, name: &str) -> Value {
    json!({"threadId": thread_id, "name": name})
}

fn is_rollout_not_ready(message: &str) -> bool {
    message.contains("no rollout found")
        || message.contains("rollout") && message.contains("is empty")
}

fn dispatch_message(
    writer: &Sender<WriteCommand>,
    pending: &Pending,
    handler: &NotificationHandler,
    terminal_turns: &Arc<Mutex<HashSet<String>>>,
    message: Value,
) {
    if let Some(method) = message.get("method").and_then(Value::as_str)
        && let Some(id) = message.get("id").cloned()
    {
        if CALLER_REQUESTS.contains(&method) {
            let accepted = handler(json!({
                "method": "dlgt/server/request",
                "params": {
                    "method": method,
                    "id": id.clone(),
                    "params": message.get("params"),
                },
            }));
            if !accepted {
                let _ = queue_message(
                    writer,
                    &json!({
                        "id": id,
                        "error": {"code": -32000, "message": "dlgt refused the request"},
                    }),
                );
            }
        } else {
            let _ = queue_message(
                writer,
                &json!({
                    "id": id,
                    "error": {"code": -32601, "message": "dlgt does not handle this server request"},
                }),
            );
            handler(json!({
                "method": "dlgt/server/unsupported",
                "params": {"method": method},
            }));
        }
        return;
    }
    if let Some(id) = message.get("id").and_then(Value::as_u64) {
        let sender = pending
            .lock()
            .ok()
            .and_then(|mut pending| pending.remove(&id));
        if let Some(sender) = sender {
            let result = if let Some(error) = message.get("error") {
                Err(error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown app-server error")
                    .to_owned())
            } else {
                Ok(message.get("result").cloned().unwrap_or(Value::Null))
            };
            let _ = sender.send(result);
        }
        return;
    }
    if message.get("method").is_some() {
        dispatch_notification(handler, terminal_turns, message);
    }
}

fn dispatch_notification(
    handler: &NotificationHandler,
    terminal_turns: &Arc<Mutex<HashSet<String>>>,
    message: Value,
) {
    if message.get("method").and_then(Value::as_str) == Some("turn/started")
        && let Some(turn_id) = message.pointer("/params/turn/id").and_then(Value::as_str)
        && is_terminal(terminal_turns, turn_id)
    {
        return;
    }
    if message.get("method").and_then(Value::as_str) == Some("turn/completed") {
        let turn = message.pointer("/params/turn");
        if let Some(turn_id) = turn.and_then(|turn| turn.get("id")).and_then(Value::as_str)
            && !mark_terminal_once(terminal_turns, turn_id)
        {
            return;
        }
    }
    handler(message);
}

fn mark_terminal_once(terminal_turns: &Arc<Mutex<HashSet<String>>>, turn_id: &str) -> bool {
    terminal_turns
        .lock()
        .map_or(true, |mut turns| turns.insert(turn_id.to_owned()))
}

fn is_terminal(terminal_turns: &Arc<Mutex<HashSet<String>>>, turn_id: &str) -> bool {
    terminal_turns
        .lock()
        .is_ok_and(|turns| turns.contains(turn_id))
}

fn terminal_turn(response: &Value, turn_id: &str) -> Option<Value> {
    if !matches!(
        response
            .pointer("/thread/status/type")
            .and_then(Value::as_str),
        Some("idle" | "systemError")
    ) {
        return None;
    }
    response
        .pointer("/thread/turns")?
        .as_array()?
        .iter()
        .find(|turn| {
            if turn.get("id").and_then(Value::as_str) != Some(turn_id) {
                return false;
            }
            matches!(
                turn.get("status").and_then(Value::as_str),
                Some("failed" | "interrupted" | "completed")
            )
        })
        .cloned()
}

fn terminal_reconciliation_ready(pending_status: &mut Option<String>, turn: &Value) -> bool {
    let status = turn.get("status").and_then(Value::as_str).unwrap_or("");
    if status == "completed" {
        return true;
    }
    if pending_status.as_deref() == Some(status) {
        return true;
    }
    *pending_status = Some(status.to_owned());
    false
}

pub(crate) fn final_agent_message_text(turn: &Value) -> Option<&str> {
    turn.get("items")
        .and_then(Value::as_array)
        .and_then(|items| {
            items
                .iter()
                .rev()
                .find(|item| item.get("type").and_then(Value::as_str) == Some("agentMessage"))
        })
        .and_then(|item| item.get("text"))
        .and_then(Value::as_str)
}

fn fail_pending(pending: &Pending, message: &str) {
    if let Ok(mut pending) = pending.lock() {
        for (_, sender) in pending.drain() {
            let _ = sender.send(Err(message.to_owned()));
        }
    }
}

fn transport_closed(message: &str) -> Value {
    json!({"method": "dlgt/transport/closed", "params": {"message": message}})
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    use super::{
        NotificationHandler, dispatch_notification, is_rollout_not_ready,
        terminal_reconciliation_ready, terminal_turn, thread_name_set_params, transport_closed,
    };

    #[test]
    fn transport_failures_are_explicit_notifications() {
        assert_eq!(
            transport_closed("gone"),
            json!({"method": "dlgt/transport/closed", "params": {"message": "gone"}})
        );
    }

    #[test]
    fn thread_read_only_reconciles_terminal_turns() {
        let response = json!({
            "thread": {
                "status": {"type": "idle"},
                "turns": [
                    {"id": "running", "status": "inProgress"},
                    {"id": "done", "status": "failed", "error": {"message": "bad"}},
                ],
            },
        });
        assert!(terminal_turn(&response, "running").is_none());
        assert_eq!(
            terminal_turn(&response, "done").and_then(|turn| turn.get("status").cloned()),
            Some(json!("failed"))
        );
        let active = json!({
            "thread": {
                "status": {"type": "active", "activeFlags": []},
                "turns": [{"id": "done", "status": "completed"}],
            },
        });
        assert!(terminal_turn(&active, "done").is_none());
        let empty_completion = json!({
            "thread": {
                "status": {"type": "idle"},
                "turns": [{"id": "done", "status": "completed", "items": []}],
            },
        });
        assert_eq!(
            terminal_turn(&empty_completion, "done").and_then(|turn| turn.get("status").cloned()),
            Some(json!("completed"))
        );
        let successful_completion = json!({
            "thread": {
                "status": {"type": "idle"},
                "turns": [{
                    "id": "done",
                    "status": "completed",
                    "items": [{"type": "agentMessage", "text": "ok"}],
                }],
            },
        });
        assert!(terminal_turn(&successful_completion, "done").is_some());
    }

    #[test]
    fn thread_read_confirms_failure_states_before_reconciling() {
        let interrupted = json!({"id": "turn", "status": "interrupted"});
        let completed = json!({"id": "turn", "status": "completed"});
        let mut pending = None;

        assert!(!terminal_reconciliation_ready(&mut pending, &interrupted));
        assert!(terminal_reconciliation_ready(&mut pending, &interrupted));

        pending = None;
        assert!(terminal_reconciliation_ready(&mut pending, &completed));
    }

    #[test]
    fn duplicate_terminal_notifications_are_suppressed() {
        let calls = Arc::new(AtomicUsize::new(0));
        let handler_calls = Arc::clone(&calls);
        let handler: NotificationHandler = Arc::new(move |_message| {
            handler_calls.fetch_add(1, Ordering::Relaxed);
            true
        });
        let terminal_turns = Arc::new(Mutex::new(HashSet::new()));
        let notification = json!({
            "method": "turn/completed",
            "params": {
                "threadId": "thread",
                "turn": {
                    "id": "turn",
                    "status": "completed",
                    "items": [{"type": "agentMessage", "text": "done"}],
                },
            },
        });
        dispatch_notification(&handler, &terminal_turns, notification.clone());
        dispatch_notification(&handler, &terminal_turns, notification);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn late_start_after_terminal_notification_is_suppressed() {
        let calls = Arc::new(AtomicUsize::new(0));
        let handler_calls = Arc::clone(&calls);
        let handler: NotificationHandler = Arc::new(move |_message| {
            handler_calls.fetch_add(1, Ordering::Relaxed);
            true
        });
        let terminal_turns = Arc::new(Mutex::new(HashSet::new()));
        let completed = json!({
            "method": "turn/completed",
            "params": {
                "threadId": "thread",
                "turn": {
                    "id": "turn",
                    "status": "completed",
                    "items": [{"type": "agentMessage", "text": "done"}],
                },
            },
        });
        let late_started = json!({
            "method": "turn/started",
            "params": {
                "threadId": "thread",
                "turn": {"id": "turn", "status": "inProgress", "items": []},
            },
        });
        dispatch_notification(&handler, &terminal_turns, completed);
        dispatch_notification(&handler, &terminal_turns, late_started);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn completed_empty_turn_is_terminal() {
        let calls = Arc::new(AtomicUsize::new(0));
        let handler_calls = Arc::clone(&calls);
        let handler: NotificationHandler = Arc::new(move |_message| {
            handler_calls.fetch_add(1, Ordering::Relaxed);
            true
        });
        let terminal_turns = Arc::new(Mutex::new(HashSet::new()));
        let incomplete = json!({
            "method": "turn/completed",
            "params": {
                "threadId": "thread",
                "turn": {"id": "turn", "status": "completed", "items": []},
            },
        });
        dispatch_notification(&handler, &terminal_turns, incomplete);
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        let empty_message = json!({
            "method": "turn/completed",
            "params": {
                "threadId": "thread",
                "turn": {
                    "id": "turn",
                    "status": "completed",
                    "items": [{"type": "agentMessage", "text": ""}],
                },
            },
        });
        dispatch_notification(&handler, &terminal_turns, empty_message);
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        let unfinished_final_message = json!({
            "method": "turn/completed",
            "params": {
                "threadId": "thread",
                "turn": {
                    "id": "turn",
                    "status": "completed",
                    "items": [
                        {"type": "agentMessage", "text": "progress"},
                        {"type": "agentMessage", "text": ""},
                    ],
                },
            },
        });
        dispatch_notification(&handler, &terminal_turns, unfinished_final_message);
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        let reconciled = json!({
            "method": "turn/completed",
            "params": {
                "threadId": "thread",
                "turn": {
                    "id": "turn",
                    "status": "completed",
                    "items": [{"type": "agentMessage", "text": "done"}],
                },
            },
        });
        dispatch_notification(&handler, &terminal_turns, reconciled);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn empty_or_missing_rollouts_are_treated_as_transient_during_join() {
        assert!(is_rollout_not_ready("no rollout found for thread id abc"));
        assert!(is_rollout_not_ready("rollout at /tmp/a.jsonl is empty"));
        assert!(!is_rollout_not_ready("permission denied"));
    }

    #[test]
    fn thread_name_set_uses_the_app_server_thread_name_protocol() {
        assert_eq!(
            thread_name_set_params("thread-1", "[dlgt] Review API boundaries"),
            json!({
                "threadId": "thread-1",
                "name": "[dlgt] Review API boundaries",
            })
        );
    }
}
