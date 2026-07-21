use std::collections::{HashMap, HashSet};
use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use tungstenite::{Message, WebSocket, client};

use crate::provider::{codex_app_server_args, codex_program};
use crate::reaper::{Reaper, Registration};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const SOCKET_TIMEOUT: Duration = Duration::from_secs(10);
const TURN_RECONCILE_INTERVAL: Duration = Duration::from_millis(250);

type NotificationHandler = Arc<dyn Fn(Value) + Send + Sync>;

enum Outbound {
    Request {
        id: u64,
        method: &'static str,
        params: Value,
        response: Sender<Result<Value, String>>,
    },
    Notification {
        method: &'static str,
        params: Value,
    },
    Close,
}

pub struct CodexConnection {
    outbound: Sender<Outbound>,
    next_id: AtomicU64,
    child: Mutex<Child>,
    socket_path: PathBuf,
    handler: NotificationHandler,
    terminal_turns: Arc<Mutex<HashSet<String>>>,
    joined: AtomicBool,
    _reaper_registration: Registration,
}

struct ChildGuard(Option<Child>);

impl CodexConnection {
    pub fn connect(
        socket_path: PathBuf,
        handler: NotificationHandler,
        reaper: &Arc<Reaper>,
    ) -> Result<Arc<Self>> {
        Self::connect_with_environment(socket_path, handler, None, reaper)
    }

    pub fn connect_with_environment(
        socket_path: PathBuf,
        handler: NotificationHandler,
        environment: Option<&HashMap<String, String>>,
        reaper: &Arc<Reaper>,
    ) -> Result<Arc<Self>> {
        let mut child_guard = ChildGuard(Some(spawn_app_server(&socket_path, environment)?));
        let child = child_guard
            .0
            .as_ref()
            .context("Codex app-server child was unavailable")?;
        let reaper_registration = reaper.watch(child.id())?;
        let stream = UnixStream::connect(&socket_path)
            .with_context(|| format!("failed to connect to {}", socket_path.display()))?;
        stream
            .set_read_timeout(Some(Duration::from_millis(50)))
            .context("failed to configure Codex app-server socket")?;
        let (websocket, _response) = client("ws://localhost/", stream)
            .context("failed to establish Codex app-server WebSocket")?;
        let (sender, receiver) = mpsc::channel();
        let io_handler = Arc::clone(&handler);
        let terminal_turns = Arc::new(Mutex::new(HashSet::new()));
        let io_terminal_turns = Arc::clone(&terminal_turns);
        std::thread::Builder::new()
            .name("dlgt-codex-app-server".to_owned())
            .spawn(move || io_loop(websocket, &receiver, &io_handler, &io_terminal_turns))
            .context("failed to start Codex app-server I/O thread")?;
        let connection = Arc::new(Self {
            outbound: sender,
            next_id: AtomicU64::new(1),
            child: Mutex::new(
                child_guard
                    .0
                    .take()
                    .context("Codex app-server child was unavailable")?,
            ),
            socket_path,
            handler,
            terminal_turns,
            joined: AtomicBool::new(false),
            _reaper_registration: reaper_registration,
        });
        connection.request(
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
        )?;
        connection.notify("initialized", json!({}))?;
        Ok(connection)
    }

    pub fn start_turn(&self, thread_id: &str, prompt: &str) -> Result<String> {
        let response = self.request(
            "turn/start",
            json!({
                "threadId": thread_id,
                "input": [{
                    "type": "text",
                    "text": prompt,
                    "text_elements": [],
                }],
            }),
        )?;
        response
            .pointer("/turn/id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .context("Codex turn/start response had no turn id")
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

    pub fn join_thread(&self, thread_id: &str) -> Result<()> {
        if self.joined.load(Ordering::Acquire) {
            return Ok(());
        }
        let deadline = std::time::Instant::now() + SOCKET_TIMEOUT;
        loop {
            match self.request("thread/resume", json!({"threadId": thread_id})) {
                Ok(_) => {
                    self.joined.store(true, Ordering::Release);
                    return Ok(());
                }
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

    pub fn interrupt_turn(&self, thread_id: &str, turn_id: &str) -> Result<()> {
        self.request(
            "turn/interrupt",
            json!({"threadId": thread_id, "turnId": turn_id}),
        )?;
        Ok(())
    }

    pub fn watch_turn(self: &Arc<Self>, thread_id: &str, turn_id: &str) -> Result<()> {
        let connection = Arc::downgrade(self);
        let thread_id = thread_id.to_owned();
        let turn_id = turn_id.to_owned();
        std::thread::Builder::new()
            .name("dlgt-codex-turn-watch".to_owned())
            .spawn(move || {
                loop {
                    let Some(connection) = connection.upgrade() else {
                        return;
                    };
                    let response = connection.request(
                        "thread/read",
                        json!({"threadId": thread_id, "includeTurns": true}),
                    );
                    if let Ok(response) = response
                        && let Some(turn) = terminal_turn(&response, &turn_id)
                    {
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
                    drop(connection);
                    std::thread::sleep(TURN_RECONCILE_INTERVAL);
                }
            })
            .context("failed to start Codex turn reconciliation thread")?;
        Ok(())
    }

    fn request(&self, method: &'static str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::channel();
        self.outbound
            .send(Outbound::Request {
                id,
                method,
                params,
                response: sender,
            })
            .context("Codex app-server connection closed")?;
        receiver
            .recv_timeout(REQUEST_TIMEOUT)
            .with_context(|| format!("timed out waiting for Codex {method}"))?
            .map_err(|message| anyhow!("Codex {method} failed: {message}"))
    }

    fn notify(&self, method: &'static str, params: Value) -> Result<()> {
        self.outbound
            .send(Outbound::Notification { method, params })
            .context("Codex app-server connection closed")
    }
}

fn is_rollout_not_ready(message: &str) -> bool {
    message.contains("no rollout found")
        || message.contains("rollout") && message.contains("is empty")
}

impl Drop for CodexConnection {
    fn drop(&mut self) {
        let _ = self.outbound.send(Outbound::Close);
        if let Ok(mut child) = self.child.lock() {
            kill_child_group(&mut child);
        }
        if let Some(parent) = self.socket_path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            kill_child_group(child);
        }
    }
}

fn kill_child_group(child: &mut Child) {
    if let Ok(pid) = i32::try_from(child.id()) {
        // SAFETY: kill has no memory-safety preconditions. The app-server is
        // spawned as the leader of this process group.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn io_loop(
    mut websocket: WebSocket<UnixStream>,
    outbound: &Receiver<Outbound>,
    handler: &NotificationHandler,
    terminal_turns: &Arc<Mutex<HashSet<String>>>,
) {
    let mut pending: HashMap<u64, Sender<Result<Value, String>>> = HashMap::new();
    loop {
        while let Ok(message) = outbound.try_recv() {
            match message {
                Outbound::Request {
                    id,
                    method,
                    params,
                    response,
                } => {
                    let payload = json!({"id": id, "method": method, "params": params});
                    if let Err(error) = websocket.send(Message::text(payload.to_string())) {
                        let _ = response.send(Err(error.to_string()));
                        fail_pending(&mut pending, &error.to_string());
                        handler(transport_closed(&error.to_string()));
                        return;
                    }
                    pending.insert(id, response);
                }
                Outbound::Notification { method, params } => {
                    let payload = json!({"method": method, "params": params});
                    if let Err(error) = websocket.send(Message::text(payload.to_string())) {
                        fail_pending(&mut pending, &error.to_string());
                        handler(transport_closed(&error.to_string()));
                        return;
                    }
                }
                Outbound::Close => {
                    let _ = websocket.close(None);
                    fail_pending(&mut pending, "Codex app-server connection closed");
                    return;
                }
            }
        }

        match websocket.read() {
            Ok(Message::Text(text)) => match serde_json::from_str::<Value>(&text) {
                Ok(message) => dispatch_message(
                    &mut websocket,
                    &mut pending,
                    handler,
                    terminal_turns,
                    message,
                ),
                Err(error) => handler(json!({
                    "method": "dlgt/protocol/error",
                    "params": {"message": error.to_string()},
                })),
            },
            Ok(Message::Close(_)) => {
                fail_pending(&mut pending, "Codex app-server closed the connection");
                handler(transport_closed("Codex app-server closed the connection"));
                return;
            }
            Ok(Message::Ping(payload)) => {
                let _ = websocket.send(Message::Pong(payload));
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(error) => {
                fail_pending(&mut pending, &error.to_string());
                handler(transport_closed(&error.to_string()));
                return;
            }
        }
    }
}

fn dispatch_message(
    websocket: &mut WebSocket<UnixStream>,
    pending: &mut HashMap<u64, Sender<Result<Value, String>>>,
    handler: &NotificationHandler,
    terminal_turns: &Arc<Mutex<HashSet<String>>>,
    message: Value,
) {
    if message.get("method").is_some()
        && let Some(id) = message.get("id").cloned()
    {
        handler(json!({
            "method": "dlgt/server/request",
            "params": {
                "method": message.get("method"),
                "id": id.clone(),
            },
        }));
        let response = json!({
            "id": id,
            "error": {"code": -32601, "message": "dlgt does not handle server requests"},
        });
        let _ = websocket.send(Message::text(response.to_string()));
        return;
    }
    if let Some(id) = message.get("id").and_then(Value::as_u64) {
        if let Some(sender) = pending.remove(&id) {
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
    if message.get("method").and_then(Value::as_str) == Some("turn/completed") {
        let turn = message.pointer("/params/turn");
        if turn
            .and_then(|turn| turn.get("status"))
            .and_then(Value::as_str)
            == Some("completed")
            && turn
                .and_then(final_agent_message_text)
                .is_none_or(str::is_empty)
        {
            return;
        }
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
            match turn.get("status").and_then(Value::as_str) {
                Some("failed" | "interrupted") => true,
                Some("completed") => {
                    final_agent_message_text(turn).is_some_and(|text| !text.is_empty())
                }
                _ => false,
            }
        })
        .cloned()
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

fn fail_pending(pending: &mut HashMap<u64, Sender<Result<Value, String>>>, message: &str) {
    for (_, sender) in pending.drain() {
        let _ = sender.send(Err(message.to_owned()));
    }
}

fn transport_closed(message: &str) -> Value {
    json!({"method": "dlgt/transport/closed", "params": {"message": message}})
}

fn spawn_app_server(
    socket_path: &Path,
    environment: Option<&HashMap<String, String>>,
) -> Result<Child> {
    let parent = socket_path.parent().context("Codex socket has no parent")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed to create {}", parent.display()))?;
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("failed to secure {}", parent.display()))?;
    let endpoint = format!("unix://{}", socket_path.display());
    let mut command = Command::new(codex_program());
    if let Some(environment) = environment {
        command.env_clear().envs(environment);
    }
    let mut child = command
        .args(codex_app_server_args(&endpoint))
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to start the managed Codex app-server")?;
    let deadline = std::time::Instant::now() + SOCKET_TIMEOUT;
    while !socket_path.exists() {
        if let Some(status) = child
            .try_wait()
            .context("failed to inspect Codex app-server")?
        {
            bail!("Codex app-server exited before creating its socket: {status}");
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("timed out waiting for {}", socket_path.display());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Ok(child)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    use super::{
        NotificationHandler, dispatch_notification, is_rollout_not_ready, terminal_turn,
        transport_closed,
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
        assert!(terminal_turn(&empty_completion, "done").is_none());
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
    fn duplicate_terminal_notifications_are_suppressed() {
        let calls = Arc::new(AtomicUsize::new(0));
        let handler_calls = Arc::clone(&calls);
        let handler: NotificationHandler = Arc::new(move |_message| {
            handler_calls.fetch_add(1, Ordering::Relaxed);
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
    fn incomplete_success_waits_for_reconciled_turn() {
        let calls = Arc::new(AtomicUsize::new(0));
        let handler_calls = Arc::clone(&calls);
        let handler: NotificationHandler = Arc::new(move |_message| {
            handler_calls.fetch_add(1, Ordering::Relaxed);
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
        assert_eq!(calls.load(Ordering::Relaxed), 0);

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
        assert_eq!(calls.load(Ordering::Relaxed), 0);

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
        assert_eq!(calls.load(Ordering::Relaxed), 0);

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
}
