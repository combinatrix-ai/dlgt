use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::paths;
use crate::protocol::{Request, Response};
use crate::raw_mode::{RawModeGuard, terminal_size};

static LAST_INFO: std::sync::Mutex<Option<Value>> = std::sync::Mutex::new(None);

fn set_info(info: Option<Value>) {
    if let Ok(mut slot) = LAST_INFO.lock() {
        *slot = info;
    }
}

#[derive(Debug)]
pub struct RpcFailure {
    pub code: String,
    pub message: String,
    pub session_id: Option<String>,
    pub launch_id: Option<String>,
    pub correlation_id: Option<String>,
    pub hint: Option<String>,
    pub session_state: Option<String>,
    pub action: Option<String>,
}

impl std::fmt::Display for RpcFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for RpcFailure {}

pub fn call(method: &str, params: Value) -> Result<Value> {
    let socket = paths::socket_path()?;
    let mut stream = connect_or_start(&socket)?;
    let request = Request {
        id: format!("req_{}", Uuid::new_v4().simple()),
        method: method.to_owned(),
        params,
    };
    write_json_line(&mut stream, &request)?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .context("failed to read daemon response")?;
    decode_response(&line)
}

pub fn call_existing(method: &str, params: Value) -> Result<Value> {
    let socket = paths::socket_path()?;
    call_socket(&socket, method, params)
}

pub(crate) fn call_socket(socket: &Path, method: &str, params: Value) -> Result<Value> {
    call_socket_with_timeout(socket, method, params, None)
}

fn call_socket_with_timeout(
    socket: &Path,
    method: &str,
    params: Value,
    timeout: Option<Duration>,
) -> Result<Value> {
    let mut stream =
        UnixStream::connect(socket).map_err(|error| socket_unavailable(socket, error))?;
    stream
        .set_read_timeout(timeout)
        .context("failed to configure daemon socket read timeout")?;
    stream
        .set_write_timeout(timeout)
        .context("failed to configure daemon socket write timeout")?;
    let request = Request {
        id: format!("req_{}", Uuid::new_v4().simple()),
        method: method.to_owned(),
        params,
    };
    write_json_line(&mut stream, &request)?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .context("failed to read daemon response")?;
    decode_response(&line)
}

#[derive(Debug)]
pub struct LiveSessionRoute {
    pub socket: std::path::PathBuf,
    pub session_id: String,
}

pub fn find_live_session(selector: &str) -> Result<Option<LiveSessionRoute>> {
    let canonical = selector
        .split_once(':')
        .filter(|(harness, id)| matches!(*harness, "codex" | "claude") && !id.is_empty());
    if canonical.is_none() {
        return Ok(None);
    }

    let mut routes = Vec::new();
    for socket in paths::runtime_sockets()? {
        let result = match call_socket_with_timeout(
            &socket,
            "session.list",
            json!({"all":false}),
            Some(Duration::from_secs(2)),
        ) {
            Ok(result) => result,
            Err(error) if socket_is_stale(&error) => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to inspect live dlgt runtime at {}",
                        socket.display()
                    )
                });
            }
        };
        let Some(sessions) = result.as_array() else {
            continue;
        };
        for session in sessions {
            let session_id = session.get("id").and_then(Value::as_str);
            if session_id == Some(selector)
                && let Some(session_id) = session_id
            {
                routes.push(LiveSessionRoute {
                    socket: socket.clone(),
                    session_id: session_id.to_owned(),
                });
            }
        }
    }
    if routes.len() > 1 {
        bail!(
            "selector {selector:?} is live in multiple dlgt runtimes; stop the duplicate before sending work"
        );
    }
    Ok(routes.pop())
}

fn socket_is_stale(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<io::Error>().is_some_and(|error| {
            matches!(
                error.kind(),
                io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
            )
        })
    })
}

pub fn list_all_versions(include_all: bool) -> Result<Vec<Value>> {
    let mut sessions = Vec::new();
    let current_socket = paths::socket_path()?;
    let mut current_info = None;
    for socket in paths::runtime_sockets()? {
        let ping = match call_socket(&socket, "server.ping", json!({})) {
            Ok(ping) => ping,
            Err(error) if socket_is_stale(&error) => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to inspect live dlgt runtime at {}",
                        socket.display()
                    )
                });
            }
        };
        let Some(version) = ping.get("version").and_then(Value::as_str) else {
            continue;
        };
        let result = match call_socket(&socket, "session.list", json!({"all":include_all})) {
            Ok(result) => result,
            Err(error) if socket_is_stale(&error) => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to inspect live dlgt runtime at {}",
                        socket.display()
                    )
                });
            }
        };
        let runtime_info = take_info();
        if socket == current_socket {
            current_info = runtime_info;
        }
        let Some(runtime_sessions) = result.as_array() else {
            continue;
        };
        for session in runtime_sessions {
            let mut session = session.clone();
            if let Some(object) = session.as_object_mut() {
                object.insert("runtime_version".to_owned(), json!(version));
                object.insert("runtime_socket".to_owned(), json!(socket));
            }
            sessions.push(session);
        }
    }
    set_info(current_info);
    Ok(sessions)
}

pub fn attach(selector: &str, steal: bool) -> Result<()> {
    let lease_id = format!("lease_{}", Uuid::new_v4().simple());
    let (rows, cols) = terminal_size(libc::STDIN_FILENO);
    call(
        "session.resize",
        json!({"session": selector, "rows": rows, "cols": cols}),
    )?;

    let socket = paths::socket_path()?;
    let mut stream = connect_or_start(&socket)?;
    let request = Request {
        id: format!("req_{}", Uuid::new_v4().simple()),
        method: "view.subscribe".to_owned(),
        params: json!({"session": selector, "steal": steal, "lease_id": lease_id}),
    };
    write_json_line(&mut stream, &request)?;
    let mut reader = BufReader::new(stream);
    let mut response_line = String::new();
    reader
        .read_line(&mut response_line)
        .context("failed to read view subscription response")?;
    let result = decode_response(&response_line)?;
    let replay = result
        .get("replay_base64")
        .and_then(Value::as_str)
        .map_or_else(
            || Ok(Vec::new()),
            |encoded| {
                base64::engine::general_purpose::STANDARD
                    .decode(encoded)
                    .context("invalid replay payload")
            },
        )?;
    io::stdout().write_all(&replay)?;
    io::stdout().flush()?;

    let output_thread = std::thread::Builder::new()
        .name("dlgt-attach-output".to_owned())
        .spawn(move || {
            let mut stdout = io::stdout().lock();
            let _ = io::copy(&mut reader, &mut stdout);
            let _ = stdout.flush();
        })
        .context("failed to start attach output thread")?;

    // When stdin is not a TTY (for example in a scripted smoke test), leave it
    // in its current mode and still forward the bytes.
    let raw_guard = if unsafe { libc::isatty(libc::STDIN_FILENO) } == 1 {
        Some(RawModeGuard::enter(libc::STDIN_FILENO)?)
    } else {
        None
    };
    let mut input = io::stdin().lock();
    let mut buffer = [0_u8; 4096];
    let mut prefix = false;
    'input: loop {
        let read = input
            .read(&mut buffer)
            .context("failed to read attach input")?;
        if read == 0 {
            break;
        }
        let (forward, detach) = filter_attach_input(&buffer[..read], &mut prefix);
        if !forward.is_empty() {
            send_input(selector, &forward, "attach", Some(&lease_id))?;
        }
        if detach {
            break 'input;
        }
    }
    if prefix {
        send_input(selector, &[0x02], "attach", Some(&lease_id))?;
    }
    drop(raw_guard);
    drop(output_thread);
    eprintln!("\ndetached from {selector}");
    Ok(())
}

pub fn send_input(
    selector: &str,
    data: &[u8],
    source: &str,
    lease_id: Option<&str>,
) -> Result<Value> {
    call(
        "session.input",
        json!({
            "session": selector,
            "data_base64": base64::engine::general_purpose::STANDARD.encode(data),
            "source": source,
            "lease_id": lease_id,
        }),
    )
}

pub fn rpc_stdio() -> Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line.context("failed to read RPC stdin")?;
        if line.trim().is_empty() {
            continue;
        }
        let request: Request = serde_json::from_str(&line).context("invalid RPC request")?;
        if request.id_too_long() {
            write_json_line(
                &mut stdout,
                &Response::error(
                    request.short_id(),
                    "INVALID_ARGUMENT",
                    format!(
                        "request id must be at most {} bytes",
                        crate::protocol::MAX_REQUEST_ID_LEN
                    ),
                ),
            )?;
            continue;
        }
        if !public_rpc_method(&request.method) {
            write_json_line(
                &mut stdout,
                &Response::error(
                    request.id,
                    "INVALID_ARGUMENT",
                    "method is not public RPC v1",
                ),
            )?;
            continue;
        }
        let response = match if request.method == "profile.list" {
            read_profiles()
        } else {
            call(&request.method, request.params)
        } {
            Ok(result) => Response::ok(request.id, result).with_info(take_info()),
            Err(error) => error.downcast_ref::<RpcFailure>().map_or_else(
                || Response::error(&request.id, "RPC_UNAVAILABLE", error.to_string()),
                |failure| {
                    if failure.session_id.is_some() || failure.launch_id.is_some() {
                        Response::session_error(
                            &request.id,
                            &failure.code,
                            &failure.message,
                            failure.session_id.clone(),
                            failure.launch_id.clone(),
                        )
                    } else {
                        Response::error(&request.id, &failure.code, &failure.message)
                    }
                },
            ),
        };
        write_json_line(&mut stdout, &response)?;
    }
    Ok(())
}

fn connect_or_start(socket: &Path) -> Result<UnixStream> {
    match UnixStream::connect(socket) {
        Ok(stream) => return Ok(stream),
        Err(error) if !is_auto_start_connect_error(&error) => {
            return Err(socket_unavailable(socket, error));
        }
        Err(_) => {}
    }
    paths::check_socket_path(socket)?;
    let log = start_daemon().map_err(|error| daemon_start_failed(socket, format!("{error:#}")))?;
    let mut last = None;
    for _ in 0..40 {
        match UnixStream::connect(socket) {
            Ok(stream) => return Ok(stream),
            Err(error) if is_auto_start_connect_error(&error) => last = Some(error),
            Err(error) => return Err(socket_unavailable(socket, error)),
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // The child's own failure is the useful part; the connect error alone
    // only ever says "no such file".
    let reason = log
        .as_deref()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
        .or_else(|| last.map(|error| error.to_string()))
        .unwrap_or_else(|| "no diagnostic output".to_owned());
    Err(daemon_start_failed(socket, reason))
}

fn is_auto_start_connect_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
    )
}

fn socket_unavailable(socket: &Path, error: io::Error) -> anyhow::Error {
    let offer_sandbox_retry = is_permission_error(&error);
    let failure = rpc_unavailable_failure(
        format!(
            "cannot access the dlgt server socket at {}: {}",
            socket.display(),
            error
        ),
        offer_sandbox_retry,
    );
    anyhow::Error::new(error).context(failure)
}

fn daemon_start_failed(socket: &Path, reason: impl Into<String>) -> anyhow::Error {
    let reason = reason.into();
    let offer_sandbox_retry = diagnostic_mentions_permission(&reason);
    rpc_unavailable_failure(
        format!(
            "dlgt server did not start at {}: {reason}",
            socket.display()
        ),
        offer_sandbox_retry,
    )
    .into()
}

const SANDBOX_RETRY_HINT: &str = concat!(
    "retry the same dlgt command outside the Codex sandbox via ",
    "tools.exec_command(... sandbox_permissions: \"require_escalated\"); ",
    "DLGT_HOME and DLGT_SOCKET must not be changed",
);

fn rpc_unavailable_failure(message: String, offer_sandbox_retry: bool) -> RpcFailure {
    let message = if offer_sandbox_retry {
        format!("{message}; {SANDBOX_RETRY_HINT}")
    } else {
        message
    };
    RpcFailure {
        code: "RPC_UNAVAILABLE".to_owned(),
        message,
        session_id: None,
        launch_id: None,
        correlation_id: None,
        hint: offer_sandbox_retry.then(|| SANDBOX_RETRY_HINT.to_owned()),
        session_state: None,
        action: None,
    }
}

fn is_permission_error(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::PermissionDenied
}

fn diagnostic_mentions_permission(diagnostic: &str) -> bool {
    let diagnostic = diagnostic.to_ascii_lowercase();
    ["permission denied", "operation not permitted", "eperm"]
        .iter()
        .any(|marker| diagnostic.contains(marker))
}

/// Spawn the daemon, returning the file its startup diagnostics go to.
fn start_daemon() -> Result<Option<std::path::PathBuf>> {
    let executable = std::env::current_exe().context("failed to locate dlgt executable")?;
    let mut command = Command::new(executable);
    let log_path = paths::socket_path()
        .ok()
        .and_then(|socket| socket.parent().map(|parent| parent.join("daemon.log")));
    let log = log_path.as_ref().and_then(|path| {
        path.parent().map(std::fs::create_dir_all);
        std::fs::File::create(path).ok()
    });
    // Only read a path back when this launch actually truncated and owns its
    // diagnostic file. Otherwise a failed open could expose stale output from
    // an unrelated daemon attempt.
    let diagnostic_path = log.as_ref().and_then(|_| log_path.clone());
    command
        .args(["server", "--daemon-child"])
        .stdin(Stdio::null());
    configure_daemon_output(&mut command, log)?;
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // SAFETY: setsid has no memory-safety preconditions and is called in
        // the child between fork and exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    command.spawn().context("failed to start dlgt server")?;
    Ok(diagnostic_path)
}

fn configure_daemon_output(command: &mut Command, log: Option<std::fs::File>) -> Result<()> {
    let (stdout, stderr) = if let Some(log) = log {
        let stdout = log
            .try_clone()
            .context("failed to duplicate daemon diagnostic log")?;
        (Stdio::from(stdout), Stdio::from(log))
    } else {
        (Stdio::null(), Stdio::null())
    };
    command.stdout(stdout).stderr(stderr);
    Ok(())
}

fn write_json_line(writer: &mut impl Write, value: &impl serde::Serialize) -> Result<()> {
    serde_json::to_writer(&mut *writer, value).context("failed to encode JSON")?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

fn decode_response(line: &str) -> Result<Value> {
    if line.is_empty() {
        bail!("dlgt server closed the connection without a response");
    }
    let response: Response = serde_json::from_str(line).context("invalid daemon response")?;
    if let Ok(mut info) = LAST_INFO.lock() {
        info.clone_from(&response.info);
    }
    if let Some(error) = response.error {
        return Err(RpcFailure {
            code: error.code,
            message: error.message,
            session_id: error.session_id,
            launch_id: error.launch_id,
            correlation_id: error.correlation_id,
            hint: error.hint,
            session_state: error.session_state,
            action: error.action,
        }
        .into());
    }
    response.result.context("daemon response had no result")
}

pub fn take_info() -> Option<Value> {
    LAST_INFO.lock().ok()?.take()
}

fn public_rpc_method(method: &str) -> bool {
    matches!(
        method,
        "session.create"
            | "session.restart"
            | "session.send"
            | "session.fetch"
            | "session.cancel"
            | "session.list"
            | "session.read"
            | "session.stop"
            | "scrollback.read"
            | "transcript.read_raw"
            | "model.list"
            | "profile.list"
            | "harness.list"
    )
}

fn read_profiles() -> Result<Value> {
    let path = std::env::var_os("DLGT_CONFIG").map_or_else(
        || {
            std::env::var_os("HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_default()
                .join(".config/dlgt/config.toml")
        },
        std::path::PathBuf::from,
    );
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(json!({"profiles":{}})),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()));
        }
    };
    let document = text
        .parse::<toml_edit::DocumentMut>()
        .context("invalid dlgt config TOML")?;
    let names = document
        .get("profiles")
        .and_then(toml_edit::Item::as_table_like)
        .map(|profiles| profiles.iter().map(|(name, _)| name).collect::<Vec<_>>())
        .unwrap_or_default();
    Ok(json!({"profiles":names}))
}

fn filter_attach_input(input: &[u8], prefix: &mut bool) -> (Vec<u8>, bool) {
    let mut forward = Vec::with_capacity(input.len() + 1);
    for &byte in input {
        if *prefix {
            if byte == b'd' {
                *prefix = false;
                return (forward, true);
            }
            if byte == 0x02 {
                forward.push(0x02);
                *prefix = false;
                continue;
            }
            forward.push(0x02);
            *prefix = false;
        }
        if byte == 0x02 {
            *prefix = true;
        } else {
            forward.push(byte);
        }
    }
    (forward, false)
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::path::Path;
    use std::process::Command;

    use super::{
        RpcFailure, configure_daemon_output, daemon_start_failed, decode_response,
        filter_attach_input, is_auto_start_connect_error, socket_is_stale, socket_unavailable,
    };

    #[test]
    fn detach_prefix_is_consumed_not_forwarded() {
        let mut prefix = false;
        let (forward, detach) = filter_attach_input(b"manual\r\x02d", &mut prefix);
        assert_eq!(forward, b"manual\r");
        assert!(detach);
        assert!(!prefix);
    }

    #[test]
    fn non_detach_prefix_is_forwarded() {
        let mut prefix = false;
        let (forward, detach) = filter_attach_input(b"\x02x", &mut prefix);
        assert_eq!(forward, b"\x02x");
        assert!(!detach);
        assert!(!prefix);
    }

    #[test]
    fn doubled_prefix_forwards_literal_and_disarms() {
        let mut prefix = false;
        let (forward, detach) = filter_attach_input(b"\x02\x02d", &mut prefix);
        assert_eq!(forward, b"\x02d");
        assert!(!detach);
        assert!(!prefix);
    }

    #[test]
    fn rpc_failure_preserves_launch_correlation_id() {
        let result = decode_response(
            r#"{"id":"req_1","error":{"code":"LAUNCH_FAILED","message":"failed","launch_id":"internal:ABC12345"}}"#,
        );
        let error = match result {
            Ok(value) => panic!("expected RPC failure, got {value}"),
            Err(error) => error,
        };
        let failure = error
            .downcast_ref::<RpcFailure>()
            .unwrap_or_else(|| panic!("RPC failure missing"));
        assert_eq!(failure.launch_id.as_deref(), Some("internal:ABC12345"));
        assert!(failure.session_id.is_none());
    }

    #[test]
    fn only_absent_or_refused_sockets_may_auto_start() {
        assert!(is_auto_start_connect_error(&io::Error::new(
            io::ErrorKind::NotFound,
            "missing",
        )));
        assert!(is_auto_start_connect_error(&io::Error::new(
            io::ErrorKind::ConnectionRefused,
            "stale",
        )));
        assert!(!is_auto_start_connect_error(&io::Error::new(
            io::ErrorKind::PermissionDenied,
            "sandbox",
        )));
        assert!(!is_auto_start_connect_error(&io::Error::other(
            "hard failure",
        )));
        assert!(!is_auto_start_connect_error(&io::Error::from_raw_os_error(
            libc::EPERM,
        )));
    }

    #[test]
    fn permission_socket_failure_is_actionable_but_missing_socket_is_not() {
        let permission = socket_unavailable(
            Path::new("/tmp/dlgt.sock"),
            io::Error::from_raw_os_error(libc::EPERM),
        );
        let failure = permission
            .downcast_ref::<RpcFailure>()
            .unwrap_or_else(|| panic!("RPC failure missing"));
        assert_eq!(failure.code, "RPC_UNAVAILABLE");
        assert!(failure.message.contains("tools.exec_command"));
        assert!(
            failure
                .message
                .contains("sandbox_permissions: \"require_escalated\"")
        );
        assert!(
            failure
                .message
                .contains("DLGT_HOME and DLGT_SOCKET must not be changed")
        );
        assert!(failure.hint.is_some());

        let missing = socket_unavailable(
            Path::new("/tmp/dlgt.sock"),
            io::Error::new(io::ErrorKind::NotFound, "missing"),
        );
        let failure = missing
            .downcast_ref::<RpcFailure>()
            .unwrap_or_else(|| panic!("RPC failure missing"));
        assert_eq!(failure.code, "RPC_UNAVAILABLE");
        assert!(failure.hint.is_none());
        assert!(!failure.message.contains("sandbox_permissions"));
        assert!(socket_is_stale(&missing));
    }

    #[test]
    fn startup_failure_retains_diagnostic_and_structured_code() {
        let failure = daemon_start_failed(
            Path::new("/tmp/dlgt.sock"),
            "{\"code\":\"EPERM\",\"message\":\"Operation not permitted\"}",
        );
        let failure = failure
            .downcast_ref::<RpcFailure>()
            .unwrap_or_else(|| panic!("RPC failure missing"));
        assert_eq!(failure.code, "RPC_UNAVAILABLE");
        assert!(failure.message.contains("Operation not permitted"));
        assert!(failure.hint.is_some());
    }

    #[test]
    fn daemon_child_stdout_and_stderr_share_diagnostic_log() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("daemon.log");
        let file = std::fs::File::create(&path)?;
        let mut command = Command::new("sh");
        command.args(["-c", "printf child-stdout; printf child-stderr >&2"]);
        configure_daemon_output(&mut command, Some(file))?;
        let status = command.status()?;
        assert!(status.success(), "child exited with {status}");
        let diagnostic = std::fs::read_to_string(path)?;
        assert!(diagnostic.contains("child-stdout"));
        assert!(diagnostic.contains("child-stderr"));
        Ok(())
    }

    #[test]
    fn embedded_skill_documents_exact_sandbox_retry_contract() {
        let skill = include_str!("../assets/dlgt-skill.md");
        assert!(skill.contains("once through `tools.exec_command`"));
        assert!(skill.contains("`sandbox_permissions: \"require_escalated\"`"));
        assert!(skill.contains("concise approval\njustification"));
        assert!(skill.contains("same `--request-id`"));
        assert!(skill.contains("Do not restart the server or change"));
        assert!(skill.contains("`DLGT_HOME` or `DLGT_SOCKET` as a\nworkaround"));
        assert!(skill.contains("Do not proactively ask for escalation"));
        assert!(
            skill.contains(
                "On `RPC_UNAVAILABLE` with the Codex sandbox retry hint, follow \"Codex\n"
            )
        );
        assert!(skill.contains("sandbox socket failures\" above and retry the exact command once"));
        assert!(!skill.contains("On `RPC_UNAVAILABLE`, run `dlgt list --all-versions`"));
    }
}
