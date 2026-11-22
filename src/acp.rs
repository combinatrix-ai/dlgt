//! ACP transport: one agent process per Session, driven by JSON-RPC over stdio.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::reaper::{Reaper, Registration};

const STDERR_TAIL_LIMIT: usize = 4096;
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
pub const REQUEST_TIMEOUT: Duration = Duration::from_mins(1);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);
/// ACP's `auth_required` error code.
const AUTH_REQUIRED: i64 = -32000;
const INTERNAL_ERROR: i64 = -32603;
const PROMPT_TIMEOUT: Duration = Duration::from_hours(24);

pub type NotificationHandler = Arc<dyn Fn(Value) -> bool + Send + Sync>;
type Pending = Arc<Mutex<HashMap<String, Sender<std::result::Result<Value, AcpRpcError>>>>>;

#[derive(Debug, Clone)]
pub struct AcpRpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl std::fmt::Display for AcpRpcError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for AcpRpcError {}

#[derive(Debug)]
pub struct AcpAuthRequired {
    pub message: String,
    pub auth_methods: Vec<String>,
}

impl std::fmt::Display for AcpAuthRequired {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for AcpAuthRequired {}

#[derive(Debug)]
enum WriteCommand {
    Message(Vec<u8>, Option<Sender<std::result::Result<(), String>>>),
    Close,
}

pub struct AcpConnection {
    pid: u32,
    writer: Sender<WriteCommand>,
    pending: Pending,
    next_id: AtomicU64,
    closed: Arc<AtomicBool>,
    stderr_tail: Arc<Mutex<String>>,
    _reaper_registration: Registration,
}

impl AcpConnection {
    #[allow(clippy::too_many_lines)]
    pub fn connect(
        command: &[String],
        cwd: &Path,
        environment: &HashMap<String, String>,
        startup_timeout: Duration,
        handler: &NotificationHandler,
        reaper: &Arc<Reaper>,
    ) -> Result<(Arc<Self>, Value)> {
        let Some(program) = command.first().filter(|program| !program.is_empty()) else {
            bail!("ACP command must contain a program");
        };
        let mut child = Command::new(program);
        child
            .args(&command[1..])
            .current_dir(cwd)
            .env_clear()
            .envs(environment)
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = child.spawn().context("failed to start ACP Harness")?;
        let pid = child.id();
        let reaper_registration = reaper.watch(pid)?;
        let stdin = child
            .stdin
            .take()
            .context("ACP Harness stdin was unavailable")?;
        let stdout = child
            .stdout
            .take()
            .context("ACP Harness stdout was unavailable")?;
        let stderr = child
            .stderr
            .take()
            .context("ACP Harness stderr was unavailable")?;
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let closed = Arc::new(AtomicBool::new(false));
        let stderr_tail = Arc::new(Mutex::new(String::new()));
        let writer = spawn_writer(stdin, Arc::clone(&closed), pid)?;

        let (drained, drained_signal) = mpsc::channel::<()>();
        let exit_handler = Arc::clone(handler);
        let exit_pending = Arc::clone(&pending);
        let exit_closed = Arc::clone(&closed);
        let exit_watch =
            crate::reaper::watch_exit(child, "dlgt-acp-exit", drained_signal, move |status| {
                let reason = status.map_or_else(
                    || "ACP Harness closed the connection".to_owned(),
                    |status| format!("ACP Harness exited: {status}"),
                );
                fail_pending(&exit_pending, &reason);
                exit_closed.store(true, Ordering::Release);
                exit_handler(json!({
                    "method":"dlgt/transport/closed",
                    "params":{"message":reason},
                }));
            })?;

        let tail = Arc::clone(&stderr_tail);
        let stderr_stop = Arc::clone(&exit_watch.stop_output);
        std::thread::Builder::new()
            .name("dlgt-acp-stderr".to_owned())
            .spawn(move || {
                retain_tail(
                    crate::reaper::StoppableOutput::new(stderr, stderr_stop),
                    &tail,
                );
            })
            .context("failed to start ACP stderr reader")?;

        let io_pending = Arc::clone(&pending);
        let io_handler = Arc::clone(handler);
        let io_writer = writer.clone();
        std::thread::Builder::new()
            .name("dlgt-acp-json-rpc".to_owned())
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
                        Ok(message) => {
                            dispatch_message(&io_writer, &io_pending, &io_handler, message);
                        }
                        Err(error) => {
                            io_handler(json!({
                                "method":"dlgt/protocol/error",
                                "params":{"message":error.to_string()},
                            }));
                        }
                    }
                }
                drop(drained);
                crate::reaper::output_closed(pid, &exit_watch.exited);
            })
            .context("failed to start ACP JSON-RPC reader")?;

        let connection = Arc::new(Self {
            pid,
            writer,
            pending,
            next_id: AtomicU64::new(1),
            closed,
            stderr_tail,
            _reaper_registration: reaper_registration,
        });
        let initialized = connection.request(
            "initialize",
            &json!({
                "protocolVersion": 1,
                "clientInfo": {
                    "name": "dlgt",
                    "title": "dlgt",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "clientCapabilities": {
                    "fs": {"readTextFile": false, "writeTextFile": false},
                    "terminal": false,
                },
            }),
            startup_timeout,
        )?;
        Ok((connection, initialized))
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

    pub fn request(&self, method: &str, params: &Value, timeout: Duration) -> Result<Value> {
        if self.closed.load(Ordering::Acquire) {
            bail!("ACP Harness connection is closed");
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let id_value = json!(id);
        let key = id_key(&id_value);
        let (sender, receiver) = mpsc::channel();
        self.pending
            .lock()
            .map_err(|_| anyhow!("ACP pending request lock poisoned"))?
            .insert(key.clone(), sender);
        let line = serialize_message(
            &json!({"jsonrpc":"2.0","id":id_value,"method":method,"params":params}),
        )?;
        if let Err(error) = self.write(line) {
            if let Ok(mut pending) = self.pending.lock() {
                pending.remove(&key);
            }
            return Err(error);
        }
        match receiver.recv_timeout(timeout) {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(error.into()),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Ok(mut pending) = self.pending.lock() {
                    pending.remove(&key);
                }
                bail!("ACP request {method} timed out")
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                bail!("ACP connection closed waiting for {method}")
            }
        }
    }

    pub fn prompt(&self, session_id: &str, prompt: &str) -> Result<Value> {
        self.request(
            "session/prompt",
            &json!({"sessionId":session_id,"prompt":[{"type":"text","text":prompt}]}),
            PROMPT_TIMEOUT,
        )
    }

    pub fn notify(&self, method: &str, params: &Value) -> Result<()> {
        let line = serialize_message(&json!({"jsonrpc":"2.0","method":method,"params":params}))?;
        self.write(line)
    }

    pub fn respond_server_request(&self, id: &str, result: &Value) -> Result<()> {
        let id: Value = serde_json::from_str(id).context("invalid ACP request ID")?;
        self.write(serialize_message(
            &json!({"jsonrpc":"2.0","id":id,"result":result}),
        )?)
    }

    pub fn cancel_prompt(&self, session_id: &str) -> Result<()> {
        self.notify("session/cancel", &json!({"sessionId":session_id}))
    }

    pub fn close_session(&self, session_id: &str) -> Result<()> {
        self.request(
            "session/close",
            &json!({"sessionId":session_id}),
            CLOSE_TIMEOUT,
        )?;
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

    fn write(&self, line: Vec<u8>) -> Result<()> {
        let (sender, receiver) = mpsc::channel();
        self.writer
            .send(WriteCommand::Message(line, Some(sender)))
            .map_err(|_| anyhow!("ACP writer is closed"))?;
        match receiver.recv_timeout(WRITE_TIMEOUT) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(message)) => bail!("{message}"),
            Err(_) => bail!("timed out writing to ACP Harness"),
        }
    }

    fn close_writer(&self) {
        let _ = self.writer.send(WriteCommand::Close);
        self.closed.store(true, Ordering::Release);
    }

    fn signal(&self, signal: i32) {
        if let Ok(pid) = i32::try_from(self.pid) {
            // SAFETY: the provider is spawned as leader of its own process group.
            unsafe { libc::kill(-pid, signal) };
        }
    }
}

impl Drop for AcpConnection {
    fn drop(&mut self) {
        self.force_stop();
    }
}

fn spawn_writer(
    mut stdin: ChildStdin,
    closed: Arc<AtomicBool>,
    pid: u32,
) -> Result<Sender<WriteCommand>> {
    let (sender, receiver): (Sender<WriteCommand>, Receiver<WriteCommand>) = mpsc::channel();
    std::thread::Builder::new()
        .name("dlgt-acp-writer".to_owned())
        .spawn(move || {
            while let Ok(command) = receiver.recv() {
                match command {
                    WriteCommand::Message(line, response) => {
                        let result =
                            stdin
                                .write_all(&line)
                                .and_then(|()| stdin.flush())
                                .map_err(|error| {
                                    closed.store(true, Ordering::Release);
                                    if let Ok(group) = i32::try_from(pid) {
                                        // SAFETY: the provider is spawned as leader of its own process group.
                                        unsafe { libc::kill(-group, libc::SIGKILL) };
                                    }
                                    error.to_string()
                                });
                        if let Some(response) = response {
                            let _ = response.send(result);
                        }
                    }
                    WriteCommand::Close => break,
                }
            }
        })
        .context("failed to start ACP writer")?;
    Ok(sender)
}

fn dispatch_message(
    writer: &Sender<WriteCommand>,
    pending: &Pending,
    handler: &NotificationHandler,
    message: Value,
) {
    if let Some(id) = message.get("id") {
        if let Ok(mut pending) = pending.lock()
            && let Some(sender) = pending.remove(&id_key(id))
        {
            let result = if let Some(error) = message.get("error") {
                Err(AcpRpcError {
                    code: error
                        .get("code")
                        .and_then(Value::as_i64)
                        .unwrap_or(INTERNAL_ERROR),
                    message: error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("ACP request failed")
                        .to_owned(),
                    data: error.get("data").cloned(),
                })
            } else {
                Ok(message.get("result").cloned().unwrap_or(Value::Null))
            };
            let _ = sender.send(result);
            return;
        }
        if message.get("method").is_some() {
            if !handler(message.clone()) {
                let line = json!({
                    "jsonrpc":"2.0",
                    "id":id,
                    "error":{"code":-32601,"message":"Method not found"},
                });
                if let Ok(line) = serialize_message(&line) {
                    let _ = writer.send(WriteCommand::Message(line, None));
                }
            }
            return;
        }
    }
    if message.get("method").is_some() {
        let _ = handler(message);
    }
}

fn id_key(id: &Value) -> String {
    id.to_string()
}

fn serialize_message(message: &Value) -> Result<Vec<u8>> {
    let mut line = serde_json::to_vec(message).context("failed to encode ACP JSON-RPC message")?;
    line.push(b'\n');
    Ok(line)
}

fn fail_pending(pending: &Pending, message: &str) {
    if let Ok(mut pending) = pending.lock() {
        for (_, sender) in pending.drain() {
            // Not -32000: a lost connection must not read as ACP's
            // `auth_required`.
            let _ = sender.send(Err(AcpRpcError {
                code: INTERNAL_ERROR,
                message: message.to_owned(),
                data: None,
            }));
        }
    }
}

fn retain_tail(mut reader: impl std::io::Read, tail: &Mutex<String>) {
    let mut buffer = [0_u8; 1024];
    while let Ok(length) = reader.read(&mut buffer) {
        if length == 0 {
            break;
        }
        if let Ok(mut tail) = tail.lock() {
            tail.push_str(&String::from_utf8_lossy(&buffer[..length]));
            if tail.len() > STDERR_TAIL_LIMIT {
                let mut start = tail.len() - STDERR_TAIL_LIMIT;
                while !tail.is_char_boundary(start) {
                    start += 1;
                }
                tail.drain(..start);
            }
        }
    }
}

/// The `AUTH_REQUIRED` launch error for an agent that refused a session
/// because nobody is signed in. `authMethods` only names the ways to sign in:
/// agents advertise them whether or not they are signed in.
pub fn auth_required(error: &AcpRpcError, harness: &str, init: &Value) -> Option<anyhow::Error> {
    let message = error.message.to_ascii_lowercase();
    let required = error.code == AUTH_REQUIRED
        || ["auth_required", "auth required", "authentication required"]
            .iter()
            .any(|phrase| message.contains(phrase));
    if !required {
        return None;
    }
    let auth_methods = error
        .data
        .as_ref()
        .and_then(|data| data.get("authMethods"))
        .and_then(Value::as_array)
        .or_else(|| init.get("authMethods").and_then(Value::as_array))
        .into_iter()
        .flatten()
        .filter_map(|method| {
            method
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect();
    let message = format!("{harness} requires sign-in");
    let auth = AcpAuthRequired {
        message: message.clone(),
        auth_methods,
    };
    Some(crate::error::coded(crate::error::Code::AuthRequired, message).context(auth))
}

/// Whether `initialize` advertised `agentCapabilities.sessionCapabilities.<name>`.
/// ACP marks a supported capability with an object, usually an empty one.
pub fn session_capability(init: &Value, name: &str) -> bool {
    init.pointer(&format!("/agentCapabilities/sessionCapabilities/{name}"))
        .is_some_and(|capability| !capability.is_null())
}

pub fn load_session_capability(init: &Value) -> bool {
    init.pointer("/agentCapabilities/loadSession")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Whether a `session/update` of this kind proves the agent took the prompt.
/// Agents also send updates nobody prompted, such as `config_option_update`
/// after a `session/set_config_option`.
pub fn update_confirms_prompt(kind: &str) -> bool {
    matches!(
        kind,
        "agent_message_chunk"
            | "agent_thought_chunk"
            | "user_message_chunk"
            | "tool_call"
            | "tool_call_update"
            | "plan"
    )
}

/// The config option of one category in a `session/new` or
/// `session/set_config_option` result.
pub fn config_option<'a>(configuration: &'a Value, category: &str) -> Option<&'a Value> {
    configuration
        .get("configOptions")
        .and_then(Value::as_array)?
        .iter()
        .find(|option| option.get("category").and_then(Value::as_str) == Some(category))
}

/// The models a session offers, as `id` and `display_name`: the `model`
/// config option's choices, else the `models` list.
pub fn session_models(session: &Value) -> Vec<Value> {
    let listed = |entries: Option<&Value>, id: &str| {
        entries
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|entry| {
                let model = entry.get(id).and_then(Value::as_str)?;
                Some(json!({
                    "id": model,
                    "display_name": entry.get("name").and_then(Value::as_str).unwrap_or(model),
                }))
            })
            .collect::<Vec<_>>()
    };
    match config_option(session, "model") {
        Some(option) => listed(option.get("options"), "value"),
        None => listed(session.pointer("/models/availableModels"), "modelId"),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{
        AcpAuthRequired, AcpRpcError, auth_required, config_option, load_session_capability,
        session_capability, session_models, update_confirms_prompt,
    };

    /// `initialize` result of agy 1.5.11.
    fn agy_initialize() -> Value {
        json!({
            "protocolVersion": 1,
            "agentCapabilities": {
                "loadSession": true,
                "promptCapabilities": {"image": true, "audio": false, "embeddedContext": true},
                "sessionCapabilities": {"resume": {}, "close": {}, "load": {}, "list": {}, "additionalDirectories": {}},
                "auth": {"logout": {}},
            },
            "agentInfo": {"name": "Antigravity ACP Server", "version": "1.5.11"},
            "authMethods": [
                {"id": "oauth-personal", "name": "Log in with Google", "description": "Log in with your Google account"},
                {"id": "oauth-business", "name": "Log in with Gemini Enterprise", "description": "Log in with your Gemini Enterprise account"},
                {"id": "gemini-api-key", "name": "Gemini API key", "description": "Use an API key with Gemini Developer API"},
                {"id": "agent-platform", "name": "Gemini Enterprise Agent Platform", "description": "Use Gemini Enterprise Agent Platform (formerly Vertex AI) with Application Default Credentials"},
            ],
        })
    }

    /// `session/new` result of agy 1.5.11, trimmed to two models.
    fn agy_session() -> Value {
        json!({
            "sessionId": "cfa001bc-3ba3-4c63-b172-5f7f3a85bfd8",
            "models": {
                "availableModels": [
                    {"modelId": "gemini-3.8-flash-high", "name": "Gemini 3.8 Flash (High)"},
                    {"modelId": "claude-sonnet-4-6", "name": "Claude Sonnet 4.6 (Thinking)"},
                ],
                "currentModelId": "gemini-3.8-flash-high",
            },
            "configOptions": [
                {"id": "model", "name": "Model", "category": "model", "type": "select",
                 "currentValue": "gemini-3.8-flash",
                 "options": [
                    {"value": "gemini-3.8-flash", "name": "Gemini 3.8 Flash"},
                    {"value": "claude-sonnet-4-6", "name": "Claude Sonnet 4.6 (Thinking)"},
                 ]},
                {"id": "thought_level", "name": "Thinking Level", "category": "thought_level",
                 "type": "select", "currentValue": "high",
                 "options": [{"value": "low", "name": "Low"}, {"value": "medium", "name": "Medium"}, {"value": "high", "name": "High"}]},
                {"id": "mode", "name": "Mode", "category": "mode", "type": "select",
                 "currentValue": "default",
                 "options": [{"value": "default", "name": "Default"}, {"value": "accept-edits", "name": "Accept edits"}, {"value": "auto", "name": "Auto"}, {"value": "plan", "name": "Plan"}]},
            ],
        })
    }

    fn rpc_error(code: i64, message: &str) -> AcpRpcError {
        AcpRpcError {
            code,
            message: message.to_owned(),
            data: None,
        }
    }

    #[test]
    fn session_capabilities_are_read_from_agent_capabilities_objects() {
        let init = agy_initialize();
        assert!(session_capability(&init, "resume"));
        assert!(session_capability(&init, "close"));
        assert!(!session_capability(&init, "fork"));
        assert!(load_session_capability(&init));
        let bare = json!({"protocolVersion": 1, "agentCapabilities": {}});
        assert!(!session_capability(&bare, "resume"));
        assert!(!load_session_capability(&bare));
    }

    #[test]
    fn advertised_auth_methods_alone_do_not_make_a_failure_auth_required() {
        let init = agy_initialize();
        assert!(auth_required(&rpc_error(-32603, "Internal error"), "agy", &init).is_none());
        assert!(auth_required(&rpc_error(-32602, "unknown session"), "agy", &init).is_none());
    }

    #[test]
    fn the_acp_auth_error_is_auth_required_and_names_the_sign_in_methods() {
        let init = agy_initialize();
        for error in [
            rpc_error(-32000, "Authentication required"),
            rpc_error(-32603, "auth_required: sign in first"),
        ] {
            let Some(required) = auth_required(&error, "agy", &init) else {
                panic!("{error:?} was not classified as auth required");
            };
            assert_eq!(
                crate::error::code_of(&required),
                Some(crate::error::Code::AuthRequired)
            );
            let methods = required
                .downcast_ref::<AcpAuthRequired>()
                .map(|auth| auth.auth_methods.clone())
                .unwrap_or_default();
            assert_eq!(methods.len(), 4);
            assert_eq!(methods[0], "Log in with Google");
        }
    }

    #[test]
    fn only_updates_caused_by_the_prompt_confirm_it() {
        for kind in [
            "agent_message_chunk",
            "agent_thought_chunk",
            "user_message_chunk",
            "tool_call",
            "tool_call_update",
            "plan",
        ] {
            assert!(update_confirms_prompt(kind), "{kind}");
        }
        for kind in [
            "config_option_update",
            "available_commands_update",
            "usage_update",
            "session_info_update",
            "current_mode_update",
        ] {
            assert!(!update_confirms_prompt(kind), "{kind}");
        }
    }

    #[test]
    fn models_come_from_the_model_option_and_fall_back_to_the_models_list() {
        let mut session = agy_session();
        assert_eq!(
            session_models(&session),
            [
                json!({"id": "gemini-3.8-flash", "display_name": "Gemini 3.8 Flash"}),
                json!({"id": "claude-sonnet-4-6", "display_name": "Claude Sonnet 4.6 (Thinking)"}),
            ]
        );
        assert_eq!(
            config_option(&session, "mode").and_then(|option| option["id"].as_str()),
            Some("mode")
        );
        session["configOptions"] = json!([]);
        assert_eq!(
            session_models(&session)[0],
            json!({"id": "gemini-3.8-flash-high", "display_name": "Gemini 3.8 Flash (High)"})
        );
    }
}
