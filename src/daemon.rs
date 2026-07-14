use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use portable_pty::PtySize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::codex::CodexConnection;
use crate::paths;
use crate::protocol::{Request, Response, SessionRecord, TurnRecord};
use crate::provider::{
    Agent, CommandSpec, LaunchOptions, codex_remote_tui_command, command_spec, prepare_workspace,
};
use crate::session::SessionRuntime;
use crate::store::{NewSession, Store};

pub fn run() -> Result<()> {
    let socket_path = paths::socket_path()?;
    if let Some(parent) = socket_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    if socket_path.exists() {
        if UnixStream::connect(&socket_path).is_ok() {
            bail!(
                "dlgt server is already running at {}",
                socket_path.display()
            );
        }
        fs::remove_file(&socket_path)
            .with_context(|| format!("failed to remove stale socket {}", socket_path.display()))?;
    }

    let store = Store::open(&paths::database_path()?)?;
    store.reconcile_after_restart()?;
    let daemon = Arc::new(Daemon {
        store: Arc::new(Mutex::new(store)),
        sessions: Arc::new(RwLock::new(HashMap::new())),
        attach_leases: Mutex::new(HashMap::new()),
        shutting_down: AtomicBool::new(false),
    });
    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("failed to bind {}", socket_path.display()))?;
    fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to secure {}", socket_path.display()))?;
    listener
        .set_nonblocking(true)
        .context("failed to make server socket nonblocking")?;

    while !daemon.shutting_down.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _address)) => {
                // Accepted sockets inherit O_NONBLOCK on macOS. RPC frames
                // larger than the socket buffer otherwise fail at ~8 KiB.
                stream
                    .set_nonblocking(false)
                    .context("failed to make accepted RPC socket blocking")?;
                let daemon = Arc::clone(&daemon);
                std::thread::Builder::new()
                    .name("dlgt-rpc".to_owned())
                    .spawn(move || {
                        if let Err(error) = daemon.handle_connection(stream) {
                            eprintln!("dlgt RPC connection failed: {error:#}");
                        }
                    })
                    .context("failed to start RPC thread")?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error).context("failed to accept RPC connection"),
        }
    }

    if let Ok(sessions) = daemon.sessions.read() {
        for runtime in sessions.values() {
            let _ = runtime.stop();
        }
    }
    drop(listener);
    if socket_path.exists() {
        fs::remove_file(&socket_path)
            .with_context(|| format!("failed to remove {}", socket_path.display()))?;
    }
    Ok(())
}

struct Daemon {
    store: Arc<Mutex<Store>>,
    sessions: Arc<RwLock<HashMap<String, Arc<AgentRuntime>>>>,
    attach_leases: Mutex<HashMap<String, String>>,
    shutting_down: AtomicBool,
}

enum AgentRuntime {
    Claude(Arc<SessionRuntime>),
    Codex {
        control: Arc<CodexConnection>,
        provider_thread_id: String,
        view: Arc<SessionRuntime>,
    },
}

impl AgentRuntime {
    fn pid(&self) -> Option<u32> {
        match self {
            Self::Claude(runtime) => runtime.pid(),
            Self::Codex { view, .. } => view.pid(),
        }
    }

    fn write(&self, data: &[u8]) -> Result<()> {
        match self {
            Self::Claude(runtime) => runtime.write(data),
            Self::Codex { view, .. } => view.write(data),
        }
    }

    fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        match self {
            Self::Claude(runtime) => runtime.resize(rows, cols),
            Self::Codex { view, .. } => view.resize(rows, cols),
        }
    }

    fn subscribe(&self) -> Result<(Vec<u8>, std::sync::mpsc::Receiver<Vec<u8>>)> {
        match self {
            Self::Claude(runtime) => runtime.subscribe(),
            Self::Codex { view, .. } => view.subscribe(),
        }
    }

    fn stop(&self) -> Result<()> {
        match self {
            Self::Claude(runtime) => runtime.stop(),
            Self::Codex { view, .. } => view.stop(),
        }
    }

    fn force_stop(&self) -> Result<()> {
        match self {
            Self::Claude(runtime) => runtime.force_stop(),
            Self::Codex { view, .. } => view.force_stop(),
        }
    }

    fn wait_for_input_ready(&self, timeout: Duration) -> Result<()> {
        match self {
            Self::Claude(runtime) => runtime.wait_for_input_ready(timeout),
            Self::Codex { view, .. } => view.wait_for_input_ready(timeout),
        }
    }

    fn start_codex_turn(&self, prompt: &str) -> Result<String> {
        match self {
            Self::Codex {
                control,
                provider_thread_id,
                ..
            } => {
                let turn_id = control.start_turn(provider_thread_id, prompt)?;
                control.join_thread(provider_thread_id)?;
                control.watch_turn(provider_thread_id, &turn_id)?;
                Ok(turn_id)
            }
            Self::Claude(_) => bail!("Claude turns use semantic PTY input"),
        }
    }

    fn interrupt_codex_turn(&self, provider_turn_id: &str) -> Result<()> {
        match self {
            Self::Codex {
                control,
                provider_thread_id,
                ..
            } => control.interrupt_turn(provider_thread_id, provider_turn_id),
            Self::Claude(_) => bail!("Claude turns use semantic PTY input"),
        }
    }
}

impl Daemon {
    fn handle_connection(&self, mut stream: UnixStream) -> Result<()> {
        let mut line = String::new();
        BufReader::new(stream.try_clone()?)
            .read_line(&mut line)
            .context("failed to read RPC request")?;
        let request = match serde_json::from_str::<Request>(&line) {
            Ok(request) => request,
            Err(error) => {
                write_response(
                    &mut stream,
                    &Response::error("unknown", "INVALID_REQUEST", error.to_string()),
                )?;
                return Ok(());
            }
        };

        if request.method == "view.subscribe" {
            return self.subscribe_view(&mut stream, &request);
        }
        if request.method == "event.subscribe" {
            return self.subscribe_events(&mut stream, &request);
        }
        let response = match self.dispatch(&request.method, &request.params) {
            Ok(result) => Response::ok(request.id, result),
            Err(error) => Response::error(request.id, classify_error(&error), format!("{error:#}")),
        };
        write_response(&mut stream, &response)
    }

    fn dispatch(&self, method: &str, params: &Value) -> Result<Value> {
        match method {
            "server.ping" => Ok(json!({"ok": true, "version": env!("CARGO_PKG_VERSION")})),
            "server.stop" => {
                self.shutting_down.store(true, Ordering::SeqCst);
                Ok(json!({"stopping": true}))
            }
            "session.create" => self.create_session(params),
            "session.restart" => self.restart_session(params),
            "session.list" => {
                let include_all = params.get("all").and_then(Value::as_bool).unwrap_or(false);
                let sessions = self.lock_store()?.list_sessions()?;
                Ok(Value::Array(
                    sessions
                        .into_iter()
                        .filter(|session| {
                            include_all || !matches!(session.state.as_str(), "stopped" | "failed")
                        })
                        .map(|session| public_session(&session))
                        .collect(),
                ))
            }
            "session.read" => self.read_session(params),
            "session.input" => self.input_session(params),
            "session.resize" => self.resize_session(params),
            "session.stop" => self.stop_session(params),
            "session.send" => self.submit_turn(params),
            "session.wait" => self.wait_session(params),
            "session.cancel" => self.cancel_session(params),
            "transcript.read_raw" => self.read_transcript(params),
            "event.read" => self.read_events(params),
            "scrollback.read" => self.read_scrollback(params),
            "model.list" => Self::list_models(params),
            "harness.list" => Self::list_harnesses(params),
            "hook.event" => self.handle_hook(params),
            _ => bail!("unknown method {method:?}"),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn create_session(&self, params: &Value) -> Result<Value> {
        let title = params_string(params, "title")?;
        let generated_alias = generate_alias(title);
        let alias = params
            .get("alias")
            .and_then(Value::as_str)
            .unwrap_or(&generated_alias);
        validate_alias(alias)?;
        let agent = Agent::parse(params_string(params, "harness")?)?;
        let cwd = params
            .get("cwd")
            .and_then(Value::as_str)
            .map_or_else(std::env::current_dir, |value| Ok(PathBuf::from(value)))?
            .canonicalize()
            .context("session cwd does not exist")?;
        if !cwd.is_dir() {
            bail!("session cwd is not a directory: {}", cwd.display());
        }
        let model = params.get("model").and_then(Value::as_str);
        let effort = params.get("effort").and_then(Value::as_str);
        let environment = params
            .get("environment")
            .and_then(Value::as_object)
            .context("missing launch environment snapshot")?
            .iter()
            .map(|(key, value)| {
                value
                    .as_str()
                    .map(|value| (key.clone(), value.to_owned()))
                    .with_context(|| format!("environment value for {key:?} must be a string"))
            })
            .collect::<Result<HashMap<_, _>>>()?;
        let rows = params_u16(params, "rows", 24)?;
        let cols = params_u16(params, "cols", 80)?;
        let mut id = generate_session_id();

        prepare_workspace(agent, &cwd)?;
        for attempt in 0..16 {
            let inserted = self.lock_store()?.insert_session(&NewSession {
                id: &id,
                alias,
                title,
                agent: agent.as_str(),
                cwd: &cwd.to_string_lossy(),
                model,
                effort,
            });
            match inserted {
                Ok(()) => break,
                Err(error) if error.to_string().contains("sessions.id") && attempt < 15 => {
                    id = generate_session_id();
                }
                Err(error) => return Err(error),
            }
        }
        self.lock_store()?
            .record_event(Some(&id), None, "session.created", &json!({}))?;
        self.lock_store()?.set_terminal_size(&id, rows, cols)?;
        let options = LaunchOptions {
            agent,
            session_id: &id,
            alias,
            cwd: &cwd,
            model,
            effort,
            resume_provider_id: None,
            environment: &environment,
        };

        let startup_timeout = Duration::from_millis(
            params
                .get("startup_timeout_ms")
                .and_then(Value::as_u64)
                .unwrap_or(60_000)
                .min(300_000),
        );
        let startup_deadline = Instant::now() + startup_timeout;
        let runtime = match agent {
            Agent::Claude => command_spec(&options)
                .and_then(|spec| self.spawn_claude_runtime(&id, &spec, rows, cols)),
            Agent::Codex => self.spawn_codex_runtime(&options, rows, cols, startup_timeout),
        };
        let runtime = match runtime {
            Ok(runtime) => runtime,
            Err(error) => {
                let store = self.lock_store()?;
                store.set_session_failed(&id)?;
                store.record_event(
                    Some(&id),
                    None,
                    "session.failed",
                    &json!({"error": error.to_string()}),
                )?;
                return Err(error);
            }
        };
        let pid = runtime.pid();
        self.sessions
            .write()
            .map_err(|_| anyhow!("session map lock poisoned"))?
            .insert(id.clone(), Arc::clone(&runtime));
        let store = self.lock_store()?;
        if !store.set_session_running(&id, pid)? {
            drop(store);
            self.sessions
                .write()
                .map_err(|_| anyhow!("session map lock poisoned"))?
                .remove(&id);
            let session = self
                .lock_store()?
                .get_session(&id)?
                .context("exited session not found")?;
            return Ok(serde_json::to_value(session)?);
        }
        store.record_event(
            Some(&id),
            None,
            "session.started",
            &json!({"agent": agent.as_str(), "pid": pid}),
        )?;
        if agent == Agent::Codex {
            store.set_session_state(&id, "idle")?;
            store.record_event(
                Some(&id),
                None,
                "session.ready",
                &json!({"provider_session_id": store.get_session(&id)?.and_then(|value| value.provider_session_id)}),
            )?;
        }
        drop(store);
        if agent == Agent::Codex {
            let remaining = startup_deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let _ = runtime.force_stop();
                bail!("launch timed out before the Session became ready");
            }
            if let Err(error) = runtime.wait_for_input_ready(remaining) {
                let _ = runtime.force_stop();
                return Err(error).context("Codex PTY did not become input-ready");
            }
        }
        let session = loop {
            let current = self.resolve_session(&id)?;
            if current.state == "idle" {
                break current;
            }
            if matches!(current.state.as_str(), "stopped" | "failed") {
                bail!("launch failed before the Session became ready");
            }
            if Instant::now() >= startup_deadline {
                let _ = runtime.force_stop();
                self.lock_store()?.set_session_failed(&id)?;
                bail!("launch timed out before the Session became ready");
            }
            std::thread::sleep(Duration::from_millis(25));
        };
        let mut response = json!({"session": public_session(&session)});
        if let Some(prompt) = params.get("prompt").and_then(Value::as_str) {
            match self.submit_turn(&json!({"session": id, "prompt": prompt})) {
                Ok(result) => response = result,
                Err(error) => {
                    let _ = runtime.force_stop();
                    self.lock_store()?.set_session_failed(&id)?;
                    return Err(error).context("initial prompt acceptance failed");
                }
            }
        }
        Ok(response)
    }

    #[allow(clippy::too_many_lines)]
    fn restart_session(&self, params: &Value) -> Result<Value> {
        let session = self.resolve_session(params_string(params, "session")?)?;
        if !matches!(session.state.as_str(), "stopped" | "failed") {
            bail!("session is unavailable in state {}", session.state);
        }
        if self
            .sessions
            .read()
            .map_err(|_| anyhow!("session map lock poisoned"))?
            .contains_key(&session.id)
        {
            bail!(
                "session is unavailable while its previous process is still active: {}",
                session.id
            );
        }
        let provider_id = session
            .provider_session_id
            .as_deref()
            .context("session is unavailable because it has no provider conversation to resume")?;
        let agent = Agent::parse(&session.agent)?;
        let cwd = PathBuf::from(&session.cwd)
            .canonicalize()
            .context("session cwd does not exist")?;
        if !cwd.is_dir() {
            bail!("session cwd is not a directory: {}", cwd.display());
        }
        let environment = params
            .get("environment")
            .and_then(Value::as_object)
            .context("missing launch environment snapshot")?
            .iter()
            .map(|(key, value)| {
                value
                    .as_str()
                    .map(|value| (key.clone(), value.to_owned()))
                    .with_context(|| format!("environment value for {key:?} must be a string"))
            })
            .collect::<Result<HashMap<_, _>>>()?;
        let rows = params_u16(params, "rows", 24)?;
        let cols = params_u16(params, "cols", 80)?;
        let startup_timeout = Duration::from_millis(
            params
                .get("startup_timeout_ms")
                .and_then(Value::as_u64)
                .unwrap_or(60_000)
                .min(300_000),
        );
        let startup_deadline = Instant::now() + startup_timeout;

        prepare_workspace(agent, &cwd)?;
        if !self.lock_store()?.restart_session(&session.id)? {
            bail!("session is unavailable in state {}", session.state);
        }
        {
            let store = self.lock_store()?;
            store.set_terminal_size(&session.id, rows, cols)?;
            store.record_event(
                Some(&session.id),
                None,
                "session.restarting",
                &json!({"provider_session_id": provider_id}),
            )?;
        }
        let options = LaunchOptions {
            agent,
            session_id: &session.id,
            alias: &session.alias,
            cwd: &cwd,
            model: session.model.as_deref(),
            effort: session.effort.as_deref(),
            resume_provider_id: Some(provider_id),
            environment: &environment,
        };
        let runtime = match agent {
            Agent::Claude => command_spec(&options)
                .and_then(|spec| self.spawn_claude_runtime(&session.id, &spec, rows, cols)),
            Agent::Codex => self.spawn_codex_runtime(&options, rows, cols, startup_timeout),
        };
        let runtime = match runtime {
            Ok(runtime) => runtime,
            Err(error) => {
                let store = self.lock_store()?;
                store.set_session_failed(&session.id)?;
                store.record_event(
                    Some(&session.id),
                    None,
                    "session.failed",
                    &json!({"error": error.to_string()}),
                )?;
                return Err(error).context("session restart launch failed");
            }
        };
        let pid = runtime.pid();
        self.sessions
            .write()
            .map_err(|_| anyhow!("session map lock poisoned"))?
            .insert(session.id.clone(), Arc::clone(&runtime));
        let store = self.lock_store()?;
        if !store.set_session_running(&session.id, pid)? {
            drop(store);
            self.sessions
                .write()
                .map_err(|_| anyhow!("session map lock poisoned"))?
                .remove(&session.id);
            let _ = runtime.force_stop();
            bail!("restart launch exited before the Session became ready");
        }
        store.record_event(
            Some(&session.id),
            None,
            "session.started",
            &json!({"agent": agent.as_str(), "pid": pid, "restart": true}),
        )?;
        if agent == Agent::Codex {
            store.set_session_state(&session.id, "idle")?;
            store.record_event(
                Some(&session.id),
                None,
                "session.ready",
                &json!({"provider_session_id": provider_id}),
            )?;
        }
        drop(store);
        if agent == Agent::Codex {
            let remaining = startup_deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let _ = runtime.force_stop();
                self.lock_store()?.set_session_failed(&session.id)?;
                bail!("restart launch timed out before the Session became ready");
            }
            if let Err(error) = runtime.wait_for_input_ready(remaining) {
                let _ = runtime.force_stop();
                self.lock_store()?.set_session_failed(&session.id)?;
                return Err(error).context("restarted Codex PTY did not become input-ready");
            }
        }
        let current = loop {
            let current = self.resolve_session(&session.id)?;
            if current.state == "idle" {
                break current;
            }
            if matches!(current.state.as_str(), "stopped" | "failed") {
                bail!("restart launch failed before the Session became ready");
            }
            if Instant::now() >= startup_deadline {
                let _ = runtime.force_stop();
                self.lock_store()?.set_session_failed(&session.id)?;
                bail!("restart launch timed out before the Session became ready");
            }
            std::thread::sleep(Duration::from_millis(25));
        };
        Ok(json!({"session": public_session(&current)}))
    }

    fn spawn_claude_runtime(
        &self,
        session_id: &str,
        spec: &CommandSpec,
        rows: u16,
        cols: u16,
    ) -> Result<Arc<AgentRuntime>> {
        let output_store = Arc::clone(&self.store);
        let output_session_id = session_id.to_owned();
        let on_output = Arc::new(move |data: &[u8]| {
            if let Ok(store) = output_store.lock()
                && let Err(error) = store.record_output(&output_session_id, data)
            {
                eprintln!("dlgt failed to persist PTY output: {error:#}");
            }
        });
        let exit_store = Arc::clone(&self.store);
        let exit_sessions = Arc::clone(&self.sessions);
        let exit_session_id = session_id.to_owned();
        let on_exit = Arc::new(move |exit_code: u32| {
            persist_session_exit(&exit_store, &exit_sessions, &exit_session_id, exit_code);
        });
        SessionRuntime::spawn(
            spec,
            PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            },
            on_output,
            on_exit,
        )
        .map(|runtime| Arc::new(AgentRuntime::Claude(runtime)))
    }

    fn spawn_codex_runtime(
        &self,
        options: &LaunchOptions<'_>,
        rows: u16,
        cols: u16,
        startup_timeout: Duration,
    ) -> Result<Arc<AgentRuntime>> {
        let session_id = options.session_id.to_owned();
        let socket_path = paths::home_dir()?
            .join("run")
            .join(&session_id)
            .join("app-server.sock");
        let (thread_sender, thread_receiver) = std::sync::mpsc::channel();
        let event_store = Arc::clone(&self.store);
        let event_session_id = session_id.clone();
        let handler = Arc::new(move |message: Value| {
            if message.get("method").and_then(Value::as_str) == Some("thread/started")
                && let Some(thread_id) =
                    message.pointer("/params/thread/id").and_then(Value::as_str)
            {
                let _ = thread_sender.send(thread_id.to_owned());
            }
            if let Ok(mut store) = event_store.lock()
                && let Err(error) =
                    apply_codex_notification(&mut store, &event_session_id, &message)
            {
                eprintln!("dlgt failed to apply Codex notification: {error:#}");
            }
        });
        let control = CodexConnection::connect_with_environment(
            socket_path.clone(),
            handler,
            Some(options.environment),
        )?;
        let spec = codex_remote_tui_command(options, &socket_path);
        let output_store = Arc::clone(&self.store);
        let output_session_id = session_id.clone();
        let on_output = Arc::new(move |data: &[u8]| {
            if let Ok(store) = output_store.lock()
                && let Err(error) = store.record_output(&output_session_id, data)
            {
                eprintln!("dlgt failed to persist Codex TUI output: {error:#}");
            }
        });
        let exit_store = Arc::clone(&self.store);
        let exit_sessions = Arc::clone(&self.sessions);
        let exit_session_id = session_id.clone();
        let on_exit = Arc::new(move |exit_code: u32| {
            persist_session_exit(&exit_store, &exit_sessions, &exit_session_id, exit_code);
        });
        let view = SessionRuntime::spawn(
            &spec,
            PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            },
            on_output,
            on_exit,
        )?;
        let provider_thread_id = match thread_receiver.recv_timeout(startup_timeout) {
            Ok(thread_id) => thread_id,
            Err(error) => {
                let _ = view.force_stop();
                return Err(error).context("Codex remote TUI did not create a thread");
            }
        };
        if let Some(expected) = options.resume_provider_id
            && provider_thread_id != expected
        {
            let _ = view.force_stop();
            bail!(
                "resumed Codex provider conversation mismatch: expected {expected}, got {provider_thread_id}"
            );
        }
        Ok(Arc::new(AgentRuntime::Codex {
            control,
            provider_thread_id,
            view,
        }))
    }

    fn input_session(&self, params: &Value) -> Result<Value> {
        let session = self.resolve_session(params_string(params, "session")?)?;
        let encoded = params_string(params, "data_base64")?;
        let data = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .context("invalid data_base64")?;
        let source = params
            .get("source")
            .and_then(Value::as_str)
            .unwrap_or("api");
        if source == "attach" {
            let lease_id = params_string(params, "lease_id")?;
            let leases = self
                .attach_leases
                .lock()
                .map_err(|_| anyhow!("attach lease lock poisoned"))?;
            if leases.get(&session.id).map(String::as_str) != Some(lease_id) {
                bail!("attach lease is no longer active");
            }
        }
        let turn_id = session.active_turn_id.as_deref();
        let runtime = self.runtime(&session.id)?;
        let seq = {
            let store = self.lock_store()?;
            if session.state == "blocked" {
                store.set_session_state(&session.id, "busy")?;
                store.record_event(
                    Some(&session.id),
                    session.active_turn_id.as_deref(),
                    "session.resumed",
                    &json!({}),
                )?;
            }
            let seq = store.record_input(&session.id, turn_id, source, &data)?;
            store.record_event(
                Some(&session.id),
                turn_id,
                "input.observed",
                &json!({"source": source, "seq": seq, "byte_len": data.len()}),
            )?;
            seq
        };
        runtime.write(&data)?;
        Ok(json!({"accepted": true, "seq": seq, "byte_len": data.len()}))
    }

    fn resize_session(&self, params: &Value) -> Result<Value> {
        let session = self.resolve_session(params_string(params, "session")?)?;
        let rows = params_u16(params, "rows", 24)?;
        let cols = params_u16(params, "cols", 80)?;
        self.runtime(&session.id)?.resize(rows, cols)?;
        self.lock_store()?
            .set_terminal_size(&session.id, rows, cols)?;
        Ok(json!({"rows": rows, "cols": cols}))
    }

    fn stop_session(&self, params: &Value) -> Result<Value> {
        let session = self.resolve_session(params_string(params, "session")?)?;
        let runtime = self.runtime(&session.id)?;
        let force = params
            .get("force")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !self
            .lock_store()?
            .set_session_state(&session.id, "stopping")?
        {
            bail!("session is already stopped or failed");
        }
        self.lock_store()?.record_event(
            Some(&session.id),
            None,
            "session.stopping",
            &json!({"force": force}),
        )?;
        if force {
            runtime.force_stop()?;
        } else {
            runtime.stop()?;
        }
        Ok(json!({"stopping": true, "force": force, "session_id": session.id}))
    }

    fn submit_turn(&self, params: &Value) -> Result<Value> {
        let session = self.resolve_session(params_string(params, "session")?)?;
        if self
            .attach_leases
            .lock()
            .map_err(|_| anyhow!("attach lease lock poisoned"))?
            .contains_key(&session.id)
        {
            bail!("session has an exclusive attach lease");
        }
        if session.state == "blocked" {
            bail!("session blocked on input");
        }
        if matches!(session.state.as_str(), "busy" | "quiescing")
            || session.active_turn_id.is_some()
        {
            bail!("session already has an active turn");
        }
        if session.state != "idle" {
            bail!("session is unavailable in state {}", session.state);
        }
        let prompt = params_string(params, "prompt")?;
        if prompt.is_empty() {
            bail!("prompt must not be empty");
        }
        let runtime = self.runtime(&session.id)?;
        let agent = Agent::parse(&session.agent)?;
        let turn_id = format!("turn_{}", Uuid::new_v4().simple());
        let input = match agent {
            Agent::Codex => prompt.as_bytes().to_vec(),
            Agent::Claude => agent.semantic_input(prompt)?,
        };
        let turn = {
            let mut store = self.lock_store()?;
            let turn = store.insert_turn(&turn_id, &session.id, prompt)?;
            store.record_input(&session.id, Some(&turn_id), "api", &input)?;
            store.record_event(
                Some(&session.id),
                Some(&turn_id),
                "turn.submitted",
                &json!({"prompt": prompt}),
            )?;
            turn
        };
        match agent {
            Agent::Codex => match runtime.start_codex_turn(prompt) {
                Ok(provider_turn_id) => {
                    let store = self.lock_store()?;
                    if store.mark_turn_started(&turn_id, Some(&provider_turn_id))? {
                        store.set_session_state(&session.id, "busy")?;
                        store.record_event(
                            Some(&session.id),
                            Some(&turn_id),
                            "turn.started",
                            &json!({"provider_turn_id": provider_turn_id}),
                        )?;
                    }
                }
                Err(error) => {
                    let store = self.lock_store()?;
                    let message = sanitize_message(&error.to_string());
                    let _ = store.finish_turn_if_matching(
                        &turn_id,
                        None,
                        "failed",
                        None,
                        Some(&message),
                    )?;
                    store.set_session_failed(&session.id)?;
                    store.record_event(
                        Some(&session.id),
                        Some(&turn_id),
                        "turn.failed",
                        &json!({"error": message}),
                    )?;
                    store.record_event(
                        Some(&session.id),
                        None,
                        "session.failed",
                        &json!({"error": message}),
                    )?;
                    drop(store);
                    let _ = runtime.force_stop();
                    return Err(error);
                }
            },
            Agent::Claude => {
                if let Err(error) = write_semantic_input(&runtime, &input) {
                    let _ = self.lock_store()?.cancel_turn(&turn_id)?;
                    return Err(error);
                }
                self.lock_store()?.set_session_state(&session.id, "busy")?;
            }
        }
        let current = self.resolve_session(&session.id)?;
        Ok(json!({
            "session": public_session(&current),
            "execution_seq": turn.execution_seq,
        }))
    }

    fn read_session(&self, params: &Value) -> Result<Value> {
        let session = self.resolve_session(params_string(params, "session")?)?;
        let latest = self.lock_store()?.latest_turn(&session.id)?;
        Ok(json!({
            "session": public_session(&session),
            "result": latest.as_ref().filter(|turn| is_terminal_turn_state(&turn.state)).map(public_result),
            "execution_seq": latest.as_ref().map(|turn| turn.execution_seq),
        }))
    }

    fn wait_session(&self, params: &Value) -> Result<Value> {
        let session = self.resolve_session(params_string(params, "session")?)?;
        let timeout_ms = params
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .context("missing positive timeout_ms")?;
        if timeout_ms == 0 {
            bail!("timeout_ms must be positive");
        }
        if session.state == "blocked" {
            bail!("session blocked on input");
        }
        let target = if let Some(id) = session.active_turn_id {
            self.resolve_turn(&id)?
        } else {
            self.lock_store()?
                .latest_turn(&session.id)?
                .context("session has no result")?
        };
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            let turn = self.resolve_turn(&target.id)?;
            if is_terminal_turn_state(&turn.state) {
                return Ok(json!({
                    "session": public_session(&self.resolve_session(&session.id)?),
                    "result": public_result(&turn),
                    "execution_seq": turn.execution_seq,
                }));
            }
            if self.resolve_session(&session.id)?.state == "blocked" {
                bail!("session blocked on input");
            }
            if Instant::now() >= deadline {
                bail!("wait timed out; execution continues");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn cancel_session(&self, params: &Value) -> Result<Value> {
        let session = self.resolve_session(params_string(params, "session")?)?;
        let Some(turn_id) = session.active_turn_id else {
            return Ok(json!({"session_id": session.id, "canceled": false}));
        };
        let timeout_ms = params
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(30_000);
        self.cancel_turn(&json!({"turn": turn_id}))?;
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            let current = self.resolve_session(&session.id)?;
            if current.state == "idle" || current.active_turn_id.is_none() {
                let result = self.lock_store()?.latest_turn(&session.id)?;
                return Ok(json!({
                    "session": public_session(&current),
                    "canceled": true,
                    "result": result.as_ref().map(public_result),
                }));
            }
            if Instant::now() >= deadline {
                bail!("cancel timed out; cancellation continues");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn cancel_turn(&self, params: &Value) -> Result<Value> {
        let turn = self.resolve_turn(params_string(params, "turn")?)?;
        let session = self.resolve_session(&turn.session_id)?;
        let agent = Agent::parse(&session.agent)?;
        let cancel_input = agent.cancel_input();
        let runtime = self.runtime(&turn.session_id)?;
        if agent == Agent::Codex {
            let provider_turn_id = turn
                .provider_turn_id
                .as_deref()
                .context("Codex turn has not been accepted by app-server")?;
            runtime.interrupt_codex_turn(provider_turn_id)?;
        }
        {
            let mut store = self.lock_store()?;
            if !store.cancel_turn(&turn.id)? {
                bail!("turn is already terminal or no longer active");
            }
            if agent == Agent::Claude {
                store.record_input(&turn.session_id, Some(&turn.id), "api", cancel_input)?;
            }
            store.record_event(
                Some(&turn.session_id),
                Some(&turn.id),
                "turn.canceled",
                &json!({}),
            )?;
        }
        if agent == Agent::Claude {
            runtime.write(cancel_input)?;
        }
        Ok(json!({"canceled": true, "turn_id": turn.id}))
    }

    fn read_transcript(&self, params: &Value) -> Result<Value> {
        let session = self.resolve_session(params_string(params, "session")?)?;
        let after = params.get("after").and_then(Value::as_i64).unwrap_or(0);
        let limit_bytes = params
            .get("limit_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(1024 * 1024)
            .min(8 * 1024 * 1024);
        let limit_bytes = usize::try_from(limit_bytes).context("limit_bytes is too large")?;
        let page = self
            .lock_store()?
            .read_output_page(&session.id, after, limit_bytes.max(1))?;
        Ok(json!({
            "session_id": session.id,
            "data_base64": base64::engine::general_purpose::STANDARD.encode(&page.data),
            "byte_len": page.data.len(),
            "next_after": page.next_after,
            "has_more": page.has_more,
        }))
    }

    fn read_events(&self, params: &Value) -> Result<Value> {
        let after = params.get("after").and_then(Value::as_i64).unwrap_or(0);
        let session_id = if let Some(selector) = params.get("session").and_then(Value::as_str) {
            Some(self.resolve_session(selector)?.id)
        } else {
            None
        };
        let events = self
            .lock_store()?
            .read_events(session_id.as_deref(), after)?;
        let store = self.lock_store()?;
        let normalized = events
            .into_iter()
            .filter_map(|event| {
                let event_type = normalize_event_type(&event.kind)?;
                let turn = event
                    .turn_id
                    .as_deref()
                    .and_then(|id| store.get_turn(id).ok().flatten());
                let execution_seq = turn.as_ref().map(|turn| turn.execution_seq);
                let mut value = json!({
                    "schema_version": 1,
                    "seq": event.seq,
                    "type": event_type,
                    "session_id": event.session_id,
                });
                if let Some(seq) = execution_seq {
                    value["execution_seq"] = json!(seq);
                }
                if event_type == "provider.retrying" {
                    value["attempt"] = event.payload.get("attempt").cloned().unwrap_or(json!(1));
                }
                if event_type == "session.idle" {
                    value["result_status"] =
                        turn.as_ref().map_or(Value::Null, |turn| json!(turn.state));
                }
                if event_type == "session.blocked" {
                    value["reason"] = json!("user_input");
                }
                Some(value)
            })
            .collect::<Vec<_>>();
        Ok(Value::Array(normalized))
    }

    fn read_scrollback(&self, params: &Value) -> Result<Value> {
        let session = self.resolve_session(params_string(params, "session")?)?;
        let lines = usize::try_from(
            params
                .get("lines")
                .and_then(Value::as_u64)
                .unwrap_or(100)
                .clamp(1, 10_000),
        )
        .unwrap_or(100);
        let mut raw = Vec::new();
        let mut after = 0;
        loop {
            let page = self
                .lock_store()?
                .read_output_page(&session.id, after, 8 * 1024 * 1024)?;
            raw.extend_from_slice(&page.data);
            if !page.has_more || page.next_after <= after {
                break;
            }
            after = page.next_after;
        }
        let (stored_rows, stored_cols) = self.lock_store()?.terminal_size(&session.id)?;
        let mut parser = vt100::Parser::new(stored_rows, stored_cols, 10_000);
        parser.process(&raw);
        parser.screen_mut().set_scrollback(usize::MAX);
        let history = parser.screen().scrollback();
        let (rows, cols) = parser.screen().size();
        let total = history + usize::from(rows);
        let before = params
            .get("before")
            .and_then(Value::as_str)
            .and_then(|cursor| cursor.strip_prefix("scr_"))
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(total)
            .min(total);
        let start = before.saturating_sub(lines);
        let mut selected = Vec::with_capacity(before.saturating_sub(start));
        for index in start..before {
            if index < history {
                parser.screen_mut().set_scrollback(history - index);
                selected.push(parser.screen().rows(0, cols).next().unwrap_or_default());
            } else {
                parser.screen_mut().set_scrollback(0);
                selected.push(
                    parser
                        .screen()
                        .rows(0, cols)
                        .nth(index - history)
                        .unwrap_or_default(),
                );
            }
        }
        Ok(json!({
            "session_id": session.id,
            "screen": {"rows": rows, "cols": cols},
            "lines": selected,
            "truncated": start > 0 || history == 10_000,
            "before": (start > 0).then(|| format!("scr_{start}")),
        }))
    }

    fn list_models(params: &Value) -> Result<Value> {
        match params_string(params, "harness")? {
            "claude" => Ok(json!({
                "harness": "claude", "source": "claude-code-aliases", "discovery": "partial",
                "models": [
                    {"id":"default","recommended":true}, {"id":"best"},
                    {"id":"sonnet"}, {"id":"opus"}, {"id":"haiku"}
                ]
            })),
            "codex" => {
                let socket = paths::home_dir()?
                    .join("run")
                    .join(format!("models-{}", Uuid::new_v4().simple()))
                    .join("app-server.sock");
                let connection = CodexConnection::connect(socket, Arc::new(|_| {}))?;
                let response = connection.list_models(
                    params
                        .get("include_hidden")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                )?;
                let models = response
                    .get("data")
                    .or_else(|| response.get("models"))
                    .cloned()
                    .unwrap_or(response);
                Ok(
                    json!({"harness":"codex","source":"app-server","discovery":"complete","models":models}),
                )
            }
            other => bail!("unsupported harness {other:?}"),
        }
    }

    fn list_harnesses(params: &Value) -> Result<Value> {
        let all = json!([
            {"id":"codex","model_discovery":"complete","effort":true},
            {"id":"claude","model_discovery":"partial","effort":true}
        ]);
        if let Some(name) = params.get("harness").and_then(Value::as_str) {
            return all
                .as_array()
                .and_then(|items| items.iter().find(|item| item["id"] == name))
                .cloned()
                .with_context(|| format!("harness not found: {name}"));
        }
        Ok(all)
    }

    fn handle_hook(&self, params: &Value) -> Result<Value> {
        let session = self.resolve_session(params_string(params, "session")?)?;
        let agent = params_string(params, "agent")?;
        if agent != session.agent {
            bail!(
                "hook agent mismatch: session uses {}, hook reported {agent}",
                session.agent
            );
        }
        let payload = params.get("payload").cloned().unwrap_or(Value::Null);
        let event_name = payload
            .get("hook_event_name")
            .and_then(Value::as_str)
            .context("hook payload has no hook_event_name")?;
        let mut store = self.lock_store()?;
        bind_provider_session(&store, &session, &payload)?;
        let outcome = apply_hook_event(&mut store, &session, event_name, &payload)?;
        let seq = store.record_event(
            Some(&session.id),
            outcome.turn_id.as_deref(),
            outcome.kind,
            &payload,
        )?;
        Ok(json!({
            "accepted": true,
            "seq": seq,
            "event": outcome.kind,
            "turn_id": outcome.turn_id,
        }))
    }

    fn subscribe_view(&self, stream: &mut UnixStream, request: &Request) -> Result<()> {
        let result = (|| -> Result<_> {
            let selector = params_string(&request.params, "session")?;
            let session = self.resolve_session(selector)?;
            let lease_id = params_string(&request.params, "lease_id")?.to_owned();
            let steal = request
                .params
                .get("steal")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let mut leases = self
                .attach_leases
                .lock()
                .map_err(|_| anyhow!("attach lease lock poisoned"))?;
            if leases.contains_key(&session.id) && !steal {
                bail!("session is already attached");
            }
            leases.insert(session.id.clone(), lease_id.clone());
            drop(leases);
            let (replay, receiver) = self.runtime(&session.id)?.subscribe()?;
            Ok((session, lease_id, replay, receiver))
        })();
        let (session, lease_id, replay, receiver) = match result {
            Ok(value) => value,
            Err(error) => {
                return write_response(
                    stream,
                    &Response::error(&request.id, classify_error(&error), format!("{error:#}")),
                );
            }
        };
        write_response(
            stream,
            &Response::ok(
                &request.id,
                json!({
                    "session_id": session.id,
                    "replay_base64": base64::engine::general_purpose::STANDARD.encode(replay),
                }),
            ),
        )?;
        for chunk in receiver {
            if stream.write_all(&chunk).is_err() || stream.flush().is_err() {
                break;
            }
        }
        if let Ok(mut leases) = self.attach_leases.lock()
            && leases.get(&session.id) == Some(&lease_id)
        {
            leases.remove(&session.id);
        }
        Ok(())
    }

    fn subscribe_events(&self, stream: &mut UnixStream, request: &Request) -> Result<()> {
        let mut after = request
            .params
            .get("after")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let session = request
            .params
            .get("session")
            .and_then(Value::as_str)
            .map(|selector| self.resolve_session(selector).map(|session| session.id))
            .transpose()?;
        write_response(
            stream,
            &Response::ok(&request.id, json!({"subscribed":true,"after":after})),
        )?;
        while !self.shutting_down.load(Ordering::SeqCst) {
            let events = self.read_events(&json!({"session":session,"after":after}))?;
            for event in events.as_array().into_iter().flatten() {
                serde_json::to_writer(&mut *stream, event)?;
                stream.write_all(b"\n")?;
                stream.flush()?;
                after = event.get("seq").and_then(Value::as_i64).unwrap_or(after);
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        Ok(())
    }

    fn resolve_session(&self, selector: &str) -> Result<SessionRecord> {
        self.lock_store()?
            .get_session(selector)?
            .with_context(|| format!("session not found: {selector}"))
    }

    fn resolve_turn(&self, id: &str) -> Result<TurnRecord> {
        self.lock_store()?
            .get_turn(id)?
            .with_context(|| format!("turn not found: {id}"))
    }

    fn runtime(&self, id: &str) -> Result<Arc<AgentRuntime>> {
        self.sessions
            .read()
            .map_err(|_| anyhow!("session map lock poisoned"))?
            .get(id)
            .cloned()
            .with_context(|| format!("session is not active: {id}"))
    }

    fn lock_store(&self) -> Result<MutexGuard<'_, Store>> {
        self.store
            .lock()
            .map_err(|_| anyhow!("SQLite store lock poisoned"))
    }
}

fn write_semantic_input(runtime: &AgentRuntime, input: &[u8]) -> Result<()> {
    let Some((&commit, paste)) = input.split_last() else {
        bail!("semantic input cannot be empty");
    };
    if commit != b'\r' {
        bail!("semantic input must end with carriage return");
    }
    runtime.write(paste)?;
    // Interactive CLIs may turn a large bracketed paste into an asynchronous
    // placeholder. Committing in the same write can be consumed before that
    // placeholder is installed, so deliver Enter separately after a short
    // provider-agnostic settle interval.
    std::thread::sleep(Duration::from_millis(100));
    runtime.write(&[commit])
}

struct HookOutcome {
    kind: &'static str,
    turn_id: Option<String>,
}

fn apply_hook_event(
    store: &mut Store,
    session: &SessionRecord,
    event_name: &str,
    payload: &Value,
) -> Result<HookOutcome> {
    match event_name {
        "SessionStart" => {
            store.set_session_state(&session.id, "idle")?;
            Ok(HookOutcome {
                kind: "session.ready",
                turn_id: session.active_turn_id.clone(),
            })
        }
        "UserPromptSubmit" => start_hook_turn(store, session, payload),
        "Stop" => complete_hook_turn(store, session, payload),
        "StopFailure" => fail_hook_turn(store, session, payload),
        "Notification"
            if payload
                .get("notification_type")
                .and_then(Value::as_str)
                .is_some_and(|kind| matches!(kind, "permission_prompt" | "elicitation_dialog")) =>
        {
            store.set_session_state(&session.id, "blocked")?;
            Ok(HookOutcome {
                kind: "session.blocked",
                turn_id: session.active_turn_id.clone(),
            })
        }
        "SessionEnd" => end_hook_session(store, session),
        _ => Ok(HookOutcome {
            kind: "provider.hook",
            turn_id: session.active_turn_id.clone(),
        }),
    }
}

fn fail_hook_turn(store: &Store, session: &SessionRecord, payload: &Value) -> Result<HookOutcome> {
    let current = store
        .get_session(&session.id)?
        .context("session disappeared while handling hook")?;
    let Some(turn_id) = current.active_turn_id else {
        store.set_session_state(&session.id, "idle")?;
        return Ok(HookOutcome {
            kind: "provider.failure_unmatched",
            turn_id: None,
        });
    };
    let provider_turn_id = payload.get("turn_id").and_then(Value::as_str);
    let final_message = payload
        .get("last_assistant_message")
        .and_then(Value::as_str);
    let error = sanitize_claude_error(payload).to_string();
    let failed = store.finish_turn_if_matching(
        &turn_id,
        provider_turn_id,
        "failed",
        final_message,
        Some(&error),
    )?;
    if failed {
        store.set_session_state(&session.id, "idle")?;
    }
    let quiesced = !failed && store.settle_canceled_turn(&turn_id, provider_turn_id)?;
    Ok(HookOutcome {
        kind: if failed {
            "turn.failed"
        } else if quiesced {
            "provider.quiesced"
        } else {
            "provider.failure_unmatched"
        },
        turn_id: Some(turn_id),
    })
}

fn start_hook_turn(
    store: &mut Store,
    session: &SessionRecord,
    payload: &Value,
) -> Result<HookOutcome> {
    let current = store
        .get_session(&session.id)?
        .context("session disappeared while handling hook")?;
    let turn_id = if let Some(turn_id) = current.active_turn_id {
        if !hook_prompt_matches_turn(store, &turn_id, payload)? {
            return Ok(HookOutcome {
                kind: "provider.prompt_unmatched",
                turn_id: Some(turn_id),
            });
        }
        turn_id
    } else {
        let turn_id = format!("turn_{}", Uuid::new_v4().simple());
        let prompt = payload
            .get("prompt")
            .or_else(|| payload.get("user_prompt"))
            .and_then(Value::as_str)
            .unwrap_or("");
        store.insert_turn(&turn_id, &session.id, prompt)?;
        turn_id
    };
    let provider_turn_id = payload.get("turn_id").and_then(Value::as_str);
    let started = store.mark_turn_started(&turn_id, provider_turn_id)?;
    if started {
        store.set_session_state(&session.id, "busy")?;
    }
    Ok(HookOutcome {
        kind: if started {
            "turn.started"
        } else {
            "provider.prompt_unmatched"
        },
        turn_id: Some(turn_id),
    })
}

fn hook_prompt_matches_turn(store: &Store, turn_id: &str, payload: &Value) -> Result<bool> {
    let provider_prompt = payload
        .get("prompt")
        .or_else(|| payload.get("user_prompt"))
        .and_then(Value::as_str);
    let Some(provider_prompt) = provider_prompt else {
        return Ok(true);
    };
    let turn = store.get_turn(turn_id)?.context("active turn not found")?;
    Ok(turn.prompt == provider_prompt)
}

fn bind_provider_session(store: &Store, session: &SessionRecord, payload: &Value) -> Result<()> {
    let Some(provider_session_id) = payload.get("session_id").and_then(Value::as_str) else {
        return Ok(());
    };
    if let Some(expected) = session.provider_session_id.as_deref()
        && expected != provider_session_id
    {
        bail!("hook provider session mismatch: expected {expected}, got {provider_session_id}");
    }
    if session.provider_session_id.is_none() {
        store.set_session_provider_id(&session.id, provider_session_id)?;
    }
    Ok(())
}

fn complete_hook_turn(
    store: &Store,
    session: &SessionRecord,
    payload: &Value,
) -> Result<HookOutcome> {
    let current = store
        .get_session(&session.id)?
        .context("session disappeared while handling hook")?;
    let Some(turn_id) = current.active_turn_id else {
        store.set_session_state(&session.id, "idle")?;
        return Ok(HookOutcome {
            kind: "provider.stop_unmatched",
            turn_id: None,
        });
    };
    let provider_turn_id = payload.get("turn_id").and_then(Value::as_str);
    let final_message = payload
        .get("last_assistant_message")
        .and_then(Value::as_str);
    let completed = store.complete_turn_if_matching(&turn_id, provider_turn_id, final_message)?;
    if completed {
        store.set_session_state(&session.id, "idle")?;
    }
    let quiesced = !completed && store.settle_canceled_turn(&turn_id, provider_turn_id)?;
    Ok(HookOutcome {
        kind: if completed {
            "turn.completed"
        } else if quiesced {
            "provider.quiesced"
        } else {
            "provider.stop_unmatched"
        },
        turn_id: Some(turn_id),
    })
}

fn end_hook_session(store: &Store, session: &SessionRecord) -> Result<HookOutcome> {
    let reason = "provider session ended before turn completion";
    let turn_id = store.interrupt_active_turn(&session.id, reason)?;
    if let Some(turn_id) = &turn_id {
        store.record_event(
            Some(&session.id),
            Some(turn_id),
            "turn.interrupted",
            &json!({"error": reason}),
        )?;
    }
    store.set_session_stopped(&session.id)?;
    Ok(HookOutcome {
        kind: "session.stopped",
        turn_id,
    })
}

#[allow(clippy::too_many_lines)]
fn apply_codex_notification(store: &mut Store, session_id: &str, message: &Value) -> Result<()> {
    let method = message
        .get("method")
        .and_then(Value::as_str)
        .context("Codex notification has no method")?;
    let params = message.get("params").unwrap_or(&Value::Null);
    let session = store
        .get_session(session_id)?
        .context("session disappeared while handling Codex notification")?;
    match method {
        "thread/started" => {
            let thread_id = params
                .pointer("/thread/id")
                .and_then(Value::as_str)
                .context("Codex thread/started had no thread id")?;
            if let Some(expected) = session.provider_session_id.as_deref()
                && expected != thread_id
            {
                return Ok(());
            }
            if session.provider_session_id.is_none() {
                store.set_session_provider_id(session_id, thread_id)?;
            }
            store.record_event(
                Some(session_id),
                None,
                "provider.thread_started",
                &json!({"provider_session_id": thread_id}),
            )?;
        }
        "turn/started" => {
            if !codex_thread_matches(&session, params)? {
                return Ok(());
            }
            let provider_turn_id = params
                .pointer("/turn/id")
                .and_then(Value::as_str)
                .context("Codex turn/started had no turn id")?;
            let current = store
                .get_session(session_id)?
                .context("session disappeared while starting Codex turn")?;
            let turn_id = if let Some(turn_id) = current.active_turn_id {
                turn_id
            } else {
                let turn_id = format!("turn_{}", Uuid::new_v4().simple());
                let prompt = codex_turn_prompt(params);
                store.insert_turn(&turn_id, session_id, &prompt)?;
                store.record_input(session_id, Some(&turn_id), "keyboard", prompt.as_bytes())?;
                store.record_event(
                    Some(session_id),
                    Some(&turn_id),
                    "turn.submitted",
                    &json!({"source": "keyboard"}),
                )?;
                turn_id
            };
            if store.mark_turn_started(&turn_id, Some(provider_turn_id))? {
                store.set_session_state(session_id, "busy")?;
                store.record_event(
                    Some(session_id),
                    Some(&turn_id),
                    "turn.started",
                    &json!({"provider_turn_id": provider_turn_id}),
                )?;
            }
        }
        "error" => {
            if !codex_thread_matches(&session, params)? {
                return Ok(());
            }
            let provider_turn_id = params.get("turnId").and_then(Value::as_str);
            let turn_id = local_turn_for_provider(store, session_id, provider_turn_id)?;
            let will_retry = params
                .get("willRetry")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            store.record_event(
                Some(session_id),
                turn_id.as_deref(),
                if will_retry {
                    "provider.error.retrying"
                } else {
                    "provider.error"
                },
                &json!({
                    "will_retry": will_retry,
                    "provider_turn_id": provider_turn_id,
                    "error": sanitize_codex_error(params.get("error")),
                }),
            )?;
        }
        "turn/completed" => {
            if !codex_thread_matches(&session, params)? {
                return Ok(());
            }
            let provider_turn_id = params
                .pointer("/turn/id")
                .and_then(Value::as_str)
                .context("Codex turn/completed had no turn id")?;
            let status = params
                .pointer("/turn/status")
                .and_then(Value::as_str)
                .context("Codex turn/completed had no status")?;
            if !matches!(status, "completed" | "failed" | "interrupted") {
                bail!("invalid Codex terminal turn status {status:?}");
            }
            let Some(turn_id) = local_turn_for_provider(store, session_id, Some(provider_turn_id))?
            else {
                store.record_event(
                    Some(session_id),
                    None,
                    "provider.completion_unmatched",
                    &json!({"provider_turn_id": provider_turn_id, "status": status}),
                )?;
                return Ok(());
            };
            let final_message = codex_final_message(params);
            let error_value = sanitize_codex_error(params.pointer("/turn/error"));
            let error = (status != "completed").then(|| error_value.to_string());
            let completed = store.finish_turn_if_matching(
                &turn_id,
                Some(provider_turn_id),
                status,
                final_message.as_deref(),
                error.as_deref(),
            )?;
            if completed {
                store.set_session_state(session_id, "idle")?;
            }
            let quiesced =
                !completed && store.settle_canceled_turn(&turn_id, Some(provider_turn_id))?;
            store.record_event(
                Some(session_id),
                Some(&turn_id),
                if completed {
                    match status {
                        "completed" => "turn.completed",
                        "failed" => "turn.failed",
                        "interrupted" => "turn.interrupted",
                        _ => unreachable!(),
                    }
                } else if quiesced {
                    "provider.quiesced"
                } else {
                    "provider.completion_unmatched"
                },
                &json!({
                    "provider_turn_id": provider_turn_id,
                    "status": status,
                    "error": (status != "completed").then_some(error_value),
                    "source": params.get("source").and_then(Value::as_str).unwrap_or("notification"),
                }),
            )?;
        }
        "dlgt/server/request" => {
            store.set_session_state(session_id, "blocked")?;
            store.record_event(
                Some(session_id),
                session.active_turn_id.as_deref(),
                "session.blocked",
                params,
            )?;
        }
        "dlgt/protocol/error" => {
            store.record_event(
                Some(session_id),
                session.active_turn_id.as_deref(),
                "provider.protocol_error",
                &json!({"message": sanitize_message(params.get("message").and_then(Value::as_str).unwrap_or("invalid message"))}),
            )?;
        }
        "dlgt/transport/closed" => {
            if session.state == "stopping" || matches!(session.state.as_str(), "stopped" | "failed")
            {
                return Ok(());
            }
            let reason = sanitize_message(
                params
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("Codex app-server connection closed"),
            );
            if let Some(turn_id) = store.interrupt_active_turn(session_id, &reason)? {
                store.record_event(
                    Some(session_id),
                    Some(&turn_id),
                    "turn.interrupted",
                    &json!({"error": reason}),
                )?;
            }
            store.set_session_failed(session_id)?;
            store.record_event(
                Some(session_id),
                None,
                "session.failed",
                &json!({"error": reason}),
            )?;
        }
        _ => {}
    }
    Ok(())
}

fn codex_thread_matches(session: &SessionRecord, params: &Value) -> Result<bool> {
    let thread_id = params
        .get("threadId")
        .and_then(Value::as_str)
        .context("Codex notification had no threadId")?;
    Ok(session.provider_session_id.as_deref() == Some(thread_id))
}

fn local_turn_for_provider(
    store: &Store,
    session_id: &str,
    provider_turn_id: Option<&str>,
) -> Result<Option<String>> {
    let Some(turn_id) = store
        .get_session(session_id)?
        .and_then(|value| value.active_turn_id)
    else {
        return Ok(None);
    };
    let turn = store.get_turn(&turn_id)?.context("active turn not found")?;
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
        .pointer("/turn/items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .rev()
        .find(|item| item.get("type").and_then(Value::as_str) == Some("agentMessage"))
        .and_then(|item| item.get("text").and_then(Value::as_str))
        .map(str::to_owned)
}

fn sanitize_codex_error(error: Option<&Value>) -> Value {
    let Some(error) = error.filter(|value| !value.is_null()) else {
        return Value::Null;
    };
    json!({
        "message": sanitize_message(error.get("message").and_then(Value::as_str).unwrap_or("Codex turn failed")),
        "code": error.get("codexErrorInfo").cloned().unwrap_or(Value::Null),
    })
}

fn sanitize_claude_error(payload: &Value) -> Value {
    json!({
        "code": payload.get("error").and_then(Value::as_str).unwrap_or("unknown"),
        "message": sanitize_message(
            payload
                .get("error_details")
                .and_then(Value::as_str)
                .unwrap_or("Claude turn failed"),
        ),
    })
}

fn sanitize_message(message: &str) -> String {
    message.chars().take(4_096).collect()
}

fn persist_session_exit(
    store: &Arc<Mutex<Store>>,
    sessions: &Arc<RwLock<HashMap<String, Arc<AgentRuntime>>>>,
    session_id: &str,
    exit_code: u32,
) {
    if let Ok(store) = store.lock() {
        let session = store.get_session(session_id).ok().flatten();
        let intentional = session
            .as_ref()
            .is_some_and(|session| matches!(session.state.as_str(), "stopping" | "stopped"));
        persist_exit_result(&store, session.as_ref(), exit_code, intentional);
        let terminal = if intentional {
            store.set_session_stopped(session_id)
        } else {
            store.set_session_failed(session_id)
        };
        if let Err(error) = terminal {
            eprintln!("dlgt failed to mark exited session terminal: {error:#}");
        }
        if let Err(error) = store.record_event(
            Some(session_id),
            None,
            if intentional {
                "session.stopped"
            } else {
                "session.failed"
            },
            &json!({"exit_code": exit_code}),
        ) {
            eprintln!("dlgt failed to persist session exit: {error:#}");
        }
    }
    if let Ok(mut sessions) = sessions.write() {
        sessions.remove(session_id);
    }
}

fn persist_exit_result(
    store: &Store,
    session: Option<&SessionRecord>,
    exit_code: u32,
    intentional: bool,
) {
    let reason = format!("agent process exited with code {exit_code}");
    let Some(session) = session else { return };
    let Some(turn_id) = session.active_turn_id.as_deref() else {
        return;
    };
    let state = if intentional { "interrupted" } else { "failed" };
    match store.finish_turn_if_matching(turn_id, None, state, None, Some(&reason)) {
        Ok(true) => {
            let _ = store.record_event(
                Some(&session.id),
                Some(turn_id),
                if intentional {
                    "turn.interrupted"
                } else {
                    "turn.failed"
                },
                &json!({"error": reason}),
            );
        }
        Ok(false) => {}
        Err(error) => eprintln!("dlgt failed to persist provider exit result: {error:#}"),
    }
}

fn params_string<'a>(params: &'a Value, name: &str) -> Result<&'a str> {
    params
        .get(name)
        .and_then(Value::as_str)
        .with_context(|| format!("missing string parameter {name:?}"))
}

fn validate_alias(alias: &str) -> Result<()> {
    if !alias.starts_with('@') || alias.len() < 2 {
        bail!("session alias must look like @name");
    }
    if alias.contains('#') {
        bail!("session alias must not contain reserved '#' characters");
    }
    Ok(())
}

fn params_u16(params: &Value, name: &str, default: u16) -> Result<u16> {
    let Some(value) = params.get(name) else {
        return Ok(default);
    };
    let value = value
        .as_u64()
        .with_context(|| format!("parameter {name:?} must be an unsigned integer"))?;
    u16::try_from(value).with_context(|| format!("parameter {name:?} is too large"))
}

fn write_response(stream: &mut impl Write, response: &Response) -> Result<()> {
    serde_json::to_writer(&mut *stream, response).context("failed to encode RPC response")?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}

fn classify_error(error: &anyhow::Error) -> &'static str {
    let message = error.to_string();
    if message.contains("wait timed out") {
        "WAIT_TIMEOUT"
    } else if message.contains("cancel timed out") {
        "CANCEL_TIMEOUT"
    } else if message.contains("blocked on input") {
        "SESSION_BLOCKED"
    } else if message.contains("already has an active turn") || message.contains("not ready") {
        "SESSION_BUSY"
    } else if message.contains("has no result") {
        "NO_RESULT"
    } else if message.contains("exclusive attach lease") {
        "SESSION_ATTACHED"
    } else if message.contains("already attached") {
        "ALREADY_ATTACHED"
    } else if message.contains("UNIQUE constraint failed: sessions.alias")
        || message.contains("active_session_alias")
    {
        "ALIAS_IN_USE"
    } else if message.contains("launch")
        || message.contains("failed to spawn")
        || message.contains("failed to start")
        || message.contains("did not become")
    {
        "LAUNCH_FAILED"
    } else if message.contains("not found") {
        "NOT_FOUND"
    } else if message.contains("not active")
        || message.contains("already stopped")
        || message.contains("is unavailable")
    {
        "SESSION_UNAVAILABLE"
    } else if message.contains("missing") || message.contains("must") || message.contains("invalid")
    {
        "INVALID_ARGUMENT"
    } else {
        "INTERNAL"
    }
}

fn public_session(session: &SessionRecord) -> Value {
    json!({
        "id": session.id,
        "alias": session.alias,
        "title": session.title,
        "harness": session.agent,
        "cwd": session.cwd,
        "state": match session.state.as_str() {
            "quiescing" => "canceling",
            "running" => "starting",
            other => other,
        },
        "model": session.model,
        "effort": session.effort,
        "created_at_ms": session.created_at_ms,
        "updated_at_ms": session.updated_at_ms,
    })
}

fn public_result(turn: &TurnRecord) -> Value {
    let status = turn.state.as_str();
    json!({
        "execution_seq": turn.execution_seq,
        "status": status,
        "final_text": turn.final_message.clone().unwrap_or_default(),
        "error": turn.error,
        "started_at_ms": turn.started_at_ms.unwrap_or(turn.created_at_ms),
        "completed_at_ms": turn.completed_at_ms,
        "usage": turn.usage,
    })
}

fn generate_session_id() -> String {
    const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let bytes = *Uuid::new_v4().as_bytes();
    let mut value = u64::from_be_bytes(bytes[..8].try_into().unwrap_or([0; 8]));
    let mut suffix = [b'0'; 8];
    for byte in suffix.iter_mut().rev() {
        *byte = ALPHABET[(value & 31) as usize];
        value >>= 5;
    }
    format!("ses_{}", String::from_utf8_lossy(&suffix))
}

fn generate_alias(title: &str) -> String {
    let slug = title
        .chars()
        .flat_map(char::to_lowercase)
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    let slug = slug
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let slug = if slug.is_empty() { "session" } else { &slug };
    let id = generate_session_id();
    format!(
        "@{}-{}",
        slug.chars().take(32).collect::<String>(),
        &id[6..12]
    )
}

fn normalize_event_type(kind: &str) -> Option<&'static str> {
    match kind {
        "session.created" => Some("session.created"),
        "session.restarting" => Some("session.restarting"),
        "session.ready" => Some("session.ready"),
        "turn.started" => Some("session.busy"),
        "session.blocked" => Some("session.blocked"),
        "session.resumed" => Some("session.resumed"),
        "turn.canceled" => Some("session.canceling"),
        "turn.completed" | "turn.failed" | "turn.interrupted" | "provider.quiesced" => {
            Some("session.idle")
        }
        "session.stopping" => Some("session.stopping"),
        "session.stopped" => Some("session.stopped"),
        "session.failed" => Some("session.failed"),
        "provider.retrying" | "provider.error.retrying" => Some("provider.retrying"),
        _ => None,
    }
}

fn is_terminal_turn_state(state: &str) -> bool {
    matches!(state, "completed" | "failed" | "canceled" | "interrupted")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        apply_codex_notification, apply_hook_event, generate_alias, generate_session_id,
        public_result, validate_alias,
    };
    use crate::store::{NewSession, Store};

    fn ready_store(agent: &str) -> (tempfile::TempDir, Store) {
        let directory =
            tempfile::tempdir().unwrap_or_else(|error| panic!("failed to create tempdir: {error}"));
        let store = Store::open(&directory.path().join("state.db"))
            .unwrap_or_else(|error| panic!("failed to open store: {error}"));
        store
            .insert_session(&NewSession {
                id: "ses_1",
                alias: "@worker",
                title: "worker",
                agent,
                cwd: "/tmp",
                model: None,
                effort: None,
            })
            .unwrap_or_else(|error| panic!("failed to insert session: {error}"));
        assert!(
            store
                .set_session_running("ses_1", Some(42))
                .unwrap_or_else(|error| panic!("failed to start session: {error}"))
        );
        assert!(
            store
                .set_session_state("ses_1", "idle")
                .unwrap_or_else(|error| panic!("failed to ready session: {error}"))
        );
        (directory, store)
    }

    #[test]
    fn archive_separator_is_reserved_in_aliases() {
        assert!(validate_alias("@worker").is_ok());
        assert!(validate_alias("@worker#old").is_err());
    }

    #[test]
    fn public_ids_and_generated_aliases_are_short_and_unambiguous() {
        let id = generate_session_id();
        assert_eq!(id.len(), 12);
        assert!(id.starts_with("ses_"));
        assert!(
            id[4..]
                .chars()
                .all(|character| { "0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(character) })
        );
        let alias = generate_alias("Run Review");
        assert!(alias.starts_with("@run-review-"));
        assert_eq!(alias.rsplit('-').next().map(str::len), Some(6));
    }

    #[test]
    fn public_result_exposes_sequence_but_not_internal_turn_identity() {
        let (_directory, mut store) = ready_store("codex");
        let turn = store
            .insert_turn("turn_private", "ses_1", "hello")
            .unwrap_or_else(|error| panic!("failed to insert turn: {error}"));
        assert_eq!(turn.execution_seq, 1);
        assert!(
            store
                .mark_turn_started("turn_private", Some("provider_private"))
                .unwrap_or_else(|error| panic!("failed to start turn: {error}"))
        );
        assert!(
            store
                .complete_turn_if_matching("turn_private", Some("provider_private"), Some("done"))
                .unwrap_or_else(|error| panic!("failed to complete turn: {error}"))
        );
        let value = public_result(
            &store
                .get_turn("turn_private")
                .unwrap_or_else(|error| panic!("failed to read turn: {error}"))
                .unwrap_or_else(|| panic!("turn missing")),
        );
        assert_eq!(value["execution_seq"], 1);
        assert_eq!(value["final_text"], "done");
        assert!(!value.to_string().contains("turn_private"));
        assert!(!value.to_string().contains("provider_private"));
    }

    #[test]
    fn claude_stop_failure_finishes_the_turn_as_failed() {
        let (_directory, mut store) = ready_store("claude");
        store
            .insert_turn("turn_1", "ses_1", "hello")
            .unwrap_or_else(|error| panic!("failed to insert turn: {error}"));
        assert!(
            store
                .mark_turn_started("turn_1", Some("provider-turn"))
                .unwrap_or_else(|error| panic!("failed to start turn: {error}"))
        );
        let session = store
            .get_session("ses_1")
            .unwrap_or_else(|error| panic!("failed to read session: {error}"))
            .unwrap_or_else(|| panic!("session missing"));
        let outcome = apply_hook_event(
            &mut store,
            &session,
            "StopFailure",
            &json!({
                "turn_id": "provider-turn",
                "error": "invalid_request",
                "error_details": "bad model",
            }),
        )
        .unwrap_or_else(|error| panic!("failed to apply StopFailure: {error}"));
        assert_eq!(outcome.kind, "turn.failed");
        let turn = store
            .get_turn("turn_1")
            .unwrap_or_else(|error| panic!("failed to read turn: {error}"))
            .unwrap_or_else(|| panic!("turn missing"));
        assert_eq!(turn.state, "failed");
        assert!(
            turn.error
                .is_some_and(|error| error.contains("invalid_request"))
        );
    }

    #[test]
    fn codex_retry_error_does_not_finish_before_authoritative_completion() {
        let (_directory, mut store) = ready_store("codex");
        store
            .set_session_provider_id("ses_1", "thread-1")
            .unwrap_or_else(|error| panic!("failed to bind thread: {error}"));
        store
            .insert_turn("turn_1", "ses_1", "hello")
            .unwrap_or_else(|error| panic!("failed to insert turn: {error}"));
        apply_codex_notification(
            &mut store,
            "ses_1",
            &json!({
                "method": "turn/started",
                "params": {"threadId": "thread-1", "turn": {"id": "provider-turn", "items": []}},
            }),
        )
        .unwrap_or_else(|error| panic!("failed to start Codex turn: {error}"));
        apply_codex_notification(
            &mut store,
            "ses_1",
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
                .unwrap_or_else(|error| panic!("failed to read turn: {error}"))
                .unwrap_or_else(|| panic!("turn missing"))
                .state,
            "running"
        );
        apply_codex_notification(
            &mut store,
            "ses_1",
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
            .unwrap_or_else(|error| panic!("failed to read turn: {error}"))
            .unwrap_or_else(|| panic!("turn missing"));
        assert_eq!(turn.state, "failed");
        let error = turn.error.unwrap_or_else(|| panic!("turn error missing"));
        assert!(error.contains("badRequest"));
        assert!(!error.contains("secret"));
        let events = store
            .read_events(Some("ses_1"), 0)
            .unwrap_or_else(|error| panic!("failed to read events: {error}"));
        assert!(
            events
                .iter()
                .any(|event| event.kind == "provider.error.retrying")
        );
        assert!(events.iter().any(|event| event.kind == "turn.failed"));
    }

    #[test]
    fn codex_terminal_event_quiesces_without_resurrecting_a_canceled_turn() {
        let (_directory, mut store) = ready_store("codex");
        store
            .set_session_provider_id("ses_1", "thread-1")
            .unwrap_or_else(|error| panic!("failed to bind thread: {error}"));
        store
            .insert_turn("turn_1", "ses_1", "hello")
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

        apply_codex_notification(
            &mut store,
            "ses_1",
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
            .unwrap_or_else(|error| panic!("failed to read turn: {error}"))
            .unwrap_or_else(|| panic!("turn missing"));
        assert_eq!(turn.state, "canceled");
        let session = store
            .get_session("ses_1")
            .unwrap_or_else(|error| panic!("failed to read session: {error}"))
            .unwrap_or_else(|| panic!("session missing"));
        assert_eq!(session.state, "idle");
        assert!(session.active_turn_id.is_none());
        assert!(
            store
                .read_events(Some("ses_1"), 0)
                .unwrap_or_else(|error| panic!("failed to read events: {error}"))
                .iter()
                .any(|event| event.kind == "provider.quiesced")
        );
    }
}
