use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::{
    FileTypeExt as _, MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _,
};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, RwLock, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

use crate::error::{Code, WithCode, coded, fail};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::claude::{self, ClaudeProcess, ClaudeTurns};
use crate::codex::CodexConnection;
use crate::codex_events::{self, CodexItems};
use crate::config::{self, AuthMismatch};
use crate::cursor;
use crate::messages::{Decider, Decision, MessageBody};
use crate::paths;
use crate::protocol::{
    ApprovalsMode, Auth, AuthPolicy, Request, Response, SessionRecord, SessionState, TurnRecord,
    TurnState,
};
use crate::provider::{
    Agent, LaunchOptions, claude_command, claude_session_marker, codex_discovery_args,
    codex_session_args, codex_thread_params, prepare_workspace, provider_display_name,
};
use crate::reaper::Reaper;
use crate::requests::{
    Answer, AnswerClaim, Outcome, PUBLISHED_REQUEST_LIMIT, ProviderRequest, RequestKind,
    RequestRecord,
};
use crate::store::{NewSession, Store, now_ms};
use crate::update;

/// A short confirmation window covers the normal provider acknowledgement
/// without turning submission into observation. Expiry is a successful
/// `pending` receipt.
const SUBMISSION_CONFIRM_TIMEOUT: Duration = Duration::from_secs(5);
/// Composite read bounds. Response size is part of the protocol: an LLM caller
/// pays for every byte, so pagination is not optional implementation tuning.
const FETCH_DEFAULT_MAX_BYTES: usize = 32 * 1024;
const FETCH_HARD_MAX_BYTES: usize = 256 * 1024;
const FETCH_EVENT_PAGE: usize = 64;
const FETCH_RESULT_PAGE: usize = 4;
const FETCH_MESSAGE_PAGE: usize = 64;
const FETCH_MESSAGE_MAX: usize = 512;
/// A long poll is bounded only to catch typos and leaked waiters.
const FETCH_MAX_WAIT_MS: u64 = 24 * 60 * 60 * 1000;
const FETCH_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Acceptance receipts retained per daemon, evicted first-in-first-out.
const REQUEST_RECEIPT_LIMIT: usize = 1024;
const EMPTY_DAEMON_IDLE_TIMEOUT: Duration = Duration::from_mins(5);
/// How long a provider gets to acknowledge a control request it normally
/// answers at once.
const CLAUDE_CONTROL_TIMEOUT: Duration = Duration::from_secs(10);

fn is_stale_socket_error(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
    )
}

fn ownership_lock_path(socket_path: &Path) -> PathBuf {
    let mut path = socket_path.as_os_str().to_os_string();
    path.push(".lock");
    PathBuf::from(path)
}

struct DaemonLock {
    // Keeping this descriptor open holds the advisory flock for the daemon's
    // entire lifetime. The lock file itself is intentionally retained so a
    // concurrent starter always has one stable inode to lock.
    _file: fs::File,
}

impl DaemonLock {
    fn acquire(socket_path: &Path) -> Result<Self> {
        let lock_path = ownership_lock_path(socket_path);
        let file = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_path)
            .with_context(|| {
                format!(
                    "failed to open dlgt ownership lock {}; check permissions and retry outside the sandbox",
                    lock_path.display()
                )
            })?;
        // SAFETY: flock only inspects and updates the kernel state attached to
        // this valid open file descriptor; it does not access Rust memory.
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result == -1 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                bail!(
                    "another dlgt server is starting or running (ownership lock {} is held); wait for it to exit or use a different DLGT_SOCKET",
                    lock_path.display()
                );
            }
            return Err(anyhow!(error)).with_context(|| {
                format!(
                    "failed to acquire dlgt ownership lock {}; check permissions and retry outside the sandbox",
                    lock_path.display()
                )
            });
        }
        Ok(Self { _file: file })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SocketIdentity {
    dev: u64,
    ino: u64,
}

impl SocketIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
        }
    }
}

fn socket_identity(socket_path: &Path) -> Result<SocketIdentity> {
    fs::symlink_metadata(socket_path)
        .map(|metadata| SocketIdentity::from_metadata(&metadata))
        .with_context(|| format!("failed to inspect bound socket {}", socket_path.display()))
}

/// Remove the endpoint only when it still names the socket this daemon bound.
/// A successor or any other replacement is left untouched.
fn remove_owned_socket(socket_path: &Path, expected: SocketIdentity) -> Result<()> {
    match fs::symlink_metadata(socket_path) {
        Ok(metadata) if SocketIdentity::from_metadata(&metadata) == expected => {
            fs::remove_file(socket_path).with_context(|| {
                format!(
                    "failed to remove owned dlgt socket {} before listener release",
                    socket_path.display()
                )
            })?;
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(anyhow!(error)).with_context(|| {
                format!(
                    "cannot verify ownership of dlgt socket {}; preserving the pathname",
                    socket_path.display()
                )
            });
        }
    }
    Ok(())
}

/// Inspect the configured endpoint before binding it. Only the two errors
/// that mean "there is no listener behind this socket" authorize removing an
/// existing Unix socket. Permission and other errors are deliberately
/// returned without unlinking anything, since they may describe a live
/// daemon or a sandbox boundary rather than stale state.
fn prepare_socket_path(socket_path: &std::path::Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(socket_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            let hint = if error.kind() == std::io::ErrorKind::PermissionDenied {
                "; check socket-directory permissions and retry outside the sandbox"
            } else {
                "; inspect the path and choose another DLGT_SOCKET if needed"
            };
            return Err(anyhow!(error)).with_context(|| {
                format!(
                    "cannot inspect existing dlgt socket {}{hint}; refusing to remove it",
                    socket_path.display()
                )
            });
        }
    };
    if !metadata.file_type().is_socket() {
        bail!(
            "dlgt socket path {} is occupied by a non-socket; refusing to remove it; choose another DLGT_SOCKET or remove the path manually",
            socket_path.display()
        );
    }

    match UnixStream::connect(socket_path) {
        Ok(_) => bail!(
            "dlgt server is already running at {}",
            socket_path.display()
        ),
        Err(error) if is_stale_socket_error(&error) => match fs::remove_file(socket_path) {
            Ok(()) => Ok(()),
            Err(remove_error) if remove_error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(remove_error) => Err(anyhow!(remove_error)).with_context(|| {
                format!(
                    "stale dlgt socket {} could not be removed; refusing to bind over it; check ownership and permissions, then retry outside the sandbox if applicable",
                    socket_path.display()
                )
            }),
        },
        Err(error) => {
            let hint = if error.kind() == std::io::ErrorKind::PermissionDenied {
                "check socket-directory permissions and retry outside the sandbox"
            } else {
                "inspect the daemon owner and choose another DLGT_SOCKET if needed"
            };
            Err(anyhow!(error)).with_context(|| {
                format!(
                    "cannot connect to existing dlgt socket {}; refusing to remove it; {hint}",
                    socket_path.display()
                )
            })
        }
    }
}

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
    paths::check_socket_path(&socket_path)?;
    if let Some(parent) = socket_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let _ownership_lock = DaemonLock::acquire(&socket_path)?;
    prepare_socket_path(&socket_path)?;

    let store = Store::new();
    let reaper = Reaper::spawn()?;
    let (update_shutdown, update_wait) = mpsc::sync_channel(1);
    let daemon = Arc::new(Daemon {
        instance_id: Uuid::new_v4().simple().to_string(),
        receipts: ReceiptLedger::default(),
        store: Arc::new(Mutex::new(store)),
        submission_changed: Arc::new(Condvar::new()),
        sessions: Arc::new(RwLock::new(HashMap::new())),
        auth_processes: Arc::new(crate::provider::AuthProcessGroups::default()),
        shutting_down: AtomicBool::new(false),
        update_notice: Arc::new(RwLock::new(None)),
        update_shutdown,
        reaper,
    });
    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("failed to bind {}", socket_path.display()))?;
    fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to secure {}", socket_path.display()))?;
    let bound_socket_identity = socket_identity(&socket_path)?;
    listener
        .set_nonblocking(true)
        .context("failed to make server socket nonblocking")?;

    let update_thread = spawn_update_checker(&daemon, update_wait)?;

    let mut empty_since = Instant::now();
    while !daemon.shutting_down.load(Ordering::SeqCst) {
        let wait_for_accept = match listener.accept() {
            Ok((stream, _address)) => {
                // A successful connect is activity from a client. Reset this
                // synchronously before handing the stream to a worker so an
                // empty-daemon timeout cannot race a request that has just
                // been accepted (including a probe that closes immediately).
                empty_since = Instant::now();
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
                false
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => true,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => false,
            Err(error) => return Err(error).context("failed to accept RPC connection"),
        };
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
        // Decide the idle timeout before waiting. A client that connects
        // during this sleep is accepted on the next iteration and can reset
        // the timer instead of being discarded at the timeout boundary.
        if wait_for_accept {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    daemon.request_shutdown();
    let _ = update_thread.join();
    if let Ok(sessions) = daemon.sessions.read() {
        for runtime in sessions.values() {
            // Daemon shutdown is the ownership boundary: no provider process
            // may outlive the runtime that created it. A graceful child-only
            // stop can leave descendants alive, so terminate the whole
            // process group here.
            runtime.force_stop();
        }
    }
    // Unlink while this listener and the advisory ownership lock are still
    // held. A successor cannot acquire the lock and bind in between this
    // identity check and cleanup, and a replacement pathname is preserved.
    let socket_cleanup = remove_owned_socket(&socket_path, bound_socket_identity);
    drop(listener);
    socket_cleanup?;
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
    submission_changed: Arc<Condvar>,
    sessions: SessionMap,
    auth_processes: Arc<crate::provider::AuthProcessGroups>,
    shutting_down: AtomicBool,
    update_notice: Arc<RwLock<Option<Value>>>,
    update_shutdown: SyncSender<()>,
    reaper: Arc<Reaper>,
}

type SessionMap = Arc<RwLock<HashMap<String, Arc<AgentRuntime>>>>;

impl Daemon {
    fn request_shutdown(&self) {
        signal_shutdown(&self.shutting_down, &self.update_shutdown);
        self.auth_processes.terminate_all();
    }
}

struct ProviderReservation {
    store: Arc<Mutex<Store>>,
    provider_ref: String,
    session_id: String,
}

impl Drop for ProviderReservation {
    fn drop(&mut self) {
        if let Ok(store) = self.store.lock() {
            store.release_provider_session(&self.provider_ref, &self.session_id);
        }
    }
}

/// The provider process behind one Session and its structured channel.
enum AgentRuntime {
    Codex {
        control: Arc<CodexConnection>,
        thread_id: String,
        generation: u64,
    },
    Claude {
        process: Arc<ClaudeProcess>,
        turns: Arc<Mutex<ClaudeTurns>>,
        generation: u64,
    },
}

struct SteerDelivery<'a> {
    session: &'a SessionRecord,
    turn: &'a TurnRecord,
    runtime_generation: u64,
    attempt_id: &'a str,
    text: &'a str,
}

impl AgentRuntime {
    fn generation(&self) -> u64 {
        match self {
            Self::Codex { generation, .. } | Self::Claude { generation, .. } => *generation,
        }
    }

    fn pid(&self) -> u32 {
        match self {
            Self::Codex { control, .. } => control.pid(),
            Self::Claude { process, .. } => process.pid(),
        }
    }

    fn stop(&self) {
        match self {
            Self::Codex { control, .. } => control.stop(),
            Self::Claude { process, .. } => process.stop(),
        }
    }

    fn force_stop(&self) {
        match self {
            Self::Codex { control, .. } => control.force_stop(),
            Self::Claude { process, .. } => process.force_stop(),
        }
    }

    fn stderr_tail(&self) -> String {
        match self {
            Self::Codex { control, .. } => control.stderr_tail(),
            Self::Claude { process, .. } => process.stderr_tail(),
        }
    }
}

/// Why a provider process never became a ready Session.
struct LaunchError {
    error: anyhow::Error,
    /// The provider's stderr tail, when the process got far enough to write.
    detail: Option<String>,
}

impl From<anyhow::Error> for LaunchError {
    fn from(error: anyhow::Error) -> Self {
        Self {
            error,
            detail: None,
        }
    }
}

fn read_rpc_line(stream: &UnixStream) -> Result<Option<String>> {
    let mut line = String::new();
    let bytes_read = BufReader::new(stream.try_clone()?)
        .read_line(&mut line)
        .context("failed to read RPC request")?;
    if bytes_read == 0 {
        Ok(None)
    } else {
        Ok(Some(line))
    }
}

impl Daemon {
    fn handle_connection(&self, mut stream: UnixStream) -> Result<()> {
        let Some(line) = read_rpc_line(&stream)? else {
            // A probe may connect only to check whether the socket is live and
            // close before sending a frame. This is normal EOF, not malformed
            // JSON, and must not produce a malformed-frame/Broken pipe log.
            return Ok(());
        };
        let request = match serde_json::from_str::<Request>(&line) {
            Ok(request) => request,
            Err(error) => {
                write_response(
                    &mut stream,
                    &Response::error("unknown", "INVALID_ARGUMENT", error.to_string()),
                )?;
                return Ok(());
            }
        };

        if request.id_too_long() {
            return write_response(
                &mut stream,
                &Response::error(
                    request.short_id(),
                    "INVALID_ARGUMENT",
                    format!(
                        "request id must be at most {} bytes",
                        crate::protocol::MAX_REQUEST_ID_LEN
                    ),
                ),
            );
        }
        let mut direct_auth_hint = None;
        let response = match self.dispatch(&request.method, &request.params) {
            Ok(result) => Response::ok(request.id, result).with_info(
                self.update_notice
                    .read()
                    .ok()
                    .and_then(|notice| notice.clone()),
            ),
            Err(error) => {
                direct_auth_hint = error.downcast_ref::<AuthMismatch>().map(AuthMismatch::hint);
                if let Some(failure) = error.downcast_ref::<SessionLaunchFailure>() {
                    failure.response(request.id)
                } else {
                    Response::error(request.id, classify_error(&error), format!("{error:#}"))
                }
            }
        };
        let mut response = response;
        if let Some(error) = response.error.as_mut() {
            if error.code == "AUTH_MISMATCH"
                && let Some(hint) = direct_auth_hint
            {
                error.hint = Some(hint);
            }
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
                    "session.send" | "session.fetch" | "session.cancel" | "session.answer"
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
                        self.describe_blocked(&session, error);
                    }
                } else if selector
                    .split_once(':')
                    .is_some_and(|(agent, id)| provider_qualified(agent, id))
                {
                    error.session_id = Some(selector.to_owned());
                }
            }
        }
        write_response(&mut stream, &response)
    }

    /// Name the pending Requests and the command that answers the first one.
    fn describe_blocked(&self, session: &SessionRecord, error: &mut crate::protocol::RpcError) {
        let Ok(store) = self.lock_store() else { return };
        let pending = store
            .session_uid(&session.id)
            .map(|uid| store.pending_requests(&uid))
            .unwrap_or_default();
        drop(store);
        let Some(first) = pending.first() else { return };
        let answer = answer_action(first);
        error.action = Some(format!("dlgt answer {} {} {answer}", session.id, first.id));
        error.request_ids = Some(pending.iter().map(|request| request.id.clone()).collect());
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
            "session.create" => {
                validate_request_id(params)?;
                self.accept_once(params, |params| self.create_session(params))
            }
            "session.restart" => self.restart_session(params),
            "session.list" => {
                let include_all = params.get("all").and_then(Value::as_bool).unwrap_or(false);
                let store = self.lock_store()?;
                Ok(Value::Array(
                    store
                        .list_sessions()
                        .into_iter()
                        .filter(|session| {
                            !session.id.starts_with("internal:")
                                && (include_all || !session.state.is_terminal())
                        })
                        .map(|session| public_session_locked(&store, &session))
                        .collect(),
                ))
            }
            "session.read" => self.read_session(params),
            "session.stop" => self.stop_session(params),
            "session.send" => {
                validate_request_id(params)?;
                let flag = |name: &str| params.get(name).and_then(Value::as_bool).unwrap_or(false);
                if flag("resume") && flag("steer") {
                    fail!(
                        InvalidArgument,
                        "invalid send: --steer and --resume are mutually exclusive"
                    );
                }
                self.accept_once(params, |params| {
                    if flag("resume") {
                        self.resume_session(params)
                    } else if flag("steer") {
                        self.steer_turn(params)
                    } else {
                        self.submit_turn(params)
                    }
                })
            }
            "session.answer" => self.answer_request(params),
            "session.fetch" => self.fetch(params),
            "session.cancel" => self.cancel_session(params),
            "model.list" => self.list_models(params),
            _ => fail!(InvalidArgument, "unknown method {method:?}"),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn create_session(&self, params: &Value) -> Result<Value> {
        let title = params_string(params, "title")?;
        validate_session_metadata(params)?;
        let generated_alias = generate_alias(title);
        let alias = params
            .get("alias")
            .and_then(Value::as_str)
            .unwrap_or(&generated_alias);
        validate_alias(alias)?;
        let agent = Agent::parse(params_string(params, "harness")?)?;
        let initial_prompt = params_string(params, "prompt")?;
        if initial_prompt.is_empty() {
            fail!(InvalidArgument, "initial prompt must not be empty");
        }
        let resume_provider_id = params.get("resume_provider_id").and_then(Value::as_str);
        if let Some(id) = resume_provider_id
            && (id.is_empty()
                || id.len() > 128
                || !id.chars().all(|character| {
                    character.is_ascii_alphanumeric() || "-_.".contains(character)
                }))
        {
            fail!(
                InvalidArgument,
                "invalid Session ID: a provider conversation ID is at most 128 letters, digits, '-', '_', or '.'"
            );
        }
        let provider_ref = resume_provider_id.map(|id| format!("{}:{id}", agent.as_str()));
        let cwd = params
            .get("cwd")
            .and_then(Value::as_str)
            .map_or_else(std::env::current_dir, |value| Ok(PathBuf::from(value)))?
            .canonicalize()
            .context("session cwd does not exist")
            .code(Code::InvalidArgument)?;
        if !cwd.is_dir() {
            fail!(
                InvalidArgument,
                "session cwd is not a directory: {}",
                cwd.display()
            );
        }
        let model = params.get("model").and_then(Value::as_str);
        let effort = params.get("effort").and_then(Value::as_str);
        let auth_policy = if agent == Agent::Claude {
            params
                .get("auth_policy")
                .and_then(Value::as_str)
                .map(AuthPolicy::parse)
                .transpose()?
                .unwrap_or_default()
        } else {
            if params
                .get("auth_policy")
                .is_some_and(|value| !value.is_null())
            {
                fail!(
                    InvalidArgument,
                    "auth_policy is only valid for the Claude harness"
                );
            }
            AuthPolicy::default()
        };
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
                        .code(Code::InvalidArgument)
                })
                .collect::<Result<Vec<_>>>()
                .context("invalid harness_options")
                .code(Code::InvalidArgument)?,
            Some(_) => fail!(InvalidArgument, "harness_options must be an array"),
        };
        crate::provider::validate_harness_options(agent, &harness_options)?;
        let approvals = params
            .get("approvals")
            .and_then(Value::as_str)
            .map(ApprovalsMode::parse)
            .transpose()?
            .unwrap_or(ApprovalsMode::Auto);
        let environment = launch_environment(params)?;
        if agent == Agent::Claude {
            config::validate_auth_environment(auth_policy, environment.iter())?;
        }
        let startup_deadline = Instant::now() + startup_timeout(params);
        let mut id = generate_internal_id();
        let new_provider_id = (agent == Agent::Claude && resume_provider_id.is_none())
            .then(|| Uuid::new_v4().to_string());

        let options = LaunchOptions {
            agent,
            title,
            cwd: &cwd,
            model,
            effort,
            harness_options: &harness_options,
            new_provider_id: new_provider_id.as_deref(),
            resume_provider_id,
            environment: &environment,
            approvals,
        };
        if agent == Agent::Claude && auth_policy == AuthPolicy::Subscription {
            let spec = crate::provider::claude_command(&options)?;
            crate::provider::verify_claude_subscription_login(
                &spec,
                startup_deadline,
                &self.reaper,
                &self.auth_processes,
                &self.shutting_down,
            )?;
        }

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
                approvals,
            });
            match inserted {
                Ok(()) => {
                    if agent == Agent::Claude {
                        self.lock_store()?.set_auth_policy(&id, auth_policy);
                    }
                    break;
                }
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
                fail!(
                    SessionBusy,
                    "provider conversation is already reserved: {provider_ref}"
                );
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
        let runtime_session_id = Arc::new(RwLock::new(id.clone()));
        let runtime = self.launch_runtime(&id, &runtime_session_id, &options, startup_deadline)?;
        let provider_id = match runtime.as_ref() {
            AgentRuntime::Codex { thread_id, .. } => thread_id.clone(),
            AgentRuntime::Claude { .. } => resume_provider_id
                .or(new_provider_id.as_deref())
                .context("Claude launch had no provider session ID")?
                .to_owned(),
        };
        let canonical_id = canonical_session_id(agent.as_str(), &provider_id);
        if let Err(error) = promote_session(
            &self.store,
            &self.sessions,
            &id,
            &canonical_id,
            &runtime_session_id,
        ) {
            runtime.force_stop();
            self.lock_store()?.set_session_failed(&id);
            return Err(self.session_launch_failure(
                &id,
                &error.context("failed to publish provider Session ID"),
                None,
            ));
        }
        id = canonical_id;
        // The launch recorded its lifecycle against an internal ID that is
        // never published. Now that the Session has a public identity, record
        // the canonical timeline a caller can actually observe.
        {
            let store = self.lock_store()?;
            store.record_event(Some(&id), None, "session.created");
            store.record_event(Some(&id), None, "session.ready");
        }
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
                runtime.force_stop();
                if self
                    .resolve_session(&id)
                    .is_ok_and(|session| session.state != SessionState::Stopped)
                {
                    self.lock_store()?.set_session_failed(&id);
                }
                let error = if session_has_auth_mismatch(&self.store, &id) {
                    AuthMismatch::default().into()
                } else {
                    error
                };
                return Err(self.session_launch_failure(
                    &id,
                    &error.context("initial prompt acceptance failed"),
                    Some(runtime.stderr_tail()),
                ));
            }
        };
        if !correlation_id.is_empty() {
            response["correlation_id"] = json!(correlation_id);
        }
        Ok(response)
    }

    /// Start the provider process for a `starting` Session and bring the
    /// Session to `idle`. Every failure is recorded on the Session.
    fn launch_runtime(
        &self,
        id: &str,
        runtime_session_id: &Arc<RwLock<String>>,
        options: &LaunchOptions<'_>,
        startup_deadline: Instant,
    ) -> Result<Arc<AgentRuntime>> {
        let generation = self
            .lock_store()?
            .runtime_generation(id)
            .context("Session runtime generation is missing")?;
        let timeout = startup_deadline.saturating_duration_since(Instant::now());
        if timeout.is_zero() {
            let error = coded(
                Code::LaunchFailed,
                "launch failed: provider startup deadline expired",
            );
            {
                let store = self.lock_store()?;
                store.set_session_failed(id);
                store.record_event(Some(id), None, "session.failed");
            }
            return Err(self.session_launch_failure(id, &error, None));
        }
        let spawned = match options.agent {
            Agent::Claude => {
                self.spawn_claude_runtime(runtime_session_id, options, timeout, generation)
            }
            Agent::Codex => {
                self.spawn_codex_runtime(runtime_session_id, options, timeout, generation)
            }
        };
        let runtime = match spawned {
            Ok(runtime) => runtime,
            Err(failure) => {
                let session_id = runtime_session_id
                    .read()
                    .map_or_else(|_| id.to_owned(), |id| id.clone());
                let store = self.lock_store()?;
                if failure.error.downcast_ref::<AuthMismatch>().is_some() {
                    if store
                        .get_session(&session_id)
                        .is_some_and(|session| session.state != SessionState::Stopped)
                    {
                        store.set_session_stopped(&session_id);
                        store.record_event(Some(&session_id), None, "session.stopped");
                    }
                } else {
                    store.set_session_failed(&session_id);
                    store.record_event(Some(&session_id), None, "session.failed");
                }
                drop(store);
                return Err(self.session_launch_failure(
                    &session_id,
                    &failure.error,
                    failure.detail,
                ));
            }
        };
        self.sessions
            .write()
            .map_err(|_| anyhow!("session map lock poisoned"))?
            .insert(id.to_owned(), Arc::clone(&runtime));
        let store = self.lock_store()?;
        if !store.set_session_running(id, Some(runtime.pid())) {
            store.set_session_failed(id);
            drop(store);
            self.sessions
                .write()
                .map_err(|_| anyhow!("session map lock poisoned"))?
                .remove(id);
            runtime.force_stop();
            return Err(self.session_launch_failure(
                id,
                &anyhow!(
                    "launch failed: the provider process exited before the Session became ready"
                ),
                Some(runtime.stderr_tail()),
            ));
        }
        store.record_event(Some(id), None, "session.started");
        store.set_session_state(id, SessionState::Idle);
        store.record_event(Some(id), None, "session.ready");
        Ok(runtime)
    }

    /// Describe a launch that never became ready. The provider's stderr tail
    /// is its own explanation, so it travels with the error and stays on the
    /// failed Session for `show`.
    fn session_launch_failure(
        &self,
        session_id: &str,
        error: &anyhow::Error,
        detail: Option<String>,
    ) -> anyhow::Error {
        let agent = self.lock_store().ok().and_then(|store| {
            store
                .get_session(session_id)
                .and_then(|session| Agent::parse(&session.agent).ok())
        });
        let failure = SessionLaunchFailure::new(session_id, agent, error, detail);
        if let Ok(store) = self.lock_store() {
            store.set_launch_failure(session_id, failure.to_json());
        }
        failure.into()
    }

    /// Resume a provider conversation without mutating an existing live
    /// Session. The provider-qualified Session ID is durable; aliases first
    /// resolve to a live runtime when present.
    fn resume_session(&self, params: &Value) -> Result<Value> {
        let selector = params_string(params, "session")?;
        let prompt = params_string(params, "prompt")?;
        if prompt.is_empty() {
            fail!(InvalidArgument, "prompt must not be empty");
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
                    fail!(SessionBlocked, "session blocked on input");
                }
                if existing.state != SessionState::Idle || existing.active_turn_id.is_some() {
                    fail!(SessionBusy, "session already has an active turn");
                }
                let mut result = self.submit_turn(params)?;
                if !correlation_id.is_empty() {
                    result["correlation_id"] = json!(correlation_id);
                }
                return Ok(result);
            }
            let provider_id = provider_id_from_session(&existing)
                .context("Session has no provider conversation")
                .code(Code::SessionNotRunning)?;
            return self.launch_resumed_session(
                params,
                &existing.agent,
                provider_id,
                Some(&existing),
            );
        }

        let (agent, provider_id) = selector
            .split_once(':')
            .filter(|(agent, id)| provider_qualified(agent, id))
            .context(
                "no live Session matches selector; use codex:<id> or claude:<id> with --resume",
            )
            .code(Code::SessionNotRunning)?;
        self.launch_resumed_session(params, agent, provider_id, None)
    }

    fn launch_resumed_session(
        &self,
        params: &Value,
        agent: &str,
        provider_id: &str,
        retained: Option<&SessionRecord>,
    ) -> Result<Value> {
        let mut create = params.clone();
        let object = create
            .as_object_mut()
            .context("resume parameters must be an object")
            .code(Code::InvalidArgument)?;
        object.insert("harness".to_owned(), json!(agent));
        object.insert("resume_provider_id".to_owned(), json!(provider_id));
        object.insert(
            "title".to_owned(),
            json!(retained.map_or("resumed provider conversation", |session| &session.title)),
        );
        if let Some(retained) = retained {
            object.insert("alias".to_owned(), json!(retained.alias));
            if object.get("cwd").is_none_or(Value::is_null) {
                object.insert("cwd".to_owned(), json!(retained.cwd));
            }
            if object.get("model").is_none_or(Value::is_null) {
                object.insert("model".to_owned(), json!(retained.model));
            }
            if object.get("effort").is_none_or(Value::is_null) {
                object.insert("effort".to_owned(), json!(retained.effort));
            }
            if object.get("harness_options").is_none_or(Value::is_null) {
                object.insert(
                    "harness_options".to_owned(),
                    json!(retained.harness_options),
                );
            }
            // The approvals mode is stored with the Session and reused
            // unless this command supplies a new one.
            if object.get("approvals").is_none_or(Value::is_null) {
                object.insert(
                    "approvals".to_owned(),
                    json!(retained.approvals.requested.as_str()),
                );
            }
            if agent == Agent::Claude.as_str()
                && object.get("auth_policy").is_none_or(Value::is_null)
            {
                object.insert(
                    "auth_policy".to_owned(),
                    json!(retained.auth_policy.unwrap_or_default().as_str()),
                );
            }
        } else {
            object.remove("alias");
            if agent == Agent::Claude.as_str()
                && object.get("auth_policy").is_none_or(Value::is_null)
            {
                object.insert(
                    "auth_policy".to_owned(),
                    json!(AuthPolicy::default().as_str()),
                );
            }
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
            fail!(
                SessionUnavailable,
                "session is unavailable in state {}",
                session.state
            );
        }
        let expected_generation = self
            .lock_store()?
            .runtime_generation(&session.id)
            .context("Session runtime generation is missing")?;
        let previous_runtime = self
            .sessions
            .read()
            .map_err(|_| anyhow!("session map lock poisoned"))?
            .get(&session.id)
            .cloned();
        let provider_id = provider_id_from_session(&session)
            .context("session is unavailable because it has no provider conversation to resume")
            .code(Code::SessionUnavailable)?
            .to_owned();
        let agent = Agent::parse(&session.agent)?;
        crate::provider::validate_harness_options(agent, &session.harness_options)?;
        let cwd = PathBuf::from(&session.cwd)
            .canonicalize()
            .context("session cwd does not exist")
            .code(Code::InvalidArgument)?;
        if !cwd.is_dir() {
            fail!(
                InvalidArgument,
                "session cwd is not a directory: {}",
                cwd.display()
            );
        }
        let startup_deadline = Instant::now() + startup_timeout(params);
        let environment = launch_environment(params)?;
        let options = LaunchOptions {
            agent,
            title: &session.title,
            cwd: &cwd,
            model: session.model.as_deref(),
            effort: session.effort.as_deref(),
            harness_options: &session.harness_options,
            new_provider_id: None,
            resume_provider_id: Some(&provider_id),
            environment: &environment,
            approvals: session.approvals.requested,
        };
        if agent == Agent::Claude {
            config::validate_auth_environment(
                session.auth_policy.unwrap_or_default(),
                environment.iter(),
            )?;
            if session.auth_policy.unwrap_or_default() == AuthPolicy::Subscription {
                let spec = crate::provider::claude_command(&options)?;
                crate::provider::verify_claude_subscription_login(
                    &spec,
                    startup_deadline,
                    &self.reaper,
                    &self.auth_processes,
                    &self.shutting_down,
                )?;
            }
        }

        prepare_workspace(agent, &cwd)?;
        if !self
            .lock_store()?
            .begin_session_restart(&session.id, expected_generation)?
        {
            fail!(
                SessionUnavailable,
                "Session {} changed state or was restarted by another request; check it before retrying",
                session.id
            );
        }
        self.lock_store()?
            .record_event(Some(&session.id), None, "session.restarting");
        if let Some(runtime) = previous_runtime {
            runtime.force_stop();
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
                    // Fail rather than stay `restarting`: a failed Session
                    // can be resumed, a stuck one cannot.
                    self.lock_store()?.set_session_failed(&session.id);
                    fail!(
                        LaunchFailed,
                        "restart launch timed out while stopping the previous process"
                    );
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
            fail!(LaunchFailed, "session left restarting state before launch");
        }
        if Instant::now() >= startup_deadline {
            self.lock_store()?.set_session_failed(&session.id);
            fail!(
                LaunchFailed,
                "restart launch timed out before starting the replacement process"
            );
        }
        // The replacement process reports its own effective mode.
        self.lock_store()?
            .set_requested_approvals(&session.id, session.approvals.requested);
        let runtime_session_id = Arc::new(RwLock::new(session.id.clone()));
        self.launch_runtime(&session.id, &runtime_session_id, &options, startup_deadline)
            .map_err(|error| {
                crate::error::with_code(
                    error.context("session restart launch failed"),
                    Code::LaunchFailed,
                )
            })?;
        let current = self.resolve_session(&session.id)?;
        Ok(json!({"session": self.public_session(&current)?}))
    }

    #[allow(clippy::too_many_lines)]
    fn spawn_claude_runtime(
        &self,
        runtime_session_id: &Arc<RwLock<String>>,
        options: &LaunchOptions<'_>,
        startup_timeout: Duration,
        generation: u64,
    ) -> Result<Arc<AgentRuntime>, LaunchError> {
        let spec = claude_command(options)?;
        let turns = Arc::new(Mutex::new(ClaudeTurns::default()));
        let process_slot: Arc<OnceLock<Weak<ClaudeProcess>>> = Arc::new(OnceLock::new());

        let event_store = Arc::clone(&self.store);
        let event_sessions = Arc::clone(&self.sessions);
        let event_session_id = Arc::clone(runtime_session_id);
        let event_turns = Arc::clone(&turns);
        let event_process = Arc::clone(&process_slot);
        let event_changed = Arc::clone(&self.submission_changed);
        let event_generation = generation;
        let on_event = Arc::new(move |event: Value| {
            let Ok(session_id) = event_session_id.read().map(|id| id.clone()) else {
                return;
            };
            let actions = {
                let Ok(mut turns) = event_turns.lock() else {
                    return;
                };
                let Ok(store) = event_store.lock() else {
                    return;
                };
                if !store.runtime_generation_is_current(&session_id, event_generation) {
                    return;
                }
                claude::apply_event(&store, &session_id, event_generation, &mut turns, &event)
            };
            event_changed.notify_all();
            let actions = match actions {
                Ok(actions) => actions,
                Err(error) => {
                    eprintln!("dlgt failed to apply Claude event: {error:#}");
                    return;
                }
            };
            for action in actions {
                match action {
                    claude::Action::AuthMismatch => {
                        if let Some(process) = event_process.get().and_then(Weak::upgrade) {
                            // The turn and Session are already terminalized in
                            // `apply_event`. Send Claude's interrupt before
                            // terminating its process group.
                            let _ = process.control_nowait(&json!({"subtype": "interrupt"}));
                            process.stop();
                        }
                    }
                    claude::Action::Interrupt { turn_id } => {
                        if let Some(process) = event_process.get().and_then(Weak::upgrade) {
                            spawn_claude_cancel_control(
                                Arc::clone(&event_store),
                                Arc::clone(&event_turns),
                                process,
                                session_id.clone(),
                                turn_id,
                                event_generation,
                            );
                        }
                    }
                    claude::Action::Refuse { request_id, reason } => {
                        if let Some(process) = event_process.get().and_then(Weak::upgrade) {
                            let _ = process.refuse(&request_id, reason);
                        }
                    }
                    claude::Action::Rekey(provider_id) => {
                        let canonical = canonical_session_id("claude", &provider_id);
                        if let Err(error) = promote_session(
                            &event_store,
                            &event_sessions,
                            &session_id,
                            &canonical,
                            &event_session_id,
                        ) {
                            eprintln!("dlgt failed to rekey a rotated Claude session: {error:#}");
                        }
                    }
                }
            }
        });

        let exit_store = Arc::clone(&self.store);
        let exit_sessions = Arc::clone(&self.sessions);
        let exit_session_id = Arc::clone(runtime_session_id);
        let exit_process = Arc::clone(&process_slot);
        let exit_changed = Arc::clone(&self.submission_changed);
        let exit_generation = generation;
        let on_exit = Arc::new(move |code: Option<i32>| {
            let reason = code.map_or_else(
                || "Claude process was terminated by a signal".to_owned(),
                |code| format!("Claude process exited with code {code}"),
            );
            let detail = exit_process
                .get()
                .and_then(Weak::upgrade)
                .map(|process| process.final_stderr_tail());
            if let Ok(session_id) = exit_session_id.read().map(|id| id.clone()) {
                record_session_exit(
                    &exit_store,
                    &exit_sessions,
                    &session_id,
                    exit_generation,
                    &reason,
                    detail.as_deref(),
                );
            }
            exit_changed.notify_all();
        });

        let process = ClaudeProcess::spawn(&spec, on_event, on_exit, &self.reaper)?;
        let _ = process_slot.set(Arc::downgrade(&process));
        let session_id = runtime_session_id
            .read()
            .map(|id| id.clone())
            .unwrap_or_default();
        if session_has_auth_mismatch(&self.store, &session_id) {
            interrupt_and_stop_claude(&process);
            return Err(LaunchError {
                error: AuthMismatch::default().into(),
                detail: Some(process.final_stderr_tail()),
            });
        }
        // Claude answers `initialize` before any prompt, which proves the
        // stream-json control channel works and reports the mode it runs in.
        let ready = match process.control(&json!({"subtype": "initialize"}), startup_timeout) {
            Ok(ready) => ready,
            Err(error) => {
                let id = runtime_session_id
                    .read()
                    .map(|id| id.clone())
                    .unwrap_or_default();
                let auth_mismatch = session_has_auth_mismatch(&self.store, &id);
                if auth_mismatch {
                    interrupt_and_stop_claude(&process);
                }
                return Err(LaunchError {
                    error: if auth_mismatch {
                        AuthMismatch::default().into()
                    } else {
                        error.context(
                            "launch failed: Claude did not answer the stream-json initialize request",
                        )
                    },
                    detail: Some(process.final_stderr_tail()),
                });
            }
        };
        let id = runtime_session_id
            .read()
            .map(|id| id.clone())
            .unwrap_or_default();
        if session_has_auth_mismatch(&self.store, &id) {
            interrupt_and_stop_claude(&process);
            return Err(LaunchError {
                error: AuthMismatch::default().into(),
                detail: Some(process.final_stderr_tail()),
            });
        }
        if let Some(mode) = ready.get("current_permission_mode").and_then(Value::as_str)
            && let Ok(session_id) = runtime_session_id.read()
            && let Ok(store) = self.store.lock()
        {
            claude::apply_permission_mode(&store, &session_id, Some(mode));
        }
        Ok(Arc::new(AgentRuntime::Claude {
            process,
            turns,
            generation,
        }))
    }

    #[allow(clippy::too_many_lines)]
    fn spawn_codex_runtime(
        &self,
        runtime_session_id: &Arc<RwLock<String>>,
        options: &LaunchOptions<'_>,
        startup_timeout: Duration,
        generation: u64,
    ) -> Result<Arc<AgentRuntime>, LaunchError> {
        let event_store = Arc::clone(&self.store);
        let event_sessions = Arc::clone(&self.sessions);
        let event_session_id = Arc::clone(runtime_session_id);
        let event_changed = Arc::clone(&self.submission_changed);
        let event_generation = generation;
        let items = Mutex::new(CodexItems::default());
        let handler = Arc::new(move |message: Value| {
            let request =
                message.get("method").and_then(Value::as_str) == Some("dlgt/server/request");
            let Ok(session_id) = event_session_id.read().map(|id| id.clone()) else {
                return !request;
            };
            let mut accepted = true;
            if message.get("method").and_then(Value::as_str) == Some("dlgt/transport/closed") {
                let reason = message
                    .pointer("/params/message")
                    .and_then(Value::as_str)
                    .unwrap_or("Codex app-server connection closed");
                record_session_exit(
                    &event_store,
                    &event_sessions,
                    &session_id,
                    event_generation,
                    reason,
                    None,
                );
            } else if let Ok(mut store) = event_store.lock()
                && let Ok(mut items) = items.lock()
            {
                if !store.runtime_generation_is_current(&session_id, event_generation) {
                    return !request;
                }
                match codex_events::apply_notification(
                    &mut store,
                    &session_id,
                    &mut items,
                    &message,
                ) {
                    Ok(result) => accepted = result,
                    Err(error) => {
                        eprintln!("dlgt failed to apply Codex notification: {error:#}");
                        accepted = !request;
                    }
                }
            } else if request {
                accepted = false;
            }
            event_changed.notify_all();
            accepted
        });
        let control = CodexConnection::connect(
            handler,
            Some(options.environment),
            Some(options.cwd),
            &codex_session_args(options)?,
            startup_timeout,
            &self.reaper,
        )?;
        let detailed = |error: anyhow::Error| LaunchError {
            error,
            detail: Some(control.stderr_tail()),
        };
        let params = codex_thread_params(options);
        let (thread_id, thread) = if let Some(thread_id) = options.resume_provider_id {
            let thread = control
                .resume_thread(&params, startup_timeout)
                .map_err(|error| {
                    detailed(crate::error::with_code(
                        error.context("launch failed: Codex thread/resume"),
                        Code::LaunchFailed,
                    ))
                })?;
            (thread_id.to_owned(), thread)
        } else {
            control
                .start_thread(params, startup_timeout)
                .map_err(|error| {
                    detailed(crate::error::with_code(
                        error.context("launch failed: Codex thread/start"),
                        Code::LaunchFailed,
                    ))
                })?
        };
        if let Some(model) = thread.get("model").and_then(Value::as_str)
            && let Ok(session_id) = runtime_session_id.read()
            && let Ok(store) = self.store.lock()
        {
            store.set_resolved_model(&session_id, model);
        }
        control
            .set_thread_name(&thread_id, &provider_display_name(options.title))
            .map_err(|error| {
                detailed(crate::error::with_code(
                    error.context("launch failed: could not set the Codex thread name"),
                    Code::LaunchFailed,
                ))
            })?;
        Ok(Arc::new(AgentRuntime::Codex {
            control,
            thread_id,
            generation,
        }))
    }

    fn public_session(&self, session: &SessionRecord) -> Result<Value> {
        let store = self.lock_store()?;
        Ok(public_session_locked(&store, session))
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
            fail!(SessionUnavailable, "session is already stopped or failed");
        }
        self.lock_store()?
            .record_event(Some(&session.id), None, "session.stopping");
        if force {
            runtime.force_stop();
        } else {
            runtime.stop();
        }
        Ok(json!({"stopping": true, "force": force, "session_id": session.id}))
    }

    /// Resolve a selector to a Session whose provider process is live.
    fn live_session(&self, selector: &str) -> Result<(SessionRecord, Arc<AgentRuntime>)> {
        let session = self.resolve_session(selector).map_err(|_| {
            coded(
                Code::SessionNotRunning,
                format!(
                    "no live Session matches {selector}; retry with --resume using the same Session ID"
                ),
            )
        })?;
        let runtime = self
            .sessions
            .read()
            .map_err(|_| anyhow!("session map lock poisoned"))?
            .get(&session.id)
            .cloned();
        match runtime {
            Some(runtime)
                if !matches!(
                    session.state,
                    SessionState::Stopped
                        | SessionState::Failed
                        | SessionState::Starting
                        | SessionState::Stopping
                ) =>
            {
                Ok((session, runtime))
            }
            _ => fail!(
                SessionNotRunning,
                "Session {} is not running; retry with --resume using the same Session ID",
                session.id
            ),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn submit_turn(&self, params: &Value) -> Result<Value> {
        let (session, runtime) = self.live_session(params_string(params, "session")?)?;
        if session_is_auth_mismatch(&session) {
            return Err(AuthMismatch::default().into());
        }
        if session.state == SessionState::Blocked {
            fail!(SessionBlocked, "session blocked on input");
        }
        if matches!(session.state, SessionState::Busy | SessionState::Quiescing)
            || session.active_turn_id.is_some()
        {
            fail!(SessionBusy, "session already has an active turn");
        }
        if session.state != SessionState::Idle {
            fail!(
                SessionUnavailable,
                "session is unavailable in state {}",
                session.state
            );
        }
        let prompt = params_string(params, "prompt")?;
        if prompt.is_empty() {
            fail!(InvalidArgument, "prompt must not be empty");
        }
        let turn_id = format!("turn_{}", Uuid::new_v4().simple());
        let claude_uuid = matches!(runtime.as_ref(), AgentRuntime::Claude { .. })
            .then(|| Uuid::new_v4().to_string());
        test_pause("DLGT_TEST_PAUSE_BEFORE_SUBMIT_RESERVATION_MS");
        // A restart may have replaced the runtime captured above. Reserve the
        // execution only for the generation this prompt will be written to.
        let owns_generation =
            |store: &Store| store.runtime_generation(&session.id) == Some(runtime.generation());
        let (turn, acceptance_cursor) = match runtime.as_ref() {
            AgentRuntime::Codex { .. } => {
                let mut store = self.lock_store()?;
                if !owns_generation(&store) {
                    fail!(
                        SessionUnavailable,
                        "Session {} was restarted before the prompt was accepted; send it again",
                        session.id
                    );
                }
                let acceptance_cursor = Self::acceptance_cursor(&store, &session.id)?;
                let turn = store.insert_turn(&turn_id, &session.id, prompt)?;
                store.set_session_state(&session.id, SessionState::Busy);
                store.record_event(Some(&session.id), Some(&turn_id), "turn.submitted");
                (turn, acceptance_cursor)
            }
            AgentRuntime::Claude { turns, .. } => {
                // Keep the established turns-then-store lock order. The unread
                // prompt reservation and active execution become visible as a
                // single operation before the transport write can begin.
                let mut turns = turns
                    .lock()
                    .map_err(|_| anyhow!("Claude turn lock poisoned"))?;
                let mut store = self.lock_store()?;
                if !owns_generation(&store) {
                    fail!(
                        SessionUnavailable,
                        "Session {} was restarted before the prompt was accepted; send it again",
                        session.id
                    );
                }
                let acceptance_cursor = Self::acceptance_cursor(&store, &session.id)?;
                let turn = store.insert_turn(&turn_id, &session.id, prompt)?;
                store.set_session_state(&session.id, SessionState::Busy);
                store.record_event(Some(&session.id), Some(&turn_id), "turn.submitted");
                test_pause("DLGT_TEST_PAUSE_BEFORE_CLAUDE_RESERVATION_MS");
                turns.begin(claude_uuid.as_deref().unwrap_or_default());
                (turn, acceptance_cursor)
            }
        };
        // Build a receipt-safe snapshot before provider delivery. After the
        // delivery boundary, internal observation failures must degrade to a
        // pending receipt rather than erase idempotency and invite a retry.
        let fallback_public_session = self.public_session(&session)?;
        let mut synchronously_confirmed = false;
        match runtime.as_ref() {
            AgentRuntime::Codex {
                control, thread_id, ..
            } => {
                let started = control
                    .start_turn(thread_id, prompt)
                    .and_then(|turn| {
                        control.watch_turn(thread_id, &turn)?;
                        Ok(turn)
                    })
                    .code(Code::SessionUnavailable);
                match started {
                    Ok(provider_turn_id) => {
                        synchronously_confirmed = true;
                        if let Ok(store) = self.lock_store()
                            && store.mark_turn_started(&turn_id, Some(&provider_turn_id))
                        {
                            store.record_event(Some(&session.id), Some(&turn_id), "turn.started");
                        }
                    }
                    Err(error) => {
                        if self.runtime_is_current(&session.id, &runtime) {
                            let store = self.lock_store()?;
                            if !store.execution_is_current(
                                &session.id,
                                runtime.generation(),
                                &turn.id,
                                turn.execution_seq,
                            ) {
                                return Err(error);
                            }
                            let message = json!({"code": "provider_error", "message": sanitize_message(&error.to_string())});
                            if store.finish_turn_if_matching(
                                &turn_id,
                                None,
                                TurnState::Failed,
                                None,
                                Some(&message),
                            )? {
                                store.set_session_failed(&session.id);
                                store.record_event(
                                    Some(&session.id),
                                    Some(&turn_id),
                                    "turn.failed",
                                );
                                store.record_event(Some(&session.id), None, "session.failed");
                                drop(store);
                                runtime.force_stop();
                            }
                        }
                        return Err(error);
                    }
                }
            }
            AgentRuntime::Claude { process, .. } => {
                let uuid = claude_uuid.as_deref().unwrap_or_default();
                if let Err(error) = process.send_user(uuid, prompt) {
                    if self.runtime_is_current(&session.id, &runtime) {
                        let store = self.lock_store()?;
                        if !store.execution_is_current(
                            &session.id,
                            runtime.generation(),
                            &turn.id,
                            turn.execution_seq,
                        ) {
                            return Err(crate::error::with_code(error, Code::SessionUnavailable));
                        }
                        let message = json!({"code": "provider_error", "message": sanitize_message(&error.to_string())});
                        if store.finish_turn_if_matching(
                            &turn_id,
                            None,
                            TurnState::Failed,
                            None,
                            Some(&message),
                        )? {
                            store.set_session_failed(&session.id);
                            store.record_event(Some(&session.id), Some(&turn_id), "turn.failed");
                            store.record_event(Some(&session.id), None, "session.failed");
                        }
                    }
                    return Err(crate::error::with_code(error, Code::SessionUnavailable));
                }
            }
        }
        let confirmed = synchronously_confirmed
            || self
                .wait_for_submission_confirmation(&turn_id)
                .unwrap_or(false);
        if !confirmed
            && self
                .lock_store()?
                .get_turn(&turn_id)
                .and_then(|turn| turn.error)
                .is_some_and(|error| error["code"] == "AUTH_MISMATCH")
        {
            return Err(AuthMismatch::default().into());
        }
        let submission = if confirmed { "confirmed" } else { "pending" };
        let public_session = self
            .resolve_session(&session.id)
            .and_then(|current| self.public_session(&current))
            .unwrap_or(fallback_public_session);
        let mut result = json!({
            "session": public_session,
            "execution_seq": turn.execution_seq,
            "cursor": acceptance_cursor,
            "submission": submission,
        });
        if submission == "pending" {
            result["hint"] =
                json!("provider confirmation has not arrived; do not resend with a new request_id");
            result["action"] = json!(format!("dlgt fetch {} --wait 25s", session.id));
        }
        if let Some(correlation_id) = params.get("correlation_id").and_then(Value::as_str)
            && !correlation_id.is_empty()
        {
            result["correlation_id"] = json!(correlation_id);
        }
        Ok(result)
    }

    /// Add a message to the execution that is already running. Never creates
    /// an execution, never queues work for later, and never interrupts.
    fn steer_turn(&self, params: &Value) -> Result<Value> {
        let (session, runtime) = self.live_session(params_string(params, "session")?)?;
        let text = params_string(params, "prompt")?;
        if text.is_empty() {
            fail!(InvalidArgument, "prompt must not be empty");
        }
        match session.state {
            SessionState::Busy => {}
            SessionState::Blocked => fail!(SessionBlocked, "session blocked on input"),
            SessionState::Idle => fail!(
                NoActiveExecution,
                "Session {} has no running execution to steer; send the prompt without --steer",
                session.id
            ),
            other => fail!(
                SessionUnavailable,
                "session is unavailable in state {}",
                other.public_name()
            ),
        }
        let turn = session
            .active_turn_id
            .as_deref()
            .and_then(|id| self.lock_store().ok()?.get_turn(id))
            .with_context(|| format!("Session {} has no running execution to steer", session.id))
            .code(Code::NoActiveExecution)?;
        let runtime_generation = runtime.generation();
        let attempt_id = Uuid::new_v4().to_string();
        let delivery = SteerDelivery {
            session: &session,
            turn: &turn,
            runtime_generation,
            attempt_id: &attempt_id,
            text,
        };
        let cursor = match runtime.as_ref() {
            AgentRuntime::Codex {
                control, thread_id, ..
            } => self.steer_codex(&delivery, control, thread_id)?,
            AgentRuntime::Claude {
                process,
                turns: turn_state,
                ..
            } => self.steer_claude(&delivery, process, turn_state)?,
        };
        let current = self.resolve_session(&session.id)?;
        Ok(json!({
            "session": self.public_session(&current)?,
            "execution_seq": turn.execution_seq,
            "steer": "accepted",
            "cursor": cursor,
        }))
    }

    fn steer_codex(
        &self,
        delivery: &SteerDelivery<'_>,
        control: &CodexConnection,
        thread_id: &str,
    ) -> Result<String> {
        let provider_turn_id = delivery
            .turn
            .provider_turn_id
            .as_deref()
            .with_context(|| {
                format!(
                    "session is unavailable: Codex has not acknowledged execution {} yet",
                    delivery.turn.execution_seq
                )
            })
            .code(Code::SessionUnavailable)?;
        let cursor = {
            let store = self.lock_store()?;
            if !store.execution_is_current(
                &delivery.session.id,
                delivery.runtime_generation,
                &delivery.turn.id,
                delivery.turn.execution_seq,
            ) {
                fail!(
                    NoActiveExecution,
                    "execution on Session {} ended before steer delivery",
                    delivery.session.id
                );
            }
            let cursor = Self::acceptance_cursor(&store, &delivery.session.id)?;
            if !store.begin_steer_attempt(
                &delivery.session.id,
                delivery.runtime_generation,
                &delivery.turn.id,
                delivery.turn.execution_seq,
                delivery.attempt_id,
                delivery.text,
            ) {
                fail!(
                    NoActiveExecution,
                    "execution on Session {} ended before steer delivery",
                    delivery.session.id
                );
            }
            cursor
        };
        let delivered = control.steer_turn(thread_id, provider_turn_id, delivery.text);
        self.lock_store()?.finish_steer_attempt(
            &delivery.session.id,
            delivery.runtime_generation,
            delivery.attempt_id,
            delivery.text,
            delivered.is_ok(),
        );
        delivered.code(Code::SessionUnavailable)?;
        Ok(cursor)
    }

    fn steer_claude(
        &self,
        delivery: &SteerDelivery<'_>,
        process: &ClaudeProcess,
        turn_state: &Arc<Mutex<ClaudeTurns>>,
    ) -> Result<String> {
        let mut turns = turn_state
            .lock()
            .map_err(|_| anyhow!("Claude turn lock poisoned"))?;
        let store = self.lock_store()?;
        if !store.execution_is_current(
            &delivery.session.id,
            delivery.runtime_generation,
            &delivery.turn.id,
            delivery.turn.execution_seq,
        ) {
            fail!(
                NoActiveExecution,
                "execution on Session {} ended before steer delivery",
                delivery.session.id
            );
        }
        let cursor = Self::acceptance_cursor(&store, &delivery.session.id)?;
        if !store.begin_steer_attempt(
            &delivery.session.id,
            delivery.runtime_generation,
            &delivery.turn.id,
            delivery.turn.execution_seq,
            delivery.attempt_id,
            delivery.text,
        ) {
            fail!(
                NoActiveExecution,
                "execution on Session {} ended before steer delivery",
                delivery.session.id
            );
        }
        turns.add_steer(delivery.attempt_id, delivery.text);
        drop(store);
        drop(turns);
        if let Err(error) = process.send_user(delivery.attempt_id, delivery.text) {
            let mut turns = turn_state
                .lock()
                .map_err(|_| anyhow!("Claude turn lock poisoned"))?;
            self.lock_store()?.finish_steer_attempt(
                &delivery.session.id,
                delivery.runtime_generation,
                delivery.attempt_id,
                delivery.text,
                false,
            );
            turns.discard_without_record(delivery.attempt_id);
            return Err(crate::error::with_code(error, Code::SessionUnavailable));
        }
        Ok(cursor)
    }

    /// Answer a pending Request. Idempotent by Request: the same answer
    /// replays the original receipt, a different one is rejected.
    fn answer_request(&self, params: &Value) -> Result<Value> {
        let (session, runtime) = self.live_session(params_string(params, "session")?)?;
        let request_id = params_string(params, "request")?;
        let answer = Answer::parse(params)?;
        let record = self
            .lock_store()?
            .request(&session.id, request_id)
            .with_context(|| format!("Session {} has no Request {request_id}", session.id))
            .code(Code::RequestNotFound)?;
        let (outcome, payload) = record.reply(&answer)?;
        let codex_request_id = if matches!(runtime.as_ref(), AgentRuntime::Codex { .. }) {
            Some(
                serde_json::from_str::<Value>(&record.draft.provider_key)
                    .context("retained Codex request ID is not valid JSON")?,
            )
        } else {
            None
        };
        let (claim, waited) = self.claim_request_answer(&session.id, &record, &answer)?;
        if let AnswerClaim::Replay(resolution) = claim {
            // Build the receipt while a waiter still pins the Request against
            // retention trimming, then release it.
            let receipt = self.answer_receipt(&session.id, &record, resolution.outcome, None);
            if let Some(attempt_id) = waited {
                self.lock_store()?
                    .release_answer_waiter(&session.id, &record.id, attempt_id);
            }
            return receipt;
        }
        let AnswerClaim::Started(attempt_id) = claim else {
            fail!(RequestFailed, "answer attempt could not be claimed");
        };
        let cursor = match self
            .lock_store()
            .and_then(|store| Self::acceptance_cursor(&store, &session.id))
        {
            Ok(cursor) => cursor,
            Err(error) => {
                if self.runtime_is_current(&session.id, &runtime)
                    && let Some(turn_id) = session.active_turn_id.as_deref()
                    && let Ok(store) = self.lock_store()
                    && store.answer_attempt_owns_execution(
                        &session.id,
                        runtime.generation(),
                        &record.id,
                        attempt_id,
                        turn_id,
                    )
                {
                    store.retry_request_answer(&session.id, &record.id, attempt_id, &answer);
                }
                self.submission_changed.notify_all();
                return Err(error);
            }
        };
        let receipt_extra = match self.deliver_answer(
            &session,
            runtime.as_ref(),
            &record,
            &answer,
            &payload,
            codex_request_id.as_ref(),
        ) {
            Ok(receipt_extra) => receipt_extra,
            Err(failure) => {
                self.handle_answer_delivery_failure(
                    &session, &runtime, &record, &answer, attempt_id, &failure,
                );
                fail!(
                    RequestDeliveryFailed,
                    "delivery failed for Request {}",
                    record.id
                );
            }
        };
        self.finish_answer(
            &session,
            &record,
            &answer,
            attempt_id,
            outcome,
            &receipt_extra,
        )?;
        self.submission_changed.notify_all();
        self.answer_receipt(&session.id, &record, outcome, Some(cursor))
    }

    fn claim_request_answer(
        &self,
        session_id: &str,
        record: &RequestRecord,
        answer: &Answer,
    ) -> Result<(AnswerClaim, Option<u64>)> {
        let mut store = self.lock_store()?;
        let claim = store
            .begin_request_answer(session_id, &record.id, answer)
            .with_context(|| format!("Session {session_id} has no Request {}", record.id))
            .code(Code::RequestNotFound)?;
        match claim {
            AnswerClaim::Started(_) | AnswerClaim::Replay(_) => Ok((claim, None)),
            AnswerClaim::Conflict => fail!(
                RequestResolved,
                "Request {} was answered with a different decision",
                record.id
            ),
            AnswerClaim::Failed => fail!(
                RequestFailed,
                "Request {} failed or its execution ended before an answer was delivered",
                record.id
            ),
            AnswerClaim::Waiting(attempt_id) => loop {
                match store.answer_attempt_status(session_id, &record.id, attempt_id) {
                    Some((crate::requests::AnswerAttemptState::Sending, _)) => {
                        let (guard, timeout) = self
                            .submission_changed
                            .wait_timeout(store, Duration::from_mins(1))
                            .map_err(|_| anyhow!("session store lock poisoned"))?;
                        store = guard;
                        if timeout.timed_out()
                            && store
                                .answer_attempt_status(session_id, &record.id, attempt_id)
                                .is_some_and(|(state, _)| {
                                    state == crate::requests::AnswerAttemptState::Sending
                                })
                        {
                            store.release_answer_waiter(session_id, &record.id, attempt_id);
                            fail!(
                                RequestBusy,
                                "answer delivery is still in progress for {}",
                                record.id
                            );
                        }
                    }
                    Some((crate::requests::AnswerAttemptState::Delivered, Some(resolution))) => {
                        return Ok((AnswerClaim::Replay(resolution), Some(attempt_id)));
                    }
                    Some((
                        crate::requests::AnswerAttemptState::Retryable
                        | crate::requests::AnswerAttemptState::Failed,
                        _,
                    )) => {
                        store.release_answer_waiter(session_id, &record.id, attempt_id);
                        fail!(
                            RequestDeliveryFailed,
                            "answer attempt failed for Request {}",
                            record.id
                        );
                    }
                    _ => {
                        store.release_answer_waiter(session_id, &record.id, attempt_id);
                        fail!(
                            RequestFailed,
                            "answer attempt disappeared for {}",
                            record.id
                        );
                    }
                }
            },
        }
    }

    fn answer_receipt(
        &self,
        session_id: &str,
        record: &RequestRecord,
        outcome: Outcome,
        cursor: Option<String>,
    ) -> Result<Value> {
        let current = self.resolve_session(session_id)?;
        let latest = self
            .lock_store()?
            .request(session_id, &record.id)
            .unwrap_or(record.clone());
        let mut value = json!({
            "session": self.public_session(&current)?,
            "request": {"id": record.id, "resolution": outcome.as_str()},
        });
        if let Some(extra) = latest.answer_receipt
            && let Some(object) = extra.as_object()
        {
            for (key, field) in object {
                value["request"][key] = field.clone();
            }
        }
        if let Some(cursor) = cursor {
            value["cursor"] = json!(cursor);
        } else {
            value["replayed"] = json!(true);
        }
        Ok(value)
    }

    fn deliver_answer(
        &self,
        session: &SessionRecord,
        runtime: &AgentRuntime,
        record: &RequestRecord,
        answer: &Answer,
        payload: &Value,
        codex_request_id: Option<&Value>,
    ) -> std::result::Result<serde_json::Map<String, Value>, crate::provider::DeliveryFailure> {
        let mut receipt_extra = serde_json::Map::new();
        test_pause("DLGT_TEST_PAUSE_BEFORE_ANSWER_DELIVERY_MS");
        let provider_delivery = match runtime {
            AgentRuntime::Claude { process, .. } => {
                process.respond_delivery(&record.draft.provider_key, payload)
            }
            AgentRuntime::Codex {
                control, thread_id, ..
            } => {
                let Some(id) = codex_request_id else {
                    return Err(crate::provider::DeliveryFailure::not_handed(
                        "Codex request ID is missing",
                    ));
                };
                let reason_delivery = if let Answer::Deny {
                    message: Some(message),
                } = &answer
                    && matches!(
                        &record.draft.provider,
                        ProviderRequest::CodexCommand
                            | ProviderRequest::CodexFileChange
                            | ProviderRequest::CodexPermissions { .. }
                    ) {
                    let provider_turn_id = session
                        .active_turn_id
                        .as_deref()
                        .and_then(|id| self.lock_store().ok()?.get_turn(id))
                        .and_then(|turn| turn.provider_turn_id);
                    Some(match provider_turn_id {
                        Some(turn_id) => control
                            .steer_turn(thread_id, &turn_id, message)
                            .map_err(|error| sanitize_message(&error.to_string())),
                        None => Err("Codex has not acknowledged the active turn".to_owned()),
                    })
                } else {
                    None
                };
                if let Some(reason_delivery) = reason_delivery {
                    receipt_extra.insert(
                        "reason_delivered".to_owned(),
                        json!(reason_delivery.is_ok()),
                    );
                    if let Err(error) = reason_delivery {
                        receipt_extra.insert("reason_error".to_owned(), json!(error));
                    }
                }
                control.respond_delivery(id, payload)
            }
        };
        provider_delivery?;
        Ok(receipt_extra)
    }

    fn handle_answer_delivery_failure(
        &self,
        session: &SessionRecord,
        runtime: &Arc<AgentRuntime>,
        record: &RequestRecord,
        answer: &Answer,
        attempt_id: u64,
        failure: &crate::provider::DeliveryFailure,
    ) {
        let runtime_is_current = self.sessions.read().is_ok_and(|sessions| {
            sessions
                .get(&session.id)
                .is_some_and(|current| Arc::ptr_eq(current, runtime))
        });
        if let Ok(store) = self.lock_store() {
            let owns_execution = runtime_is_current
                && session.active_turn_id.as_deref().is_some_and(|turn_id| {
                    store.answer_attempt_owns_execution(
                        &session.id,
                        runtime.generation(),
                        &record.id,
                        attempt_id,
                        turn_id,
                    )
                });
            if !owns_execution {
                // A replaced provider may report its write failure after the
                // Request was withdrawn for restart. Settle that old attempt
                // without touching the replacement Session or execution.
                store.fail_withdrawn_request_answer(&session.id, &record.id, attempt_id, answer);
            } else if let Some(turn_id) = session.active_turn_id.as_deref() {
                match failure.certainty {
                    crate::provider::DeliveryCertainty::NotHandedToTransport => {
                        store.retry_request_answer(&session.id, &record.id, attempt_id, answer);
                    }
                    crate::provider::DeliveryCertainty::Ambiguous => {
                        if store.fail_request_answer(&session.id, &record.id, attempt_id, answer) {
                            let structured = json!({
                                "code": "REQUEST_DELIVERY_FAILED",
                                "message": sanitize_message(&failure.to_string()),
                            });
                            if store
                                .finish_turn_if_matching(
                                    turn_id,
                                    None,
                                    TurnState::Failed,
                                    None,
                                    Some(&structured),
                                )
                                .unwrap_or(false)
                            {
                                store.set_session_failed(&session.id);
                                store.record_event(Some(&session.id), Some(turn_id), "turn.failed");
                                store.record_event(Some(&session.id), None, "session.failed");
                            }
                            runtime.force_stop();
                        }
                    }
                }
            }
        }
        self.submission_changed.notify_all();
    }

    fn finish_answer(
        &self,
        session: &SessionRecord,
        record: &RequestRecord,
        answer: &Answer,
        attempt_id: u64,
        outcome: Outcome,
        receipt_extra: &serde_json::Map<String, Value>,
    ) -> Result<()> {
        let extra = Value::Object(receipt_extra.clone());
        test_pause("DLGT_TEST_PAUSE_BEFORE_ANSWER_RECEIPT_MS");
        {
            let store = self.lock_store()?;
            if !store.finish_request_answer(
                &session.id,
                &record.id,
                attempt_id,
                answer,
                outcome,
                extra,
            ) {
                fail!(
                    RequestResolved,
                    "Request {} was withdrawn before delivery completed",
                    record.id
                );
            }
            if record.draft.kind == RequestKind::Approval {
                store.append_execution_message(
                    &session.id,
                    record.execution_seq,
                    MessageBody::approval(
                        if outcome == Outcome::Allowed {
                            Decision::Approved
                        } else {
                            Decision::Denied
                        },
                        Decider::Caller,
                        None,
                        Some(&record.id),
                        &record.draft.tool,
                        &record.draft.summary,
                    ),
                );
            }
        }
        Ok(())
    }

    fn wait_for_submission_confirmation(&self, turn_id: &str) -> Result<bool> {
        let deadline = Instant::now() + SUBMISSION_CONFIRM_TIMEOUT;
        let mut store = self.lock_store()?;
        loop {
            let turn = store
                .get_turn(turn_id)
                .context("accepted turn disappeared before provider confirmation")?;
            if turn.started_at_ms.is_some() {
                return Ok(true);
            }
            if turn.state.is_terminal() {
                return Ok(false);
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(false);
            }
            let (next, timeout) = self
                .submission_changed
                .wait_timeout(store, deadline.saturating_duration_since(now))
                .map_err(|_| anyhow!("session store lock poisoned"))?;
            store = next;
            if timeout.timed_out() {
                return Ok(store
                    .get_turn(turn_id)
                    .is_some_and(|turn| turn.started_at_ms.is_some()));
            }
        }
    }

    fn read_session(&self, params: &Value) -> Result<Value> {
        let session = self.resolve_session(params_string(params, "session")?)?;
        let store = self.lock_store()?;
        let latest = store.latest_turn(&session.id);
        // A running execution must not hide the answer to the previous one.
        let result = store.latest_terminal_turn(&session.id);
        let launch_failure = (session.state == SessionState::Failed)
            .then(|| store.launch_failure(&session.id))
            .flatten();
        let requests = store
            .session_uid(&session.id)
            .map(|uid| store.pending_requests(&uid))
            .unwrap_or_default();
        let public = public_session_locked(&store, &session);
        drop(store);
        let mut value = json!({
            "session": public,
            "requests": requests
                .iter()
                .take(PUBLISHED_REQUEST_LIMIT)
                .map(crate::requests::RequestRecord::public)
                .collect::<Vec<_>>(),
            "result": result.as_ref().map(public_result),
            "execution_seq": latest.as_ref().map(|turn| turn.execution_seq),
        });
        if let Some(provider_id) = provider_id_from_session(&session)
            && let Ok(agent) = Agent::parse(&session.agent)
        {
            value["resume_command"] = json!(agent.resume_command(provider_id));
        }
        if let Some(launch_failure) = launch_failure {
            value["launch_failure"] = launch_failure;
        }
        Ok(value)
    }

    fn cancel_session(&self, params: &Value) -> Result<Value> {
        let session = self.resolve_session(params_string(params, "session")?)?;
        let Some(turn_id) = session.active_turn_id.clone() else {
            return Ok(json!({
                "session_id": session.id,
                "canceled": false,
                "reason": "NO_ACTIVE_WORK",
            }));
        };
        let captured = self.resolve_turn(&turn_id)?;
        let timeout_ms = params
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(30_000);
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        if captured.state.is_active()
            && let Err(error) = self.interrupt_turn(&session, &turn_id)
            && !self.resolve_turn(&turn_id)?.state.is_terminal()
        {
            return Err(error);
        }
        loop {
            let current = self.resolve_session(&session.id)?;
            let (turn, settled) = {
                let store = self.lock_store()?;
                (
                    store.get_turn(&turn_id).context(
                        "captured execution disappeared while cancellation was in progress",
                    )?,
                    store.cancel_execution_complete(&turn_id),
                )
            };
            if settled {
                return Ok(json!({
                    "session": self.public_session(&current)?,
                    "canceled": turn.state == TurnState::Canceled,
                    "result": public_result(&turn),
                }));
            }
            if Instant::now() >= deadline {
                fail!(
                    CancelTimeout,
                    "cancellation continues for execution {turn_id}"
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Record the cancel intent, close pending Requests, and ask the provider
    /// to interrupt. The Session returns to idle only when the provider
    /// reports the interrupted turn's result.
    fn interrupt_turn(&self, session: &SessionRecord, turn_id: &str) -> Result<()> {
        let runtime = self.runtime(&session.id)?;
        let turn = self.resolve_turn(turn_id)?;
        let runtime_generation = runtime.generation();
        match runtime.as_ref() {
            AgentRuntime::Codex {
                control, thread_id, ..
            } => self.interrupt_codex_turn(
                session,
                turn_id,
                &turn,
                Arc::clone(control),
                thread_id.clone(),
                runtime_generation,
            ),
            AgentRuntime::Claude { process, turns, .. } => self.interrupt_claude_turn(
                session,
                turn_id,
                &turn,
                Arc::clone(process),
                Arc::clone(turns),
                runtime_generation,
            ),
        }
    }

    fn interrupt_codex_turn(
        &self,
        session: &SessionRecord,
        turn_id: &str,
        turn: &TurnRecord,
        control: Arc<CodexConnection>,
        thread_id: String,
        runtime_generation: u64,
    ) -> Result<()> {
        {
            let mut store = self.lock_store()?;
            if turn.state == TurnState::Canceled && session.state == SessionState::Quiescing {
                return Ok(());
            }
            if !store.execution_can_cancel(
                &session.id,
                runtime_generation,
                turn_id,
                turn.execution_seq,
            ) || !store.cancel_turn_with_controls(turn_id, runtime_generation, 1)?
            {
                fail!(
                    NoActiveExecution,
                    "turn is already terminal or no longer active"
                );
            }
            store.record_event(Some(&session.id), Some(turn_id), "turn.canceled");
        }
        let store = Arc::clone(&self.store);
        let session_id = session.id.clone();
        let turn_id = turn_id.to_owned();
        std::thread::Builder::new()
            .name("dlgt-codex-cancel".to_owned())
            .spawn(move || {
                loop {
                    let current = store.lock().ok().and_then(|store| {
                        let session = store.get_session(&session_id)?;
                        let turn = store.get_turn(&turn_id)?;
                        Some((
                            session,
                            turn,
                            store.runtime_generation_is_current(&session_id, runtime_generation),
                        ))
                    });
                    let Some((session, turn, owns_generation)) = current else {
                        return;
                    };
                    if session.state != SessionState::Quiescing
                        || session.active_turn_id.as_deref() != Some(turn_id.as_str())
                        || !owns_generation
                    {
                        return;
                    }
                    if let Some(provider_turn_id) = turn.provider_turn_id {
                        if let Err(error) = control.interrupt_turn(&thread_id, &provider_turn_id) {
                            fail_cancelled_turn(
                                &store,
                                &session_id,
                                &turn_id,
                                runtime_generation,
                                &error.to_string(),
                            );
                            control.force_stop();
                            return;
                        }
                        if let Ok(store) = store.lock()
                            && store.finish_cancel_control(&turn_id)
                        {
                            let _ = store.settle_canceled_turn(&turn_id, Some(&provider_turn_id));
                        }
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
            })
            .context("failed to start Codex cancellation worker")?;
        Ok(())
    }

    fn interrupt_claude_turn(
        &self,
        session: &SessionRecord,
        turn_id: &str,
        turn: &TurnRecord,
        process: Arc<ClaudeProcess>,
        turns: Arc<Mutex<ClaudeTurns>>,
        runtime_generation: u64,
    ) -> Result<()> {
        // The unread snapshot and the cancel record are one step under the
        // turns-then-store locks, so a steer is either in the snapshot or
        // refused because the execution is already canceling.
        let unread = {
            let turn_state = turns
                .lock()
                .map_err(|_| anyhow!("Claude turn lock poisoned"))?;
            let unread = turn_state.unread();
            test_pause("DLGT_TEST_PAUSE_AFTER_CANCEL_SNAPSHOT_MS");
            let mut store = self.lock_store()?;
            if turn.state == TurnState::Canceled && session.state == SessionState::Quiescing {
                return Ok(());
            }
            if !store.execution_can_cancel(
                &session.id,
                runtime_generation,
                turn_id,
                turn.execution_seq,
            ) || !store.cancel_turn_with_controls(
                turn_id,
                runtime_generation,
                unread.len() + 1,
            )? {
                fail!(
                    NoActiveExecution,
                    "turn is already terminal or no longer active"
                );
            }
            store.record_event(Some(&session.id), Some(turn_id), "turn.canceled");
            unread
        };
        let store = Arc::clone(&self.store);
        let session_id = session.id.clone();
        let turn_id = turn_id.to_owned();
        std::thread::Builder::new()
            .name("dlgt-claude-cancel".to_owned())
            .spawn(move || {
                for message in unread {
                    if !cancel_target_is_current(&store, &session_id, &turn_id, runtime_generation)
                    {
                        return;
                    }
                    let request = json!({
                        "subtype": "cancel_async_message",
                        "message_uuid": message.uuid,
                    });
                    let response = process.control(&request, CLAUDE_CONTROL_TIMEOUT);
                    let Ok(response) = response else {
                        fail_cancelled_turn(
                            &store,
                            &session_id,
                            &turn_id,
                            runtime_generation,
                            "Claude rejected or timed out a cancel control",
                        );
                        process.force_stop();
                        return;
                    };
                    if response.get("cancelled").and_then(Value::as_bool) == Some(true)
                        && let (Ok(mut turns), Ok(store)) = (turns.lock(), store.lock())
                    {
                        turns.discard(&store, &session_id, runtime_generation, &message.uuid);
                    }
                    if let Ok(store) = store.lock() {
                        store.finish_cancel_control(&turn_id);
                    }
                }
                if !cancel_target_is_current(&store, &session_id, &turn_id, runtime_generation) {
                    return;
                }
                if process
                    .control(&json!({"subtype": "interrupt"}), CLAUDE_CONTROL_TIMEOUT)
                    .is_err()
                {
                    fail_cancelled_turn(
                        &store,
                        &session_id,
                        &turn_id,
                        runtime_generation,
                        "Claude rejected or timed out the interrupt control",
                    );
                    process.force_stop();
                    return;
                }
                if let Ok(store) = store.lock() {
                    store.finish_cancel_control(&turn_id);
                }
                if cancel_target_is_current(&store, &session_id, &turn_id, runtime_generation)
                    && let (Ok(mut turns), Ok(store)) = (turns.lock(), store.lock())
                {
                    let _ = turns.settle(&store, &session_id);
                }
            })
            .context("failed to start Claude cancellation worker")?;
        Ok(())
    }

    /// Position an observation cursor immediately before an acceptance.
    fn acceptance_cursor(store: &Store, session_id: &str) -> Result<String> {
        let uid = store
            .session_uid(session_id)
            .with_context(|| format!("session not found: {session_id}"))
            .code(Code::NotFound)?;
        let mut cursor = cursor::Cursor::new();
        cursor.e = store.latest_event_seq();
        cursor.set_session(cursor::SessionCursor {
            m: store.message_head(&uid),
            x: store.latest_result_seq(&uid),
            px: None,
            po: 0,
        });
        let position = store.reserve_cursor(&uid);
        store.store_cursor(&uid, position, cursor);
        Ok(position.to_string())
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
        let receipt = self
            .receipts
            .accept_once(request_id, request_digest(params), || run(params))?;
        self.refresh_submission_receipt(receipt)
    }

    /// A replayed pending receipt follows the live turn and becomes confirmed
    /// once the provider acknowledgement arrives. The execution itself is
    /// never submitted twice.
    fn refresh_submission_receipt(&self, mut receipt: Value) -> Result<Value> {
        let store = self.lock_store()?;
        refresh_submission_receipt_from_store(&store, &mut receipt);
        Ok(receipt)
    }

    /// One composite forward-delta read: current state, pending Requests,
    /// newly terminalized results, lifecycle events, and the forward message
    /// delta, from a cursor position. Every observation is a success; only
    /// malformed requests, unknown Sessions, and unusable cursors are errors.
    fn fetch(&self, params: &Value) -> Result<Value> {
        let options = self.fetch_options(params)?;
        let deadline = Instant::now() + options.wait;
        let mut bound: Option<i64> = None;
        loop {
            let cut = self.fetch_session_cut(&options, &mut bound)?;
            let rendered = cut.render(&options)?;
            if rendered.settled
                || Instant::now() >= deadline
                || self.shutting_down.load(Ordering::SeqCst)
            {
                return self.publish(&options, rendered);
            }
            std::thread::sleep(FETCH_POLL_INTERVAL);
        }
    }

    /// Attach the response to a position.
    ///
    /// A poll whose watermark vector did not move keeps the caller's own
    /// position rather than minting a new one: there is nothing new to name,
    /// the caller has nothing to update, and an idle long poll cannot churn
    /// the bounded set of retained positions. Internal bookkeeping can still
    /// advance the vector beneath an empty public delta, which mints.
    fn publish(&self, options: &FetchOptions, rendered: Rendered) -> Result<Value> {
        let mut value = rendered.value;
        if let Some(previous) = options.cursor.as_ref()
            && *previous == rendered.cursor
            && let Some(incoming) = options.incoming.as_ref()
        {
            // Never longer than the reserved number, so the measured document
            // can only shrink.
            value["cursor"] = json!(incoming);
            return Ok(value);
        }
        self.lock_store()?
            .store_cursor(&options.scope, options.position, rendered.cursor);
        Ok(value)
    }

    fn fetch_options(&self, params: &Value) -> Result<FetchOptions> {
        let selector = params.get("session").and_then(Value::as_str);
        let selector = selector
            .context("fetch requires a Session selector")
            .code(Code::InvalidArgument)?;
        let wait_ms = params.get("wait_ms").and_then(Value::as_u64).unwrap_or(0);
        if wait_ms > FETCH_MAX_WAIT_MS {
            fail!(
                InvalidArgument,
                "invalid wait duration; the maximum long poll is 24h"
            );
        }
        let max_bytes = usize::try_from(
            params
                .get("max_bytes")
                .and_then(Value::as_u64)
                .unwrap_or(FETCH_DEFAULT_MAX_BYTES as u64),
        )
        .unwrap_or(FETCH_DEFAULT_MAX_BYTES)
        .min(FETCH_HARD_MAX_BYTES);
        if max_bytes == 0 {
            fail!(
                InvalidArgument,
                "invalid max_bytes; the response budget must be positive"
            );
        }
        let message_limit = match params.get("messages").unwrap_or(&Value::Null) {
            Value::Bool(false) => 0,
            // Absent means the documented default: the message delta is on.
            Value::Null | Value::Bool(true) => FETCH_MESSAGE_PAGE,
            Value::Number(count) => usize::try_from(
                count
                    .as_u64()
                    .filter(|count| *count > 0)
                    .context("invalid message budget; use a positive count")
                    .code(Code::InvalidArgument)?
                    .min(FETCH_MESSAGE_MAX as u64),
            )
            .unwrap_or(FETCH_MESSAGE_PAGE),
            _ => fail!(
                InvalidArgument,
                "invalid messages selection; use a boolean or a count"
            ),
        };
        // Resolve through the UID index so a pre-rekey public ID, which a
        // UID-bound cursor implies is still valid, keeps addressing the same
        // logical Session.
        let scope = self
            .lock_store()?
            .session_uid(selector)
            .with_context(|| format!("session not found: {selector}"))
            .code(Code::NotFound)?;
        // A position is a number, so accept it spelled either way. Anything
        // that is not a position at all is a malformed request rather than a
        // silent baseline.
        let incoming = match params.get("cursor") {
            None | Some(Value::Null) => None,
            Some(Value::String(position)) => Some(position.clone()),
            Some(Value::Number(position)) => Some(position.to_string()),
            Some(_) => fail!(CursorInvalid, "cursor must be a position number"),
        }
        // Canonicalize the spelling: "0001" names position 1 and must be
        // published back as "1", or the no-mint echo would replace the
        // measured document's short reserved number with an arbitrarily long
        // caller-chosen spelling and break the byte bound.
        .map(|position| {
            position
                .parse::<u64>()
                .map(|number| number.to_string())
                .map_err(|_| {
                    coded(
                        Code::CursorInvalid,
                        format!("{position:?} is not a cursor position"),
                    )
                })
        })
        .transpose()?;
        let from_start = params
            .get("from_start")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let cursor = match incoming.as_deref() {
            Some(position) => Some(self.lock_store()?.resolve_cursor(&scope, position)?),
            // The start of everything this daemon retains for the Session.
            None if from_start => Some(cursor::Cursor::new()),
            None => None,
        };
        // A response that advances the observation takes the scope's next
        // position. It is reserved before rendering so the document can carry
        // its own number while it is being measured.
        let position = self.lock_store()?.reserve_cursor(&scope);
        Ok(FetchOptions {
            scope,
            position,
            incoming,
            cursor,
            wait: Duration::from_millis(wait_ms),
            message_limit,
            max_bytes,
            instance_id: self.instance_id.clone(),
        })
    }

    fn fetch_session_cut(&self, options: &FetchOptions, bound: &mut Option<i64>) -> Result<Cut> {
        let uid = options.scope.clone();
        let store = self.lock_store()?;
        let session = store
            .session_for_uid(&uid)
            .context("session not found: cursor no longer addresses a live Session")
            .code(Code::NotFound)?;
        let baseline = options.baseline();
        let mut position = options.position();
        let mut scope_gaps = Vec::new();
        let mut gaps = Vec::new();

        let (events, events_more, event_watermark) = if baseline {
            (Vec::new(), false, store.latest_event_seq())
        } else {
            let after = options.event_position();
            if after < store.evicted_event_seq() {
                scope_gaps.push(retention_gap("events"));
            }
            normalized_page(&store, &uid, after, FETCH_EVENT_PAGE)
        };

        let (results, results_more) = if baseline {
            let latest = store.latest_terminal_turn(&session.id);
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

        let messages = if options.message_limit == 0 {
            crate::messages::MessagePage {
                start: store.message_head(&uid),
                ..crate::messages::MessagePage::default()
            }
        } else if baseline {
            store.messages_tail(&uid, options.message_limit)
        } else {
            store.messages_after(&uid, position.m, options.message_limit)
        };
        if messages.gap {
            gaps.push(retention_gap("messages"));
        }
        position.m = messages.start;

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
        let requests = store.pending_requests(&uid);
        let public = public_session_locked(&store, &session);
        drop(store);

        Ok(Cut {
            baseline,
            state: Some(session.state),
            bound_seq: *bound,
            bound_terminal,
            full_event_watermark: event_watermark,
            events_more,
            dropped_event_seq: None,
            scope_gaps,
            sources: vec![BucketSource {
                session: public,
                requests: requests
                    .iter()
                    .take(PUBLISHED_REQUEST_LIMIT)
                    .map(crate::requests::RequestRecord::public)
                    .collect(),
                requests_more: requests.len() > PUBLISHED_REQUEST_LIMIT,
                gaps,
                events: events.into_iter().map(|(_, event)| event).collect(),
                results,
                results_more,
                messages: messages
                    .messages
                    .iter()
                    .map(|message| serde_json::to_value(message).unwrap_or(Value::Null))
                    .collect(),
                messages_more: messages.has_more,
                include_messages: options.message_limit > 0,
                incoming: position,
            }],
        })
    }

    fn list_models(&self, params: &Value) -> Result<Value> {
        match params_string(params, "harness")? {
            "claude" => Ok(crate::claude_models::list_models()),
            "codex" => {
                let connection = CodexConnection::connect(
                    Arc::new(|_| true),
                    None,
                    None,
                    &codex_discovery_args(),
                    Duration::from_mins(1),
                    &self.reaper,
                )
                .code(Code::LaunchFailed)?;
                let response = connection
                    .list_models(
                        params
                            .get("include_hidden")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    )
                    .code(Code::LaunchFailed)?;
                let models = response
                    .get("data")
                    .or_else(|| response.get("models"))
                    .cloned()
                    .unwrap_or(response);
                Ok(
                    json!({"harness":"codex","source":"app-server","discovery":"complete","models":models}),
                )
            }
            other => fail!(InvalidArgument, "unsupported harness {other:?}"),
        }
    }

    fn resolve_session(&self, selector: &str) -> Result<SessionRecord> {
        self.lock_store()?
            .get_session(selector)
            .with_context(|| format!("session not found: {selector}"))
            .code(Code::NotFound)
    }

    fn resolve_turn(&self, id: &str) -> Result<TurnRecord> {
        self.lock_store()?
            .get_turn(id)
            .with_context(|| format!("turn not found: {id}"))
            .code(Code::NotFound)
    }

    fn runtime(&self, id: &str) -> Result<Arc<AgentRuntime>> {
        self.sessions
            .read()
            .map_err(|_| anyhow!("session map lock poisoned"))?
            .get(id)
            .cloned()
            .with_context(|| format!("session is not active: {id}"))
            .code(Code::SessionUnavailable)
    }

    fn runtime_is_current(&self, id: &str, candidate: &Arc<AgentRuntime>) -> bool {
        self.sessions.read().is_ok_and(|sessions| {
            sessions
                .get(id)
                .is_some_and(|current| Arc::ptr_eq(current, candidate))
        }) && self
            .lock_store()
            .is_ok_and(|store| store.runtime_generation_is_current(id, candidate.generation()))
    }

    fn lock_store(&self) -> Result<MutexGuard<'_, Store>> {
        self.store
            .lock()
            .map_err(|_| anyhow!("session store lock poisoned"))
    }
}

/// Move a Session and its runtime to a new public ID.
fn promote_session(
    store: &Mutex<Store>,
    sessions: &SessionMap,
    from: &str,
    to: &str,
    runtime_session_id: &RwLock<String>,
) -> Result<()> {
    let mut runtime_id = runtime_session_id
        .write()
        .map_err(|_| anyhow!("runtime session ID lock poisoned"))?;
    if from == to {
        to.clone_into(&mut runtime_id);
        return Ok(());
    }
    if sessions
        .read()
        .map_err(|_| anyhow!("session map lock poisoned"))?
        .contains_key(to)
    {
        bail!("active runtime already exists for Session {to}");
    }
    let store = store
        .lock()
        .map_err(|_| anyhow!("session store lock poisoned"))?;
    store.rekey_session(from, to)?;
    let mut sessions = sessions
        .write()
        .map_err(|_| anyhow!("session map lock poisoned"))?;
    if let Some(runtime) = sessions.remove(from)
        && sessions.insert(to.to_owned(), runtime).is_some()
    {
        bail!("active runtime already exists for Session {to}");
    }
    to.clone_into(&mut runtime_id);
    Ok(())
}

fn launch_environment(params: &Value) -> Result<HashMap<String, String>> {
    params
        .get("environment")
        .and_then(Value::as_object)
        .context("missing launch environment snapshot")
        .code(Code::InvalidArgument)?
        .iter()
        .map(|(key, value)| {
            value
                .as_str()
                .map(|value| (key.clone(), value.to_owned()))
                .with_context(|| format!("environment value for {key:?} must be a string"))
                .code(Code::InvalidArgument)
        })
        .collect()
}

fn session_has_auth_mismatch(store: &Mutex<Store>, session_id: &str) -> bool {
    store.lock().ok().is_some_and(|store| {
        store
            .get_session(session_id)
            .is_some_and(|session| session_is_auth_mismatch(&session))
    })
}

fn session_is_auth_mismatch(session: &SessionRecord) -> bool {
    session.agent == Agent::Claude.as_str()
        && session.auth_policy.unwrap_or_default() == AuthPolicy::Subscription
        && session.auth == Some(Auth::ApiKey)
}

fn interrupt_and_stop_claude(process: &ClaudeProcess) {
    let _ = process.control_nowait(&json!({"subtype": "interrupt"}));
    process.stop();
}

fn startup_timeout(params: &Value) -> Duration {
    Duration::from_millis(
        params
            .get("startup_timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(60_000)
            .min(300_000),
    )
}

fn sanitize_message(message: &str) -> String {
    message.chars().take(4_096).collect()
}

/// Record that a Session's provider process is gone. An exit nobody asked
/// for fails the Session and whatever it was executing.
fn record_session_exit(
    store: &Mutex<Store>,
    sessions: &SessionMap,
    session_id: &str,
    runtime_generation: u64,
    reason: &str,
    detail: Option<&str>,
) {
    if let Ok(store) = store.lock() {
        let owns_generation = store.runtime_generation_is_current(session_id, runtime_generation);
        let session = owns_generation
            .then(|| store.get_session(session_id))
            .flatten();
        let restarting = session
            .as_ref()
            .is_some_and(|session| session.state == SessionState::Restarting);
        let intentional = session.as_ref().is_some_and(|session| {
            matches!(
                session.state,
                SessionState::Stopping | SessionState::Stopped | SessionState::Restarting
            )
        });
        if owns_generation && !intentional {
            let detail = detail.filter(|detail| !detail.is_empty());
            store.append_message(
                session_id,
                MessageBody::notice(&detail.map_or_else(
                    || reason.to_owned(),
                    |detail| format!("{reason}; stderr: {detail}"),
                )),
            );
        }
        if owns_generation {
            record_exit_result(&store, session.as_ref(), reason, intentional);
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
    }
    if let Ok(mut sessions) = sessions.write()
        && sessions
            .get(session_id)
            .is_some_and(|runtime| runtime.generation() == runtime_generation)
    {
        sessions.remove(session_id);
    }
}

fn record_exit_result(
    store: &Store,
    session: Option<&SessionRecord>,
    reason: &str,
    intentional: bool,
) {
    let Some(session) = session else { return };
    let Some(turn_id) = session.active_turn_id.as_deref() else {
        return;
    };
    let state = if intentional {
        TurnState::Interrupted
    } else {
        TurnState::Failed
    };
    let error = json!({"code": "provider_exit", "message": reason});
    match store.finish_turn_if_matching(turn_id, None, state, None, Some(&error)) {
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

fn cancel_target_is_current(
    store: &Arc<Mutex<Store>>,
    session_id: &str,
    turn_id: &str,
    runtime_generation: u64,
) -> bool {
    store.lock().is_ok_and(|store| {
        store.runtime_generation_is_current(session_id, runtime_generation)
            && store.get_session(session_id).is_some_and(|session| {
                session.state == SessionState::Quiescing
                    && session.active_turn_id.as_deref() == Some(turn_id)
            })
    })
}

fn fail_cancelled_turn(
    store: &Arc<Mutex<Store>>,
    session_id: &str,
    turn_id: &str,
    runtime_generation: u64,
    message: &str,
) {
    if let Ok(store) = store.lock()
        && store.fail_canceled_turn(
            turn_id,
            runtime_generation,
            "cancel_failed",
            &sanitize_message(message),
        )
    {
        store.record_event(Some(session_id), Some(turn_id), "turn.failed");
        store.record_event(Some(session_id), None, "session.failed");
    }
}

fn spawn_claude_cancel_control(
    store: Arc<Mutex<Store>>,
    turns: Arc<Mutex<ClaudeTurns>>,
    process: Arc<ClaudeProcess>,
    session_id: String,
    turn_id: String,
    runtime_generation: u64,
) {
    let _ = std::thread::Builder::new()
        .name("dlgt-claude-late-cancel".to_owned())
        .spawn(move || {
            if !cancel_target_is_current(&store, &session_id, &turn_id, runtime_generation) {
                if let Ok(store) = store.lock() {
                    store.finish_cancel_control(&turn_id);
                }
                return;
            }
            if let Err(error) =
                process.control(&json!({"subtype": "interrupt"}), CLAUDE_CONTROL_TIMEOUT)
            {
                fail_cancelled_turn(
                    &store,
                    &session_id,
                    &turn_id,
                    runtime_generation,
                    &error.to_string(),
                );
                process.force_stop();
                return;
            }
            if let Ok(store) = store.lock() {
                store.finish_cancel_control(&turn_id);
            }
            if cancel_target_is_current(&store, &session_id, &turn_id, runtime_generation)
                && let (Ok(mut turns), Ok(store)) = (turns.lock(), store.lock())
            {
                let _ = turns.settle(&store, &session_id);
            }
        });
}

/// A pause at a named race window, for the integration regressions only.
/// Release builds compile it away.
fn test_pause(name: &str) {
    if cfg!(debug_assertions)
        && let Some(milliseconds) = std::env::var(name)
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
    {
        std::thread::sleep(Duration::from_millis(milliseconds));
    }
}

fn params_string<'a>(params: &'a Value, name: &str) -> Result<&'a str> {
    params
        .get(name)
        .and_then(Value::as_str)
        .with_context(|| format!("missing string parameter {name:?}"))
        .code(Code::InvalidArgument)
}

/// Bounds for caller strings that every `fetch` repeats in the Session
/// bucket. With them the bucket stays small enough that a blocked Session's
/// minimum Request still fits under the hard response limit.
const SESSION_METADATA_LIMITS: [(&str, usize); 5] = [
    ("title", 200),
    ("alias", 64),
    ("model", 128),
    ("effort", 32),
    ("cwd", 4096),
];

fn validate_session_metadata(params: &Value) -> Result<()> {
    for (name, limit) in SESSION_METADATA_LIMITS {
        if let Some(value) = params.get(name).and_then(Value::as_str)
            && value.chars().count() > limit
        {
            fail!(
                InvalidArgument,
                "invalid {name}: at most {limit} characters"
            );
        }
    }
    Ok(())
}

fn validate_alias(alias: &str) -> Result<()> {
    if !alias.starts_with('@') || alias.len() < 2 {
        fail!(InvalidArgument, "session alias must look like @name");
    }
    if alias.contains('#') {
        fail!(
            InvalidArgument,
            "session alias must not contain reserved '#' characters"
        );
    }
    Ok(())
}

fn write_response(stream: &mut impl Write, response: &Response) -> Result<()> {
    serde_json::to_writer(&mut *stream, response).context("failed to encode RPC response")?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}

fn classify_error(error: &anyhow::Error) -> &'static str {
    crate::error::code_of(error).map_or("INTERNAL", Code::as_str)
}

fn answer_action(request: &RequestRecord) -> &'static str {
    match (&request.draft.kind, &request.draft.provider) {
        (_, ProviderRequest::CodexElicitation { form: true, .. }) => "--json <OBJECT>",
        (RequestKind::Approval, _) => "--allow|--deny",
        (RequestKind::Question, _) => "--text <TEXT>|--choice <N>",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BusyMetrics {
    busy_for_ms: u64,
    quiet_for_ms: u64,
}

fn clamp_busy_metrics(busy_for_ms: u64, last_event_age_ms: Option<u64>) -> BusyMetrics {
    BusyMetrics {
        busy_for_ms,
        // No provider event means no evidence of activity during this busy
        // interval, so report the whole interval as quiet. If the last event
        // predates the turn, min() removes that prior-idle time.
        quiet_for_ms: last_event_age_ms.map_or(busy_for_ms, |age| age.min(busy_for_ms)),
    }
}

fn busy_metrics(store: &Store, session: &SessionRecord) -> Option<BusyMetrics> {
    if session.state != SessionState::Busy {
        return None;
    }
    let now = now_ms();
    let age = |since: i64| u64::try_from(now.saturating_sub(since)).unwrap_or(0);
    // A turn is reserved before provider execution starts. Use that creation
    // timestamp rather than the provider's started_at timestamp so the
    // reported age cannot jump backwards while a provider binds.
    let busy_for_ms = session
        .active_turn_id
        .as_deref()
        .and_then(|turn_id| store.get_turn(turn_id))
        .map_or(0, |turn| age(turn.created_at_ms));
    Some(clamp_busy_metrics(
        busy_for_ms,
        store.last_provider_event_ms(&session.id).map(age),
    ))
}

/// Build the public snapshot from an already-held store guard. The store
/// mutex is not reentrant, so composite readers that take one cut under the
/// lock use this directly.
fn public_session_locked(store: &Store, session: &SessionRecord) -> Value {
    public_session_with_metrics(session, busy_metrics(store, session))
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
        "approvals": session.approvals,
        "created_at_ms": session.created_at_ms,
        "updated_at_ms": session.updated_at_ms,
    });
    let Some(object) = value.as_object_mut() else {
        return value;
    };
    if let Some(auth) = session.auth {
        object.insert("auth".to_owned(), json!(auth));
    }
    if let Some(auth_policy) = session.auth_policy {
        object.insert("auth_policy".to_owned(), json!(auth_policy));
    }
    if let Some(model) = &session.resolved_model {
        object.insert("resolved_model".to_owned(), json!(model));
    }
    if let Some(metrics) = busy_metrics
        && session.state == SessionState::Busy
    {
        object.insert("busy_for_ms".to_owned(), json!(metrics.busy_for_ms));
        object.insert("quiet_for_ms".to_owned(), json!(metrics.quiet_for_ms));
    }
    if session.agent == Agent::Claude.as_str()
        && let Some(provider_id) = provider_id_from_session(session)
    {
        object.insert(
            "marker".to_owned(),
            json!(claude_session_marker(provider_id)),
        );
    }
    value
}

#[derive(Debug)]
struct SessionLaunchFailure {
    session_id: String,
    code: &'static str,
    message: String,
    hint: String,
    action: Option<String>,
    detail: Option<String>,
}

impl SessionLaunchFailure {
    fn new(
        session_id: &str,
        agent: Option<Agent>,
        error: &anyhow::Error,
        detail: Option<String>,
    ) -> Self {
        // Without a terminal there is no startup screen for a human to
        // answer, so a provider that needs interaction to start is a failed
        // launch that names the command to run once by hand.
        let code = match classify_error(error) {
            "INTERNAL" | "NOT_FOUND" | "SESSION_UNAVAILABLE" => "LAUNCH_FAILED",
            code => code,
        };
        Self {
            session_id: session_id.to_owned(),
            code,
            message: format!("{error:#}"),
            hint: if code == "AUTH_MISMATCH" {
                error
                    .downcast_ref::<AuthMismatch>()
                    .map_or_else(|| AuthMismatch::default().hint(), AuthMismatch::hint)
            } else {
                "detail carries the provider's own stderr tail; if the provider needs a login, \
                   a trust decision, or first-run setup, run it once by hand in the Session's \
                   working directory and retry"
                    .to_owned()
            },
            action: agent.map(|agent| agent.as_str().to_owned()),
            detail: detail.filter(|detail| !detail.is_empty()),
        }
    }

    fn response(&self, request_id: String) -> Response {
        let (session_id, launch_id) = if self.session_id.starts_with("internal:") {
            (None, Some(self.session_id.clone()))
        } else {
            (Some(self.session_id.clone()), None)
        };
        let mut response = Response::session_error(
            request_id,
            self.code,
            self.message.clone(),
            session_id,
            launch_id,
        );
        if let Some(error) = response.error.as_mut() {
            error.hint = Some(self.hint.clone());
            error.action.clone_from(&self.action);
            error.detail.clone_from(&self.detail);
        }
        response
    }

    fn to_json(&self) -> Value {
        json!({
            "code": self.code,
            "message": self.message,
            "hint": self.hint,
            "action": self.action,
            "detail": self.detail,
        })
    }
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
        "denied_requests": turn.denied_requests,
        "error": turn.error,
        "started_at_ms": turn.started_at_ms.unwrap_or(turn.created_at_ms),
        "completed_at_ms": turn.completed_at_ms,
        "usage": turn.usage,
    })
}

/// Where a retained `final_text` came from. `missing` is an explicit
/// diagnostic: the execution still completed, but no answer was recovered.
fn final_text_source(turn: &TurnRecord) -> &'static str {
    if turn.final_message.as_deref().is_none_or(str::is_empty) {
        "missing"
    } else if turn.final_text_recovered {
        "messages"
    } else {
        "provider"
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

fn provider_qualified(agent: &str, id: &str) -> bool {
    matches!(agent, "codex" | "claude") && !id.is_empty()
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

fn refresh_submission_receipt_from_store(store: &Store, receipt: &mut Value) {
    // Confirmation is monotonic. A retained receipt can outlive its turn, so
    // missing live evidence must never downgrade a previously confirmed one.
    if receipt.get("submission").and_then(Value::as_str) != Some("pending") {
        return;
    }
    let Some(session_id) = receipt
        .get("session")
        .and_then(|session| session.get("id"))
        .and_then(Value::as_str)
    else {
        return;
    };
    let Some(execution_seq) = receipt.get("execution_seq").and_then(Value::as_i64) else {
        return;
    };
    let current_session_id = store
        .session_uid(session_id)
        .and_then(|uid| store.session_for_uid(&uid))
        .map_or_else(|| session_id.to_owned(), |session| session.id);
    if store
        .turn_for_execution(&current_session_id, execution_seq)
        .is_none_or(|turn| turn.started_at_ms.is_none())
    {
        return;
    }
    receipt["submission"] = json!("confirmed");
    if let Some(object) = receipt.as_object_mut() {
        object.remove("hint");
        object.remove("action");
    }
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
                    fail!(
                        InvalidArgument,
                        "invalid request_id reuse: {request_id:?} already accepted a different payload"
                    );
                }
                let mut replay = receipt.value.clone();
                replay["replayed"] = json!(true);
                return Ok(replay);
            }
            match state.inflight.get(request_id) {
                Some(&reserved) if reserved != digest => {
                    fail!(
                        InvalidArgument,
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
/// Every acceptance carries an idempotency key.
///
/// A key that only shows up on the retry is useless: the first attempt has
/// already created a Session by then. Requiring it up front is what makes a
/// lost acceptance response recoverable at all.
fn validate_request_id(params: &Value) -> Result<()> {
    let request_id = params
        .get("request_id")
        .and_then(Value::as_str)
        .filter(|request_id| !request_id.is_empty())
        .context("missing request_id: every acceptance must carry an idempotency key")
        .code(Code::InvalidArgument)?;
    if request_id.len() > crate::protocol::MAX_ACCEPTANCE_REQUEST_ID_LEN {
        fail!(
            InvalidArgument,
            "invalid request_id: must be at most {} bytes",
            crate::protocol::MAX_ACCEPTANCE_REQUEST_ID_LEN
        );
    }
    Ok(())
}

fn request_digest(params: &Value) -> u128 {
    const VOLATILE: [&str; 3] = ["environment", "correlation_id", "request_id"];
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

#[derive(Clone)]
struct FetchOptions {
    /// One immutable Session UID.
    scope: String,
    /// Observation position this response publishes when it advances.
    position: u64,
    /// The position the caller supplied, kept when nothing advanced.
    incoming: Option<String>,
    cursor: Option<cursor::Cursor>,
    wait: Duration,
    /// Zero omits messages entirely.
    message_limit: usize,
    max_bytes: usize,
    instance_id: String,
}

impl FetchOptions {
    /// A cursorless request is the bounded baseline snapshot.
    fn baseline(&self) -> bool {
        self.cursor.is_none()
    }

    fn position(&self) -> cursor::SessionCursor {
        self.cursor
            .as_ref()
            .map(cursor::Cursor::session)
            .unwrap_or_default()
    }

    fn event_position(&self) -> i64 {
        self.cursor.as_ref().map_or(0, |cursor| cursor.e)
    }
}

/// An immutable observation cut taken under the store lock. Serialization
/// happens after the lock is released, so anything arriving during
/// serialization belongs to the next cursor.
struct Cut {
    baseline: bool,
    state: Option<SessionState>,
    /// Execution bound by a waiting fetch, and whether it has terminalized.
    bound_seq: Option<i64>,
    bound_terminal: bool,
    /// Watermark the event scan reached. Publishable only when every
    /// candidate event is committed and the cut hid none.
    full_event_watermark: i64,
    /// The scan stopped at its page limit; more events already exist.
    events_more: bool,
    /// Lowest event sequence this cut refused to include at all, because the
    /// Session it belongs to did not fit the page.
    dropped_event_seq: Option<i64>,
    /// Scope-wide gaps. Per-Session gaps live on the bucket.
    scope_gaps: Vec<Value>,
    sources: Vec<BucketSource>,
}

/// Everything one Session could contribute. The builder decides what actually
/// ships; nothing here is a promise.
#[allow(clippy::struct_excessive_bools)]
struct BucketSource {
    session: Value,
    /// Pending Requests. Always current, never positioned by the cursor.
    requests: Vec<Value>,
    /// More Requests are pending than one response publishes.
    requests_more: bool,
    gaps: Vec<Value>,
    events: Vec<Value>,
    results: Vec<TurnRecord>,
    results_more: bool,
    /// Messages after `incoming.m`, ascending, each carrying its `seq`.
    messages: Vec<Value>,
    messages_more: bool,
    include_messages: bool,
    incoming: cursor::SessionCursor,
}

/// One Session bucket as committed so far.
struct Committed {
    session: Value,
    requests: Vec<Value>,
    /// Pending Requests exist that this response does not carry.
    requests_truncated: bool,
    gaps: Vec<Value>,
    results: Vec<Value>,
    events: Vec<Value>,
    messages: Vec<Value>,
    include_messages: bool,
    /// Watermarks for exactly the units committed above.
    position: cursor::SessionCursor,
}

impl Committed {
    fn value(&self) -> Value {
        let mut bucket = json!({
            "session": self.session,
            "requests": self.requests,
            "gaps": self.gaps,
            "results": self.results,
            "events": self.events,
        });
        if self.requests_truncated {
            bucket["requests_truncated"] = json!(true);
        }
        if self.include_messages {
            bucket["messages"] = json!(self.messages);
        }
        bucket
    }
}

/// A committed unit, recorded so a rollback can undo exactly what it did.
enum Unit {
    Bucket,
    Request {
        bucket: usize,
    },
    Result {
        bucket: usize,
        previous: cursor::SessionCursor,
    },
    Event {
        bucket: usize,
    },
    Message {
        bucket: usize,
        previous: cursor::SessionCursor,
    },
}

struct Rendered {
    value: Value,
    /// Watermarks for exactly what `value` carries. The caller stores this and
    /// substitutes the handle it gets back.
    cursor: cursor::Cursor,
    /// The long poll is finished: either something wake-worthy happened or the
    /// requested binding is satisfied.
    settled: bool,
}

/// Assembles a response one unit at a time.
///
/// A watermark is recorded only when the unit it describes is committed, and
/// the published vector is derived last from the committed set, so shedding
/// cannot leave a watermark past data the caller never received. The position
/// number is reserved before rendering, so the document can carry its own
/// number while it is being measured. Byte accounting is exact
/// per unit; the finished document is measured once and units are rolled back
/// until it fits, which can only shrink the cursor further.
///
/// Measuring the full document per committed unit is bounded-quadratic in the
/// unit count (page caps keep the worst case around a few hundred cycles of a
/// ≤256 KiB serialization). Accepted trade-off for exactness; benchmark before
/// optimizing.
struct Builder<'a> {
    cut: &'a Cut,
    options: &'a FetchOptions,
    buckets: Vec<Committed>,
    units: Vec<Unit>,
}

/// How many times a chunk is resized before the unit is abandoned. Each
/// attempt strictly shrinks, so this only bounds pathological input.
const MAX_FIT_ATTEMPTS: usize = 24;

impl<'a> Builder<'a> {
    const fn new(cut: &'a Cut, options: &'a FetchOptions) -> Self {
        Self {
            cut,
            options,
            buckets: Vec::new(),
            units: Vec::new(),
        }
    }

    /// Keep everything committed since `mark` only if the finished document
    /// fits the budget, and report by how much it did not.
    ///
    /// Every commit is measured rather than estimated. No arithmetic can
    /// predict the encoded size, because committing a unit also lengthens the
    /// cursor the same document has to carry.
    fn verify(&mut self, mark: usize) -> Result<Option<usize>> {
        let length = serialized_len(&self.document())?;
        let budget = self.budget();
        if length <= budget {
            return Ok(None);
        }
        while self.units.len() > mark {
            self.rollback();
        }
        Ok(Some(length - budget))
    }

    /// Bytes available to the daemon's document. The caller's budget covers
    /// the complete compact CLI line, so the wrapper the client adds around
    /// this document is reserved up front.
    fn budget(&self) -> usize {
        self.options
            .max_bytes
            .saturating_sub(cli_wrapper_overhead())
    }

    fn empty_bucket(source: &BucketSource) -> Committed {
        Committed {
            session: source.session.clone(),
            requests: Vec::new(),
            requests_truncated: source.requests_more,
            gaps: source.gaps.clone(),
            results: Vec::new(),
            events: Vec::new(),
            messages: Vec::new(),
            include_messages: source.include_messages,
            // A bucket without messages carries the message head forward:
            // the cut placed it in `incoming.m`.
            position: source.incoming,
        }
    }

    /// Commit the mandatory part of each Session bucket: identity and gaps.
    fn commit_buckets(&mut self) -> Result<()> {
        for source in &self.cut.sources {
            let mark = self.units.len();
            self.buckets.push(Self::empty_bucket(source));
            self.units.push(Unit::Bucket);
            if self.verify(mark)?.is_some() {
                return Ok(());
            }
        }
        Ok(())
    }

    /// Commit pending Requests, oldest first. They are not positioned by the
    /// cursor, so one that does not fit is flagged rather than paged.
    fn commit_requests(&mut self) -> Result<()> {
        for index in 0..self.buckets.len() {
            for request in &self.cut.sources[index].requests {
                let mark = self.units.len();
                self.buckets[index].requests.push(request.clone());
                self.units.push(Unit::Request { bucket: index });
                if self.verify(mark)?.is_some() {
                    self.buckets[index].requests_truncated = true;
                    break;
                }
            }
        }
        Ok(())
    }

    fn commit_results(&mut self) -> Result<()> {
        for index in 0..self.buckets.len() {
            for position in 0..self.cut.sources[index].results.len() {
                let complete = self.commit_result(index, position)?;
                if !complete {
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// Commit one result, chunked to fit. Returns whether the whole body
    /// shipped; anything else ends the results for this response.
    fn commit_result(&mut self, index: usize, position: usize) -> Result<bool> {
        let turn = self.cut.sources[index].results[position].clone();
        let offset = if self.buckets[index].position.px == Some(turn.execution_seq) {
            self.buckets[index].position.po
        } else {
            0
        };
        // Try the whole body first. Chunking adds continuation fields to the
        // cursor, so a chunk can cost more than the complete record it
        // replaces; only measurement can tell.
        let mut hint = usize::MAX;
        for _ in 0..MAX_FIT_ATTEMPTS {
            let separator = usize::from(!self.buckets[index].results.is_empty());
            let Some((value, take, complete)) =
                fit_result(&turn, offset, hint.saturating_sub(separator))
            else {
                return Ok(false);
            };
            let attempted = serialized_len(&value)?;
            let mark = self.units.len();
            let previous = self.buckets[index].position;
            self.buckets[index].results.push(value);
            let watermark = &mut self.buckets[index].position;
            if complete {
                watermark.x = turn.execution_seq;
                watermark.px = None;
                watermark.po = 0;
            } else {
                watermark.px = Some(turn.execution_seq);
                watermark.po = offset + u64::try_from(take).unwrap_or(0);
            }
            self.units.push(Unit::Result {
                bucket: index,
                previous,
            });
            let Some(excess) = self.verify(mark)? else {
                return Ok(complete);
            };
            // Trim from what was actually attempted by exactly the overshoot,
            // rather than dropping a result the caller is waiting for.
            hint = attempted.saturating_sub(excess.max(1));
            if hint == 0 {
                return Ok(false);
            }
        }
        Ok(false)
    }

    /// Commit events as a global sequence-ascending prefix.
    ///
    /// Delivering a later event while an earlier one is unreachable would
    /// leave a hole the watermark can never pass, so the caller would receive
    /// the same later events on every call forever. Committing in order and
    /// stopping at the first refusal makes the watermark a true prefix by
    /// construction.
    fn commit_events(&mut self) -> Result<()> {
        // Anything belonging to a Session this response cannot carry is a
        // hole: nothing at or after it may be committed.
        let mut limit = self.cut.dropped_event_seq.unwrap_or(i64::MAX);
        for source in self.cut.sources.iter().skip(self.buckets.len()) {
            for event in &source.events {
                limit = limit.min(event_seq(event));
            }
        }
        let mut queue = Vec::new();
        for (index, source) in self.cut.sources.iter().enumerate().take(self.buckets.len()) {
            for (offset, event) in source.events.iter().enumerate() {
                queue.push((event_seq(event), index, offset));
            }
        }
        queue.sort_unstable();
        for (seq, index, offset) in queue {
            if seq >= limit {
                return Ok(());
            }
            let event = self.cut.sources[index].events[offset].clone();
            let mark = self.units.len();
            self.buckets[index].events.push(event);
            self.units.push(Unit::Event { bucket: index });
            if self.verify(mark)?.is_some() {
                return Ok(());
            }
        }
        Ok(())
    }

    /// Commit messages as an ascending prefix. A message is never split: one
    /// that does not fit ends the messages for this response.
    fn commit_messages(&mut self) -> Result<()> {
        for index in 0..self.buckets.len() {
            if !self.cut.sources[index].include_messages {
                continue;
            }
            for offset in 0..self.cut.sources[index].messages.len() {
                let message = self.cut.sources[index].messages[offset].clone();
                let mark = self.units.len();
                let previous = self.buckets[index].position;
                self.buckets[index].position.m = message_seq(&message);
                self.buckets[index].messages.push(message);
                self.units.push(Unit::Message {
                    bucket: index,
                    previous,
                });
                if self.verify(mark)?.is_some() {
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// Undo the most recent commit, including the watermark it recorded.
    fn rollback(&mut self) -> bool {
        match self.units.pop() {
            Some(Unit::Message { bucket, previous }) => {
                self.buckets[bucket].messages.pop();
                self.buckets[bucket].position = previous;
                true
            }
            Some(Unit::Event { bucket }) => {
                self.buckets[bucket].events.pop();
                true
            }
            Some(Unit::Result { bucket, previous }) => {
                self.buckets[bucket].results.pop();
                self.buckets[bucket].position = previous;
                true
            }
            Some(Unit::Request { bucket }) => {
                self.buckets[bucket].requests.pop();
                self.buckets[bucket].requests_truncated = true;
                true
            }
            Some(Unit::Bucket) => {
                self.buckets.pop();
                true
            }
            None => false,
        }
    }

    fn pending(&self) -> bool {
        if self.buckets.len() < self.cut.sources.len() {
            return true;
        }
        if self.cut.events_more || self.cut.dropped_event_seq.is_some() {
            return true;
        }
        self.buckets
            .iter()
            .zip(&self.cut.sources)
            .any(|(committed, source)| {
                source.results_more
                    || source.messages_more
                    || committed.events.len() < source.events.len()
                    || committed.results.len() < source.results.len()
                    || committed.messages.len() < source.messages.len()
                    || committed.position.px.is_some()
            })
    }

    fn delivered(&self) -> bool {
        self.buckets.iter().any(|bucket| {
            !bucket.events.is_empty() || !bucket.results.is_empty() || !bucket.messages.is_empty()
        })
    }

    /// Whether this response moves the caller forward at all.
    fn progress(&self) -> bool {
        if self.cut.baseline {
            return !self.buckets.is_empty();
        }
        self.delivered()
    }

    fn blocked_request_missing(&self) -> bool {
        self.cut.state == Some(SessionState::Blocked)
            && self
                .cut
                .sources
                .iter()
                .any(|source| !source.requests.is_empty())
            && !self
                .buckets
                .iter()
                .any(|bucket| !bucket.requests.is_empty())
    }

    /// Derive the cursor from the committed set only.
    fn cursor(&self) -> cursor::Cursor {
        let mut next = cursor::Cursor::new();
        if let Some(previous) = self.options.cursor.as_ref() {
            next.set_session(previous.p);
        }
        for bucket in &self.buckets {
            next.set_session(bucket.position);
        }
        // Events are committed as an ascending prefix, so the highest one
        // this document carries is a watermark with no holes behind it. The
        // scan watermark may be published only when nothing was left behind,
        // since it also covers events that normalize away.
        let held_back = self.cut.dropped_event_seq.is_some()
            || self
                .cut
                .sources
                .iter()
                .skip(self.buckets.len())
                .any(|source| !source.events.is_empty())
            || self
                .buckets
                .iter()
                .zip(&self.cut.sources)
                .any(|(committed, source)| committed.events.len() < source.events.len());
        next.e = if held_back {
            self.buckets
                .iter()
                .flat_map(|bucket| bucket.events.iter())
                .map(event_seq)
                .max()
                .unwrap_or_else(|| self.options.event_position())
        } else {
            self.cut.full_event_watermark
        };
        next
    }

    fn gapped(&self) -> bool {
        !self.cut.scope_gaps.is_empty() || self.buckets.iter().any(|bucket| !bucket.gaps.is_empty())
    }

    fn result_ready(&self) -> bool {
        self.cut.bound_terminal
            && self.cut.bound_seq.is_some_and(|seq| {
                self.buckets
                    .first()
                    .is_some_and(|bucket| seq <= bucket.position.x)
            })
    }

    /// Why this response returned. Computed from the committed set so it is
    /// part of the measured document; a reason decided afterwards could be
    /// longer than the placeholder and push an exact-budget response over.
    fn reason(&self) -> &'static str {
        if self.gapped() {
            "gap"
        } else if self.cut.state == Some(SessionState::Blocked) {
            "blocked"
        } else if self.options.wait > Duration::ZERO && self.result_ready() {
            "result"
        } else if self.pending() {
            "page_full"
        } else if self.cut.baseline {
            "snapshot"
        } else if self.delivered() {
            "change"
        } else {
            "timeout"
        }
    }

    fn document(&self) -> Value {
        json!({
            "schema_version": SCHEMA_VERSION,
            "runtime": {
                "version": env!("CARGO_PKG_VERSION"),
                "instance_id": self.options.instance_id,
            },
            "reason": self.reason(),
            "has_more": self.pending(),
            "gaps": self.cut.scope_gaps,
            "cursor": self.options.position.to_string(),
            "sessions": self
                .buckets
                .iter()
                .map(Committed::value)
                .collect::<Vec<_>>(),
        })
    }

    /// A budget that demonstrably works for this cut, or `None` if none does.
    ///
    /// Only a value that has actually rendered is ever returned. Searching is
    /// not an option: whether a budget makes progress is not monotonic in the
    /// budget, so a bisection can converge on a value that fails.
    fn workable_budget(&self) -> Result<Option<usize>> {
        let mut probe = Builder::new(self.cut, self.options);
        probe.commit_minimum();
        let floor = serialized_len(&probe.document())? + cli_wrapper_overhead();
        // Recommend something strictly larger than the budget that just
        // failed, so the advice cannot repeat the caller's mistake.
        let mut candidate = floor.max(self.options.max_bytes.saturating_add(1)).max(1);
        loop {
            let clamped = candidate.min(FETCH_HARD_MAX_BYTES);
            if self.renders_at(clamped)? {
                return Ok(Some(clamped));
            }
            if clamped == FETCH_HARD_MAX_BYTES {
                // Chunking means the hard limit should always suffice; report
                // no recommendation rather than one that was never verified.
                return Ok(None);
            }
            candidate = clamped.saturating_mul(2);
        }
    }

    /// Whether a real attempt at `max_bytes` would succeed.
    fn renders_at(&self, max_bytes: usize) -> Result<bool> {
        let options = FetchOptions {
            max_bytes,
            ..self.options.clone()
        };
        let mut trial = Builder::new(self.cut, &options);
        trial.commit_all()?;
        let value = trial.settle()?;
        Ok(serialized_len(&value)? <= trial.budget()
            && (!trial.pending() || trial.progress())
            && !trial.blocked_request_missing())
    }

    /// Commit one bucket and one unit of progress, ignoring the budget.
    fn commit_minimum(&mut self) {
        let Some(source) = self.cut.sources.first() else {
            return;
        };
        self.buckets.push(Self::empty_bucket(source));
        self.units.push(Unit::Bucket);
        if self.cut.state == Some(SessionState::Blocked)
            && let Some(request) = source.requests.first()
        {
            self.buckets[0].requests.push(request.clone());
            self.buckets[0].requests_truncated = source.requests_more || source.requests.len() > 1;
            self.units.push(Unit::Request { bucket: 0 });
        } else if let Some(turn) = source.results.first() {
            self.buckets[0].results.push(minimum_result_value(turn));
        } else if let Some(event) = source.events.first() {
            self.buckets[0].events.push(event.clone());
        } else if let Some(message) = source.messages.first() {
            self.buckets[0].messages.push(message.clone());
        }
    }

    /// State first, then pending Requests, terminal results, and events.
    /// Messages go last, so they are what a squeezed response drops first.
    fn commit_all(&mut self) -> Result<()> {
        self.commit_buckets()?;
        self.commit_requests()?;
        self.commit_results()?;
        self.commit_events()?;
        self.commit_messages()
    }

    /// Roll units back until the encoded document fits the budget.
    fn settle(&mut self) -> Result<Value> {
        let mut value = self.document();
        while serialized_len(&value)? > self.budget() {
            if !self.rollback() {
                break;
            }
            value = self.document();
        }
        Ok(value)
    }

    fn finish(mut self) -> Result<Rendered> {
        let value = self.settle()?;
        let length = serialized_len(&value)?;
        if length > self.budget()
            || (self.pending() && !self.progress())
            || self.blocked_request_missing()
        {
            // Progress must come from chunking, never from oversizing. If not
            // even one chunk fits, the request itself is unsatisfiable.
            let Some(minimum) = self.workable_budget()? else {
                fail!(
                    InvalidArgument,
                    "invalid max_bytes {}: too small to carry one unit of progress",
                    self.options.max_bytes,
                );
            };
            fail!(
                InvalidArgument,
                "invalid max_bytes {}: too small to carry one unit of progress; --max-bytes {minimum} is verified to work for the current state",
                self.options.max_bytes,
            );
        }
        // New messages alone never complete a long poll: they ride along when
        // something else wakes it or the deadline fires.
        let settled = self.gapped()
            || self.cut.state == Some(SessionState::Blocked)
            || self.pending()
            || if self.options.wait > Duration::ZERO {
                self.result_ready()
            } else {
                self.cut.baseline || self.delivered()
            };
        Ok(Rendered {
            value,
            cursor: self.cursor(),
            settled,
        })
    }
}

impl Cut {
    fn render(self, options: &FetchOptions) -> Result<Rendered> {
        let mut builder = Builder::new(&self, options);
        builder.commit_all()?;
        builder.finish()
    }
}

/// Version of the fetch document and lifecycle event schemas.
const SCHEMA_VERSION: u32 = 2;

fn message_seq(message: &Value) -> u64 {
    message.get("seq").and_then(Value::as_u64).unwrap_or(0)
}

/// Largest index at or below `index` that is a UTF-8 character boundary.
fn char_floor(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// Encode a result, chunking `final_text` at a UTF-8 boundary to fit `budget`.
/// `None` when the record cannot carry even an empty body.
fn fit_result(turn: &TurnRecord, offset: u64, budget: usize) -> Option<(Value, usize, bool)> {
    let mut value = public_result(turn);
    let text = turn.final_message.clone().unwrap_or_default();
    let start = char_floor(&text, usize::try_from(offset).unwrap_or(usize::MAX));
    let remainder = &text[start..];
    value["final_text_offset"] = json!(offset);
    let mut take = remainder.len();
    loop {
        value["final_text"] = json!(&remainder[..take]);
        value["final_text_complete"] = json!(take == remainder.len());
        if encoded_len(&value) <= budget {
            return Some((value, take, take == remainder.len()));
        }
        if take == 0 {
            return None;
        }
        let next = char_floor(remainder, take * 3 / 4);
        take = if next == take {
            char_floor(remainder, take - 1)
        } else {
            next
        };
    }
}

/// A result record carrying one character of body.
fn minimum_result_value(turn: &TurnRecord) -> Value {
    let mut value = public_result(turn);
    let text = turn.final_message.clone().unwrap_or_default();
    let head = text.chars().next().map(String::from).unwrap_or_default();
    value["final_text"] = json!(head);
    value["final_text_offset"] = json!(0);
    value["final_text_complete"] = json!(false);
    value
}

/// Bytes the client adds when it prints a daemon result as one compact line:
/// the injected `"ok":true` member, its separating comma, and the newline.
/// Derived rather than guessed so it cannot drift from `print_success`.
fn cli_wrapper_overhead() -> usize {
    // `{"ok":true}` without its braces, plus a comma and a newline.
    serde_json::to_string(&json!({"ok": true})).map_or(11, |text| text.len() - 2) + 2
}

fn event_seq(event: &Value) -> i64 {
    event.get("seq").and_then(Value::as_i64).unwrap_or(0)
}

/// Encoded length of a value, saturating when it cannot be encoded so a
/// caller sizing a chunk never admits one by accident.
fn encoded_len(value: &Value) -> usize {
    serde_json::to_string(value).map_or(usize::MAX, |text| text.len())
}

fn serialized_len(value: &Value) -> Result<usize> {
    Ok(serde_json::to_string(value)?.len())
}

fn retention_gap(component: &str) -> Value {
    json!({"component": component, "reason": "retention_overrun"})
}

/// A continuation offset is caller-supplied. Reject one that does not name a
/// UTF-8 boundary inside the unit it claims to continue, rather than indexing
/// with it.
fn validate_continuation(position: &cursor::SessionCursor, results: &[TurnRecord]) -> Result<()> {
    if let Some(execution_seq) = position.px
        && let Some(turn) = results
            .first()
            .filter(|turn| turn.execution_seq == execution_seq)
    {
        let text = turn.final_message.as_deref().unwrap_or_default();
        let offset = usize::try_from(position.po).unwrap_or(usize::MAX);
        if offset > text.len() || !text.is_char_boundary(offset) {
            fail!(
                CursorInvalid,
                "continuation offset is not a character boundary in the retained result"
            );
        }
    }
    Ok(())
}

type EventPage = (Vec<(Option<String>, Value)>, bool, i64);

fn normalized_page(store: &Store, uid: &str, after: i64, limit: usize) -> EventPage {
    let mut page = Vec::new();
    let mut watermark = after;
    let mut has_more = false;
    for event in store.read_events(Some(uid), after) {
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

fn normalize_event(event: &crate::protocol::EventRecord) -> Option<Value> {
    let event_type = normalize_event_type(&event.kind)?;
    // Pre-bind events belong to a launch that had no public identity. They are
    // plumbing, and publishing them would expose an internal launch ID that
    // the contract says is never a Session ID.
    if event
        .session_id
        .as_deref()
        .is_some_and(|id| id.starts_with("internal:"))
    {
        return None;
    }
    let mut value = json!({
        "schema_version": SCHEMA_VERSION,
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
    use std::fs;
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::Duration;

    use serde_json::{Value, json};

    use super::{
        Agent, BucketSource, BusyMetrics, Cut, DaemonLock, FETCH_DEFAULT_MAX_BYTES,
        FETCH_EVENT_PAGE, FETCH_HARD_MAX_BYTES, FETCH_MESSAGE_PAGE, FetchOptions,
        ProviderReservation, ReceiptLedger, SessionLaunchFailure, canonical_session_id,
        clamp_busy_metrics, classify_error, cli_wrapper_overhead, generate_alias,
        generate_internal_id, is_stale_socket_error, ownership_lock_path, prepare_socket_path,
        provider_id_from_session, public_result, public_session, public_session_with_metrics,
        read_rpc_line, refresh_submission_receipt_from_store, remove_owned_socket, retention_gap,
        signal_shutdown, socket_identity, validate_alias, validate_continuation,
        wait_for_update_check,
    };
    use crate::protocol::{ApprovalsMode, SessionState, TurnState};
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
                approvals: ApprovalsMode::Auto,
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
    fn startup_leaves_an_occupied_non_socket_path_untouched() {
        let directory = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("failed to create temp directory: {error}"));
        let socket_path = directory.path().join("dlgt.sock");
        fs::write(&socket_path, b"keep me")
            .unwrap_or_else(|error| panic!("failed to create occupied path: {error}"));
        let error = prepare_socket_path(&socket_path)
            .err()
            .unwrap_or_else(|| panic!("occupied non-socket path was accepted"));
        assert!(socket_path.exists());
        assert!(error.to_string().contains("non-socket"));
    }

    #[test]
    fn only_connection_refused_and_not_found_authorize_stale_socket_cleanup() {
        assert!(is_stale_socket_error(&std::io::Error::from(
            std::io::ErrorKind::ConnectionRefused
        )));
        assert!(is_stale_socket_error(&std::io::Error::from(
            std::io::ErrorKind::NotFound
        )));
        assert!(!is_stale_socket_error(&std::io::Error::from(
            std::io::ErrorKind::PermissionDenied
        )));
        assert!(!is_stale_socket_error(&std::io::Error::from(
            std::io::ErrorKind::Other
        )));
    }

    #[test]
    fn a_peer_that_closes_before_an_rpc_is_quiet_eof() {
        let (server, peer) = UnixStream::pair()
            .unwrap_or_else(|error| panic!("failed to create test socket pair: {error}"));
        drop(peer);
        let request = read_rpc_line(&server)
            .unwrap_or_else(|error| panic!("failed to read peer EOF: {error:#}"));
        assert!(request.is_none());
    }

    #[test]
    fn daemon_ownership_lock_serializes_starters() {
        let directory = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("failed to create temp directory: {error}"));
        let socket_path = directory.path().join("dlgt.sock");
        let first = DaemonLock::acquire(&socket_path)
            .unwrap_or_else(|error| panic!("failed to acquire first lock: {error:#}"));
        assert_eq!(
            ownership_lock_path(&socket_path),
            directory.path().join("dlgt.sock.lock")
        );
        let second = DaemonLock::acquire(&socket_path)
            .err()
            .unwrap_or_else(|| panic!("second starter acquired the ownership lock"));
        assert!(second.to_string().contains("ownership lock"));
        drop(first);
        // Other tests spawn processes concurrently. A child forked before its
        // exec briefly shares every open lock file, so allow that window to
        // pass instead of treating it as a held lock.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            match DaemonLock::acquire(&socket_path) {
                Ok(_) => break,
                Err(error) if std::time::Instant::now() >= deadline => {
                    panic!("lock was not released with the daemon: {error:#}")
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(10)),
            }
        }
    }

    #[test]
    fn cleanup_preserves_a_replacement_path_with_a_different_identity() {
        let directory = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("failed to create temp directory: {error}"));
        let socket_path = directory.path().join("dlgt.sock");
        let replacement_path = directory.path().join("replacement");
        fs::write(&socket_path, b"owned")
            .unwrap_or_else(|error| panic!("failed to create owned path: {error}"));
        let owned = socket_identity(&socket_path)
            .unwrap_or_else(|error| panic!("failed to record path identity: {error:#}"));
        fs::write(&replacement_path, b"replacement")
            .unwrap_or_else(|error| panic!("failed to create replacement: {error}"));
        fs::rename(&replacement_path, &socket_path)
            .unwrap_or_else(|error| panic!("failed to install replacement: {error}"));

        remove_owned_socket(&socket_path, owned)
            .unwrap_or_else(|error| panic!("replacement cleanup failed: {error:#}"));
        assert_eq!(
            fs::read(&socket_path)
                .unwrap_or_else(|error| panic!("replacement disappeared: {error}")),
            b"replacement"
        );
    }

    #[test]
    fn archive_separator_is_reserved_in_aliases() {
        assert!(validate_alias("@worker").is_ok());
        assert!(validate_alias("@worker#old").is_err());
    }

    #[test]
    fn a_launch_failure_names_the_provider_command_and_carries_its_stderr() {
        let failure = SessionLaunchFailure::new(
            "internal:ABC12345",
            Some(Agent::Claude),
            &anyhow::anyhow!("Claude did not answer the initialize control request"),
            Some("Invalid API key · Please run /login".to_owned()),
        );
        assert_eq!(
            failure.code, "LAUNCH_FAILED",
            "a provider that cannot start is never an internal error or a blocked launch"
        );
        assert_eq!(failure.action.as_deref(), Some("claude"));
        let response = failure.response("req_1".to_owned());
        let error = response
            .error
            .unwrap_or_else(|| panic!("launch failure had no error"));
        assert_eq!(error.launch_id.as_deref(), Some("internal:ABC12345"));
        assert_eq!(error.session_id, None);
        assert_eq!(
            error.detail.as_deref(),
            Some("Invalid API key · Please run /login")
        );
        assert_eq!(
            failure.to_json()["detail"],
            error.detail.as_deref().unwrap_or("")
        );

        let quiet = SessionLaunchFailure::new(
            "codex:thread-1",
            Some(Agent::Codex),
            &crate::error::coded(
                crate::error::Code::AliasInUse,
                "active session alias already exists",
            ),
            Some(String::new()),
        );
        assert_eq!(quiet.code, "ALIAS_IN_USE");
        assert_eq!(quiet.detail, None);
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
    fn every_claude_session_exposes_its_search_marker_but_codex_does_not() {
        let store = ready_store("claude");
        let session = store
            .get_session("claude:thread-1")
            .unwrap_or_else(|| panic!("session missing"));
        let value = public_session(&session);
        assert_eq!(value["marker"], "dlgt:746872656164");

        let store = ready_store("codex");
        let session = store
            .get_session("codex:thread-1")
            .unwrap_or_else(|| panic!("session missing"));
        assert!(public_session(&session).get("marker").is_none());
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
                quiet_for_ms: 987,
            }),
        );
        assert_eq!(value["state"], "busy");
        assert_eq!(value["busy_for_ms"], 1_234);
        assert_eq!(value["quiet_for_ms"], 987);

        let idle = ready_store("codex")
            .get_session("codex:thread-1")
            .unwrap_or_else(|| panic!("idle session missing"));
        let idle_value = public_session(&idle);
        assert!(idle_value.get("busy_for_ms").is_none());
        assert!(idle_value.get("quiet_for_ms").is_none());
    }

    #[test]
    fn quiet_age_is_clamped_to_current_busy_interval() {
        assert_eq!(
            clamp_busy_metrics(500, Some(1_200)),
            BusyMetrics {
                busy_for_ms: 500,
                quiet_for_ms: 500,
            }
        );
        assert_eq!(
            clamp_busy_metrics(500, Some(250)),
            BusyMetrics {
                busy_for_ms: 500,
                quiet_for_ms: 250,
            }
        );
        assert_eq!(
            clamp_busy_metrics(500, None),
            BusyMetrics {
                busy_for_ms: 500,
                quiet_for_ms: 500,
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
    fn restart_interrupt_result_uses_a_structured_error_object() {
        let mut store = ready_store("codex");
        store
            .insert_turn("turn_restart", "codex:thread-1", "hello")
            .unwrap_or_else(|error| panic!("failed to insert turn: {error}"));
        assert_eq!(
            store.interrupt_active_turn(
                "codex:thread-1",
                "session restarted before execution completed"
            ),
            Some("turn_restart".to_owned())
        );
        let turn = store
            .get_turn("turn_restart")
            .unwrap_or_else(|| panic!("interrupted turn missing"));
        assert_eq!(
            public_result(&turn)["error"],
            json!({
                "code": "interrupted",
                "message": "session restarted before execution completed"
            })
        );
    }

    #[test]
    fn pending_submission_receipt_upgrades_across_session_rekey() {
        let mut store = ready_store("claude");
        store
            .insert_turn("turn_1", "claude:thread-1", "hello")
            .unwrap_or_else(|error| panic!("failed to insert turn: {error}"));
        assert!(store.mark_turn_started("turn_1", Some("provider-turn")));
        store
            .rekey_session("claude:thread-1", "claude:thread-2")
            .unwrap_or_else(|error| panic!("failed to rekey session: {error}"));
        let mut receipt = json!({
            "session": {"id": "claude:thread-1"},
            "execution_seq": 1,
            "submission": "pending",
            "hint": "wait",
            "action": "fetch"
        });

        refresh_submission_receipt_from_store(&store, &mut receipt);

        assert_eq!(receipt["submission"], "confirmed");
        assert!(receipt.get("hint").is_none());
        assert!(receipt.get("action").is_none());
    }

    #[test]
    fn confirmed_submission_receipt_never_downgrades_when_turn_is_gone() {
        let store = ready_store("claude");
        let mut receipt = json!({
            "session": {"id": "claude:thread-1"},
            "execution_seq": 99,
            "submission": "confirmed"
        });

        refresh_submission_receipt_from_store(&store, &mut receipt);

        assert_eq!(receipt["submission"], "confirmed");
    }

    fn fetch_options(max_bytes: usize, cursor: Option<crate::cursor::Cursor>) -> FetchOptions {
        FetchOptions {
            scope: "su_test".to_owned(),
            position: 1,
            incoming: None,
            cursor,
            wait: Duration::from_millis(0),
            message_limit: FETCH_MESSAGE_PAGE,
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
            denied_requests: 0,
            recorded_denials: 0,
            provider_denials: 0,
            error: None,
            created_at_ms: 0,
            started_at_ms: Some(0),
            completed_at_ms: Some(1),
            usage: None,
        }
    }

    fn lifecycle_events(range: std::ops::RangeInclusive<i64>) -> Vec<Value> {
        range
            .map(|seq| {
                json!({
                    "schema_version": 2,
                    "seq": seq,
                    "type": "session.busy",
                    "session_id": "claude:x",
                    "execution_seq": seq,
                })
            })
            .collect()
    }

    fn text_message(seq: u64, text: &str) -> Value {
        json!({"seq": seq, "execution_seq": 1, "kind": "assistant", "text": text})
    }

    fn message_seqs(value: &Value) -> Vec<u64> {
        value["sessions"][0]["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|message| message["seq"].as_u64())
            .collect()
    }

    struct SourceSpec {
        session: Value,
        requests: Vec<Value>,
        events: Vec<Value>,
        results: Vec<crate::protocol::TurnRecord>,
        messages: Vec<Value>,
        incoming: crate::cursor::SessionCursor,
    }

    impl SourceSpec {
        fn new(_uid: &str) -> Self {
            Self {
                session: json!({"id": "claude:x", "state": "idle"}),
                requests: Vec::new(),
                events: Vec::new(),
                results: Vec::new(),
                messages: Vec::new(),
                incoming: crate::cursor::SessionCursor::default(),
            }
        }

        fn build(self) -> BucketSource {
            BucketSource {
                session: self.session,
                requests: self.requests,
                requests_more: false,
                gaps: Vec::new(),
                events: self.events,
                results: self.results,
                results_more: false,
                messages: self.messages,
                messages_more: false,
                include_messages: true,
                incoming: self.incoming,
            }
        }
    }

    fn test_cut(sources: Vec<BucketSource>) -> Cut {
        let full_event_watermark = sources
            .iter()
            .flat_map(|source| source.events.iter())
            .filter_map(|event| event.get("seq").and_then(Value::as_i64))
            .max()
            .unwrap_or(0);
        Cut {
            baseline: false,
            state: Some(SessionState::Idle),
            bound_seq: None,
            bound_terminal: false,
            full_event_watermark,
            events_more: false,
            dropped_event_seq: None,
            scope_gaps: Vec::new(),
            sources,
        }
    }

    /// The published position, which a real daemon would have bound to the
    /// returned vector.
    fn position(value: &Value) -> u64 {
        value["cursor"]
            .as_str()
            .and_then(|position| position.parse().ok())
            .unwrap_or_else(|| panic!("no cursor position"))
    }

    /// Length of the complete compact response line a client would print.
    fn line_length(value: &Value) -> usize {
        serde_json::to_string(value)
            .map(|text| text.len())
            .unwrap_or_default()
            + cli_wrapper_overhead()
    }

    fn event_seqs(value: &Value) -> Vec<i64> {
        value["sessions"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|bucket| bucket["events"].as_array().into_iter().flatten())
            .filter_map(|event| event.get("seq").and_then(Value::as_i64))
            .collect()
    }

    #[test]
    fn a_response_publishes_its_position_as_a_plain_number() {
        let mut options = fetch_options(FETCH_HARD_MAX_BYTES, None);
        options.position = 42;
        let rendered = test_cut(vec![SourceSpec::new("su_test").build()])
            .render(&options)
            .unwrap_or_else(|error| panic!("failed to render: {error}"));

        // A caller carries this across turns, so it stays short and typed as
        // a string for field stability.
        assert_eq!(rendered.value["cursor"], json!("42"));
        assert_eq!(position(&rendered.value), 42);
    }

    #[test]
    fn the_event_watermark_never_passes_an_event_the_response_dropped() {
        let cut = test_cut(vec![
            SourceSpec {
                events: lifecycle_events(1..=8),
                ..SourceSpec::new("su_test")
            }
            .build(),
        ]);
        let rendered = cut
            .render(&fetch_options(900, None))
            .unwrap_or_else(|error| panic!("failed to render: {error}"));

        let delivered = event_seqs(&rendered.value);
        assert!(delivered.len() < 8, "budget did not drop any event");
        assert_eq!(rendered.value["has_more"], true);
        assert_eq!(
            rendered.cursor.e,
            delivered.last().copied().unwrap_or(0),
            "watermark passed an undelivered event"
        );
    }

    #[test]
    fn shedding_a_bucket_never_advances_the_event_watermark_past_it() {
        let first = SourceSpec {
            events: lifecycle_events(1..=2),
            ..SourceSpec::new("su_first")
        }
        .build();
        let mut second = SourceSpec {
            events: lifecycle_events(3..=4),
            ..SourceSpec::new("su_second")
        };
        // The second bucket cannot fit any plausible budget on its own.
        second.session = json!({"id": "claude:y", "title": "t".repeat(8_000)});
        let cut = test_cut(vec![first, second.build()]);
        let rendered = cut
            .render(&fetch_options(4_096, None))
            .unwrap_or_else(|error| panic!("failed to render: {error}"));

        let buckets = rendered.value["sessions"]
            .as_array()
            .unwrap_or_else(|| panic!("no sessions"))
            .len();
        assert_eq!(buckets, 1, "budget did not drop the second bucket");
        assert_eq!(rendered.value["has_more"], true);
        assert_eq!(
            rendered.cursor.e, 2,
            "watermark passed the dropped bucket's events"
        );
    }

    #[test]
    fn a_chunked_baseline_result_resumes_on_the_same_execution() {
        let text = "x".repeat(4_000);
        let incoming = crate::cursor::SessionCursor {
            x: 6,
            ..crate::cursor::SessionCursor::default()
        };
        let mut cut = test_cut(vec![
            SourceSpec {
                results: vec![test_turn(7, &text)],
                incoming,
                ..SourceSpec::new("su_test")
            }
            .build(),
        ]);
        cut.baseline = true;
        let first = cut
            .render(&fetch_options(1_500, None))
            .unwrap_or_else(|error| panic!("failed to render: {error}"));

        let result = &first.value["sessions"][0]["results"][0];
        assert_eq!(result["final_text_complete"], false);
        assert_eq!(result["final_text_offset"], 0);
        let cursor = first.cursor.clone();
        let position = cursor.session();
        assert_eq!(position.px, Some(7));
        assert_eq!(position.x, 6, "an incomplete result must not advance x");
        assert!(position.po > 0);

        let second = test_cut(vec![
            SourceSpec {
                results: vec![test_turn(7, &text)],
                incoming: position,
                ..SourceSpec::new("su_test")
            }
            .build(),
        ])
        .render(&fetch_options(FETCH_HARD_MAX_BYTES, Some(cursor)))
        .unwrap_or_else(|error| panic!("failed to render: {error}"));
        let result = &second.value["sessions"][0]["results"][0];
        assert_eq!(result["final_text_complete"], true);
        assert_eq!(result["final_text_offset"], json!(position.po));
        let position = second.cursor.session();
        assert_eq!(position.x, 7);
        assert_eq!(position.px, None);
    }

    #[test]
    fn wait_reports_a_result_only_once_it_is_fully_delivered() {
        let text = "y".repeat(4_000);
        let source = || {
            SourceSpec {
                results: vec![test_turn(7, &text)],
                ..SourceSpec::new("su_test")
            }
            .build()
        };
        let mut cut = test_cut(vec![source()]);
        cut.bound_seq = Some(7);
        cut.bound_terminal = true;
        let mut options = fetch_options(1_500, None);
        options.wait = Duration::from_secs(1);
        let partial = cut
            .render(&options)
            .unwrap_or_else(|error| panic!("failed to render: {error}"));
        assert_eq!(partial.value["reason"], "page_full");
        assert!(partial.settled, "a page-full response must return at once");

        let mut cut = test_cut(vec![source()]);
        cut.bound_seq = Some(7);
        cut.bound_terminal = true;
        let mut options = fetch_options(FETCH_HARD_MAX_BYTES, None);
        options.wait = Duration::from_secs(1);
        assert_eq!(
            cut.render(&options)
                .unwrap_or_else(|error| panic!("failed to render: {error}"))
                .value["reason"],
            "result"
        );
    }

    #[test]
    fn a_scope_gap_survives_a_response_with_no_session_buckets() {
        let mut cut = test_cut(Vec::new());
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
    fn a_budget_too_small_for_any_progress_is_rejected() {
        let cut = test_cut(vec![
            SourceSpec {
                results: vec![test_turn(1, "answer")],
                ..SourceSpec::new("su_test")
            }
            .build(),
        ]);
        let error = cut
            .render(&fetch_options(64, None))
            .err()
            .unwrap_or_else(|| panic!("an unsatisfiable budget was accepted"));
        assert_eq!(classify_error(&error), "INVALID_ARGUMENT");
        assert!(
            error
                .to_string()
                .contains("is verified to work for the current state"),
            "the error must name a workable budget: {error}"
        );
    }

    /// Drive one synthetic history to quiescence and return what was
    /// delivered, asserting the byte bound on every response.
    fn drain_history(
        events: &[Value],
        result: &crate::protocol::TurnRecord,
        messages: &[Value],
        max_bytes: usize,
    ) -> (Vec<i64>, String, Vec<u64>) {
        let mut cursor: Option<crate::cursor::Cursor> = None;
        let mut seen_events = Vec::new();
        let mut body = String::new();
        let mut seen_messages = Vec::new();
        for _ in 0..10_000 {
            let options = fetch_options(max_bytes, cursor.clone());
            let position = options.position();
            let after = options.event_position();
            let pending_events = events
                .iter()
                .filter(|event| event["seq"].as_i64().unwrap_or(0) > after)
                .cloned()
                .collect::<Vec<_>>();
            let events_more = pending_events.len() > FETCH_EVENT_PAGE;
            let pending_messages = messages
                .iter()
                .filter(|message| message["seq"].as_u64().unwrap_or(0) > position.m)
                .cloned()
                .collect::<Vec<_>>();
            let mut cut = test_cut(vec![
                SourceSpec {
                    events: pending_events.into_iter().take(FETCH_EVENT_PAGE).collect(),
                    results: if position.x >= result.execution_seq {
                        Vec::new()
                    } else {
                        vec![result.clone()]
                    },
                    messages: pending_messages
                        .iter()
                        .take(FETCH_MESSAGE_PAGE)
                        .cloned()
                        .collect(),
                    incoming: position,
                    ..SourceSpec::new("su_test")
                }
                .build(),
            ]);
            cut.sources[0].messages_more = pending_messages.len() > FETCH_MESSAGE_PAGE;
            cut.events_more = events_more;
            cut.full_event_watermark = events
                .iter()
                .filter_map(|event| event["seq"].as_i64())
                .filter(|seq| *seq > after)
                .take(FETCH_EVENT_PAGE)
                .last()
                .unwrap_or(after);
            let rendered = cut
                .render(&options)
                .unwrap_or_else(|error| panic!("failed to render at {max_bytes}: {error}"));
            let encoded = serde_json::to_string(&rendered.value)
                .unwrap_or_else(|error| panic!("failed to serialize: {error}"));
            assert!(
                encoded.len() <= max_bytes,
                "response of {} bytes exceeded the {max_bytes} byte budget",
                encoded.len()
            );
            seen_events.extend(event_seqs(&rendered.value));
            for chunk in rendered.value["sessions"][0]["results"]
                .as_array()
                .into_iter()
                .flatten()
            {
                body.push_str(chunk["final_text"].as_str().unwrap_or_default());
            }
            for message in rendered.value["sessions"][0]["messages"]
                .as_array()
                .into_iter()
                .flatten()
            {
                assert!(
                    messages.contains(message),
                    "a message was altered or split: {message}"
                );
                seen_messages.push(message["seq"].as_u64().unwrap_or(0));
            }
            cursor = Some(rendered.cursor.clone());
            if rendered.value["has_more"] != json!(true) {
                return (seen_events, body, seen_messages);
            }
        }
        panic!("history never reached quiescence at {max_bytes} bytes");
    }

    #[test]
    fn a_squeezed_response_drops_messages_first_and_never_splits_one() {
        let messages = (1..=3)
            .map(|seq| text_message(seq, &"m".repeat(1_500)))
            .collect::<Vec<_>>();
        let cut = || {
            test_cut(vec![
                SourceSpec {
                    events: lifecycle_events(1..=2),
                    results: vec![test_turn(1, "the answer")],
                    messages: messages.clone(),
                    ..SourceSpec::new("su_test")
                }
                .build(),
            ])
        };
        let roomy = cut()
            .render(&fetch_options(FETCH_HARD_MAX_BYTES, None))
            .unwrap_or_else(|error| panic!("failed to render: {error}"));
        assert_eq!(message_seqs(&roomy.value), [1, 2, 3]);
        assert_eq!(roomy.cursor.p.m, 3);

        let tight = cut()
            .render(&fetch_options(2_600, None))
            .unwrap_or_else(|error| panic!("failed to render: {error}"));
        let bucket = &tight.value["sessions"][0];
        assert_eq!(bucket["results"][0]["final_text"], "the answer");
        assert_eq!(event_seqs(&tight.value), [1, 2]);
        assert_eq!(message_seqs(&tight.value), [1], "only whole messages ship");
        assert_eq!(bucket["messages"][0], messages[0]);
        assert_eq!(tight.cursor.p.m, 1, "the watermark stops at what shipped");
        assert_eq!(tight.value["reason"], "page_full");
        assert_eq!(tight.value["has_more"], true);
    }

    #[test]
    fn new_messages_alone_never_complete_a_long_poll() {
        let cut = |bound_terminal: bool| {
            let mut cut = test_cut(vec![
                SourceSpec {
                    messages: vec![text_message(1, "still working")],
                    ..SourceSpec::new("su_test")
                }
                .build(),
            ]);
            cut.state = Some(SessionState::Busy);
            cut.bound_seq = Some(1);
            cut.bound_terminal = bound_terminal;
            cut
        };
        let mut waiting = fetch_options(FETCH_HARD_MAX_BYTES, None);
        waiting.wait = Duration::from_mins(1);
        let rendered = cut(false)
            .render(&waiting)
            .unwrap_or_else(|error| panic!("failed to render: {error}"));
        assert!(!rendered.settled, "a message woke a waiting fetch");
        assert_eq!(
            message_seqs(&rendered.value),
            [1],
            "the message still rides along when the deadline fires"
        );

        let immediate = cut(false)
            .render(&fetch_options(FETCH_HARD_MAX_BYTES, None))
            .unwrap_or_else(|error| panic!("failed to render: {error}"));
        assert!(immediate.settled);
        assert_eq!(immediate.value["reason"], "change");
    }

    #[test]
    fn pending_requests_ship_with_state_and_are_flagged_when_they_do_not_fit() {
        let request = |id: &str| json!({"id": id, "kind": "approval", "tool": "Bash", "summary": "s".repeat(900)});
        let cut = || {
            let mut cut = test_cut(vec![
                SourceSpec {
                    requests: vec![request("r1"), request("r2")],
                    messages: vec![text_message(1, "asking")],
                    ..SourceSpec::new("su_test")
                }
                .build(),
            ]);
            cut.state = Some(SessionState::Blocked);
            cut
        };
        let mut waiting = fetch_options(FETCH_HARD_MAX_BYTES, None);
        waiting.wait = Duration::from_mins(1);
        let rendered = cut()
            .render(&waiting)
            .unwrap_or_else(|error| panic!("failed to render: {error}"));
        assert!(rendered.settled, "a pending Request returns early");
        assert_eq!(rendered.value["reason"], "blocked");
        let bucket = &rendered.value["sessions"][0];
        assert_eq!(bucket["requests"].as_array().map(Vec::len), Some(2));
        assert!(bucket.get("requests_truncated").is_none());

        let tight = cut()
            .render(&fetch_options(1_400, None))
            .unwrap_or_else(|error| panic!("failed to render: {error}"));
        let bucket = &tight.value["sessions"][0];
        assert_eq!(bucket["requests"].as_array().map(Vec::len), Some(1));
        assert_eq!(bucket["requests"][0]["id"], "r1");
        assert_eq!(bucket["requests_truncated"], true);
        assert_eq!(tight.value["reason"], "blocked");
    }

    #[test]
    fn blocked_fetch_never_reports_an_empty_complete_request_set() {
        let cut = || {
            let mut cut = test_cut(vec![
                SourceSpec {
                    requests: vec![json!({
                        "id": "r1", "kind": "question", "tool": "elicitation",
                        "summary": "Complete the profile", "fields": [
                            {"name": "name", "type": "string", "required": true}
                        ]
                    })],
                    ..SourceSpec::new("su_test")
                }
                .build(),
            ]);
            cut.state = Some(SessionState::Blocked);
            cut
        };
        let rendered = cut()
            .render(&fetch_options(700, None))
            .unwrap_or_else(|error| panic!("700-byte answerable Request should fit: {error}"));
        assert_eq!(rendered.value["sessions"][0]["requests"][0]["id"], "r1");

        let error = cut()
            .render(&fetch_options(200, None))
            .err()
            .unwrap_or_else(|| panic!("blocked fetch accepted a budget too small for its Request"));
        let message = error.to_string();
        let minimum = message
            .rsplit("--max-bytes ")
            .next()
            .and_then(|tail| tail.split_whitespace().next())
            .and_then(|number| number.parse::<usize>().ok())
            .unwrap_or_else(|| panic!("missing verified budget in {message}"));
        let rendered = cut()
            .render(&fetch_options(minimum, None))
            .unwrap_or_else(|error| panic!("reported budget {minimum} failed: {error}"));
        assert_eq!(rendered.value["sessions"][0]["requests"][0]["id"], "r1");
        assert_eq!(rendered.value["reason"], "blocked");
    }

    #[test]
    fn a_blocked_fetch_with_300kb_tool_and_cwd_fits_the_hard_request_budget() {
        let mut requests = crate::requests::RequestTable::default();
        let request = requests
            .open(
                crate::requests::RequestDraft {
                    kind: crate::requests::RequestKind::Approval,
                    provider_key: "provider-1".to_owned(),
                    provider: crate::requests::ProviderRequest::CodexCommand,
                    tool: "T".repeat(300_000),
                    summary: "A short command".to_owned(),
                    input: json!({"command": "echo hello"}),
                    cwd: Some(format!("/{}", "c".repeat(300_000))),
                    reason: None,
                    text: None,
                    choices: Vec::new(),
                },
                Some(1),
                1,
            )
            .public();
        let mut cut = test_cut(vec![
            SourceSpec {
                requests: vec![request],
                ..SourceSpec::new("su_test")
            }
            .build(),
        ]);
        cut.state = Some(SessionState::Blocked);

        let rendered = cut
            .render(&fetch_options(FETCH_HARD_MAX_BYTES, None))
            .unwrap_or_else(|error| panic!("hard-limit Request failed: {error}"));
        let bucket = &rendered.value["sessions"][0];
        assert_eq!(bucket["requests"][0]["id"], "r1");
        assert!(line_length(&rendered.value) <= FETCH_HARD_MAX_BYTES);
    }

    #[test]
    fn a_maximal_session_and_request_fit_one_hard_limit_fetch() {
        let worst = |limit: usize| "\u{1}".repeat(limit);
        let [title, alias, model, effort, cwd] =
            super::SESSION_METADATA_LIMITS.map(|(_, limit)| worst(limit));
        assert!(
            super::validate_session_metadata(&json!({
                "title": title, "alias": alias, "model": model, "effort": effort, "cwd": cwd
            }))
            .is_ok()
        );
        assert!(super::validate_session_metadata(&json!({"title": "t".repeat(300_000)})).is_err());
        let store = Store::new();
        store
            .insert_session(&NewSession {
                id: "claude:thread-1",
                alias: &alias,
                title: &title,
                agent: "claude",
                cwd: &cwd,
                model: Some(&model),
                effort: Some(&effort),
                harness_options: &[],
                approvals: ApprovalsMode::Ask,
            })
            .unwrap_or_else(|error| panic!("failed to insert session: {error}"));
        let session = store
            .get_session("claude:thread-1")
            .unwrap_or_else(|| panic!("session missing"));
        let request = crate::requests::RequestTable::default()
            .open(
                crate::requests::RequestDraft {
                    kind: crate::requests::RequestKind::Approval,
                    provider_key: "provider-1".to_owned(),
                    provider: crate::requests::ProviderRequest::CodexCommand,
                    tool: worst(300_000),
                    summary: worst(300_000),
                    input: json!({"command": worst(300_000)}),
                    cwd: Some(worst(300_000)),
                    reason: Some(worst(300_000)),
                    text: None,
                    choices: Vec::new(),
                },
                Some(1),
                1,
            )
            .public();
        let mut cut = test_cut(vec![
            SourceSpec {
                session: public_session(&session),
                requests: vec![request],
                ..SourceSpec::new("su_test")
            }
            .build(),
        ]);
        cut.state = Some(SessionState::Blocked);

        let rendered = cut
            .render(&fetch_options(FETCH_HARD_MAX_BYTES, None))
            .unwrap_or_else(|error| panic!("maximal Session and Request did not fit: {error}"));
        assert_eq!(rendered.value["sessions"][0]["requests"][0]["id"], "r1");
        assert!(line_length(&rendered.value) <= FETCH_HARD_MAX_BYTES);
    }

    #[test]
    fn a_request_at_the_reported_minimum_budget_succeeds() {
        let cut = || {
            test_cut(vec![
                SourceSpec {
                    results: vec![test_turn(1, &"answer ".repeat(400))],
                    ..SourceSpec::new("su_test")
                }
                .build(),
            ])
        };
        let error = cut()
            .render(&fetch_options(64, None))
            .err()
            .unwrap_or_else(|| panic!("an unsatisfiable budget was accepted"));
        let message = error.to_string();
        let minimum = message
            .rsplit("--max-bytes ")
            .next()
            .and_then(|tail| tail.split(' ').next())
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or_else(|| panic!("no minimum in {message}"));

        let rendered = cut()
            .render(&fetch_options(minimum, None))
            .unwrap_or_else(|error| panic!("the reported minimum {minimum} failed: {error}"));
        let length = serde_json::to_string(&rendered.value)
            .map(|text| text.len())
            .unwrap_or_default();
        assert!(
            length <= minimum,
            "{length} exceeded the reported {minimum}"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn every_reason_variant_respects_an_exact_budget() {
        type Variant = (&'static str, Box<dyn Fn() -> Cut>);
        let variants: Vec<Variant> = vec![
            (
                "snapshot",
                Box::new(|| {
                    let mut cut = test_cut(vec![SourceSpec::new("su_test").build()]);
                    cut.baseline = true;
                    cut
                }),
            ),
            (
                "page_full",
                Box::new(|| {
                    let mut cut = test_cut(vec![
                        SourceSpec {
                            events: lifecycle_events(1..=8),
                            ..SourceSpec::new("su_test")
                        }
                        .build(),
                    ]);
                    cut.events_more = true;
                    cut
                }),
            ),
            (
                "blocked",
                Box::new(|| {
                    let mut cut = test_cut(vec![SourceSpec::new("su_test").build()]);
                    cut.state = Some(SessionState::Blocked);
                    cut
                }),
            ),
            (
                "gap",
                Box::new(|| {
                    let mut cut = test_cut(vec![SourceSpec::new("su_test").build()]);
                    cut.scope_gaps.push(retention_gap("events"));
                    cut
                }),
            ),
            (
                "timeout",
                Box::new(|| test_cut(vec![SourceSpec::new("su_test").build()])),
            ),
            (
                "change",
                Box::new(|| {
                    test_cut(vec![
                        SourceSpec {
                            events: lifecycle_events(1..=3),
                            ..SourceSpec::new("su_test")
                        }
                        .build(),
                    ])
                }),
            ),
            (
                "result",
                Box::new(|| {
                    let mut cut = test_cut(vec![
                        SourceSpec {
                            results: vec![test_turn(7, "the answer")],
                            ..SourceSpec::new("su_test")
                        }
                        .build(),
                    ]);
                    cut.bound_seq = Some(7);
                    cut.bound_terminal = true;
                    cut
                }),
            ),
        ];
        for (name, build) in variants {
            let mut options = fetch_options(FETCH_HARD_MAX_BYTES, None);
            options.wait = if name == "result" {
                Duration::from_secs(1)
            } else {
                Duration::default()
            };
            let relaxed = build()
                .render(&options)
                .unwrap_or_else(|error| panic!("{name} failed to render: {error}"));
            assert_eq!(relaxed.value["reason"], json!(name));
            // The budget covers the complete response line, wrapper included.
            let exact = line_length(&relaxed.value);

            // At exactly that budget the response must render unchanged.
            for budget in [exact, exact + 64] {
                let mut options = fetch_options(budget, None);
                options.wait = if name == "result" {
                    Duration::from_secs(1)
                } else {
                    Duration::default()
                };
                let rendered = build()
                    .render(&options)
                    .unwrap_or_else(|error| panic!("{name} failed at {budget}: {error}"));
                assert_eq!(
                    rendered.value["reason"],
                    json!(name),
                    "{name} changed reason at {budget}"
                );
                assert!(
                    line_length(&rendered.value) <= budget,
                    "{name} exceeded a {budget} byte budget"
                );
            }

            // One byte short may legitimately fail or shed content, but must
            // never overflow.
            let mut options = fetch_options(exact.saturating_sub(1), None);
            options.wait = if name == "result" {
                Duration::from_secs(1)
            } else {
                Duration::default()
            };
            if let Ok(rendered) = build().render(&options) {
                assert!(
                    line_length(&rendered.value) < exact,
                    "{name} exceeded a {} byte budget",
                    exact - 1
                );
            }
        }
    }

    #[test]
    fn the_reported_minimum_holds_for_a_messages_only_response() {
        let cut = || {
            test_cut(vec![
                SourceSpec {
                    messages: vec![text_message(1, &"q".repeat(4_000))],
                    ..SourceSpec::new("su_test")
                }
                .build(),
            ])
        };
        let options = fetch_options(32, None);
        let error = cut()
            .render(&options)
            .err()
            .unwrap_or_else(|| panic!("an unusable budget was accepted"));
        let message = error.to_string();
        let minimum = message
            .rsplit("--max-bytes ")
            .next()
            .and_then(|tail| tail.split(' ').next())
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or_else(|| panic!("no minimum in {message}"));
        let at_minimum = FetchOptions {
            max_bytes: minimum,
            ..options
        };
        let rendered = cut()
            .render(&at_minimum)
            .unwrap_or_else(|error| panic!("minimum {minimum} failed: {error}"));
        assert!(
            line_length(&rendered.value) <= minimum,
            "the response exceeded the reported minimum {minimum}"
        );
        assert_eq!(message_seqs(&rendered.value), [1]);
    }

    #[test]
    #[allow(clippy::too_many_lines, clippy::items_after_statements)]
    fn a_reported_minimum_budget_always_renders() {
        // Each of these fails at a tiny budget for a different reason: an
        // oversized Session record, an oversized result body, a wide message.
        type Case = (&'static str, Box<dyn Fn() -> Cut>, FetchOptions);
        let cases: Vec<Case> = vec![
            (
                "wide session record",
                Box::new(|| {
                    let mut source = SourceSpec {
                        events: lifecycle_events(1..=4),
                        ..SourceSpec::new("su_test")
                    };
                    source.session = json!({"id": "claude:x", "title": "t".repeat(6_000)});
                    test_cut(vec![source.build()])
                }),
                fetch_options(16, None),
            ),
            (
                "oversized result body",
                Box::new(|| {
                    test_cut(vec![
                        SourceSpec {
                            results: vec![test_turn(1, &"body ".repeat(20_000))],
                            ..SourceSpec::new("su_test")
                        }
                        .build(),
                    ])
                }),
                fetch_options(16, None),
            ),
            (
                "wide message",
                Box::new(|| {
                    test_cut(vec![
                        SourceSpec {
                            messages: vec![text_message(1, &"w".repeat(4_096))],
                            ..SourceSpec::new("su_test")
                        }
                        .build(),
                    ])
                }),
                fetch_options(16, None),
            ),
        ];

        for (name, cut, options) in cases {
            let error = cut()
                .render(&options)
                .err()
                .unwrap_or_else(|| panic!("{name}: a 16 byte budget was accepted"));
            let message = error.to_string();
            let minimum = message
                .rsplit("--max-bytes ")
                .next()
                .and_then(|tail| tail.split(' ').next())
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or_else(|| panic!("{name}: no recommendation in {message}"));
            assert!(
                minimum > options.max_bytes,
                "{name}: recommended {minimum}, which is not above the budget that failed"
            );

            // The recommendation is only useful if it actually renders.
            let retry = FetchOptions {
                max_bytes: minimum,
                ..options.clone()
            };
            let rendered = cut()
                .render(&retry)
                .unwrap_or_else(|error| panic!("{name}: minimum {minimum} failed: {error}"));
            assert!(
                line_length(&rendered.value) <= minimum,
                "{name}: the response exceeded its own recommendation"
            );
        }
    }

    #[test]
    fn every_delivered_window_covers_the_full_history_exactly() {
        let events = lifecycle_events(1..=40);
        let result = test_turn(1, &"body ".repeat(3_000));
        let messages = (1..=300_u64)
            .map(|seq| text_message(seq, &"w".repeat(usize::try_from(seq % 97).unwrap_or(0))))
            .collect::<Vec<_>>();
        // A deterministic spread of tight budgets.
        let mut seed = 0x2545_f491_4f6c_dd1d_u64;
        for _ in 0..12 {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let max_bytes = 1_400 + usize::try_from(seed >> 33).unwrap_or(0) % 6_000;
            let (delivered, body, read) = drain_history(&events, &result, &messages, max_bytes);

            assert!(
                delivered.windows(2).all(|pair| pair[0] < pair[1]),
                "events were duplicated or reordered at {max_bytes}: {delivered:?}"
            );
            assert_eq!(
                delivered,
                (1..=40).collect::<Vec<_>>(),
                "event loss at {max_bytes}"
            );
            assert_eq!(
                body,
                result.final_message.clone().unwrap_or_default(),
                "result body loss at {max_bytes}"
            );
            assert_eq!(
                read,
                (1..=300).collect::<Vec<_>>(),
                "messages were lost, duplicated, or reordered at {max_bytes}"
            );
        }
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
}
