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
    pub provider_session_id: Option<String>,
    pub correlation_id: Option<String>,
    pub hint: Option<String>,
    pub resume_ref: Option<String>,
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
    let mut stream = UnixStream::connect(socket)
        .with_context(|| format!("dlgt server is not running at {}", socket.display()))?;
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
    let durable = selector
        .split_once(':')
        .filter(|(harness, id)| matches!(*harness, "codex" | "claude") && !id.is_empty());
    if durable.is_none() && !selector.starts_with("ses_") {
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
            let is_match = if let Some((harness, provider_id)) = durable {
                session.get("harness").and_then(Value::as_str) == Some(harness)
                    && session.get("provider_session_id").and_then(Value::as_str)
                        == Some(provider_id)
            } else {
                session_id == Some(selector)
            };
            if is_match && let Some(session_id) = session_id {
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
        let Ok(ping) = call_socket(&socket, "server.ping", json!({})) else {
            continue;
        };
        let Some(version) = ping.get("version").and_then(Value::as_str) else {
            continue;
        };
        let Ok(result) = call_socket(&socket, "session.list", json!({"all":include_all})) else {
            continue;
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
        if request.method == "event.subscribe" {
            return proxy_subscription(&request, &mut stdout);
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
                    if let Some(session_id) = &failure.session_id {
                        Response::session_error(
                            &request.id,
                            &failure.code,
                            &failure.message,
                            session_id,
                            failure.provider_session_id.clone(),
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
    if let Ok(stream) = UnixStream::connect(socket) {
        return Ok(stream);
    }
    start_daemon()?;
    for _ in 0..40 {
        if let Ok(stream) = UnixStream::connect(socket) {
            return Ok(stream);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    bail!("dlgt server did not start at {}", socket.display())
}

fn start_daemon() -> Result<()> {
    let executable = std::env::current_exe().context("failed to locate dlgt executable")?;
    let mut command = Command::new(executable);
    command
        .args(["server", "--daemon-child"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
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
            provider_session_id: error.provider_session_id,
            correlation_id: error.correlation_id,
            hint: error.hint,
            resume_ref: error.resume_ref,
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
            | "session.wait"
            | "session.cancel"
            | "session.list"
            | "session.read"
            | "session.stop"
            | "event.read"
            | "event.subscribe"
            | "scrollback.read"
            | "transcript.read_raw"
            | "model.list"
            | "profile.list"
            | "harness.list"
    )
}

pub fn follow_events(session: Option<&str>, after: i64) -> Result<()> {
    let socket = paths::socket_path()?;
    let mut stream = connect_or_start(&socket)?;
    let request = Request {
        id: format!("req_{}", Uuid::new_v4().simple()),
        method: "event.subscribe".to_owned(),
        params: json!({"session":session,"after":after}),
    };
    write_json_line(&mut stream, &request)?;
    let mut reader = BufReader::new(stream);
    let mut acknowledgement = String::new();
    reader.read_line(&mut acknowledgement)?;
    decode_response(&acknowledgement)?;
    let mut stdout = io::stdout().lock();
    for line in reader.lines() {
        stdout.write_all(line?.as_bytes())?;
        stdout.write_all(b"\n")?;
        stdout.flush()?;
    }
    Ok(())
}

fn proxy_subscription(request: &Request, stdout: &mut impl Write) -> Result<()> {
    let socket = paths::socket_path()?;
    let mut stream = connect_or_start(&socket)?;
    write_json_line(&mut stream, request)?;
    for line in BufReader::new(stream).lines() {
        stdout.write_all(line?.as_bytes())?;
        stdout.write_all(b"\n")?;
        stdout.flush()?;
    }
    Ok(())
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
    use super::{RpcFailure, decode_response, filter_attach_input};

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
    fn rpc_failure_preserves_session_correlation_ids() {
        let result = decode_response(
            r#"{"id":"req_1","error":{"code":"LAUNCH_FAILED","message":"failed","session_id":"ses_1","provider_session_id":"provider_1"}}"#,
        );
        let error = match result {
            Ok(value) => panic!("expected RPC failure, got {value}"),
            Err(error) => error,
        };
        let failure = error
            .downcast_ref::<RpcFailure>()
            .unwrap_or_else(|| panic!("RPC failure missing"));
        assert_eq!(failure.session_id.as_deref(), Some("ses_1"));
        assert_eq!(failure.provider_session_id.as_deref(), Some("provider_1"));
    }
}
