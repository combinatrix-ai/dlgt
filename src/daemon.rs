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
        receipts: Mutex::new(VecDeque::new()),
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
    /// Bounded request-id to acceptance-receipt map. A caller that never saw
    /// its acceptance response replays the original receipt instead of
    /// creating a duplicate Session or execution.
    receipts: Mutex<VecDeque<Receipt>>,
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
        let digest = request_digest(params);
        let receipts = self
            .receipts
            .lock()
            .map_err(|_| anyhow!("request receipt lock poisoned"))?;
        if let Some(receipt) = receipts.iter().find(|receipt| receipt.id == request_id) {
            if receipt.digest != digest {
                bail!(
                    "invalid request_id reuse: {request_id:?} already accepted a different payload"
                );
            }
            let mut replay = receipt.value.clone();
            replay["replayed"] = json!(true);
            return Ok(replay);
        }
        drop(receipts);
        let result = run(params)?;
        let mut receipts = self
            .receipts
            .lock()
            .map_err(|_| anyhow!("request receipt lock poisoned"))?;
        if !receipts.iter().any(|receipt| receipt.id == request_id) {
            receipts.push_back(Receipt {
                id: request_id.to_owned(),
                digest,
                value: result.clone(),
            });
            while receipts.len() > REQUEST_RECEIPT_LIMIT {
                receipts.pop_front();
            }
        }
        Ok(result)
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
            let selector = selector.unwrap_or_default();
            let session = self.resolve_session(selector)?;
            self.lock_store()?
                .session_uid(&session.id)
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
        let baseline = options.cursor.is_none();
        let position = options
            .cursor
            .as_ref()
            .map(|cursor| cursor.session(&uid))
            .unwrap_or_default();
        let mut gaps = Vec::new();

        let (events, events_more, event_seq) = if baseline {
            (Vec::new(), false, store.latest_event_seq())
        } else {
            let after = options.cursor.as_ref().map_or(0, |cursor| cursor.e);
            if after < store.evicted_event_seq() {
                gaps.push(retention_gap("events"));
            }
            normalized_page(&store, Some(&session.id), after, FETCH_EVENT_PAGE)
        };

        let (results, results_more) = if baseline {
            let latest = store
                .latest_turn(&session.id)
                .filter(|turn| turn.state.is_terminal());
            (latest.into_iter().collect::<Vec<_>>(), false)
        } else {
            if position.x < store.evicted_result_seq(&uid) {
                gaps.push(retention_gap("results"));
            }
            store.results_after(&uid, position.x, FETCH_RESULT_PAGE)
        };

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
            bound_terminal,
            event_seq,
            events_more,
            sessions: vec![SessionDelta {
                uid,
                session: public,
                events,
                results,
                results_more,
                stable,
                live,
                epoch_reset,
                gaps,
                position,
            }],
            sessions_more: false,
        })
    }

    #[allow(clippy::too_many_lines)]
    fn fetch_all_cut(&self, options: &FetchOptions) -> Result<Cut> {
        let store = self.lock_store()?;
        let baseline = options.cursor.is_none();
        let after = options.cursor.as_ref().map_or(0, |cursor| cursor.e);
        let mut gaps = Vec::new();
        if !baseline && after < store.evicted_event_seq() {
            gaps.push(retention_gap("events"));
        }
        let (events, mut events_more, mut event_seq) = if baseline {
            (Vec::new(), false, store.latest_event_seq())
        } else {
            normalized_page(&store, None, after, FETCH_EVENT_PAGE)
        };

        // Bucket the global event page, truncating it rather than advancing
        // the shared watermark past a Session that did not fit this page.
        let mut order = Vec::new();
        let mut buckets: HashMap<String, Vec<Value>> = HashMap::new();
        let mut delivered_events = Vec::new();
        for event in events {
            let Some(id) = event.get("session_id").and_then(Value::as_str) else {
                continue;
            };
            let Some(uid) = store.session_uid(id) else {
                continue;
            };
            if !buckets.contains_key(&uid) {
                if order.len() >= FETCH_SESSION_PAGE {
                    events_more = true;
                    event_seq = delivered_events
                        .last()
                        .and_then(|value: &Value| value.get("seq"))
                        .and_then(Value::as_i64)
                        .unwrap_or(after);
                    break;
                }
                order.push(uid.clone());
            }
            buckets.entry(uid).or_default().push(event.clone());
            delivered_events.push(event);
        }

        let mut deltas = Vec::new();
        let mut sessions_more = false;
        for uid in store.session_uids() {
            let Some(session) = store.session_for_uid(&uid) else {
                continue;
            };
            if session.id.starts_with("internal:") {
                continue;
            }
            let position = options
                .cursor
                .as_ref()
                .map(|cursor| cursor.session(&uid))
                .unwrap_or_default();
            let (results, results_more) = if baseline {
                (Vec::new(), false)
            } else {
                store.results_after(&uid, position.x, FETCH_RESULT_PAGE)
            };
            let events = buckets.remove(&uid).unwrap_or_default();
            let changed = !events.is_empty() || !results.is_empty();
            if !baseline && !changed {
                continue;
            }
            if deltas.len() >= FETCH_SESSION_PAGE {
                sessions_more = true;
                break;
            }
            let mut session_gaps = Vec::new();
            if !baseline && position.x < store.evicted_result_seq(&uid) {
                session_gaps.push(retention_gap("results"));
            }
            let baseline_result = baseline
                .then(|| {
                    store
                        .latest_turn(&session.id)
                        .filter(|turn| turn.state.is_terminal())
                })
                .flatten();
            let result_seq = baseline_result
                .as_ref()
                .map_or(position.x, |turn| turn.execution_seq);
            deltas.push(SessionDelta {
                uid: uid.clone(),
                session: self.public_session_locked(&store, &session)?,
                events,
                results: if baseline {
                    baseline_result.into_iter().collect()
                } else {
                    results
                },
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
                gaps: session_gaps,
                position: crate::cursor::SessionCursor {
                    x: result_seq,
                    ..position
                },
            });
        }
        drop(store);
        if !gaps.is_empty()
            && let Some(first) = deltas.first_mut()
        {
            first.gaps.extend(gaps);
        }
        Ok(Cut {
            baseline,
            state: None,
            bound_terminal: false,
            event_seq,
            events_more,
            sessions: deltas,
            sessions_more,
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
        let session_id = if let Some(selector) = params.get("session").and_then(Value::as_str) {
            Some(self.resolve_session(selector)?.id)
        } else {
            None
        };
        let store = self.lock_store()?;
        let normalized = store
            .read_events(session_id.as_deref(), after)
            .iter()
            .filter_map(|event| normalize_event(&store, event))
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
        let mut store = self.lock_store()?;
        let outcome = apply_hook_event(&mut store, &session, event_name, &payload)?;
        let seq = store.record_event(Some(&session.id), outcome.turn_id.as_deref(), outcome.kind);
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

fn apply_hook_event(
    store: &mut Store,
    session: &SessionRecord,
    event_name: &str,
    payload: &Value,
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
        "Stop" => complete_hook_turn(store, session, payload),
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
    let final_message = payload
        .get("last_assistant_message")
        .and_then(Value::as_str);
    let completed = store.complete_turn_if_matching(&turn_id, provider_turn_id, final_message)?;
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
        "error": turn.error,
        "started_at_ms": turn.started_at_ms.unwrap_or(turn.created_at_ms),
        "completed_at_ms": turn.completed_at_ms,
        "usage": turn.usage,
    })
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

/// An immutable observation cut taken under the store lock. Serialization
/// happens after the lock is released, so output arriving during serialization
/// belongs to the next cursor.
#[allow(clippy::struct_excessive_bools)]
struct Cut {
    baseline: bool,
    state: Option<SessionState>,
    bound_terminal: bool,
    event_seq: i64,
    events_more: bool,
    sessions: Vec<SessionDelta>,
    sessions_more: bool,
}

struct SessionDelta {
    uid: String,
    session: Value,
    events: Vec<Value>,
    results: Vec<TurnRecord>,
    results_more: bool,
    stable: crate::screen::StablePage,
    live: crate::store::LiveScreen,
    epoch_reset: bool,
    gaps: Vec<Value>,
    position: cursor::SessionCursor,
}

struct Rendered {
    value: Value,
    /// The long poll is finished: either something wake-worthy happened or the
    /// requested binding is satisfied.
    settled: bool,
}

impl Cut {
    fn render(mut self, options: &FetchOptions) -> Result<Rendered> {
        let mut next = cursor::Cursor::new(&options.instance_id, &options.scope);
        next.e = self.event_seq;
        if let Some(previous) = options.cursor.as_ref() {
            next.p.clone_from(&previous.p);
        }
        let mut has_more = self.events_more || self.sessions_more;
        let mut gapped = false;
        // Envelope, cursor, and reason are reserved before any payload so a
        // squeezed response can still carry state and gaps.
        let mut budget = options.max_bytes.saturating_sub(
            FETCH_ENVELOPE_RESERVE + FETCH_SESSION_CURSOR_RESERVE * self.sessions.len().max(1),
        );
        let mut buckets = Vec::new();
        for delta in &mut self.sessions {
            let (value, position, more, gap) = delta.render(options, &mut budget);
            has_more = has_more || more;
            gapped = gapped || gap;
            next.set_session(&delta.uid, position);
            buckets.push(value);
        }
        let delivered = buckets.iter().any(|bucket| {
            bucket
                .get("events")
                .and_then(Value::as_array)
                .is_some_and(|events| !events.is_empty())
                || bucket
                    .get("results")
                    .and_then(Value::as_array)
                    .is_some_and(|results| !results.is_empty())
                || bucket
                    .pointer("/screen/stable")
                    .and_then(Value::as_array)
                    .is_some_and(|stable| !stable.is_empty())
        });
        let blocked = self.state == Some(SessionState::Blocked);
        let reason = if gapped {
            "gap"
        } else if blocked {
            "blocked"
        } else if options.until_result && self.bound_terminal {
            "result"
        } else if has_more {
            "page_full"
        } else if self.baseline {
            "snapshot"
        } else if delivered {
            "change"
        } else {
            "timeout"
        };
        let settled = gapped
            || blocked
            || has_more
            || if options.until_result {
                self.bound_terminal
            } else {
                self.baseline || delivered
            };
        Ok(Rendered {
            value: json!({
                "schema_version": 1,
                "runtime": {
                    "version": env!("CARGO_PKG_VERSION"),
                    "instance_id": options.instance_id,
                },
                "reason": reason,
                "has_more": has_more,
                "cursor": next.encode()?,
                "sessions": buckets,
            }),
            settled,
        })
    }
}

impl SessionDelta {
    /// Fill one Session bucket in priority order: state, gaps, terminal
    /// results, events, then screen text.
    fn render(
        &mut self,
        options: &FetchOptions,
        budget: &mut usize,
    ) -> (Value, cursor::SessionCursor, bool, bool) {
        let mut position = self.position;
        let mut has_more = self.results_more;
        let gapped = !self.gaps.is_empty();
        let mut bucket = json!({
            "session": self.session,
            "gaps": self.gaps,
        });
        *budget = budget.saturating_sub(json_cost(&bucket));

        let mut results = Vec::new();
        for turn in &self.results {
            let resume = if position.px == Some(turn.execution_seq) {
                position.po
            } else {
                0
            };
            let (value, complete, delivered) = fetch_result(turn, resume, *budget);
            if !complete && delivered == 0 {
                has_more = true;
                break;
            }
            *budget = budget.saturating_sub(json_cost(&value));
            results.push(value);
            if complete {
                position.x = turn.execution_seq;
                position.px = None;
                position.po = 0;
            } else {
                position.px = Some(turn.execution_seq);
                position.po = resume + delivered;
                has_more = true;
                break;
            }
        }
        bucket["results"] = Value::Array(results);

        let mut events = Vec::new();
        for event in &self.events {
            let cost = json_cost(event);
            if cost > *budget {
                has_more = true;
                break;
            }
            *budget -= cost;
            events.push(event.clone());
        }
        bucket["events"] = Value::Array(events);

        if options.stable_limit > 0 {
            let mut stable = Vec::new();
            let mut row = self.stable.next_after.saturating_sub(self.delivered_rows());
            for line in &self.stable.lines {
                let cost = line.len() + 4;
                if cost > *budget {
                    has_more = true;
                    break;
                }
                *budget -= cost;
                row += 1;
                stable.push(Value::String(line.clone()));
            }
            has_more = has_more || self.stable.has_more;
            position.r = row;
            let live = if *budget > 0 {
                self.live.rows.clone()
            } else {
                Vec::new()
            };
            bucket["screen"] = json!({
                "epoch": self.live.epoch,
                "reset": self.epoch_reset,
                "reset_reason": self.epoch_reset.then_some(self.live.reset_reason),
                "stable": stable,
                "live": live,
                "live_truncated": self.live.truncated,
            });
        } else {
            position.r = self.stable.next_after;
        }
        position.ep = self.live.epoch;
        (bucket, position, has_more, gapped)
    }

    fn delivered_rows(&self) -> u64 {
        u64::try_from(self.stable.lines.len()).unwrap_or(0)
    }
}

fn json_cost(value: &Value) -> usize {
    serde_json::to_string(value).map_or(0, |text| text.len()) + 1
}

fn retention_gap(component: &str) -> Value {
    json!({"component": component, "reason": "retention_overrun"})
}

/// One bounded page of normalized lifecycle events plus its watermark.
fn normalized_page(
    store: &Store,
    session_id: Option<&str>,
    after: i64,
    limit: usize,
) -> (Vec<Value>, bool, i64) {
    let mut page = Vec::new();
    let mut watermark = after;
    let mut has_more = false;
    for event in store.read_events(session_id, after) {
        if page.len() >= limit {
            has_more = true;
            break;
        }
        watermark = event.seq;
        if let Some(value) = normalize_event(store, &event) {
            page.push(value);
        }
    }
    (page, has_more, watermark)
}

/// A retained result, with `final_text` chunked at a UTF-8 boundary when the
/// remaining response budget cannot carry all of it.
fn fetch_result(turn: &TurnRecord, offset: u64, budget: usize) -> (Value, bool, u64) {
    let mut value = public_result(turn);
    value["final_text"] = json!("");
    value["final_text_offset"] = json!(offset);
    value["final_text_complete"] = json!(true);
    let overhead = json_cost(&value);
    let text = turn.final_message.clone().unwrap_or_default();
    let start = usize::try_from(offset).unwrap_or(0).min(text.len());
    let remaining = &text[start..];
    let mut end = remaining.len().min(budget.saturating_sub(overhead));
    loop {
        while end > 0 && !remaining.is_char_boundary(end) {
            end -= 1;
        }
        value["final_text"] = json!(&remaining[..end]);
        // JSON escaping can inflate the encoded length past the raw byte
        // budget, so the measured cost is authoritative.
        if end == 0 || json_cost(&value) <= budget {
            break;
        }
        end = end * 3 / 4;
    }
    let complete = end == remaining.len();
    value["final_text_complete"] = json!(complete);
    (value, complete, u64::try_from(end).unwrap_or(0))
}

fn normalize_event(store: &Store, event: &crate::protocol::EventRecord) -> Option<Value> {
    let event_type = normalize_event_type(&event.kind)?;
    let turn = event.turn_id.as_deref().and_then(|id| store.get_turn(id));
    let mut value = json!({
        "schema_version": 1,
        "seq": event.seq,
        "type": event_type,
        "session_id": event.session_id,
    });
    if let Some(turn) = turn.as_ref() {
        value["execution_seq"] = json!(turn.execution_seq);
    }
    if event_type == "provider.retrying" {
        value["attempt"] = json!(event.retry_attempt.unwrap_or(1));
    }
    if event_type == "session.idle" {
        value["result_status"] = turn.as_ref().map_or(Value::Null, |turn| json!(turn.state));
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

    use serde_json::json;

    use super::{
        BusyMetrics, ProviderReservation, apply_codex_notification, apply_hook_event,
        canonical_session_id, clamp_busy_metrics, classify_error, generate_alias,
        generate_internal_id, provider_id_from_session, public_result, public_session,
        public_session_with_metrics, signal_shutdown, validate_alias, wait_for_update_check,
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
                .complete_turn_if_matching("turn_private", Some("provider_private"), Some("done"))
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
        let events = store.read_events(Some("codex:thread-1"), 0);
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
        assert!(
            store
                .read_events(Some("codex:thread-1"), 0)
                .iter()
                .any(|event| event.kind == "provider.quiesced")
        );
    }
}
