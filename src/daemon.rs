use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use portable_pty::PtySize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::codex::{CodexConnection, final_agent_message_text};
use crate::cursor;
use crate::paths;
use crate::protocol::{Request, Response, SessionRecord, SessionState, TurnRecord, TurnState};
use crate::provider::{
    Agent, CommandSpec, LaunchOptions, codex_remote_tui_command, command_spec, prepare_workspace,
    provider_display_name,
};
use crate::reaper::Reaper;
use crate::session::SessionRuntime;
use crate::store::{NewSession, Store, now_ms};
use crate::update;

const CLAUDE_INPUT_READY_TIMEOUT: Duration = Duration::from_secs(10);
/// Composite read bounds. Response size is part of the protocol: an LLM caller
/// pays for every byte, so pagination is not optional implementation tuning.
const FETCH_DEFAULT_MAX_BYTES: usize = 32 * 1024;
const FETCH_HARD_MAX_BYTES: usize = 256 * 1024;
const FETCH_MIN_MAX_BYTES: usize = 2 * 1024;
const FETCH_EVENT_PAGE: usize = 64;
const FETCH_RESULT_PAGE: usize = 4;
const FETCH_STABLE_PAGE: usize = 128;
const FETCH_STABLE_MAX: usize = 512;
const FETCH_LIVE_ROWS: usize = 40;
const FETCH_SESSION_PAGE: usize = 32;
/// A long poll is bounded only to catch typos and leaked waiters.
const FETCH_MAX_WAIT_MS: u64 = 24 * 60 * 60 * 1000;
const FETCH_POLL_INTERVAL: Duration = Duration::from_millis(100);
const FETCH_ENVELOPE_RESERVE: usize = 1024;
/// Acceptance receipts retained per daemon, evicted first-in-first-out.
const REQUEST_RECEIPT_LIMIT: usize = 1024;
const FETCH_SESSION_CURSOR_RESERVE: usize = 128;
const CLAUDE_INPUT_SETTLE_INTERVAL: Duration = Duration::from_secs(2);
const EMPTY_DAEMON_IDLE_TIMEOUT: Duration = Duration::from_mins(5);

fn wait_for_update_check(shutdown: &Receiver<()>, interval: Duration) -> bool {
    match shutdown.recv_timeout(interval) {
        Ok(()) | Err(RecvTimeoutError::Disconnected) => false,
        Err(RecvTimeoutError::Timeout) => true,
    }
}

fn signal_shutdown(shutting_down: &AtomicBool, shutdown: &SyncSender<()>) {
    shutting_down.store(true, Ordering::SeqCst);
    let _ = shutdown.try_send(());
}

fn spawn_update_checker(
    daemon: &Arc<Daemon>,
    shutdown: Receiver<()>,
) -> Result<std::thread::JoinHandle<()>> {
    let update_notice = Arc::clone(&daemon.update_notice);
    let update_shutdown = Arc::clone(daemon);
    std::thread::Builder::new()
        .name("dlgt-update-check".to_owned())
        .spawn(move || {
            update::run_periodic_check_loop(
                update::UPDATE_CHECK_INTERVAL,
                || update_shutdown.shutting_down.load(Ordering::SeqCst),
                |interval| wait_for_update_check(&shutdown, interval),
                || update::refresh_notice(&update_notice),
            );
        })
        .context("failed to start update check")
}

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

    let store = Store::new();
    let reaper = Reaper::spawn()?;
    let (update_shutdown, update_wait) = mpsc::sync_channel(1);
    let daemon = Arc::new(Daemon {
        instance_id: Uuid::new_v4().simple().to_string(),
        receipts: ReceiptLedger::default(),
        store: Arc::new(Mutex::new(store)),
        sessions: Arc::new(RwLock::new(HashMap::new())),
        attach_leases: Mutex::new(HashMap::new()),
        pending_provider_ids: Arc::new(Mutex::new(HashMap::new())),
        shutting_down: AtomicBool::new(false),
        update_notice: Arc::new(RwLock::new(None)),
        update_shutdown,
        reaper,
    });
    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("failed to bind {}", socket_path.display()))?;
    fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to secure {}", socket_path.display()))?;
    listener
        .set_nonblocking(true)
        .context("failed to make server socket nonblocking")?;

    let update_thread = spawn_update_checker(&daemon, update_wait)?;

    let mut empty_since = Instant::now();
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
        let empty = daemon
            .sessions
            .read()
            .map_or(true, |sessions| sessions.is_empty());
        if empty {
            if empty_since.elapsed() >= EMPTY_DAEMON_IDLE_TIMEOUT {
                break;
            }
        } else {
            empty_since = Instant::now();
        }
    }

    daemon.request_shutdown();
    let _ = update_thread.join();
    if let Ok(sessions) = daemon.sessions.read() {
        for runtime in sessions.values() {
            // Daemon shutdown is the ownership boundary: no provider process
            // may outlive the runtime that created it. A graceful child-only
            // stop can leave descendants alive, so terminate the whole PTY
            // process group here.
            let _ = runtime.force_stop();
        }
    }
    drop(listener);
    if socket_path.exists() {
        fs::remove_file(&socket_path)
            .with_context(|| format!("failed to remove {}", socket_path.display()))?;
    }
    if let Some(directory) = socket_path.parent() {
        let _ = fs::remove_dir(directory);
    }
    Ok(())
}

struct Daemon {
    /// Daemon boot identity. Runtime state is memory-only, so no cursor may
    /// survive a restart.
    instance_id: String,
    /// Bounded request-id to acceptance-receipt ledger. A caller that never
    /// saw its acceptance response replays the original receipt instead of
    /// creating a duplicate Session or execution.
    receipts: ReceiptLedger,
    store: Arc<Mutex<Store>>,
    sessions: Arc<RwLock<HashMap<String, Arc<AgentRuntime>>>>,
    attach_leases: Mutex<HashMap<String, String>>,
    pending_provider_ids: Arc<Mutex<HashMap<String, Option<String>>>>,
    shutting_down: AtomicBool,
    update_notice: Arc<RwLock<Option<Value>>>,
    update_shutdown: SyncSender<()>,
    reaper: Arc<Reaper>,
}

impl Daemon {
    fn request_shutdown(&self) {
        signal_shutdown(&self.shutting_down, &self.update_shutdown);
    }
}

struct ProviderReservation {
    store: Arc<Mutex<Store>>,
    provider_ref: String,
    session_id: String,
}

struct PendingProviderId {
    pending: Arc<Mutex<HashMap<String, Option<String>>>>,
    launch_id: String,
}

impl PendingProviderId {
    fn register(
        pending: &Arc<Mutex<HashMap<String, Option<String>>>>,
        launch_id: &str,
    ) -> Result<Self> {
        let mut bindings = pending
            .lock()
            .map_err(|_| anyhow!("pending provider ID map lock poisoned"))?;
        if bindings.insert(launch_id.to_owned(), None).is_some() {
            bail!("pending provider ID already exists: {launch_id}");
        }
        Ok(Self {
            pending: Arc::clone(pending),
            launch_id: launch_id.to_owned(),
        })
    }

    fn take(&mut self) -> Result<String> {
        self.pending
            .lock()
            .map_err(|_| anyhow!("pending provider ID map lock poisoned"))?
            .remove(&self.launch_id)
            .flatten()
            .context("provider did not report a Session ID")
    }
}

impl Drop for PendingProviderId {
    fn drop(&mut self) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&self.launch_id);
        }
    }
}

impl Drop for ProviderReservation {
    fn drop(&mut self) {
        if let Ok(store) = self.store.lock() {
            store.release_provider_session(&self.provider_ref, &self.session_id);
        }
    }
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
    fn provider_id(&self) -> Option<&str> {
        match self {
            Self::Claude(_) => None,
            Self::Codex {
                provider_thread_id, ..
            } => Some(provider_thread_id),
        }
    }

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

    fn last_output_age(&self) -> Result<Option<Duration>> {
        match self {
            Self::Claude(runtime) => runtime.last_output_age(),
            Self::Codex { view, .. } => view.last_output_age(),
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
                control.join_thread(provider_thread_id)?;
                let turn_id = control.start_turn(provider_thread_id, prompt)?;
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
            Ok(result) => Response::ok(request.id, result).with_info(
                self.update_notice
                    .read()
                    .ok()
                    .and_then(|notice| notice.clone()),
            ),
            Err(error) => {
                if let Some(failure) = error.downcast_ref::<SessionLaunchFailure>() {
                    let (session_id, launch_id) = if failure.session_id.starts_with("internal:") {
                        (None, Some(failure.session_id.clone()))
                    } else {
                        (Some(failure.session_id.clone()), None)
                    };
                    Response::session_error(
                        request.id,
                        classify_error(&error),
                        format!("{error:#}"),
                        session_id,
                        launch_id,
                    )
                } else {
                    Response::error(request.id, classify_error(&error), format!("{error:#}"))
                }
            }
        };
        let mut response = response;
        if let Some(error) = response.error.as_mut() {
            if let Some(correlation_id) =
                request.params.get("correlation_id").and_then(Value::as_str)
            {
                error.correlation_id = Some(correlation_id.to_owned());
            }
            if error.code == "SESSION_NOT_RUNNING" {
                error.hint = Some("retry with --resume using the same session_id".to_owned());
            }
            if (error.code == "SESSION_NOT_RUNNING"
                || matches!(
                    request.method.as_str(),
                    "session.send" | "session.fetch" | "session.cancel"
                ))
                && let Some(selector) = request.params.get("session").and_then(Value::as_str)
            {
                if let Ok(Some(session)) =
                    self.lock_store().map(|store| store.get_session(selector))
                {
                    if session.id.starts_with("internal:") {
                        error.launch_id = Some(session.id.clone());
                    } else {
                        error.session_id = Some(session.id.clone());
                    }
                    error.session_state = Some(session.state.public_name().to_owned());
                    if error.code == "SESSION_BLOCKED" && error.session_id.is_some() {
                        error.action = Some(format!(
                            "dlgt attach {}",
                            error.session_id.as_deref().unwrap_or(selector)
                        ));
                    }
                } else if selector.split_once(':').is_some_and(|(agent, id)| {
                    matches!(agent, "codex" | "claude") && !id.is_empty()
                }) {
                    error.session_id = Some(selector.to_owned());
                }
            }
        }
        write_response(&mut stream, &response)
    }

    fn dispatch(&self, method: &str, params: &Value) -> Result<Value> {
        match method {
            "server.ping" => Ok(json!({
                "ok": true,
                "version": env!("CARGO_PKG_VERSION"),
                "socket": paths::socket_path()?,
            })),
            "server.stop" => {
                self.request_shutdown();
                Ok(json!({"stopping": true}))
            }
            "session.create" => self.accept_once(params, |params| self.create_session(params)),
            "session.restart" => self.restart_session(params),
            "session.list" => {
                let include_all = params.get("all").and_then(Value::as_bool).unwrap_or(false);
                let sessions = self.lock_store()?.list_sessions();
                let mut public = Vec::new();
                for session in sessions.into_iter().filter(|session| {
                    !session.id.starts_with("internal:")
                        && (include_all || !session.state.is_terminal())
                }) {
                    public.push(self.public_session(&session)?);
                }
                Ok(Value::Array(public))
            }
            "session.read" => self.read_session(params),
            "session.input" => self.input_session(params),
            "session.resize" => self.resize_session(params),
            "session.stop" => self.stop_session(params),
            "session.send" => self.accept_once(params, |params| {
                if params
                    .get("resume")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    self.resume_session(params)
                } else {
                    self.submit_turn(params)
                }
            }),
            "session.fetch" => self.fetch(params),
            "session.cancel" => self.cancel_session(params),
            "transcript.read_raw" => self.read_transcript(params),
            "event.read" => self.read_events(params),
            "scrollback.read" => self.read_scrollback(params),
            "model.list" => self.list_models(params),
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
        let initial_prompt = params_string(params, "prompt")?;
        if initial_prompt.is_empty() {
            bail!("initial prompt must not be empty");
        }
        let resume_provider_id = params.get("resume_provider_id").and_then(Value::as_str);
        let provider_ref = resume_provider_id.map(|id| format!("{}:{id}", agent.as_str()));
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
        if agent == Agent::Claude {
            crate::claude_models::validate_model_effort(model, effort)?;
        }
        let harness_options = match params.get("harness_options") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(options)) => options
                .iter()
                .map(|option| {
                    option
                        .as_str()
                        .map(str::to_owned)
                        .context("harness option must be a string")
                })
                .collect::<Result<Vec<_>>>()
                .context("invalid harness_options")?,
            Some(_) => bail!("harness_options must be an array"),
        };
        if agent == Agent::Codex && !harness_options.is_empty() {
            bail!("harness options are currently supported only for Claude");
        }
        let auto_approve = params
            .get("auto_approve")
            .and_then(Value::as_bool)
            .unwrap_or(true);
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
        let mut id = generate_internal_id();

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
                harness_options: &harness_options,
                auto_approve,
            });
            match inserted {
                Ok(()) => break,
                Err(error) if error.to_string().contains("session id already") && attempt < 15 => {
                    id = generate_internal_id();
                }
                Err(error) => return Err(error),
            }
        }
        let _provider_reservation = if let Some(provider_ref) = provider_ref.as_deref() {
            let reserved = self
                .lock_store()?
                .reserve_provider_session(provider_ref, &id);
            if !reserved {
                self.lock_store()?.set_session_failed(&id);
                bail!("provider conversation is already reserved: {provider_ref}");
            }
            let reservation = ProviderReservation {
                store: Arc::clone(&self.store),
                provider_ref: provider_ref.to_owned(),
                session_id: id.clone(),
            };
            Some(reservation)
        } else {
            None
        };
        self.lock_store()?
            .record_event(Some(&id), None, "session.created");
        self.lock_store()?.set_terminal_size(&id, rows, cols);
        let runtime_session_id = Arc::new(RwLock::new(id.clone()));
        let mut pending_provider_id = if agent == Agent::Claude {
            Some(PendingProviderId::register(
                &self.pending_provider_ids,
                &id,
            )?)
        } else {
            None
        };
        let options = LaunchOptions {
            agent,
            session_id: &id,
            title,
            cwd: &cwd,
            model,
            effort,
            harness_options: &harness_options,
            resume_provider_id,
            environment: &environment,
            auto_approve,
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
                .and_then(|spec| self.spawn_claude_runtime(&runtime_session_id, &spec, rows, cols)),
            Agent::Codex => {
                self.spawn_codex_runtime(&runtime_session_id, &options, rows, cols, startup_timeout)
            }
        };
        let runtime = match runtime {
            Ok(runtime) => runtime,
            Err(error) => {
                let store = self.lock_store()?;
                store.set_session_failed(&id);
                store.record_event(Some(&id), None, "session.failed");
                drop(store);
                return Err(Self::session_launch_failure(&id, &error));
            }
        };
        let pid = runtime.pid();
        self.sessions
            .write()
            .map_err(|_| anyhow!("session map lock poisoned"))?
            .insert(id.clone(), Arc::clone(&runtime));
        let store = self.lock_store()?;
        if !store.set_session_running(&id, pid) {
            drop(store);
            self.sessions
                .write()
                .map_err(|_| anyhow!("session map lock poisoned"))?
                .remove(&id);
            let session = self
                .lock_store()?
                .get_session(&id)
                .context("exited session not found")?;
            return Err(Self::session_launch_failure(
                &id,
                &anyhow!(
                    "launch failed before the Session became ready: state={}",
                    session.state
                ),
            ));
        }
        store.record_event(Some(&id), None, "session.started");
        if agent == Agent::Codex {
            store.set_session_state(&id, SessionState::Idle);
            store.record_event(Some(&id), None, "session.ready");
        }
        drop(store);
        if agent == Agent::Codex {
            let remaining = startup_deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let _ = runtime.force_stop();
                return Err(Self::session_launch_failure(
                    &id,
                    &anyhow!("launch timed out before the Session became ready"),
                ));
            }
            if let Err(error) = runtime.wait_for_input_ready(remaining) {
                let _ = runtime.force_stop();
                return Err(Self::session_launch_failure(
                    &id,
                    &error.context("Codex PTY did not become input-ready"),
                ));
            }
        }
        loop {
            let current = self.resolve_session(&id)?;
            if current.state == SessionState::Idle {
                break;
            }
            if current.state.is_terminal() {
                return Err(Self::session_launch_failure(
                    &id,
                    &anyhow!("launch failed before the Session became ready"),
                ));
            }
            if Instant::now() >= startup_deadline {
                let _ = runtime.force_stop();
                self.lock_store()?.set_session_failed(&id);
                return Err(Self::session_launch_failure(
                    &id,
                    &anyhow!("launch timed out before the Session became ready"),
                ));
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let provider_id = match agent {
            Agent::Claude => pending_provider_id
                .as_mut()
                .context("missing Claude provider ID binding")?
                .take()?,
            Agent::Codex => runtime
                .provider_id()
                .context("Codex runtime did not report a provider thread ID")?
                .to_owned(),
        };
        let canonical_id = canonical_session_id(agent.as_str(), &provider_id);
        if let Err(error) = self.promote_session(&id, &canonical_id, &runtime_session_id) {
            let _ = runtime.force_stop();
            self.lock_store()?.set_session_failed(&id);
            return Err(Self::session_launch_failure(
                &id,
                &error.context("failed to publish provider Session ID"),
            ));
        }
        id = canonical_id;
        let correlation_id = params
            .get("correlation_id")
            .and_then(Value::as_str)
            .unwrap_or("");
        let mut response = match self.submit_turn(&json!({
            "session": id,
            "prompt": initial_prompt,
            "correlation_id": correlation_id,
        })) {
            Ok(result) => result,
            Err(error) => {
                let _ = runtime.force_stop();
                self.lock_store()?.set_session_failed(&id);
                return Err(Self::session_launch_failure(
                    &id,
                    &error.context("initial prompt acceptance failed"),
                ));
            }
        };
        if !correlation_id.is_empty() {
            response["correlation_id"] = json!(correlation_id);
        }
        Ok(response)
    }

    fn session_launch_failure(session_id: &str, error: &anyhow::Error) -> anyhow::Error {
        SessionLaunchFailure {
            session_id: session_id.to_owned(),
            message: format!("{error:#}"),
        }
        .into()
    }

    /// Resume a provider conversation without mutating an existing live
    /// Session. The provider-qualified Session ID is durable; aliases first
    /// resolve to a live runtime when present.
    fn resume_session(&self, params: &Value) -> Result<Value> {
        let selector = params_string(params, "session")?;
        let prompt = params_string(params, "prompt")?;
        if prompt.is_empty() {
            bail!("prompt must not be empty");
        }
        let correlation_id = params
            .get("correlation_id")
            .and_then(Value::as_str)
            .unwrap_or("");

        if let Ok(existing) = self.resolve_session(selector) {
            let live = self
                .sessions
                .read()
                .map_err(|_| anyhow!("session map lock poisoned"))?
                .contains_key(&existing.id);
            if live {
                if existing.state == SessionState::Blocked {
                    bail!("session blocked on input");
                }
                if existing.state != SessionState::Idle || existing.active_turn_id.is_some() {
                    bail!("session already has an active turn");
                }
                let mut result = self.submit_turn(params)?;
                if !correlation_id.is_empty() {
                    result["correlation_id"] = json!(correlation_id);
                }
                return Ok(result);
            }
            let provider_id = provider_id_from_session(&existing)
                .context("SESSION_NOT_RUNNING: Session has no provider conversation")?;
            return self.launch_resumed_session(
                params,
                &existing.agent,
                provider_id,
                Some(&existing.alias),
                Some(&existing.title),
            );
        }

        let (agent, provider_id) = selector
            .split_once(':')
            .filter(|(agent, id)| matches!(*agent, "codex" | "claude") && !id.is_empty())
            .context(
                "SESSION_NOT_RUNNING: no live Session matches selector; use codex:<id> or claude:<id> with --resume",
            )?;
        self.launch_resumed_session(params, agent, provider_id, None, None)
    }

    fn launch_resumed_session(
        &self,
        params: &Value,
        agent: &str,
        provider_id: &str,
        alias: Option<&str>,
        title: Option<&str>,
    ) -> Result<Value> {
        let mut create = params.clone();
        let object = create
            .as_object_mut()
            .context("resume parameters must be an object")?;
        object.insert("harness".to_owned(), json!(agent));
        object.insert("resume_provider_id".to_owned(), json!(provider_id));
        object.insert(
            "title".to_owned(),
            json!(title.unwrap_or("resumed provider conversation")),
        );
        if let Some(alias) = alias {
            object.insert("alias".to_owned(), json!(alias));
        } else {
            object.remove("alias");
        }
        object.remove("resume");
        self.create_session(&create)
    }

    #[allow(clippy::too_many_lines)]
    fn restart_session(&self, params: &Value) -> Result<Value> {
        let session = self.resolve_session(params_string(params, "session")?)?;
        if matches!(
            session.state,
            SessionState::Starting | SessionState::Stopping | SessionState::Restarting
        ) {
            bail!("session is unavailable in state {}", session.state);
        }
        let previous_runtime = self
            .sessions
            .read()
            .map_err(|_| anyhow!("session map lock poisoned"))?
            .get(&session.id)
            .cloned();
        let provider_id = provider_id_from_session(&session)
            .context("session is unavailable because it has no provider conversation to resume")?
            .to_owned();
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
        if !self.lock_store()?.begin_session_restart(&session.id)? {
            bail!("session is unavailable in state {}", session.state);
        }
        {
            let store = self.lock_store()?;
            store.set_terminal_size(&session.id, rows, cols);
            store.record_event(Some(&session.id), None, "session.restarting");
        }
        if let Some(runtime) = previous_runtime {
            self.attach_leases
                .lock()
                .map_err(|_| anyhow!("attach lease lock poisoned"))?
                .remove(&session.id);
            let _ = runtime.force_stop();
            loop {
                let active = self
                    .sessions
                    .read()
                    .map_err(|_| anyhow!("session map lock poisoned"))?
                    .contains_key(&session.id);
                if !active {
                    break;
                }
                if Instant::now() >= startup_deadline {
                    bail!("restart launch timed out while stopping the previous process");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        } else {
            let store = self.lock_store()?;
            if let Some(turn_id) = store
                .interrupt_active_turn(&session.id, "session restarted before execution completed")
            {
                store.record_event(Some(&session.id), Some(&turn_id), "turn.interrupted");
            }
            store.finish_session_restart_stop(&session.id);
        }
        if !self.lock_store()?.start_restarted_session(&session.id) {
            bail!("session left restarting state before launch");
        }
        // A replacement PTY is a new terminal generation even when the
        // provider-qualified Session ID does not rotate.
        self.lock_store()?.restart_screen(&session.id);
        let remaining = startup_deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            self.lock_store()?.set_session_failed(&session.id);
            bail!("restart launch timed out before starting the replacement process");
        }
        let options = LaunchOptions {
            agent,
            session_id: &session.id,
            title: &session.title,
            cwd: &cwd,
            model: session.model.as_deref(),
            effort: session.effort.as_deref(),
            harness_options: &session.harness_options,
            resume_provider_id: Some(&provider_id),
            environment: &environment,
            auto_approve: session.auto_approve,
        };
        let runtime_session_id = Arc::new(RwLock::new(session.id.clone()));
        let mut pending_provider_id = if agent == Agent::Claude {
            Some(PendingProviderId::register(
                &self.pending_provider_ids,
                &session.id,
            )?)
        } else {
            None
        };
        let runtime = match agent {
            Agent::Claude => command_spec(&options)
                .and_then(|spec| self.spawn_claude_runtime(&runtime_session_id, &spec, rows, cols)),
            Agent::Codex => {
                self.spawn_codex_runtime(&runtime_session_id, &options, rows, cols, remaining)
            }
        };
        let runtime = match runtime {
            Ok(runtime) => runtime,
            Err(error) => {
                let store = self.lock_store()?;
                store.set_session_failed(&session.id);
                store.record_event(Some(&session.id), None, "session.failed");
                return Err(error).context("session restart launch failed");
            }
        };
        let pid = runtime.pid();
        self.sessions
            .write()
            .map_err(|_| anyhow!("session map lock poisoned"))?
            .insert(session.id.clone(), Arc::clone(&runtime));
        let store = self.lock_store()?;
        if !store.set_session_running(&session.id, pid) {
            drop(store);
            self.sessions
                .write()
                .map_err(|_| anyhow!("session map lock poisoned"))?
                .remove(&session.id);
            let _ = runtime.force_stop();
            bail!("restart launch exited before the Session became ready");
        }
        store.record_event(Some(&session.id), None, "session.started");
        if agent == Agent::Codex {
            store.set_session_state(&session.id, SessionState::Idle);
            store.record_event(Some(&session.id), None, "session.ready");
        }
        drop(store);
        if agent == Agent::Codex {
            let remaining = startup_deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let _ = runtime.force_stop();
                self.lock_store()?.set_session_failed(&session.id);
                bail!("restart launch timed out before the Session became ready");
            }
            if let Err(error) = runtime.wait_for_input_ready(remaining) {
                let _ = runtime.force_stop();
                self.lock_store()?.set_session_failed(&session.id);
                return Err(error).context("restarted Codex PTY did not become input-ready");
            }
        }
        loop {
            let current = self.resolve_session(&session.id)?;
            if current.state == SessionState::Idle {
                break;
            }
            if current.state.is_terminal() {
                bail!("restart launch failed before the Session became ready");
            }
            if Instant::now() >= startup_deadline {
                let _ = runtime.force_stop();
                self.lock_store()?.set_session_failed(&session.id);
                bail!("restart launch timed out before the Session became ready");
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let rebound_provider_id = match agent {
            Agent::Claude => pending_provider_id
                .as_mut()
                .context("missing Claude provider ID binding")?
                .take()?,
            Agent::Codex => runtime
                .provider_id()
                .context("Codex runtime did not report a provider thread ID")?
                .to_owned(),
        };
        let canonical_id = canonical_session_id(agent.as_str(), &rebound_provider_id);
        self.promote_session(&session.id, &canonical_id, &runtime_session_id)?;
        let current = self.resolve_session(&canonical_id)?;
        Ok(json!({"session": self.public_session(&current)?}))
    }

    fn spawn_claude_runtime(
        &self,
        runtime_session_id: &Arc<RwLock<String>>,
        spec: &CommandSpec,
        rows: u16,
        cols: u16,
    ) -> Result<Arc<AgentRuntime>> {
        let output_store = Arc::clone(&self.store);
        let output_session_id = Arc::clone(runtime_session_id);
        let on_output = Arc::new(move |data: &[u8]| {
            if let Ok(session_id) = output_session_id.read()
                && let Ok(store) = output_store.lock()
            {
                store.record_output(&session_id, data);
            }
        });
        let exit_store = Arc::clone(&self.store);
        let exit_sessions = Arc::clone(&self.sessions);
        let exit_session_id = Arc::clone(runtime_session_id);
        let on_exit = Arc::new(move |exit_code: u32| {
            if let Ok(session_id) = exit_session_id.read() {
                record_session_exit(&exit_store, &exit_sessions, &session_id, exit_code);
            }
        });
        let runtime = SessionRuntime::spawn(
            spec,
            PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            },
            on_output,
            on_exit,
        )?;
        runtime.track_with(
            self.reaper
                .watch(runtime.pid().context("Claude runtime had no pid")?)?,
        )?;
        Ok(Arc::new(AgentRuntime::Claude(runtime)))
    }

    fn spawn_codex_runtime(
        &self,
        runtime_session_id: &Arc<RwLock<String>>,
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
        let event_session_id = Arc::clone(runtime_session_id);
        let handler = Arc::new(move |message: Value| {
            let started_thread_id = (message.get("method").and_then(Value::as_str)
                == Some("thread/started"))
            .then(|| {
                message
                    .pointer("/params/thread/id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .flatten();
            if let Ok(session_id) = event_session_id.read()
                && let Ok(mut store) = event_store.lock()
            {
                match apply_codex_notification(&mut store, &session_id, &message) {
                    Ok(()) => {
                        if let Some(thread_id) = started_thread_id {
                            let _ = thread_sender.send(thread_id);
                        }
                    }
                    Err(error) => {
                        eprintln!("dlgt failed to apply Codex notification: {error:#}");
                    }
                }
            }
        });
        let control = CodexConnection::connect_with_environment(
            socket_path.clone(),
            handler,
            Some(options.environment),
            &self.reaper,
        )?;
        let spec = codex_remote_tui_command(options, &socket_path);
        let output_store = Arc::clone(&self.store);
        let output_session_id = Arc::clone(runtime_session_id);
        let on_output = Arc::new(move |data: &[u8]| {
            if let Ok(session_id) = output_session_id.read()
                && let Ok(store) = output_store.lock()
            {
                store.record_output(&session_id, data);
            }
        });
        let exit_store = Arc::clone(&self.store);
        let exit_sessions = Arc::clone(&self.sessions);
        let exit_session_id = Arc::clone(runtime_session_id);
        let on_exit = Arc::new(move |exit_code: u32| {
            if let Ok(session_id) = exit_session_id.read() {
                record_session_exit(&exit_store, &exit_sessions, &session_id, exit_code);
            }
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
        view.track_with(
            self.reaper
                .watch(view.pid().context("Codex TUI runtime had no pid")?)?,
        )?;
        let provider_thread_id = if let Some(expected) = options.resume_provider_id {
            expected.to_owned()
        } else {
            match thread_receiver.recv_timeout(startup_timeout) {
                Ok(thread_id) => thread_id,
                Err(error) => {
                    let _ = view.force_stop();
                    return Err(error).context("Codex remote TUI did not create a thread");
                }
            }
        };
        if let Err(error) =
            control.set_thread_name(&provider_thread_id, &provider_display_name(options.title))
        {
            let _ = view.force_stop();
            return Err(error).context("failed to set Codex provider thread name");
        }
        Ok(Arc::new(AgentRuntime::Codex {
            control,
            provider_thread_id,
            view,
        }))
    }

    fn public_session(&self, session: &SessionRecord) -> Result<Value> {
        let store = self.lock_store()?;
        self.public_session_locked(&store, session)
    }

    /// Build the public snapshot from an already-held store guard. The store
    /// mutex is not reentrant, so composite readers that take one cut under
    /// the lock must use this instead of `public_session`.
    fn public_session_locked(&self, store: &Store, session: &SessionRecord) -> Result<Value> {
        let busy_metrics = self.busy_metrics(store, session)?;
        Ok(public_session_with_metrics(session, busy_metrics))
    }

    fn busy_metrics(&self, store: &Store, session: &SessionRecord) -> Result<Option<BusyMetrics>> {
        if session.state != SessionState::Busy {
            return Ok(None);
        }

        // A turn is reserved before provider execution starts. Use that
        // creation timestamp rather than the provider's started_at timestamp
        // so the reported age cannot jump backwards while a provider binds.
        let busy_for_ms = session
            .active_turn_id
            .as_deref()
            .and_then(|turn_id| store.get_turn(turn_id))
            .map_or(0, |turn| {
                u64::try_from(now_ms().saturating_sub(turn.created_at_ms)).unwrap_or(0)
            });

        // PTY output is a presentation/fallback signal only. If this runtime
        // has not emitted output yet (or has already exited), consider the
        // entire current busy interval quiet. The pure helper below clamps a
        // prior output's age to busy_for_ms, preventing pre-turn idle time from
        // leaking into this interval.
        let runtime = self
            .sessions
            .read()
            .map_err(|_| anyhow!("session map lock poisoned"))?
            .get(&session.id)
            .cloned();
        let last_output_age_ms = runtime
            .map(|runtime| runtime.last_output_age())
            .transpose()?
            .flatten()
            .map(|age| u64::try_from(age.as_millis()).unwrap_or(u64::MAX));

        Ok(Some(clamp_busy_metrics(busy_for_ms, last_output_age_ms)))
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
            if session.state == SessionState::Blocked {
                store.set_session_state(&session.id, SessionState::Busy);
                store.record_event(
                    Some(&session.id),
                    session.active_turn_id.as_deref(),
                    "session.resumed",
                );
            }
            let seq = store.allocate_input_sequence();
            store.record_event(Some(&session.id), turn_id, "input.observed");
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
            .set_terminal_size(&session.id, rows, cols);
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
            .set_session_state(&session.id, SessionState::Stopping)
        {
            bail!("session is already stopped or failed");
        }
        self.lock_store()?
            .record_event(Some(&session.id), None, "session.stopping");
        if force {
            runtime.force_stop()?;
        } else {
            runtime.stop()?;
        }
        Ok(json!({"stopping": true, "force": force, "session_id": session.id}))
    }

    #[allow(clippy::too_many_lines)]
    fn submit_turn(&self, params: &Value) -> Result<Value> {
        let selector = params_string(params, "session")?;
        let session = self.resolve_session(selector).map_err(|_| {
            anyhow!(
                "SESSION_NOT_RUNNING: no live Session matches {selector}; retry with --resume using the same Session ID"
            )
        })?;
        if matches!(
            session.state,
            SessionState::Stopped
                | SessionState::Failed
                | SessionState::Starting
                | SessionState::Stopping
        ) || !self
            .sessions
            .read()
            .map_err(|_| anyhow!("session map lock poisoned"))?
            .contains_key(&session.id)
        {
            bail!(
                "SESSION_NOT_RUNNING: Session {} is not running; retry with --resume using the same Session ID",
                session.id
            );
        }
        if self
            .attach_leases
            .lock()
            .map_err(|_| anyhow!("attach lease lock poisoned"))?
            .contains_key(&session.id)
        {
            bail!("session has an exclusive attach lease");
        }
        if session.state == SessionState::Blocked {
            bail!("session blocked on input");
        }
        if matches!(session.state, SessionState::Busy | SessionState::Quiescing)
            || session.active_turn_id.is_some()
        {
            bail!("session already has an active turn");
        }
        if session.state != SessionState::Idle {
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
        let (turn, acceptance_cursor) = {
            let mut store = self.lock_store()?;
            // The observation cursor is captured under this lock immediately
            // before acceptance is recorded, so a fast provider cannot emit
            // output or finish in front of the position it names.
            let acceptance_cursor = self.acceptance_cursor(&store, &session.id)?;
            let turn = store.insert_turn(&turn_id, &session.id, prompt)?;
            store.allocate_input_sequence();
            store.record_event(Some(&session.id), Some(&turn_id), "turn.submitted");
            (turn, acceptance_cursor)
        };
        match agent {
            Agent::Codex => match runtime.start_codex_turn(prompt) {
                Ok(provider_turn_id) => {
                    let store = self.lock_store()?;
                    if store.mark_turn_started(&turn_id, Some(&provider_turn_id)) {
                        store.set_session_state(&session.id, SessionState::Busy);
                        store.record_event(Some(&session.id), Some(&turn_id), "turn.started");
                    }
                }
                Err(error) => {
                    let store = self.lock_store()?;
                    let message = sanitize_message(&error.to_string());
                    let _ = store.finish_turn_if_matching(
                        &turn_id,
                        None,
                        TurnState::Failed,
                        None,
                        Some(&message),
                    )?;
                    store.set_session_failed(&session.id);
                    store.record_event(Some(&session.id), Some(&turn_id), "turn.failed");
                    store.record_event(Some(&session.id), None, "session.failed");
                    drop(store);
                    let _ = runtime.force_stop();
                    return Err(error);
                }
            },
            Agent::Claude => {
                if let Err(error) = write_semantic_input(&runtime, &input) {
                    let store = self.lock_store()?;
                    let message = sanitize_message(&error.to_string());
                    let _ = store.finish_turn_if_matching(
                        &turn_id,
                        None,
                        TurnState::Failed,
                        None,
                        Some(&message),
                    )?;
                    store.set_session_state(&session.id, SessionState::Idle);
                    store.record_event(Some(&session.id), Some(&turn_id), "turn.failed");
                    return Err(error);
                }
                self.lock_store()?
                    .set_session_state(&session.id, SessionState::Busy);
            }
        }
        let current = self.resolve_session(&session.id)?;
        let mut result = json!({
            "session": self.public_session(&current)?,
            "execution_seq": turn.execution_seq,
            "cursor": acceptance_cursor,
        });
        if let Some(correlation_id) = params.get("correlation_id").and_then(Value::as_str)
            && !correlation_id.is_empty()
        {
            result["correlation_id"] = json!(correlation_id);
        }
        Ok(result)
    }

    fn read_session(&self, params: &Value) -> Result<Value> {
        let session = self.resolve_session(params_string(params, "session")?)?;
        if session.id.starts_with("internal:") {
            bail!("SESSION_UNAVAILABLE: Session has not published its provider ID");
        }
        let latest = self.lock_store()?.latest_turn(&session.id);
        Ok(json!({
            "session": self.public_session(&session)?,
            "result": latest.as_ref().filter(|turn| turn.state.is_terminal()).map(public_result),
            "execution_seq": latest.as_ref().map(|turn| turn.execution_seq),
        }))
    }

    fn cancel_session(&self, params: &Value) -> Result<Value> {
        let session = self.resolve_session(params_string(params, "session")?)?;
        let Some(turn_id) = session.active_turn_id else {
            return Ok(json!({
                "session_id": session.id,
                "canceled": false,
                "reason": "NO_ACTIVE_WORK",
            }));
        };
        let timeout_ms = params
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(30_000);
        self.cancel_turn(&json!({"turn": turn_id}))?;
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            let current = self.resolve_session(&session.id)?;
            if current.state == SessionState::Idle || current.active_turn_id.is_none() {
                let result = self.lock_store()?.latest_turn(&session.id);
                return Ok(json!({
                    "session": self.public_session(&current)?,
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
                store.allocate_input_sequence();
            }
            store.record_event(Some(&turn.session_id), Some(&turn.id), "turn.canceled");
        }
        if agent == Agent::Claude {
            runtime.write(cancel_input)?;
        }
        Ok(json!({"canceled": true, "turn_id": turn.id}))
    }

    /// Position an observation cursor immediately before an acceptance.
    fn acceptance_cursor(&self, store: &Store, session_id: &str) -> Result<String> {
        let uid = store
            .session_uid(session_id)
            .with_context(|| format!("session not found: {session_id}"))?;
        let mut cursor = cursor::Cursor::new(&self.instance_id, &uid);
        cursor.e = store.latest_event_seq();
        cursor.set_session(
            &uid,
            cursor::SessionCursor {
                r: store.stable_head(&uid),
                ep: store.screen_epoch(&uid),
                x: store.latest_result_seq(&uid),
                px: None,
                po: 0,
            },
        );
        cursor.encode()
    }

    /// Run an acceptance exactly once per caller-supplied request ID.
    fn accept_once(
        &self,
        params: &Value,
        run: impl FnOnce(&Value) -> Result<Value>,
    ) -> Result<Value> {
        let Some(request_id) = params.get("request_id").and_then(Value::as_str) else {
            return run(params);
        };
        self.receipts
            .accept_once(request_id, request_digest(params), || run(params))
    }

    /// One composite forward-delta read: current state, newly terminalized
    /// results, lifecycle events, and the forward screen delta, from an opaque
    /// cursor. Every observation is a success; only malformed requests,
    /// unknown Sessions, and unusable cursors are errors.
    fn fetch(&self, params: &Value) -> Result<Value> {
        let options = self.fetch_options(params)?;
        let deadline = Instant::now() + options.wait;
        let mut bound: Option<i64> = None;
        loop {
            let cut = if options.scope == cursor::SCOPE_ALL {
                self.fetch_all_cut(&options)?
            } else {
                self.fetch_session_cut(&options, &mut bound)?
            };
            let rendered = cut.render(&options)?;
            if rendered.settled
                || Instant::now() >= deadline
                || self.shutting_down.load(Ordering::SeqCst)
            {
                return Ok(rendered.value);
            }
            std::thread::sleep(FETCH_POLL_INTERVAL);
        }
    }

    fn fetch_options(&self, params: &Value) -> Result<FetchOptions> {
        let all = params.get("all").and_then(Value::as_bool).unwrap_or(false);
        let selector = params.get("session").and_then(Value::as_str);
        if all == selector.is_some() {
            bail!("fetch requires exactly one of a Session selector or --all");
        }
        let until_result = match params.get("until").and_then(Value::as_str).unwrap_or("any") {
            "any" => false,
            "result" => true,
            other => bail!("invalid until {other:?}; use any or result"),
        };
        let wait_ms = params.get("wait_ms").and_then(Value::as_u64).unwrap_or(0);
        if wait_ms > FETCH_MAX_WAIT_MS {
            bail!("invalid wait duration; the maximum long poll is 24h");
        }
        let max_bytes = usize::try_from(
            params
                .get("max_bytes")
                .and_then(Value::as_u64)
                .unwrap_or(FETCH_DEFAULT_MAX_BYTES as u64),
        )
        .unwrap_or(FETCH_DEFAULT_MAX_BYTES)
        .clamp(FETCH_MIN_MAX_BYTES, FETCH_HARD_MAX_BYTES);
        let screen = params.get("screen").unwrap_or(&Value::Null);
        let stable_limit = match screen {
            Value::Bool(false) => 0,
            // Absent means the documented default: the screen delta is on for
            // a single Session and unavailable for --all.
            Value::Null | Value::Bool(true) => FETCH_STABLE_PAGE,
            Value::Number(lines) => usize::try_from(
                lines
                    .as_u64()
                    .context("invalid screen line budget")?
                    .min(FETCH_STABLE_MAX as u64),
            )
            .unwrap_or(FETCH_STABLE_PAGE),
            _ => bail!("invalid screen selection; use a boolean or a line count"),
        };
        let scope = if all {
            if !matches!(screen, Value::Null | Value::Bool(false)) {
                bail!("invalid screen selection; fetch --all cannot aggregate screens");
            }
            if until_result {
                bail!("invalid until selection; fetch --all cannot bind to one execution");
            }
            cursor::SCOPE_ALL.to_owned()
        } else {
            // Resolve through the UID index so a pre-rekey public ID, which a
            // UID-bound cursor implies is still valid, keeps addressing the
            // same logical Session.
            let selector = selector.unwrap_or_default();
            self.lock_store()?
                .session_uid(selector)
                .with_context(|| format!("session not found: {selector}"))?
        };
        let cursor = params
            .get("cursor")
            .and_then(Value::as_str)
            .map(|text| cursor::Cursor::decode(text, &self.instance_id))
            .transpose()?;
        if let Some(cursor) = cursor.as_ref() {
            cursor.require_scope(&scope)?;
        }
        Ok(FetchOptions {
            scope,
            cursor,
            wait: Duration::from_millis(wait_ms),
            until_result,
            stable_limit: if all { 0 } else { stable_limit },
            max_bytes,
            instance_id: self.instance_id.clone(),
        })
    }

    #[allow(clippy::too_many_lines)]
    fn fetch_session_cut(&self, options: &FetchOptions, bound: &mut Option<i64>) -> Result<Cut> {
        let uid = options.scope.clone();
        let store = self.lock_store()?;
        let session = store
            .session_for_uid(&uid)
            .context("session not found: cursor no longer addresses a live Session")?;
        let baseline = options.baseline();
        let mut position = options.position(&uid);
        let mut scope_gaps = Vec::new();
        let mut gaps = Vec::new();

        let (events, events_more, event_watermark) = if baseline {
            (Vec::new(), false, store.latest_event_seq())
        } else {
            let after = options.event_position();
            if after < store.evicted_event_seq() {
                scope_gaps.push(retention_gap("events"));
            }
            normalized_page(&store, Some(&uid), after, FETCH_EVENT_PAGE)
        };

        let (results, results_more) = if baseline {
            let latest = store
                .latest_turn(&session.id)
                .filter(|turn| turn.state.is_terminal());
            // A baseline delivers only the latest retained result. Anchor the
            // watermark immediately below it so a chunked body resumes on the
            // same result instead of restarting from the oldest retained one.
            if let Some(turn) = latest.as_ref() {
                position.x = turn.execution_seq - 1;
                position.px = None;
                position.po = 0;
            }
            (latest.into_iter().collect::<Vec<_>>(), false)
        } else {
            if position.x < store.evicted_result_seq(&uid) {
                gaps.push(retention_gap("results"));
            }
            store.results_after(&uid, position.x, FETCH_RESULT_PAGE)
        };
        validate_continuation(&position, &results)?;

        let stable = if options.stable_limit == 0 {
            crate::screen::StablePage {
                next_after: store.stable_head(&uid),
                ..crate::screen::StablePage::default()
            }
        } else if baseline {
            store.stable_tail(&uid, options.stable_limit)
        } else {
            store.stable_page(&uid, position.r, options.stable_limit)
        };
        if stable.gap {
            gaps.push(retention_gap("screen"));
        }
        let live = if options.stable_limit == 0 {
            crate::store::LiveScreen {
                epoch: store.screen_epoch(&uid),
                ..crate::store::LiveScreen::default()
            }
        } else {
            store.live_screen(&uid, FETCH_LIVE_ROWS)
        };
        let epoch_reset = !baseline && position.ep != 0 && position.ep != live.epoch;

        if bound.is_none() {
            *bound = session
                .active_turn_id
                .as_deref()
                .and_then(|id| store.get_turn(id))
                .or_else(|| store.latest_turn(&session.id))
                .map(|turn| turn.execution_seq);
        }
        let bound_terminal = bound.is_some_and(|seq| {
            seq <= position.x
                || store
                    .turn_for_execution(&session.id, seq)
                    .is_some_and(|turn| turn.state.is_terminal())
        });
        let public = self.public_session_locked(&store, &session)?;
        drop(store);

        Ok(Cut {
            baseline,
            state: Some(session.state),
            bound_seq: *bound,
            bound_terminal,
            event_watermark,
            events_more,
            dropped_event_seq: None,
            scope_gaps,
            buckets: vec![Bucket::new(BucketInput {
                uid,
                session: public,
                gaps,
                events: events.into_iter().map(|(_, event)| event).collect(),
                results,
                results_more,
                stable,
                live,
                epoch_reset,
                screen: options.stable_limit > 0,
                incoming: position,
            })],
            sessions_more: false,
            baseline_next: None,
        })
    }

    #[allow(clippy::too_many_lines)]
    fn fetch_all_cut(&self, options: &FetchOptions) -> Result<Cut> {
        let store = self.lock_store()?;
        let baseline = options.baseline();
        let after = options.event_position();
        let mut scope_gaps = Vec::new();
        if !baseline && after < store.evicted_event_seq() {
            scope_gaps.push(retention_gap("events"));
        }
        let (events, events_more, event_watermark) = if baseline {
            // Freeze the watermark at the first baseline page so events
            // produced while the baseline is still paging are delivered as a
            // delta once enumeration completes.
            let watermark = if options.cursor.is_some() {
                after
            } else {
                store.latest_event_seq()
            };
            (Vec::new(), false, watermark)
        } else {
            normalized_page(&store, None, after, FETCH_EVENT_PAGE)
        };

        // Bucket the global event page. A Session that does not fit this page
        // records the sequence it refused so the shared watermark cannot move
        // past it.
        let mut order: Vec<String> = Vec::new();
        let mut by_uid: HashMap<String, Vec<Value>> = HashMap::new();
        let mut dropped_event_seq: Option<i64> = None;
        for (uid, event) in events {
            let Some(uid) = uid else { continue };
            if !by_uid.contains_key(&uid) && order.len() >= FETCH_SESSION_PAGE {
                let seq = event.get("seq").and_then(Value::as_i64).unwrap_or(0);
                dropped_event_seq =
                    Some(dropped_event_seq.map_or(seq, |current: i64| current.min(seq)));
                continue;
            }
            if !by_uid.contains_key(&uid) {
                order.push(uid.clone());
            }
            by_uid.entry(uid).or_default().push(event);
        }

        let mut candidates = store.session_uids();
        if baseline && let Some(resume) = options.resume_after() {
            let start = candidates
                .iter()
                .position(|uid| uid == resume)
                .map_or(0, |index| index + 1);
            candidates.drain(..start);
        }

        let mut buckets = Vec::new();
        let mut included_last = None;
        let mut remaining = false;
        for uid in candidates {
            let Some(session) = store.session_for_uid(&uid) else {
                continue;
            };
            if session.id.starts_with("internal:") {
                continue;
            }
            let mut position = options.position(&uid);
            let events = by_uid.remove(&uid).unwrap_or_default();
            let mut gaps = Vec::new();
            let (results, results_more) = if baseline {
                let latest = store
                    .latest_turn(&session.id)
                    .filter(|turn| turn.state.is_terminal());
                if let Some(turn) = latest.as_ref() {
                    position.x = turn.execution_seq - 1;
                    position.px = None;
                    position.po = 0;
                }
                (latest.into_iter().collect::<Vec<_>>(), false)
            } else {
                if position.x < store.evicted_result_seq(&uid) {
                    gaps.push(retention_gap("results"));
                }
                store.results_after(&uid, position.x, FETCH_RESULT_PAGE)
            };
            validate_continuation(&position, &results)?;
            if !baseline && events.is_empty() && results.is_empty() {
                continue;
            }
            if buckets.len() >= FETCH_SESSION_PAGE {
                remaining = true;
                break;
            }
            buckets.push(Bucket::new(BucketInput {
                uid: uid.clone(),
                session: self.public_session_locked(&store, &session)?,
                gaps,
                events,
                results,
                results_more,
                stable: crate::screen::StablePage {
                    next_after: store.stable_head(&uid),
                    ..crate::screen::StablePage::default()
                },
                live: crate::store::LiveScreen {
                    epoch: store.screen_epoch(&uid),
                    ..crate::store::LiveScreen::default()
                },
                epoch_reset: false,
                screen: false,
                incoming: position,
            }));
            included_last = Some(uid);
        }
        drop(store);

        Ok(Cut {
            baseline,
            state: None,
            bound_seq: None,
            bound_terminal: false,
            event_watermark,
            events_more,
            dropped_event_seq,
            scope_gaps,
            buckets,
            sessions_more: remaining && !baseline,
            baseline_next: if remaining && baseline {
                included_last
            } else {
                None
            },
        })
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
            .read_output_page(&session.id, after, limit_bytes.max(1));
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
        let store = self.lock_store()?;
        let uid = if let Some(selector) = params.get("session").and_then(Value::as_str) {
            Some(
                store
                    .session_uid(selector)
                    .with_context(|| format!("session not found: {selector}"))?,
            )
        } else {
            None
        };
        let normalized = store
            .read_events(uid.as_deref(), after)
            .iter()
            .filter_map(normalize_event)
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
        let before = params
            .get("before")
            .and_then(Value::as_str)
            .and_then(|cursor| cursor.strip_prefix("scr_"))
            .and_then(|value| value.parse::<u64>().ok());
        let store = self.lock_store()?;
        let uid = store
            .session_uid(&session.id)
            .context("session has no retained screen")?;
        let page = store.rendered_rows(&uid, before, lines);
        drop(store);
        Ok(json!({
            "session_id": session.id,
            "screen": {"rows": page.rows, "cols": page.cols},
            "lines": page.lines,
            "truncated": page.truncated,
            "before": page.before.map(|row| format!("scr_{row}")),
        }))
    }

    fn list_models(&self, params: &Value) -> Result<Value> {
        match params_string(params, "harness")? {
            "claude" => Ok(crate::claude_models::list_models()),
            "codex" => {
                let socket = paths::home_dir()?
                    .join("run")
                    .join(format!("models-{}", Uuid::new_v4().simple()))
                    .join("app-server.sock");
                let connection = CodexConnection::connect(socket, Arc::new(|_| {}), &self.reaper)?;
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
            {"id":"claude","model_discovery":"snapshot","effort":true}
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
        let selector = params_string(params, "session")?;
        let agent = params_string(params, "agent")?;
        let payload = params.get("payload").cloned().unwrap_or(Value::Null);
        let session = self.resolve_session(selector).or_else(|_| {
            let provider_id = payload
                .get("session_id")
                .and_then(Value::as_str)
                .context("hook payload has no provider Session ID")?;
            self.resolve_session(&canonical_session_id(agent, provider_id))
        })?;
        if agent != session.agent {
            bail!(
                "hook agent mismatch: session uses {}, hook reported {agent}",
                session.agent
            );
        }
        let event_name = payload
            .get("hook_event_name")
            .and_then(Value::as_str)
            .context("hook payload has no hook_event_name")?;
        if event_name == "SessionStart"
            && let Some(provider_id) = payload.get("session_id").and_then(Value::as_str)
        {
            let mut pending = self
                .pending_provider_ids
                .lock()
                .map_err(|_| anyhow!("pending provider ID map lock poisoned"))?;
            if let Some(binding) = pending.get_mut(selector) {
                *binding = Some(provider_id.to_owned());
            }
        }
        // The transcript fallback reads an untrusted provider file, so it runs
        // with no store lock held and is revalidated before it terminalizes
        // anything.
        let recovery = if event_name == "Stop" {
            self.recover_final_text(&session, &payload)?
        } else {
            None
        };
        let mut store = self.lock_store()?;
        let outcome = apply_hook_event(&mut store, &session, event_name, &payload, recovery)?;
        let seq = store.record_event(Some(&session.id), outcome.turn_id.as_deref(), outcome.kind);
        Ok(json!({
            "accepted": true,
            "seq": seq,
            "event": outcome.kind,
            "turn_id": outcome.turn_id,
        }))
    }

    /// Recover a missing Claude `final_text` from the Session transcript.
    /// Returns the guard values the caller must revalidate before use.
    fn recover_final_text(
        &self,
        session: &SessionRecord,
        payload: &Value,
    ) -> Result<Option<TranscriptRecovery>> {
        if payload
            .get("last_assistant_message")
            .and_then(Value::as_str)
            .is_some_and(|text| !text.is_empty())
        {
            return Ok(None);
        }
        let Some(provider_session_id) = provider_id_from_session(session).map(str::to_owned) else {
            return Ok(None);
        };
        let captured = {
            let store = self.lock_store()?;
            let Some(current) = store.get_session(&session.id) else {
                return Ok(None);
            };
            let Some(turn_id) = current.active_turn_id else {
                return Ok(None);
            };
            let Some(turn) = store.get_turn(&turn_id) else {
                return Ok(None);
            };
            let Some(uid) = store.session_uid(&session.id) else {
                return Ok(None);
            };
            let Some((path, boundary)) = transcript_window(&turn) else {
                return Ok(None);
            };
            TranscriptRecovery {
                // A new PTY is a new screen epoch, so it doubles as the
                // process-generation guard.
                generation: store.screen_epoch(&uid),
                provider_turn_id: turn.provider_turn_id.clone(),
                boundary,
                turn_id,
                uid,
                path,
                text: String::new(),
            }
        };
        let Some(text) =
            crate::transcript::recover(&captured.path, &provider_session_id, captured.boundary)
        else {
            return Ok(None);
        };
        Ok(Some(TranscriptRecovery { text, ..captured }))
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
            .get_session(selector)
            .with_context(|| format!("session not found: {selector}"))
    }

    fn promote_session(
        &self,
        from: &str,
        to: &str,
        runtime_session_id: &Arc<RwLock<String>>,
    ) -> Result<()> {
        let mut runtime_id = runtime_session_id
            .write()
            .map_err(|_| anyhow!("runtime session ID lock poisoned"))?;
        if from == to {
            to.clone_into(&mut runtime_id);
            return Ok(());
        }

        if self
            .sessions
            .read()
            .map_err(|_| anyhow!("session map lock poisoned"))?
            .contains_key(to)
        {
            bail!("active runtime already exists for Session {to}");
        }
        let store = self.lock_store()?;
        store.rekey_session(from, to)?;
        let mut sessions = self
            .sessions
            .write()
            .map_err(|_| anyhow!("session map lock poisoned"))?;
        if let Some(runtime) = sessions.remove(from)
            && sessions.insert(to.to_owned(), runtime).is_some()
        {
            bail!("active runtime already exists for Session {to}");
        }
        let mut leases = self
            .attach_leases
            .lock()
            .map_err(|_| anyhow!("attach lease lock poisoned"))?;
        if let Some(lease) = leases.remove(from) {
            leases.insert(to.to_owned(), lease);
        }
        to.clone_into(&mut runtime_id);
        Ok(())
    }

    fn resolve_turn(&self, id: &str) -> Result<TurnRecord> {
        self.lock_store()?
            .get_turn(id)
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
            .map_err(|_| anyhow!("session store lock poisoned"))
    }
}

fn write_semantic_input(runtime: &AgentRuntime, input: &[u8]) -> Result<()> {
    let Some((&commit, paste)) = input.split_last() else {
        bail!("semantic input cannot be empty");
    };
    if commit != b'\r' {
        bail!("semantic input must end with carriage return");
    }
    runtime
        .wait_for_input_ready(CLAUDE_INPUT_READY_TIMEOUT)
        .context("Claude PTY did not become input-ready")?;
    // Claude emits SessionStart before its interactive input handler is fully
    // installed. Keep a provider-specific settle interval after terminal mode
    // and output become quiet, then recheck in case startup rendered again.
    std::thread::sleep(CLAUDE_INPUT_SETTLE_INTERVAL);
    runtime
        .wait_for_input_ready(CLAUDE_INPUT_READY_TIMEOUT)
        .context("Claude PTY did not remain input-ready")?;
    runtime.write(paste)?;
    // Interactive CLIs may turn a large bracketed paste into an asynchronous
    // placeholder. Committing in the same write can be consumed before that
    // placeholder is installed, so deliver Enter separately after a short
    // provider-agnostic settle interval.
    std::thread::sleep(Duration::from_secs(1));
    runtime.write(&[commit])
}

struct HookOutcome {
    kind: &'static str,
    turn_id: Option<String>,
}

/// The transcript fallback is eligible only when the execution recorded both
/// a transcript path and the byte boundary where it began. Substituting a zero
/// boundary would let a previous turn's answer be returned as this one's.
fn transcript_window(turn: &TurnRecord) -> Option<(String, u64)> {
    Some((turn.transcript_path.clone()?, turn.transcript_offset?))
}

/// A transcript fallback captured outside the store lock, together with the
/// identity it was captured against.
struct TranscriptRecovery {
    turn_id: String,
    uid: String,
    generation: u64,
    provider_turn_id: Option<String>,
    boundary: u64,
    path: String,
    text: String,
}

fn apply_hook_event(
    store: &mut Store,
    session: &SessionRecord,
    event_name: &str,
    payload: &Value,
    recovery: Option<TranscriptRecovery>,
) -> Result<HookOutcome> {
    match event_name {
        "SessionStart" => {
            store.set_session_state(&session.id, SessionState::Idle);
            Ok(HookOutcome {
                kind: "session.ready",
                turn_id: session.active_turn_id.clone(),
            })
        }
        "UserPromptSubmit" => start_hook_turn(store, session, payload),
        "Stop" => complete_hook_turn(store, session, payload, recovery),
        "StopFailure" => fail_hook_turn(store, session, payload),
        "Notification"
            if payload
                .get("notification_type")
                .and_then(Value::as_str)
                .is_some_and(|kind| matches!(kind, "permission_prompt" | "elicitation_dialog")) =>
        {
            store.set_session_state(&session.id, SessionState::Blocked);
            Ok(HookOutcome {
                kind: "session.blocked",
                turn_id: session.active_turn_id.clone(),
            })
        }
        "SessionEnd" => Ok(end_hook_session(store, session)),
        _ => Ok(HookOutcome {
            kind: "provider.hook",
            turn_id: session.active_turn_id.clone(),
        }),
    }
}

fn fail_hook_turn(store: &Store, session: &SessionRecord, payload: &Value) -> Result<HookOutcome> {
    let current = store
        .get_session(&session.id)
        .context("session disappeared while handling hook")?;
    let Some(turn_id) = current.active_turn_id else {
        store.set_session_state(&session.id, SessionState::Idle);
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
        TurnState::Failed,
        final_message,
        Some(&error),
    )?;
    if failed {
        store.set_session_state(&session.id, SessionState::Idle);
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
        .get_session(&session.id)
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
    let started = store.mark_turn_started(&turn_id, provider_turn_id);
    if started {
        store.set_session_state(&session.id, SessionState::Busy);
        // Record where this execution starts in the provider transcript so a
        // later fallback cannot return the previous turn's answer.
        if let Some(path) = payload.get("transcript_path").and_then(Value::as_str) {
            store.set_turn_transcript(&turn_id, path, crate::transcript::boundary(path));
        }
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
    let turn = store.get_turn(turn_id).context("active turn not found")?;
    Ok(turn.prompt == provider_prompt)
}

fn complete_hook_turn(
    store: &Store,
    session: &SessionRecord,
    payload: &Value,
    recovery: Option<TranscriptRecovery>,
) -> Result<HookOutcome> {
    let current = store
        .get_session(&session.id)
        .context("session disappeared while handling hook")?;
    let Some(turn_id) = current.active_turn_id else {
        store.set_session_state(&session.id, SessionState::Idle);
        return Ok(HookOutcome {
            kind: "provider.stop_unmatched",
            turn_id: None,
        });
    };
    let provider_turn_id = payload.get("turn_id").and_then(Value::as_str);
    let hook_message = payload
        .get("last_assistant_message")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty());
    // The fallback text is only usable if the Session identity, process
    // generation, active execution, and provider turn are all unchanged since
    // it was read outside the lock. Failing that check never fails the
    // execution; it only leaves the text missing.
    let recovered = recovery.filter(|recovery| {
        hook_message.is_none()
            && recovery.turn_id == turn_id
            && store.session_uid(&session.id).as_deref() == Some(recovery.uid.as_str())
            && store.screen_epoch(&recovery.uid) == recovery.generation
            && store
                .get_turn(&turn_id)
                .and_then(|turn| turn.provider_turn_id)
                .as_deref()
                == recovery.provider_turn_id.as_deref()
    });
    let final_message = hook_message.or(recovered.as_ref().map(|recovery| recovery.text.as_str()));
    let completed = store.complete_turn_if_matching(
        &turn_id,
        provider_turn_id,
        final_message.or_else(|| {
            payload
                .get("last_assistant_message")
                .and_then(Value::as_str)
        }),
        recovered.is_some(),
    )?;
    if completed {
        store.set_session_state(&session.id, SessionState::Idle);
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

fn end_hook_session(store: &Store, session: &SessionRecord) -> HookOutcome {
    let reason = "provider session ended before turn completion";
    let turn_id = store.interrupt_active_turn(&session.id, reason);
    if let Some(turn_id) = &turn_id {
        store.record_event(Some(&session.id), Some(turn_id), "turn.interrupted");
    }
    let restarting = store
        .get_session(&session.id)
        .is_some_and(|current| current.state == SessionState::Restarting);
    if restarting {
        store.finish_session_restart_stop(&session.id);
    } else {
        store.set_session_stopped(&session.id);
    }
    HookOutcome {
        kind: if restarting {
            "provider.session_ended_for_restart"
        } else {
            "session.stopped"
        },
        turn_id,
    }
}

#[allow(clippy::too_many_lines)]
fn apply_codex_notification(store: &mut Store, session_id: &str, message: &Value) -> Result<()> {
    let method = message
        .get("method")
        .and_then(Value::as_str)
        .context("Codex notification has no method")?;
    let params = message.get("params").unwrap_or(&Value::Null);
    let session = store
        .get_session(session_id)
        .context("session disappeared while handling Codex notification")?;
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
                return Ok(());
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
                store.allocate_input_sequence();
                store.record_event(Some(session_id), Some(&turn_id), "turn.submitted");
                turn_id
            };
            if store.mark_turn_started(&turn_id, Some(provider_turn_id)) {
                store.set_session_state(session_id, SessionState::Busy);
                store.record_event(Some(session_id), Some(&turn_id), "turn.started");
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
            if will_retry {
                let attempt = params.get("attempt").and_then(Value::as_u64).unwrap_or(1);
                store.record_provider_retry_event(Some(session_id), turn_id.as_deref(), attempt);
            } else {
                store.record_event(Some(session_id), turn_id.as_deref(), "provider.error");
            }
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
            let turn_state = match status {
                "completed" => TurnState::Completed,
                "failed" => TurnState::Failed,
                "interrupted" => TurnState::Interrupted,
                _ => bail!("invalid Codex terminal turn status {status:?}"),
            };
            let Some(turn_id) = local_turn_for_provider(store, session_id, Some(provider_turn_id))?
            else {
                store.record_event(Some(session_id), None, "provider.completion_unmatched");
                return Ok(());
            };
            let final_message = codex_final_message(params);
            let error_value = sanitize_codex_error(params.pointer("/turn/error"));
            let error = (turn_state != TurnState::Completed).then(|| error_value.to_string());
            let completed = store.finish_turn_if_matching(
                &turn_id,
                Some(provider_turn_id),
                turn_state,
                final_message.as_deref(),
                error.as_deref(),
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
        "dlgt/server/request" => {
            store.set_session_state(session_id, SessionState::Blocked);
            store.record_event(
                Some(session_id),
                session.active_turn_id.as_deref(),
                "session.blocked",
            );
        }
        "dlgt/protocol/error" => {
            store.record_event(
                Some(session_id),
                session.active_turn_id.as_deref(),
                "provider.protocol_error",
            );
        }
        "dlgt/transport/closed" => {
            if session.state == SessionState::Stopping || session.state.is_terminal() {
                return Ok(());
            }
            let reason = sanitize_message(
                params
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("Codex app-server connection closed"),
            );
            if let Some(turn_id) = store.interrupt_active_turn(session_id, &reason) {
                store.record_event(Some(session_id), Some(&turn_id), "turn.interrupted");
            }
            store.set_session_failed(session_id);
            store.record_event(Some(session_id), None, "session.failed");
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
    Ok(provider_id_from_session(session).is_some_and(|provider_id| provider_id == thread_id))
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

fn record_session_exit(
    store: &Arc<Mutex<Store>>,
    sessions: &Arc<RwLock<HashMap<String, Arc<AgentRuntime>>>>,
    session_id: &str,
    exit_code: u32,
) {
    if let Ok(store) = store.lock() {
        let session = store.get_session(session_id);
        let restarting = session
            .as_ref()
            .is_some_and(|session| session.state == SessionState::Restarting);
        let intentional = session.as_ref().is_some_and(|session| {
            matches!(
                session.state,
                SessionState::Stopping | SessionState::Stopped | SessionState::Restarting
            )
        });
        record_exit_result(&store, session.as_ref(), exit_code, intentional);
        if restarting {
            store.finish_session_restart_stop(session_id);
        } else if intentional {
            store.set_session_stopped(session_id);
        } else {
            store.set_session_failed(session_id);
        }
        store.record_event(
            Some(session_id),
            None,
            if restarting {
                "provider.exited_for_restart"
            } else if intentional {
                "session.stopped"
            } else {
                "session.failed"
            },
        );
    }
    if let Ok(mut sessions) = sessions.write() {
        sessions.remove(session_id);
    }
}

fn record_exit_result(
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
    let state = if intentional {
        TurnState::Interrupted
    } else {
        TurnState::Failed
    };
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
            );
        }
        Ok(false) => {}
        Err(error) => eprintln!("dlgt failed to record provider exit result: {error:#}"),
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
    if message.contains("SESSION_NOT_RUNNING") {
        "SESSION_NOT_RUNNING"
    } else if message.contains("CURSOR_VERSION_UNSUPPORTED") {
        "CURSOR_VERSION_UNSUPPORTED"
    } else if message.contains("CURSOR_SCOPE_MISMATCH") {
        "CURSOR_SCOPE_MISMATCH"
    } else if message.contains("CURSOR_EXPIRED") {
        "CURSOR_EXPIRED"
    } else if message.contains("CURSOR_INVALID") {
        "CURSOR_INVALID"
    } else if message.contains("cancel timed out") {
        "CANCEL_TIMEOUT"
    } else if message.contains("blocked on input") {
        "SESSION_BLOCKED"
    } else if message.contains("already has an active turn")
        || message.contains("not ready")
        || message.contains("provider conversation is already reserved")
        || message.contains("provider conversation is already running")
    {
        "SESSION_BUSY"
    } else if message.contains("has no result") {
        "NO_RESULT"
    } else if message.contains("exclusive attach lease") {
        "SESSION_ATTACHED"
    } else if message.contains("already attached") {
        "ALREADY_ATTACHED"
    } else if message.contains("active session alias already exists") {
        "ALIAS_IN_USE"
    } else if message.contains("launch")
        || message.contains("failed to spawn")
        || message.contains("failed to start")
        || message.contains("failed to establish")
        || message.contains("did not become")
        || message.contains("did not create a thread")
        || message.contains("did not report a Session ID")
        || message.contains("app-server child was unavailable")
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BusyMetrics {
    busy_for_ms: u64,
    pty_quiet_for_ms: u64,
}

fn clamp_busy_metrics(busy_for_ms: u64, last_output_age_ms: Option<u64>) -> BusyMetrics {
    BusyMetrics {
        busy_for_ms,
        // No output observed means no evidence of activity during this busy
        // interval, so report the whole interval as quiet. If output predates
        // the turn, min() removes that prior-idle time from the public value.
        pty_quiet_for_ms: last_output_age_ms.map_or(busy_for_ms, |age| age.min(busy_for_ms)),
    }
}

#[cfg(test)]
fn public_session(session: &SessionRecord) -> Value {
    public_session_with_metrics(session, None)
}

fn public_session_with_metrics(
    session: &SessionRecord,
    busy_metrics: Option<BusyMetrics>,
) -> Value {
    let mut value = json!({
        "id": session.id,
        "alias": session.alias,
        "title": session.title,
        "harness": session.agent,
        "cwd": session.cwd,
        "state": session.state.public_name(),
        "model": session.model,
        "effort": session.effort,
        "auto_approve": session.auto_approve,
        "created_at_ms": session.created_at_ms,
        "updated_at_ms": session.updated_at_ms,
    });
    if let Some(metrics) = busy_metrics
        && session.state == SessionState::Busy
        && let Some(object) = value.as_object_mut()
    {
        object.insert("busy_for_ms".to_owned(), json!(metrics.busy_for_ms));
        object.insert(
            "pty_quiet_for_ms".to_owned(),
            json!(metrics.pty_quiet_for_ms),
        );
    }
    value
}

#[derive(Debug)]
struct SessionLaunchFailure {
    session_id: String,
    message: String,
}

impl std::fmt::Display for SessionLaunchFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for SessionLaunchFailure {}

fn public_result(turn: &TurnRecord) -> Value {
    let status = turn.state.as_str();
    json!({
        "execution_seq": turn.execution_seq,
        "status": status,
        "final_text": turn.final_message.clone().unwrap_or_default(),
        "final_text_source": final_text_source(turn),
        "error": turn.error,
        "started_at_ms": turn.started_at_ms.unwrap_or(turn.created_at_ms),
        "completed_at_ms": turn.completed_at_ms,
        "usage": turn.usage,
    })
}

/// Where a retained `final_text` came from. `missing` is an explicit
/// diagnostic: the execution still completed, but no answer was recovered.
fn final_text_source(turn: &TurnRecord) -> &'static str {
    if turn.final_text_recovered {
        "transcript"
    } else if turn
        .final_message
        .as_deref()
        .is_some_and(|text| !text.is_empty())
    {
        "hook"
    } else {
        "missing"
    }
}

fn generate_internal_id() -> String {
    const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let bytes = *Uuid::new_v4().as_bytes();
    let mut value = u64::from_be_bytes(bytes[..8].try_into().unwrap_or([0; 8]));
    let mut suffix = [b'0'; 8];
    for byte in suffix.iter_mut().rev() {
        *byte = ALPHABET[(value & 31) as usize];
        value >>= 5;
    }
    format!("internal:{}", String::from_utf8_lossy(&suffix))
}

fn canonical_session_id(agent: &str, provider_id: &str) -> String {
    format!("{agent}:{provider_id}")
}

fn provider_id_from_session(session: &SessionRecord) -> Option<&str> {
    session
        .id
        .split_once(':')
        .filter(|(agent, id)| *agent == session.agent && !id.is_empty())
        .map(|(_, id)| id)
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
    let id = generate_internal_id();
    let suffix = id.strip_prefix("internal:").unwrap_or(&id);
    format!(
        "@{}-{}",
        slug.chars().take(32).collect::<String>(),
        &suffix[..6]
    )
}

struct Receipt {
    id: String,
    digest: u128,
    value: Value,
}

#[derive(Default)]
struct ReceiptState {
    completed: VecDeque<Receipt>,
    /// Acceptances currently running, so a concurrent duplicate waits for the
    /// winner instead of launching a second Session.
    inflight: HashMap<String, u128>,
}

#[derive(Default)]
struct ReceiptLedger {
    state: Mutex<ReceiptState>,
    settled: std::sync::Condvar,
}

impl ReceiptLedger {
    /// Run `run` at most once for `(request_id, digest)`.
    ///
    /// A retry with the same payload replays the original receipt. A
    /// concurrent duplicate blocks until the winner settles and then replays
    /// its receipt, so two racing calls can never create two Sessions. The
    /// same ID with a different payload is rejected against both a stored and
    /// an in-flight acceptance.
    fn accept_once(
        &self,
        request_id: &str,
        digest: u128,
        run: impl FnOnce() -> Result<Value>,
    ) -> Result<Value> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("request receipt lock poisoned"))?;
        loop {
            if let Some(receipt) = state
                .completed
                .iter()
                .find(|receipt| receipt.id == request_id)
            {
                if receipt.digest != digest {
                    bail!(
                        "invalid request_id reuse: {request_id:?} already accepted a different payload"
                    );
                }
                let mut replay = receipt.value.clone();
                replay["replayed"] = json!(true);
                return Ok(replay);
            }
            match state.inflight.get(request_id) {
                Some(&reserved) if reserved != digest => {
                    bail!(
                        "invalid request_id reuse: {request_id:?} is already accepting a different payload"
                    );
                }
                Some(_) => {
                    state = self
                        .settled
                        .wait(state)
                        .map_err(|_| anyhow!("request receipt lock poisoned"))?;
                }
                None => {
                    state.inflight.insert(request_id.to_owned(), digest);
                    break;
                }
            }
        }
        drop(state);

        let result = run();
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("request receipt lock poisoned"))?;
        state.inflight.remove(request_id);
        if let Ok(value) = result.as_ref() {
            state.completed.push_back(Receipt {
                id: request_id.to_owned(),
                digest,
                value: value.clone(),
            });
            while state.completed.len() > REQUEST_RECEIPT_LIMIT {
                state.completed.pop_front();
            }
        }
        drop(state);
        // A failed acceptance leaves no receipt, so a waiter retries it.
        self.settled.notify_all();
        result
    }
}

/// Identity of an acceptance payload: the prompt and every launch option that
/// changes what the Session does, excluding per-invocation noise such as the
/// environment snapshot, terminal size, and correlation ID.
fn request_digest(params: &Value) -> u128 {
    const VOLATILE: [&str; 5] = [
        "environment",
        "rows",
        "cols",
        "correlation_id",
        "request_id",
    ];
    let mut canonical = params.clone();
    if let Some(object) = canonical.as_object_mut() {
        for key in VOLATILE {
            object.remove(key);
        }
    }
    // serde_json orders object keys, so the canonical form is stable.
    fnv1a128(
        serde_json::to_string(&canonical)
            .unwrap_or_default()
            .as_bytes(),
    )
}

fn fnv1a128(bytes: &[u8]) -> u128 {
    const OFFSET: u128 = 144_066_263_297_769_815_596_495_629_667_062_367_629;
    const PRIME: u128 = 309_485_009_821_345_068_724_781_371;
    let mut hash = OFFSET;
    for byte in bytes {
        hash ^= u128::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

struct FetchOptions {
    /// One Session UID, or `all`.
    scope: String,
    cursor: Option<cursor::Cursor>,
    wait: Duration,
    until_result: bool,
    /// Zero disables the screen projection entirely.
    stable_limit: usize,
    max_bytes: usize,
    instance_id: String,
}

impl FetchOptions {
    /// A cursorless request, or an `--all` baseline that is still enumerating
    /// Sessions. Baseline mode persists until enumeration completes so a
    /// caller cannot lose the Sessions it has not been shown yet.
    fn baseline(&self) -> bool {
        self.cursor
            .as_ref()
            .is_none_or(|cursor| cursor.bl.is_some())
    }

    fn resume_after(&self) -> Option<&str> {
        self.cursor.as_ref().and_then(|cursor| cursor.bl.as_deref())
    }

    fn position(&self, uid: &str) -> cursor::SessionCursor {
        self.cursor
            .as_ref()
            .map(|cursor| cursor.session(uid))
            .unwrap_or_default()
    }

    fn event_position(&self) -> i64 {
        self.cursor.as_ref().map_or(0, |cursor| cursor.e)
    }
}

/// An immutable observation cut taken under the store lock. Serialization
/// happens after the lock is released, so output arriving during serialization
/// belongs to the next cursor.
#[allow(clippy::struct_excessive_bools)]
struct Cut {
    baseline: bool,
    state: Option<SessionState>,
    /// Execution bound by `--until result`, and whether it has terminalized.
    bound_seq: Option<i64>,
    bound_terminal: bool,
    /// Event watermark to publish when every candidate event is delivered.
    event_watermark: i64,
    events_more: bool,
    /// Lowest event sequence this cut already refused to bucket.
    dropped_event_seq: Option<i64>,
    /// Scope-wide gaps. Per-Session gaps live on the bucket.
    scope_gaps: Vec<Value>,
    buckets: Vec<Bucket>,
    sessions_more: bool,
    /// `--all` baseline enumeration position for the next page.
    baseline_next: Option<String>,
}

/// One Session's renderable payload. Every variable-length field keeps its
/// full candidate list plus an inclusion count, so the shrink loop can shed
/// content in reverse priority order and still know what it left behind.
#[allow(clippy::struct_excessive_bools)]
struct Bucket {
    uid: String,
    session: Value,
    gaps: Vec<Value>,
    events: Vec<Value>,
    events_len: usize,
    results: Vec<ResultChunk>,
    results_len: usize,
    results_more: bool,
    stable: Vec<String>,
    stable_len: usize,
    /// Byte length the last included stable row is truncated to.
    stable_truncate: Option<usize>,
    /// Row ID immediately before `stable[0]`.
    stable_start: u64,
    stable_more: bool,
    live: Vec<String>,
    live_included: bool,
    live_truncated: bool,
    epoch: u64,
    epoch_reset: bool,
    reset_reason: Option<&'static str>,
    screen: bool,
    incoming: cursor::SessionCursor,
}

/// A retained result plus how much of its `final_text` this response carries.
struct ResultChunk {
    exec: i64,
    base: Value,
    text: String,
    offset: u64,
    take: usize,
}

impl ResultChunk {
    fn new(turn: &TurnRecord, offset: u64) -> Self {
        let mut base = public_result(turn);
        base["final_text"] = json!("");
        let text = turn.final_message.clone().unwrap_or_default();
        let start = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(text.len());
        let start = char_floor(&text, start);
        let text = text[start..].to_owned();
        let take = text.len();
        Self {
            exec: turn.execution_seq,
            base,
            text,
            offset: u64::try_from(start).unwrap_or(0),
            take,
        }
    }

    fn complete(&self) -> bool {
        self.take == self.text.len()
    }

    /// Take the largest UTF-8 prefix whose encoded record fits `budget`.
    /// Chunking beats dropping: the result body is what the caller asked for.
    fn fit(&mut self, budget: usize) {
        self.take = self.text.len();
        while self.take > 0 && json_cost(&self.value()) > budget {
            let next = self.take * 3 / 4;
            let next = if next == self.take {
                self.take - 1
            } else {
                next
            };
            self.take = char_floor(&self.text, next);
        }
    }

    fn value(&self) -> Value {
        let mut value = self.base.clone();
        value["final_text"] = json!(&self.text[..self.take]);
        value["final_text_offset"] = json!(self.offset);
        value["final_text_complete"] = json!(self.complete());
        value
    }
}

/// Largest index at or below `index` that is a UTF-8 character boundary.
fn char_floor(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

struct BucketInput {
    uid: String,
    session: Value,
    gaps: Vec<Value>,
    events: Vec<Value>,
    results: Vec<TurnRecord>,
    results_more: bool,
    stable: crate::screen::StablePage,
    live: crate::store::LiveScreen,
    epoch_reset: bool,
    screen: bool,
    incoming: cursor::SessionCursor,
}

impl Bucket {
    fn new(input: BucketInput) -> Self {
        let stable_start = input
            .stable
            .next_after
            .saturating_sub(u64::try_from(input.stable.lines.len()).unwrap_or(0));
        let mut incoming = input.incoming;
        let results = input
            .results
            .iter()
            .map(|turn| {
                let offset = if incoming.px == Some(turn.execution_seq) {
                    incoming.po
                } else {
                    0
                };
                ResultChunk::new(turn, offset)
            })
            .collect::<Vec<_>>();
        // A partial-delivery marker that no longer names the next retained
        // result is stale; dropping it keeps the continuation unambiguous.
        if incoming
            .px
            .is_some_and(|exec| results.first().is_none_or(|chunk| chunk.exec != exec))
        {
            incoming.px = None;
            incoming.po = 0;
        }
        let events_len = input.events.len();
        let results_len = results.len();
        let stable_len = input.stable.lines.len();
        Self {
            uid: input.uid,
            session: input.session,
            gaps: input.gaps,
            events: input.events,
            events_len,
            results,
            results_len,
            results_more: input.results_more,
            stable: input.stable.lines,
            stable_len,
            stable_truncate: None,
            stable_start,
            stable_more: input.stable.has_more,
            live: input.live.rows,
            live_included: input.screen,
            live_truncated: input.live.truncated,
            epoch: input.live.epoch,
            epoch_reset: input.epoch_reset,
            reset_reason: input.live.reset_reason,
            screen: input.screen,
            incoming,
        }
    }

    fn value(&self) -> Value {
        let mut stable = self.stable[..self.stable_len]
            .iter()
            .map(|line| Value::String(line.clone()))
            .collect::<Vec<_>>();
        if let Some(limit) = self.stable_truncate
            && let Some(Value::String(last)) = stable.last_mut()
        {
            last.truncate(char_floor(last, limit));
        }
        let mut bucket = json!({
            "session": self.session,
            "gaps": self.gaps,
            "results": self.results[..self.results_len]
                .iter()
                .map(ResultChunk::value)
                .collect::<Vec<_>>(),
            "events": self.events[..self.events_len],
        });
        if self.screen {
            bucket["screen"] = json!({
                "epoch": self.epoch,
                "reset": self.epoch_reset,
                "reset_reason": self.epoch_reset.then_some(self.reset_reason).flatten(),
                "stable": stable,
                "live": if self.live_included { self.live.clone() } else { Vec::new() },
                "live_truncated": self.live_truncated,
            });
        }
        bucket
    }

    /// Outgoing watermarks for exactly the content this bucket still carries.
    fn position(&self) -> cursor::SessionCursor {
        let mut position = self.incoming;
        for chunk in &self.results[..self.results_len] {
            if chunk.complete() {
                position.x = chunk.exec;
                position.px = None;
                position.po = 0;
            } else {
                position.px = Some(chunk.exec);
                position.po = chunk.offset + u64::try_from(chunk.take).unwrap_or(0);
            }
        }
        position.r = self.stable_start + u64::try_from(self.stable_len).unwrap_or(0);
        position.ep = self.epoch;
        position
    }

    fn delivered(&self) -> bool {
        self.events_len > 0 || self.results_len > 0 || self.stable_len > 0
    }

    /// Content this response does not carry and a later one must.
    fn pending(&self) -> bool {
        self.results_more
            || self.stable_more
            || self.events_len < self.events.len()
            || self.results_len < self.results.len()
            || self.stable_len < self.stable.len()
            || self.results[..self.results_len]
                .iter()
                .any(|chunk| !chunk.complete())
    }

    /// Drop the lowest-priority remaining content: screen text, then events,
    /// then result bodies. State and gaps are never shed.
    fn shed(&mut self) -> bool {
        if self.live_included && !self.live.is_empty() {
            self.live_included = false;
            return true;
        }
        if self.stable_len > 0 {
            self.stable_len -= 1;
            self.stable_truncate = None;
            return true;
        }
        if self.events_len > 0 {
            self.events_len -= 1;
            return true;
        }
        if self.results_len > 0 {
            let index = self.results_len - 1;
            let chunk = &mut self.results[index];
            if chunk.take > 0 {
                chunk.take = char_floor(&chunk.text, chunk.take * 3 / 4);
                if chunk.take > 0 {
                    return true;
                }
            }
            self.results_len -= 1;
            return true;
        }
        false
    }

    /// Re-admit exactly one unit of content so a `has_more` response always
    /// advances a watermark. Result bodies come first because they are the
    /// answer the caller asked for; a single oversized screen row is truncated
    /// mid-line rather than blocking the cursor forever.
    fn force_progress(&mut self, budget: usize) -> bool {
        if !self.results.is_empty() {
            self.results_len = 1;
            let chunk = &mut self.results[0];
            if chunk.take == 0 {
                chunk.take = char_floor(&chunk.text, budget.max(1));
                if chunk.take == 0 {
                    chunk.take = chunk
                        .text
                        .char_indices()
                        .nth(1)
                        .map_or(chunk.text.len(), |(index, _)| index);
                }
            }
            return true;
        }
        if !self.events.is_empty() {
            self.events_len = 1;
            return true;
        }
        if !self.stable.is_empty() {
            self.stable_len = 1;
            self.stable_truncate = Some(budget);
            return true;
        }
        false
    }
}

struct Rendered {
    value: Value,
    /// The long poll is finished: either something wake-worthy happened or the
    /// requested binding is satisfied.
    settled: bool,
}

#[allow(clippy::struct_excessive_bools)]
struct Assembled {
    value: Value,
    has_more: bool,
    progress: bool,
    delivered: bool,
    result_ready: bool,
    gapped: bool,
}

impl Cut {
    /// Serialize the cut, shedding content until the encoded document fits the
    /// caller's byte budget. The measured document is authoritative: estimates
    /// cannot bound JSON escaping or the cursor's own length.
    fn render(mut self, options: &FetchOptions) -> Result<Rendered> {
        self.pre_trim(options);
        let mut assembled = self.assemble(options)?;
        while serialized_len(&assembled.value)? > options.max_bytes {
            if !self.shed() {
                break;
            }
            assembled = self.assemble(options)?;
        }
        if assembled.has_more && !assembled.progress {
            // Every has_more response must advance at least one watermark or
            // the caller loops forever on an unchanged cursor.
            let floor = serialized_len(&assembled.value)?;
            let budget = options.max_bytes.saturating_sub(floor);
            if self.force_progress(budget) {
                assembled = self.assemble(options)?;
            }
        }
        let blocked = self.state == Some(SessionState::Blocked);
        let reason = if assembled.gapped {
            "gap"
        } else if blocked {
            "blocked"
        } else if options.until_result && assembled.result_ready {
            "result"
        } else if assembled.has_more {
            "page_full"
        } else if self.baseline {
            "snapshot"
        } else if assembled.delivered {
            "change"
        } else {
            "timeout"
        };
        let settled = assembled.gapped
            || blocked
            || assembled.has_more
            || if options.until_result {
                assembled.result_ready
            } else {
                self.baseline || assembled.delivered
            };
        let mut value = assembled.value;
        value["reason"] = json!(reason);
        Ok(Rendered { value, settled })
    }

    /// Cheap greedy fit before the exact loop, so the measured loop normally
    /// converges in a couple of iterations instead of hundreds.
    fn pre_trim(&mut self, options: &FetchOptions) {
        let reserve = FETCH_ENVELOPE_RESERVE
            + FETCH_SESSION_CURSOR_RESERVE * self.buckets.len().max(1)
            + self
                .buckets
                .iter()
                .map(|bucket| {
                    json_cost(&bucket.session) + json_cost(&Value::Array(bucket.gaps.clone()))
                })
                .sum::<usize>();
        let mut budget = options.max_bytes.saturating_sub(reserve);
        for bucket in &mut self.buckets {
            let mut kept = 0;
            for chunk in &mut bucket.results {
                let cost = json_cost(&chunk.value());
                if cost <= budget {
                    budget -= cost;
                    kept += 1;
                    continue;
                }
                chunk.fit(budget);
                if chunk.take > 0 {
                    kept += 1;
                    budget = 0;
                }
                break;
            }
            bucket.results_len = kept;
            let mut kept = 0;
            for event in &bucket.events {
                let cost = json_cost(event);
                if cost > budget {
                    break;
                }
                budget -= cost;
                kept += 1;
            }
            bucket.events_len = kept;
            let mut kept = 0;
            for line in &bucket.stable {
                let cost = json_cost(&Value::String(line.clone()));
                if cost > budget {
                    break;
                }
                budget -= cost;
                kept += 1;
            }
            bucket.stable_len = kept;
            let live_cost = bucket
                .live
                .iter()
                .map(|line| json_cost(&Value::String(line.clone())))
                .sum::<usize>();
            bucket.live_included = bucket.screen && live_cost <= budget;
            if bucket.live_included {
                budget -= live_cost;
            }
        }
    }

    fn shed(&mut self) -> bool {
        for bucket in self.buckets.iter_mut().rev() {
            if bucket.shed() {
                return true;
            }
        }
        // Only the mandatory floor is left; drop whole trailing buckets.
        if self.buckets.len() > 1 {
            self.buckets.pop();
            self.sessions_more = true;
            return true;
        }
        false
    }

    fn force_progress(&mut self, budget: usize) -> bool {
        self.buckets
            .first_mut()
            .is_some_and(|bucket| bucket.force_progress(budget))
    }

    #[allow(clippy::too_many_lines)]
    fn assemble(&self, options: &FetchOptions) -> Result<Assembled> {
        let mut next = cursor::Cursor::new(&options.instance_id, &options.scope);
        if let Some(previous) = options.cursor.as_ref() {
            next.p.clone_from(&previous.p);
        }
        // The event watermark may only advance over events this document
        // actually carries.
        let mut first_dropped = self.dropped_event_seq;
        for bucket in &self.buckets {
            for event in bucket.events.iter().skip(bucket.events_len) {
                let seq = event.get("seq").and_then(Value::as_i64).unwrap_or(0);
                first_dropped = Some(first_dropped.map_or(seq, |current| current.min(seq)));
            }
        }
        next.e = first_dropped.map_or(self.event_watermark, |seq| seq - 1);
        next.bl.clone_from(&self.baseline_next);

        let mut buckets = Vec::with_capacity(self.buckets.len());
        let mut has_more = self.events_more || self.sessions_more || self.baseline_next.is_some();
        let mut delivered = false;
        let mut progress = self.baseline;
        let mut gapped = !self.scope_gaps.is_empty();
        for bucket in &self.buckets {
            let position = bucket.position();
            has_more = has_more || bucket.pending();
            delivered = delivered || bucket.delivered();
            gapped = gapped || !bucket.gaps.is_empty();
            progress = progress || position != bucket.incoming;
            next.set_session(&bucket.uid, position);
            buckets.push(bucket.value());
        }
        progress = progress || next.e > options.event_position();
        let result_ready = self.bound_terminal
            && self.bound_seq.is_some_and(|seq| {
                self.buckets
                    .first()
                    .is_some_and(|bucket| seq <= bucket.position().x)
            });
        let value = json!({
            "schema_version": 1,
            "runtime": {
                "version": env!("CARGO_PKG_VERSION"),
                "instance_id": options.instance_id,
            },
            "reason": "timeout",
            "has_more": has_more,
            "gaps": self.scope_gaps,
            "cursor": next.encode()?,
            "sessions": buckets,
        });
        Ok(Assembled {
            value,
            has_more,
            progress,
            delivered,
            result_ready,
            gapped,
        })
    }
}

/// A continuation offset is caller-supplied. Reject one that does not name a
/// UTF-8 boundary inside the result it claims to continue, rather than
/// indexing a body with it.
fn validate_continuation(position: &cursor::SessionCursor, results: &[TurnRecord]) -> Result<()> {
    let Some(execution_seq) = position.px else {
        return Ok(());
    };
    let Some(turn) = results
        .first()
        .filter(|turn| turn.execution_seq == execution_seq)
    else {
        return Ok(());
    };
    let text = turn.final_message.as_deref().unwrap_or_default();
    let offset = usize::try_from(position.po).unwrap_or(usize::MAX);
    if offset > text.len() || !text.is_char_boundary(offset) {
        bail!(
            "CURSOR_INVALID: continuation offset is not a character boundary in the retained result"
        );
    }
    Ok(())
}

fn serialized_len(value: &Value) -> Result<usize> {
    Ok(serde_json::to_string(value)?.len())
}

fn json_cost(value: &Value) -> usize {
    serde_json::to_string(value).map_or(0, |text| text.len()) + 1
}

fn retention_gap(component: &str) -> Value {
    json!({"component": component, "reason": "retention_overrun"})
}

/// One bounded page of normalized lifecycle events plus its watermark.
type EventPage = (Vec<(Option<String>, Value)>, bool, i64);

fn normalized_page(store: &Store, uid: Option<&str>, after: i64, limit: usize) -> EventPage {
    let mut page = Vec::new();
    let mut watermark = after;
    let mut has_more = false;
    for event in store.read_events(uid, after) {
        if page.len() >= limit {
            has_more = true;
            break;
        }
        watermark = event.seq;
        if let Some(value) = normalize_event(&event) {
            page.push((event.session_uid.clone(), value));
        }
    }
    (page, has_more, watermark)
}

/// Normalize one retained event. Every field comes from the record itself, so
/// replaying an old cursor yields byte-identical events even after the turn it
/// described has been evicted.
fn normalize_event(event: &crate::protocol::EventRecord) -> Option<Value> {
    let event_type = normalize_event_type(&event.kind)?;
    let mut value = json!({
        "schema_version": 1,
        "seq": event.seq,
        "type": event_type,
        "session_id": event.session_id,
    });
    if let Some(execution_seq) = event.execution_seq {
        value["execution_seq"] = json!(execution_seq);
    }
    if event_type == "provider.retrying" {
        value["attempt"] = json!(event.retry_attempt.unwrap_or(1));
    }
    if event_type == "session.idle" {
        value["result_status"] = event
            .result_status
            .map_or(Value::Null, |status| json!(status));
    }
    if event_type == "session.blocked" {
        value["reason"] = json!("user_input");
    }
    Some(value)
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::Duration;

    use serde_json::{Value, json};

    use super::{
        Bucket, BucketInput, BusyMetrics, Cut, FETCH_DEFAULT_MAX_BYTES, FETCH_HARD_MAX_BYTES,
        FETCH_MIN_MAX_BYTES, FETCH_STABLE_PAGE, FetchOptions, ProviderReservation, ReceiptLedger,
        TranscriptRecovery, apply_codex_notification, apply_hook_event, canonical_session_id,
        clamp_busy_metrics, classify_error, generate_alias, generate_internal_id,
        provider_id_from_session, public_result, public_session, public_session_with_metrics,
        retention_gap, signal_shutdown, transcript_window, validate_alias, validate_continuation,
        wait_for_update_check,
    };
    use crate::protocol::{SessionState, TurnState};
    use crate::store::{NewSession, Store};

    fn ready_store(agent: &str) -> Store {
        let store = Store::new();
        let session_id = canonical_session_id(agent, "thread-1");
        store
            .insert_session(&NewSession {
                id: &session_id,
                alias: "@worker",
                title: "worker",
                agent,
                cwd: "/tmp",
                model: None,
                effort: None,
                harness_options: &[],
                auto_approve: true,
            })
            .unwrap_or_else(|error| panic!("failed to insert session: {error}"));
        assert!(store.set_session_running(&session_id, Some(42)));
        assert!(store.set_session_state(&session_id, SessionState::Idle));
        store
    }

    #[test]
    fn queued_shutdown_signal_cannot_be_lost_before_wait() {
        let shutting_down = AtomicBool::new(false);
        let (shutdown_sender, shutdown_receiver) = mpsc::sync_channel(1);
        signal_shutdown(&shutting_down, &shutdown_sender);
        assert!(shutting_down.load(Ordering::SeqCst));
        assert!(!wait_for_update_check(
            &shutdown_receiver,
            Duration::from_secs(1)
        ));
    }

    #[test]
    fn archive_separator_is_reserved_in_aliases() {
        assert!(validate_alias("@worker").is_ok());
        assert!(validate_alias("@worker#old").is_err());
    }

    #[test]
    fn stopped_send_error_has_resume_code_and_hint_source() {
        let error = anyhow::anyhow!(
            "SESSION_NOT_RUNNING: Session codex:thread-1 is not running; retry with --resume"
        );
        assert_eq!(classify_error(&error), "SESSION_NOT_RUNNING");
    }

    #[test]
    fn harness_startup_failures_are_launch_failures() {
        for message in [
            "Codex remote TUI did not create a thread: timed out waiting on channel",
            "provider did not report a Session ID",
            "Codex app-server child was unavailable",
            "failed to establish Codex app-server WebSocket",
        ] {
            let error = anyhow::anyhow!("{message}");
            assert_eq!(
                classify_error(&error),
                "LAUNCH_FAILED",
                "{message} must not be reported as an internal invariant failure"
            );
        }
    }

    #[test]
    fn provider_reservation_guard_releases_after_failure_scope() {
        let store = Arc::new(Mutex::new(Store::new()));
        assert!(
            store
                .lock()
                .unwrap_or_else(|error| panic!("store lock failed: {error}"))
                .reserve_provider_session("claude:provider", "internal:failed")
        );
        drop(ProviderReservation {
            store: Arc::clone(&store),
            provider_ref: "claude:provider".to_owned(),
            session_id: "internal:failed".to_owned(),
        });
        assert!(
            store
                .lock()
                .unwrap_or_else(|error| panic!("store lock failed: {error}"))
                .reserve_provider_session("claude:provider", "internal:retry")
        );
    }

    #[test]
    fn internal_ids_and_generated_aliases_are_short_and_unambiguous() {
        let id = generate_internal_id();
        assert_eq!(id.len(), 17);
        assert!(id.starts_with("internal:"));
        assert!(
            id[9..]
                .chars()
                .all(|character| { "0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(character) })
        );
        let alias = generate_alias("Run Review");
        assert!(alias.starts_with("@run-review-"));
        assert_eq!(alias.rsplit('-').next().map(str::len), Some(6));
        assert!(alias.rsplit('-').next().is_some_and(|suffix| {
            suffix
                .chars()
                .all(|character| "0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(character))
        }));
    }

    #[test]
    fn public_session_exposes_one_canonical_id() {
        let store = ready_store("codex");
        let session = store
            .get_session("codex:thread-1")
            .unwrap_or_else(|| panic!("session missing"));
        let value = public_session(&session);
        assert_eq!(value["id"], "codex:thread-1");
        assert!(value.get("provider_session_id").is_none());
        assert!(value.get("resume_ref").is_none());
        assert_eq!(provider_id_from_session(&session), Some("thread-1"));
    }

    #[test]
    fn busy_public_session_exposes_bounded_diagnostic_ages() {
        let mut store = ready_store("codex");
        store
            .insert_turn("turn_busy", "codex:thread-1", "hello")
            .unwrap_or_else(|error| panic!("failed to insert turn: {error}"));
        store.set_session_state("codex:thread-1", SessionState::Busy);
        let session = store
            .get_session("codex:thread-1")
            .unwrap_or_else(|| panic!("session missing"));

        let value = public_session_with_metrics(
            &session,
            Some(BusyMetrics {
                busy_for_ms: 1_234,
                pty_quiet_for_ms: 987,
            }),
        );
        assert_eq!(value["state"], "busy");
        assert_eq!(value["busy_for_ms"], 1_234);
        assert_eq!(value["pty_quiet_for_ms"], 987);

        let idle = ready_store("codex")
            .get_session("codex:thread-1")
            .unwrap_or_else(|| panic!("idle session missing"));
        let idle_value = public_session(&idle);
        assert!(idle_value.get("busy_for_ms").is_none());
        assert!(idle_value.get("pty_quiet_for_ms").is_none());
    }

    #[test]
    fn pty_quiet_age_is_clamped_to_current_busy_interval() {
        assert_eq!(
            clamp_busy_metrics(500, Some(1_200)),
            BusyMetrics {
                busy_for_ms: 500,
                pty_quiet_for_ms: 500,
            }
        );
        assert_eq!(
            clamp_busy_metrics(500, Some(250)),
            BusyMetrics {
                busy_for_ms: 500,
                pty_quiet_for_ms: 250,
            }
        );
        assert_eq!(
            clamp_busy_metrics(500, None),
            BusyMetrics {
                busy_for_ms: 500,
                pty_quiet_for_ms: 500,
            }
        );
    }

    #[test]
    fn public_result_exposes_sequence_but_not_internal_turn_identity() {
        let mut store = ready_store("codex");
        let turn = store
            .insert_turn("turn_private", "codex:thread-1", "hello")
            .unwrap_or_else(|error| panic!("failed to insert turn: {error}"));
        assert_eq!(turn.execution_seq, 1);
        assert!(store.mark_turn_started("turn_private", Some("provider_private")));
        assert!(
            store
                .complete_turn_if_matching(
                    "turn_private",
                    Some("provider_private"),
                    Some("done"),
                    false,
                )
                .unwrap_or_else(|error| panic!("failed to complete turn: {error}"))
        );
        let value = public_result(
            &store
                .get_turn("turn_private")
                .unwrap_or_else(|| panic!("turn missing")),
        );
        assert_eq!(value["execution_seq"], 1);
        assert_eq!(value["final_text"], "done");
        assert!(!value.to_string().contains("turn_private"));
        assert!(!value.to_string().contains("provider_private"));
    }

    #[test]
    fn claude_stop_failure_finishes_the_turn_as_failed() {
        let mut store = ready_store("claude");
        store
            .insert_turn("turn_1", "claude:thread-1", "hello")
            .unwrap_or_else(|error| panic!("failed to insert turn: {error}"));
        assert!(store.mark_turn_started("turn_1", Some("provider-turn")));
        let session = store
            .get_session("claude:thread-1")
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
            None,
        )
        .unwrap_or_else(|error| panic!("failed to apply StopFailure: {error}"));
        assert_eq!(outcome.kind, "turn.failed");
        let turn = store
            .get_turn("turn_1")
            .unwrap_or_else(|| panic!("turn missing"));
        assert_eq!(turn.state, TurnState::Failed);
        assert!(
            turn.error
                .is_some_and(|error| error.contains("invalid_request"))
        );
    }

    fn started_claude_turn(store: &mut Store) -> (crate::protocol::SessionRecord, String) {
        store
            .insert_turn("turn_1", "claude:thread-1", "hello")
            .unwrap_or_else(|error| panic!("failed to insert turn: {error}"));
        assert!(store.mark_turn_started("turn_1", Some("provider-turn")));
        let session = store
            .get_session("claude:thread-1")
            .unwrap_or_else(|| panic!("session missing"));
        let uid = store
            .session_uid("claude:thread-1")
            .unwrap_or_else(|| panic!("session uid missing"));
        (session, uid)
    }

    fn recovery(turn_id: &str, uid: &str, generation: u64) -> TranscriptRecovery {
        TranscriptRecovery {
            turn_id: turn_id.to_owned(),
            uid: uid.to_owned(),
            generation,
            provider_turn_id: Some("provider-turn".to_owned()),
            boundary: 0,
            path: "/transcript.jsonl".to_owned(),
            text: "recovered answer".to_owned(),
        }
    }

    #[test]
    fn a_missing_hook_final_text_is_recovered_from_the_transcript() {
        let mut store = ready_store("claude");
        let (session, uid) = started_claude_turn(&mut store);
        let generation = store.screen_epoch(&uid);

        let outcome = apply_hook_event(
            &mut store,
            &session,
            "Stop",
            &json!({"turn_id": "provider-turn", "last_assistant_message": ""}),
            Some(recovery("turn_1", &uid, generation)),
        )
        .unwrap_or_else(|error| panic!("failed to apply Stop: {error}"));

        assert_eq!(outcome.kind, "turn.completed");
        let turn = store
            .get_turn("turn_1")
            .unwrap_or_else(|| panic!("turn missing"));
        assert_eq!(turn.state, TurnState::Completed);
        assert_eq!(turn.final_message.as_deref(), Some("recovered answer"));
        assert_eq!(public_result(&turn)["final_text_source"], "transcript");
    }

    #[test]
    fn a_stale_recovery_never_replaces_text_or_fails_the_execution() {
        let mut store = ready_store("claude");
        let (session, uid) = started_claude_turn(&mut store);
        let generation = store.screen_epoch(&uid);

        let outcome = apply_hook_event(
            &mut store,
            &session,
            "Stop",
            &json!({"turn_id": "provider-turn", "last_assistant_message": ""}),
            Some(recovery(
                "turn_from_a_previous_generation",
                &uid,
                generation,
            )),
        )
        .unwrap_or_else(|error| panic!("failed to apply Stop: {error}"));

        assert_eq!(outcome.kind, "turn.completed");
        let turn = store
            .get_turn("turn_1")
            .unwrap_or_else(|| panic!("turn missing"));
        assert_eq!(turn.state, TurnState::Completed);
        assert_eq!(public_result(&turn)["final_text"], "");
        assert_eq!(public_result(&turn)["final_text_source"], "missing");
    }

    #[test]
    fn a_reported_hook_final_text_always_wins_over_the_transcript() {
        let mut store = ready_store("claude");
        let (session, uid) = started_claude_turn(&mut store);
        let generation = store.screen_epoch(&uid);

        apply_hook_event(
            &mut store,
            &session,
            "Stop",
            &json!({"turn_id": "provider-turn", "last_assistant_message": "hook answer"}),
            Some(recovery("turn_1", &uid, generation)),
        )
        .unwrap_or_else(|error| panic!("failed to apply Stop: {error}"));

        let turn = store
            .get_turn("turn_1")
            .unwrap_or_else(|| panic!("turn missing"));
        assert_eq!(turn.final_message.as_deref(), Some("hook answer"));
        assert_eq!(public_result(&turn)["final_text_source"], "hook");
    }

    fn fetch_options(max_bytes: usize, cursor: Option<crate::cursor::Cursor>) -> FetchOptions {
        FetchOptions {
            scope: "su_test".to_owned(),
            cursor,
            wait: Duration::from_millis(0),
            until_result: false,
            stable_limit: FETCH_STABLE_PAGE,
            max_bytes,
            instance_id: "boot".to_owned(),
        }
    }

    fn test_turn(execution_seq: i64, text: &str) -> crate::protocol::TurnRecord {
        crate::protocol::TurnRecord {
            id: format!("turn_{execution_seq}"),
            session_id: "claude:x".to_owned(),
            execution_seq,
            prompt: "p".to_owned(),
            state: TurnState::Completed,
            provider_turn_id: None,
            final_message: Some(text.to_owned()),
            final_text_recovered: false,
            transcript_path: None,
            transcript_offset: None,
            error: None,
            created_at_ms: 0,
            started_at_ms: Some(0),
            completed_at_ms: Some(1),
            usage: None,
        }
    }

    fn test_cut(
        events: Vec<Value>,
        results: Vec<crate::protocol::TurnRecord>,
        stable: Vec<String>,
        incoming: crate::cursor::SessionCursor,
    ) -> Cut {
        let event_watermark = events
            .last()
            .and_then(|event| event.get("seq"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let stable_len = u64::try_from(stable.len()).unwrap_or(0);
        Cut {
            baseline: false,
            state: Some(SessionState::Idle),
            bound_seq: None,
            bound_terminal: false,
            event_watermark,
            events_more: false,
            dropped_event_seq: None,
            scope_gaps: Vec::new(),
            buckets: vec![Bucket::new(BucketInput {
                uid: "su_test".to_owned(),
                session: json!({"id": "claude:x", "state": "idle"}),
                gaps: Vec::new(),
                events,
                results,
                results_more: false,
                stable: crate::screen::StablePage {
                    next_after: incoming.r + stable_len,
                    lines: stable,
                    has_more: false,
                    gap: false,
                },
                live: crate::store::LiveScreen::default(),
                epoch_reset: false,
                screen: true,
                incoming,
            })],
            sessions_more: false,
            baseline_next: None,
        }
    }

    fn lifecycle_events(count: i64) -> Vec<Value> {
        (1..=count)
            .map(|seq| {
                json!({
                    "schema_version": 1,
                    "seq": seq,
                    "type": "session.busy",
                    "session_id": "claude:x",
                    "execution_seq": seq,
                })
            })
            .collect()
    }

    fn decode(value: &Value) -> crate::cursor::Cursor {
        crate::cursor::Cursor::decode(
            value["cursor"]
                .as_str()
                .unwrap_or_else(|| panic!("no cursor")),
            "boot",
        )
        .unwrap_or_else(|error| panic!("failed to decode cursor: {error}"))
    }

    #[test]
    fn the_event_watermark_never_passes_an_event_the_response_dropped() {
        let cut = test_cut(
            lifecycle_events(8),
            Vec::new(),
            Vec::new(),
            crate::cursor::SessionCursor::default(),
        );
        let rendered = cut
            .render(&fetch_options(700, None))
            .unwrap_or_else(|error| panic!("failed to render: {error}"));

        let delivered = rendered.value["sessions"][0]["events"]
            .as_array()
            .unwrap_or_else(|| panic!("no events"))
            .len();
        assert!(delivered < 8, "budget did not drop any event");
        assert_eq!(rendered.value["has_more"], true);
        let cursor = decode(&rendered.value);
        assert_eq!(
            cursor.e,
            i64::try_from(delivered).unwrap_or(0),
            "watermark skipped an undelivered event"
        );

        // The next call from that cursor still owes the dropped events.
        let remaining = lifecycle_events(8)
            .into_iter()
            .filter(|event| event["seq"].as_i64().unwrap_or(0) > cursor.e)
            .collect::<Vec<_>>();
        assert_eq!(remaining.len(), 8 - delivered);
    }

    #[test]
    fn a_chunked_baseline_result_resumes_on_the_same_execution() {
        let text = "x".repeat(4_000);
        // A baseline anchors just below the latest retained result.
        let incoming = crate::cursor::SessionCursor {
            x: 6,
            ..crate::cursor::SessionCursor::default()
        };
        let mut cut = test_cut(Vec::new(), vec![test_turn(7, &text)], Vec::new(), incoming);
        cut.baseline = true;
        let first = cut
            .render(&fetch_options(1_500, None))
            .unwrap_or_else(|error| panic!("failed to render: {error}"));

        let result = &first.value["sessions"][0]["results"][0];
        assert_eq!(result["final_text_complete"], false);
        assert_eq!(result["final_text_offset"], 0);
        let cursor = decode(&first.value);
        let position = cursor.session("su_test");
        assert_eq!(position.px, Some(7));
        assert_eq!(position.x, 6, "an incomplete result must not advance x");
        assert!(position.po > 0);

        // The continuation only ever sees execution 7 again, and completes it.
        let second = test_cut(Vec::new(), vec![test_turn(7, &text)], Vec::new(), position)
            .render(&fetch_options(FETCH_HARD_MAX_BYTES, Some(cursor)))
            .unwrap_or_else(|error| panic!("failed to render: {error}"));
        let result = &second.value["sessions"][0]["results"][0];
        assert_eq!(result["final_text_complete"], true);
        assert_eq!(result["final_text_offset"], json!(position.po));
        let position = decode(&second.value).session("su_test");
        assert_eq!(position.x, 7);
        assert_eq!(position.px, None);
        assert_eq!(position.po, 0);
    }

    #[test]
    fn until_result_reports_a_result_only_once_it_is_fully_delivered() {
        let text = "y".repeat(4_000);
        let mut cut = test_cut(
            Vec::new(),
            vec![test_turn(7, &text)],
            Vec::new(),
            crate::cursor::SessionCursor::default(),
        );
        cut.bound_seq = Some(7);
        cut.bound_terminal = true;
        let mut options = fetch_options(1_500, None);
        options.until_result = true;
        let partial = cut
            .render(&options)
            .unwrap_or_else(|error| panic!("failed to render: {error}"));
        assert_eq!(partial.value["reason"], "page_full");
        assert!(partial.settled, "a page-full response must return at once");

        let mut cut = test_cut(
            Vec::new(),
            vec![test_turn(7, &text)],
            Vec::new(),
            crate::cursor::SessionCursor::default(),
        );
        cut.bound_seq = Some(7);
        cut.bound_terminal = true;
        let mut options = fetch_options(FETCH_HARD_MAX_BYTES, None);
        options.until_result = true;
        let complete = cut
            .render(&options)
            .unwrap_or_else(|error| panic!("failed to render: {error}"));
        assert_eq!(complete.value["reason"], "result");
    }

    #[test]
    fn an_oversized_screen_row_still_advances_the_cursor() {
        let row = "z".repeat(100_000);
        let cut = test_cut(
            Vec::new(),
            Vec::new(),
            vec![row, "next".to_owned()],
            crate::cursor::SessionCursor::default(),
        );
        let rendered = cut
            .render(&fetch_options(FETCH_MIN_MAX_BYTES, None))
            .unwrap_or_else(|error| panic!("failed to render: {error}"));

        assert_eq!(rendered.value["has_more"], true);
        let position = decode(&rendered.value).session("su_test");
        assert!(position.r >= 1, "an oversized row blocked the cursor");
        let stable = rendered.value["sessions"][0]["screen"]["stable"]
            .as_array()
            .unwrap_or_else(|| panic!("no stable array"));
        assert_eq!(stable.len(), 1);
        assert!(
            stable[0].as_str().unwrap_or_default().len() < 100_000,
            "the oversized row was not truncated"
        );
    }

    #[test]
    fn a_scope_gap_survives_a_response_with_no_session_buckets() {
        let mut cut = test_cut(
            Vec::new(),
            Vec::new(),
            Vec::new(),
            crate::cursor::SessionCursor::default(),
        );
        cut.buckets.clear();
        cut.state = None;
        cut.scope_gaps.push(retention_gap("events"));

        let rendered = cut
            .render(&fetch_options(FETCH_DEFAULT_MAX_BYTES, None))
            .unwrap_or_else(|error| panic!("failed to render: {error}"));
        assert_eq!(rendered.value["reason"], "gap");
        assert_eq!(rendered.value["gaps"][0]["component"], "events");
        assert!(rendered.settled);
    }

    #[test]
    fn an_unfinished_all_baseline_keeps_paging_from_its_enumeration_position() {
        let mut cut = test_cut(
            Vec::new(),
            Vec::new(),
            Vec::new(),
            crate::cursor::SessionCursor::default(),
        );
        cut.baseline = true;
        cut.baseline_next = Some("su_last".to_owned());
        let rendered = cut
            .render(&fetch_options(FETCH_DEFAULT_MAX_BYTES, None))
            .unwrap_or_else(|error| panic!("failed to render: {error}"));

        assert_eq!(rendered.value["has_more"], true);
        let cursor = decode(&rendered.value);
        assert_eq!(cursor.bl.as_deref(), Some("su_last"));
        let resumed = FetchOptions {
            cursor: Some(cursor),
            ..fetch_options(FETCH_DEFAULT_MAX_BYTES, None)
        };
        assert!(
            resumed.baseline(),
            "baseline mode must persist while paging"
        );
        assert_eq!(resumed.resume_after(), Some("su_last"));
    }

    #[test]
    fn concurrent_acceptances_with_one_request_id_run_exactly_once() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicUsize;

        let ledger = Arc::new(ReceiptLedger::default());
        let runs = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(std::sync::Barrier::new(4));
        let handles = (0..4)
            .map(|_| {
                let ledger = Arc::clone(&ledger);
                let runs = Arc::clone(&runs);
                let started = Arc::clone(&started);
                std::thread::spawn(move || {
                    started.wait();
                    ledger.accept_once("retry-1", 42, || {
                        // Hold the acceptance open long enough that every
                        // other thread must observe the reservation.
                        std::thread::sleep(Duration::from_millis(50));
                        let seq = runs.fetch_add(1, Ordering::SeqCst) + 1;
                        Ok(json!({"session": {"id": "claude:x"}, "execution_seq": seq}))
                    })
                })
            })
            .collect::<Vec<_>>();

        let mut replayed = 0;
        for handle in handles {
            let value = handle
                .join()
                .unwrap_or_else(|_| panic!("acceptance thread panicked"))
                .unwrap_or_else(|error| panic!("acceptance failed: {error}"));
            assert_eq!(value["execution_seq"], 1, "a duplicate acceptance ran");
            if value["replayed"] == json!(true) {
                replayed += 1;
            }
        }
        assert_eq!(runs.load(Ordering::SeqCst), 1, "the acceptance ran twice");
        assert_eq!(replayed, 3, "duplicates must be marked as replays");

        let mismatch = ledger
            .accept_once("retry-1", 43, || Ok(json!({})))
            .err()
            .unwrap_or_else(|| panic!("a different payload was accepted"));
        assert!(mismatch.to_string().contains("invalid request_id reuse"));
        assert_eq!(classify_error(&mismatch), "INVALID_ARGUMENT");
    }

    #[test]
    fn a_failed_acceptance_leaves_no_receipt_and_can_be_retried() {
        let ledger = ReceiptLedger::default();
        assert!(
            ledger
                .accept_once("retry-2", 7, || Err(anyhow::anyhow!("launch failed")))
                .is_err()
        );
        let value = ledger
            .accept_once("retry-2", 7, || Ok(json!({"ok": true})))
            .unwrap_or_else(|error| panic!("retry after failure failed: {error}"));
        assert!(value.get("replayed").is_none());
    }

    #[test]
    fn the_transcript_fallback_needs_a_recorded_boundary() {
        let mut turn = test_turn(1, "");
        assert!(transcript_window(&turn).is_none());
        turn.transcript_path = Some("/x.jsonl".to_owned());
        assert!(
            transcript_window(&turn).is_none(),
            "a path without a boundary must not enable the fallback"
        );
        turn.transcript_offset = Some(12);
        assert_eq!(transcript_window(&turn), Some(("/x.jsonl".to_owned(), 12)));
    }

    #[test]
    fn a_continuation_offset_off_a_character_boundary_is_rejected() {
        let turn = test_turn(7, "héllo");
        let inside = crate::cursor::SessionCursor {
            px: Some(7),
            po: 2,
            ..crate::cursor::SessionCursor::default()
        };
        let error = validate_continuation(&inside, std::slice::from_ref(&turn))
            .err()
            .unwrap_or_else(|| panic!("a split character was accepted"));
        assert_eq!(classify_error(&error), "CURSOR_INVALID");

        let past_end = crate::cursor::SessionCursor {
            px: Some(7),
            po: 9_999,
            ..crate::cursor::SessionCursor::default()
        };
        assert!(validate_continuation(&past_end, std::slice::from_ref(&turn)).is_err());

        let valid = crate::cursor::SessionCursor {
            px: Some(7),
            po: 1,
            ..crate::cursor::SessionCursor::default()
        };
        assert!(validate_continuation(&valid, std::slice::from_ref(&turn)).is_ok());
        // A marker that no longer names the next retained result is stale, not
        // hostile: it is dropped rather than rejected.
        let stale = crate::cursor::SessionCursor {
            px: Some(99),
            po: 5,
            ..crate::cursor::SessionCursor::default()
        };
        assert!(validate_continuation(&stale, std::slice::from_ref(&turn)).is_ok());
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
        assert!(error.contains("badRequest"));
        assert!(!error.contains("secret"));
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
