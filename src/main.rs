mod claude_models;
mod client;
mod codex;
mod cursor;
mod daemon;
mod doctor;
mod paths;
mod protocol;
mod provider;
mod raw_mode;
mod reaper;
mod screen;
mod session;
mod skill;
mod store;
mod transcript;
mod update;

use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use serde_json::{Map, Value, json};
use uuid::Uuid;

fn main() {
    if let Err(error) = run() {
        let failure = error.downcast_ref::<client::RpcFailure>();
        let code = failure.map_or("INVALID_ARGUMENT", |failure| failure.code.as_str());
        let message =
            failure.map_or_else(|| format!("{error:#}"), |failure| failure.message.clone());
        let mut error_json = json!({"code":code,"message":message});
        if let Some(failure) = failure
            && let Some(error_object) = error_json.as_object_mut()
        {
            if let Some(session_id) = &failure.session_id {
                error_object.insert("session_id".to_owned(), json!(session_id));
            }
            if let Some(launch_id) = &failure.launch_id {
                error_object.insert("launch_id".to_owned(), json!(launch_id));
            }
            if let Some(correlation_id) = &failure.correlation_id {
                error_object.insert("correlation_id".to_owned(), json!(correlation_id));
            }
            if let Some(hint) = &failure.hint {
                error_object.insert("hint".to_owned(), json!(hint));
            }
            if let Some(session_state) = &failure.session_state {
                error_object.insert("session_state".to_owned(), json!(session_state));
            }
            if let Some(action) = &failure.action {
                error_object.insert("action".to_owned(), json!(action));
            }
        }
        let response = json!({"ok":false,"error":error_json});
        println!("{response}");
        std::process::exit(exit_status(code));
    }
}

fn run() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let Some(command) = args.first().map(String::as_str) else {
        print_usage();
        return Ok(());
    };
    if command == "help" {
        return command_help(&args[1..]);
    }
    if has_help_flag(&args[1..]) {
        return print_command_usage(command);
    }
    match command {
        "server" => command_server(&args[1..]),
        "new" => command_new(&args[1..]),
        "restart" => command_restart(&args[1..]),
        "send" => command_send(&args[1..]),
        "fetch" => command_fetch(&args[1..]),
        "cancel" => command_cancel(&args[1..]),
        "list" | "ls" => command_list(&args[1..]),
        "show" => command_show(&args[1..]),
        "attach" => command_attach(&args[1..]),
        "stop" => command_stop(&args[1..]),
        "scrollback" => command_scrollback(&args[1..]),
        "logs" => command_logs(&args[1..]),
        "models" => command_models(&args[1..]),
        "profiles" => command_profiles(&args[1..]),
        "harnesses" => command_harnesses(&args[1..]),
        "doctor" => command_doctor(&args[1..]),
        "rpc" => command_rpc(&args[1..]),
        "hook" => command_hook(&args[1..]),
        "update" => command_update(&args[1..]),
        "skill" => {
            print!("{}", skill::TEXT);
            Ok(())
        }
        "--version" | "-V" | "version" => {
            print_success(json!({"version":env!("CARGO_PKG_VERSION")}), false)
        }
        "--help" | "-h" => {
            print_usage();
            Ok(())
        }
        _ => bail!("unknown command {command:?}; run `dlgt help`"),
    }
}

fn command_help(args: &[String]) -> Result<()> {
    match args {
        [] => {
            print_usage();
            Ok(())
        }
        [flag] if matches!(flag.as_str(), "--help" | "-h") => print_command_usage("help"),
        [command] => print_command_usage(command),
        _ => bail!("help accepts at most one command"),
    }
}

fn has_help_flag(args: &[String]) -> bool {
    args.iter()
        .take_while(|argument| argument.as_str() != "--")
        .any(|argument| matches!(argument.as_str(), "--help" | "-h"))
}

fn command_server(args: &[String]) -> Result<()> {
    if args.first().is_some_and(|value| value == "stop") {
        return print_success(client::call_existing("server.stop", json!({}))?, false);
    }
    if args.first().is_some_and(|value| value == "--reaper") {
        return reaper::run();
    }
    let parsed = Args::parse("server", args, &["--foreground", "--daemon-child"], &[])?;
    parsed.no_positionals()?;
    daemon::run()
}

fn command_new(args: &[String]) -> Result<()> {
    let parsed = Args::parse(
        "new",
        args,
        &["--stdin", "--pretty", "--clean-env", "--no-auto-approve"],
        LAUNCH_OPTIONS,
    )?;
    let title = parsed.required("--title")?;
    let profile = parsed.one("--profile").map(load_profile).transpose()?;
    let harness = parsed
        .one("--harness")
        .or_else(|| {
            profile
                .as_ref()
                .and_then(|value| value.get("harness"))
                .and_then(Value::as_str)
        })
        .context("missing --harness or profile harness")?;
    // Validated before the prompt so a caller is never asked for stdin only to
    // be told the invocation was wrong.
    let request_id = require_request_id("new", &parsed)?;
    let prompt =
        prompt_from(&parsed, 0)?.context("missing initial prompt; use --stdin or -- PROMPT")?;
    let cwd = launch_cwd(&parsed)?;
    let model = parsed.one("--model").or_else(|| {
        profile
            .as_ref()
            .and_then(|value| value.get("model"))
            .and_then(Value::as_str)
    });
    let effort = parsed.one("--effort").or_else(|| {
        profile
            .as_ref()
            .and_then(|value| value.get("effort"))
            .and_then(Value::as_str)
    });
    let harness_options = launch_harness_options(&parsed, profile.as_ref())?;
    let auto_approve = if parsed.flag("--no-auto-approve") {
        false
    } else {
        profile
            .as_ref()
            .and_then(|value| value.get("auto_approve"))
            .and_then(Value::as_bool)
            .unwrap_or(true)
    };
    let environment = launch_environment(&parsed, profile.as_ref())?;
    let (rows, cols) = raw_mode::terminal_size(libc::STDIN_FILENO);
    let result = client::call(
        "session.create",
        json!({
            "title": title,
            "request_id": request_id,
            "alias": parsed.one("--alias"),
            "harness": harness,
            "cwd": cwd,
            "model": model,
            "effort": effort,
            "harness_options": harness_options,
            "auto_approve": auto_approve,
            "prompt": prompt,
            "startup_timeout_ms": parsed.one("--startup-timeout")
                .map(parse_duration).transpose()?.unwrap_or(Duration::from_mins(1)).as_millis(),
            "environment": environment,
            "rows": rows,
            "cols": cols,
        }),
    )?;
    print_success(result, parsed.flag("--pretty"))
}

fn command_send(args: &[String]) -> Result<()> {
    let parsed = Args::parse(
        "send",
        args,
        &[
            "--stdin",
            "--pretty",
            "--resume",
            "--clean-env",
            "--no-auto-approve",
        ],
        LAUNCH_OPTIONS,
    )?;
    let session = parsed
        .positionals
        .first()
        .context("missing Session selector")?;
    let request_id = require_request_id("send", &parsed)?;
    let prompt = prompt_from(&parsed, 1)?.context("missing prompt; use --stdin or -- PROMPT")?;
    if parsed.one("--harness").is_some() {
        bail!("--harness is derived from the provider-qualified resume selector");
    }
    let correlation_id = format!("corr_{}", Uuid::new_v4().simple());
    let mut params = json!({
        "session":session,
        "prompt":prompt,
        "correlation_id":correlation_id,
        "request_id": request_id,
        "resume": parsed.flag("--resume"),
    });
    if parsed.flag("--resume") {
        let object = params.as_object_mut().context("invalid send parameters")?;
        object.insert("cwd".to_owned(), json!(launch_cwd(&parsed)?));
        object.insert("model".to_owned(), json!(parsed.one("--model")));
        object.insert("effort".to_owned(), json!(parsed.one("--effort")));
        object.insert(
            "harness_options".to_owned(),
            json!(launch_harness_options(&parsed, None)?),
        );
        object.insert(
            "auto_approve".to_owned(),
            json!(!parsed.flag("--no-auto-approve")),
        );
        object.insert(
            "startup_timeout_ms".to_owned(),
            json!(
                parsed
                    .one("--startup-timeout")
                    .map(parse_duration)
                    .transpose()?
                    .unwrap_or(Duration::from_mins(1))
                    .as_millis()
            ),
        );
        object.insert(
            "environment".to_owned(),
            json!(launch_environment(&parsed, None)?),
        );
        let (rows, cols) = raw_mode::terminal_size(libc::STDIN_FILENO);
        object.insert("rows".to_owned(), json!(rows));
        object.insert("cols".to_owned(), json!(cols));
    }
    let route = client::find_live_session(session)?;
    if let Some(route) = &route {
        params["session"] = json!(route.session_id);
    }
    let result = if let Some(route) = &route {
        client::call_socket(&route.socket, "session.send", params)?
    } else {
        client::call("session.send", params)?
    };
    print_success(result, parsed.flag("--pretty"))
}

fn command_restart(args: &[String]) -> Result<()> {
    let parsed = Args::parse(
        "restart",
        args,
        &["--pretty", "--clean-env"],
        LAUNCH_OPTIONS,
    )?;
    let session = parsed.one_positional("Session selector")?;
    let environment = launch_environment(&parsed, None)?;
    let (rows, cols) = raw_mode::terminal_size(libc::STDIN_FILENO);
    let result = client::call(
        "session.restart",
        json!({
            "session": session,
            "startup_timeout_ms": parsed.one("--startup-timeout")
                .map(parse_duration).transpose()?.unwrap_or(Duration::from_mins(1)).as_millis(),
            "environment": environment,
            "rows": rows,
            "cols": cols,
        }),
    )?;
    print_success(result, parsed.flag("--pretty"))
}

fn command_fetch(args: &[String]) -> Result<()> {
    let parsed = Args::parse(
        "fetch",
        args,
        &["--screen", "--no-screen", "--pretty"],
        &["--cursor", "--wait", "--screen", "--max-bytes"],
    )?;
    let session = Some(parsed.one_positional("Session selector")?);
    if parsed.flag("--no-screen") && parsed.flag("--screen") {
        bail!("--screen and --no-screen are mutually exclusive");
    }
    let screen = if parsed.flag("--no-screen") {
        json!(false)
    } else if let Some(lines) = parsed.one("--screen") {
        json!(
            lines
                .parse::<u64>()
                .context("invalid --screen line budget")?
        )
    } else if parsed.flag("--screen") {
        json!(true)
    } else {
        Value::Null
    };
    let params = json!({
        "session": session,
        "cursor": parsed.one("--cursor"),
        "wait_ms": parsed.one("--wait").map(parse_duration).transpose()?.map(duration_ms),
        "screen": screen,
        "max_bytes": parsed.one("--max-bytes").map(str::parse::<u64>).transpose()
            .context("invalid --max-bytes")?,
    });
    let route = session
        .map(client::find_live_session)
        .transpose()?
        .flatten();
    let result = if let Some(route) = &route {
        client::call_socket(&route.socket, "session.fetch", params)?
    } else {
        client::call("session.fetch", params)?
    };
    print_success(result, parsed.flag("--pretty"))
}

fn command_cancel(args: &[String]) -> Result<()> {
    let parsed = Args::parse("cancel", args, &["--pretty"], &["--timeout"])?;
    let session = parsed.one_positional("Session selector")?;
    let timeout = parsed
        .one("--timeout")
        .map(parse_duration)
        .transpose()?
        .unwrap_or(Duration::from_secs(30));
    let result = client::call(
        "session.cancel",
        json!({"session":session,"timeout_ms":duration_ms(timeout)}),
    )?;
    print_success(result, parsed.flag("--pretty"))
}

fn command_list(args: &[String]) -> Result<()> {
    let parsed = Args::parse("list", args, &["--all", "--all-versions", "--pretty"], &[])?;
    parsed.no_positionals()?;
    let sessions = if parsed.flag("--all-versions") {
        client::list_all_versions(parsed.flag("--all"))?
    } else {
        client::call("session.list", json!({"all":parsed.flag("--all")}))?
            .as_array()
            .context("invalid session list response")?
            .clone()
    };
    print_success(json!({"sessions":sessions}), parsed.flag("--pretty"))
}

fn command_show(args: &[String]) -> Result<()> {
    let parsed = Args::parse("show", args, &["--pretty"], &[])?;
    let result = client::call(
        "session.read",
        json!({"session":parsed.one_positional("Session selector")?}),
    )?;
    print_success(result, parsed.flag("--pretty"))
}

fn command_attach(args: &[String]) -> Result<()> {
    let parsed = Args::parse("attach", args, &["--steal"], &[])?;
    let selector = parsed.one_positional("Session selector")?;
    // attach is an interactive terminal takeover. Without a TTY on both ends
    // it would dump raw ANSI into a pipe and read input that no one is typing.
    if !is_tty(libc::STDIN_FILENO) || !is_tty(libc::STDOUT_FILENO) {
        return Err(client::RpcFailure {
            code: "ATTACH_REQUIRES_TTY".to_owned(),
            message: "attach requires an interactive terminal on stdin and stdout".to_owned(),
            session_id: None,
            launch_id: None,
            correlation_id: None,
            hint: Some(format!("dlgt fetch {selector}")),
            session_state: None,
            action: Some(format!("dlgt fetch {selector}")),
        }
        .into());
    }
    client::attach(selector, parsed.flag("--steal"))
}

fn is_tty(descriptor: i32) -> bool {
    // SAFETY: isatty only inspects the descriptor and has no memory-safety
    // preconditions.
    unsafe { libc::isatty(descriptor) == 1 }
}

fn command_stop(args: &[String]) -> Result<()> {
    let parsed = Args::parse("stop", args, &["--force", "--pretty"], &[])?;
    let result = client::call(
        "session.stop",
        json!({
            "session":parsed.one_positional("Session selector")?, "force":parsed.flag("--force")
        }),
    )?;
    print_success(result, parsed.flag("--pretty"))
}

fn command_scrollback(args: &[String]) -> Result<()> {
    let parsed = Args::parse("scrollback", args, &["--pretty"], &["--lines", "--before"])?;
    let session = parsed.one_positional("Session selector")?;
    let lines = parsed
        .one("--lines")
        .map(str::parse::<u64>)
        .transpose()
        .context("invalid --lines")?
        .unwrap_or(100);
    let result = client::call(
        "scrollback.read",
        json!({"session":session,"lines":lines,"before":parsed.one("--before")}),
    )?;
    print_success(result, parsed.flag("--pretty"))
}

fn command_logs(args: &[String]) -> Result<()> {
    let parsed = Args::parse("logs", args, &["--raw", "--json"], &[])?;
    let session = parsed.one_positional("Session selector")?;
    if !parsed.flag("--raw") {
        bail!("logs requires the explicit --raw capability flag");
    }
    let mut after = 0_i64;
    let mut all = Vec::new();
    loop {
        let value = client::call(
            "transcript.read_raw",
            json!({"session":session,"after":after}),
        )?;
        let bytes = base64::engine::general_purpose::STANDARD.decode(
            value
                .get("data_base64")
                .and_then(Value::as_str)
                .context("raw transcript has no data")?,
        )?;
        if parsed.flag("--json") {
            all.extend(bytes);
        } else {
            io::stdout().write_all(&bytes)?;
        }
        if !value
            .get("has_more")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            break;
        }
        after = value
            .get("next_after")
            .and_then(Value::as_i64)
            .context("raw transcript cursor missing")?;
    }
    if parsed.flag("--json") {
        print_success(
            json!({"session_id":session,"data_base64":base64::engine::general_purpose::STANDARD.encode(all)}),
            false,
        )
    } else {
        io::stdout().flush().map_err(Into::into)
    }
}

fn command_models(args: &[String]) -> Result<()> {
    let parsed = Args::parse(
        "models",
        args,
        &["--include-hidden", "--pretty"],
        &["--harness"],
    )?;
    parsed.no_positionals()?;
    let include_hidden = parsed.flag("--include-hidden");
    if let Some(harness) = parsed.one("--harness") {
        let result = client::call(
            "model.list",
            json!({"harness":harness,"include_hidden":include_hidden}),
        )?;
        return print_success(result, parsed.flag("--pretty"));
    }
    // Bare `dlgt models` is a discovery request, not an assertion that every
    // Harness is reachable: one unavailable provider must not hide the other.
    let harnesses = ["codex", "claude"]
        .into_iter()
        .map(|harness| {
            client::call(
                "model.list",
                json!({"harness":harness,"include_hidden":include_hidden}),
            )
            .unwrap_or_else(|error| {
                json!({
                    "harness": harness,
                    "discovery": "unavailable",
                    "models": [],
                    "error": format!("{error:#}"),
                })
            })
        })
        .collect::<Vec<_>>();
    print_success(json!({"harnesses":harnesses}), parsed.flag("--pretty"))
}

fn command_profiles(args: &[String]) -> Result<()> {
    let parsed = Args::parse("profiles", args, &["--pretty"], &[])?;
    let action = parsed.positionals.first().map_or("list", String::as_str);
    let profiles = load_profiles()?;
    let result = match action {
        // Bare `dlgt profiles` is the same request as `profiles list`.
        "list" if parsed.positionals.len() <= 1 => json!({"profiles":profiles}),
        "show" if parsed.positionals.len() == 2 => {
            let name = &parsed.positionals[1];
            json!({"name":name,"profile":profiles.get(name).with_context(|| format!("profile not found: {name}"))?})
        }
        _ => bail!("usage: dlgt profiles list | profiles show NAME"),
    };
    print_success(result, parsed.flag("--pretty"))
}

fn command_harnesses(args: &[String]) -> Result<()> {
    let parsed = Args::parse("harnesses", args, &["--pretty"], &[])?;
    if parsed.positionals.len() > 1 {
        bail!("harnesses accepts at most one Harness name");
    }
    let result = client::call(
        "harness.list",
        json!({"harness":parsed.positionals.first()}),
    )?;
    print_success(json!({"harnesses":result}), parsed.flag("--pretty"))
}

fn command_doctor(args: &[String]) -> Result<()> {
    let parsed = Args::parse("doctor", args, &["--json", "--probe"], &[])?;
    parsed.no_positionals()?;
    let report = doctor::run(parsed.flag("--probe"));
    if parsed.flag("--json") {
        print_success(report.as_json(), false)
    } else {
        print!("{}", doctor::human(&report));
        Ok(())
    }
}

fn command_rpc(args: &[String]) -> Result<()> {
    let parsed = Args::parse("rpc", args, &["--stdio"], &[])?;
    parsed.no_positionals()?;
    if !parsed.flag("--stdio") {
        bail!("rpc requires --stdio");
    }
    client::rpc_stdio()
}

fn command_update(args: &[String]) -> Result<()> {
    let parsed = Args::parse("update", args, &["--pretty"], &[])?;
    parsed.no_positionals()?;
    print_success(update::install_latest()?, parsed.flag("--pretty"))
}

fn command_hook(args: &[String]) -> Result<()> {
    if args.len() != 3 || args[0] != "emit" {
        bail!("invalid internal hook invocation");
    }
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    let payload = serde_json::from_str::<Value>(&input).unwrap_or(Value::Null);
    let _ = client::call_existing(
        "hook.event",
        json!({"session":args[1],"agent":args[2],"payload":payload}),
    );
    Ok(())
}

/// Options accepted by the Session launch commands. `new` and `send` share
/// this set so a Profile-driven launch and a `--resume` launch stay aligned.
const LAUNCH_OPTIONS: &[&str] = &[
    "--title",
    "--alias",
    "--profile",
    "--harness",
    "--model",
    "--effort",
    "--cwd",
    "--harness-option",
    "--startup-timeout",
    "--request-id",
    "--pass-env",
    "--env",
    "--unset-env",
];

#[derive(Default)]
struct Args {
    positionals: Vec<String>,
    options: HashMap<String, Vec<String>>,
    flags: HashSet<String>,
}

impl Args {
    /// Parse one command's arguments against its declared flags and options.
    ///
    /// An unrecognized long option is rejected by name instead of silently
    /// consuming the next token as its value, which used to turn a typo into
    /// an unrelated "missing value for --json" style failure.
    fn parse(command: &str, args: &[String], flags: &[&str], options: &[&str]) -> Result<Self> {
        let known_flags = flags.iter().copied().collect::<HashSet<_>>();
        let known_options = options.iter().copied().collect::<HashSet<_>>();
        let mut parsed = Self::default();
        let mut index = 0;
        let mut positional = false;
        while index < args.len() {
            let value = &args[index];
            if positional {
                parsed.positionals.push(value.clone());
            } else if value == "--" {
                positional = true;
            } else if let Some((name, inline)) = value
                .starts_with("--")
                .then(|| value.split_once('='))
                .flatten()
            {
                if !known_flags.contains(name) && !known_options.contains(name) {
                    return Err(unknown_option(command, name));
                }
                if known_flags.contains(name) {
                    parsed.flags.insert(name.to_owned());
                }
                parsed
                    .options
                    .entry(name.to_owned())
                    .or_default()
                    .push(inline.to_owned());
            } else if value.starts_with("--") {
                if known_flags.contains(value.as_str()) {
                    parsed.flags.insert(value.clone());
                } else if known_options.contains(value.as_str()) {
                    index += 1;
                    let option = args
                        .get(index)
                        .with_context(|| format!("missing value for {value}"))?;
                    parsed
                        .options
                        .entry(value.clone())
                        .or_default()
                        .push(option.clone());
                } else {
                    return Err(unknown_option(command, value));
                }
            } else {
                parsed.positionals.push(value.clone());
            }
            index += 1;
        }
        Ok(parsed)
    }
    fn flag(&self, name: &str) -> bool {
        self.flags.contains(name)
    }
    fn one(&self, name: &str) -> Option<&str> {
        self.options
            .get(name)
            .and_then(|values| values.last())
            .map(String::as_str)
    }
    fn many(&self, name: &str) -> impl Iterator<Item = &str> {
        self.options
            .get(name)
            .into_iter()
            .flatten()
            .map(String::as_str)
    }
    fn required(&self, name: &str) -> Result<&str> {
        self.one(name)
            .with_context(|| format!("missing required option {name}"))
    }
    fn one_positional(&self, label: &str) -> Result<&str> {
        if self.positionals.len() != 1 {
            bail!("expected exactly one {label}");
        }
        Ok(&self.positionals[0])
    }
    fn no_positionals(&self) -> Result<()> {
        if self.positionals.is_empty() {
            Ok(())
        } else {
            bail!(
                "unexpected positional arguments: {}",
                self.positionals.join(" ")
            )
        }
    }
}

/// Acceptance idempotency only works when the key exists before the first
/// attempt, so it cannot be something a caller remembers to add afterwards.
fn require_request_id<'a>(command: &str, parsed: &'a Args) -> Result<&'a str> {
    let Some(request_id) = parsed.one("--request-id").filter(|id| !id.is_empty()) else {
        return Err(request_id_error(
            command,
            "missing required option --request-id; every acceptance needs an idempotency key \
so a lost response can be retried without creating a second Session",
        ));
    };
    if request_id.len() > crate::protocol::MAX_ACCEPTANCE_REQUEST_ID_LEN {
        return Err(request_id_error(
            command,
            &format!(
                "invalid --request-id: must be at most {} bytes",
                crate::protocol::MAX_ACCEPTANCE_REQUEST_ID_LEN
            ),
        ));
    }
    Ok(request_id)
}

fn request_id_error(command: &str, reason: &str) -> anyhow::Error {
    command_usage(command).map_or_else(
        |_| anyhow::anyhow!("{reason}"),
        |usage| anyhow::anyhow!("{reason}\n\n{usage}"),
    )
}

fn unknown_option(command: &str, name: &str) -> anyhow::Error {
    command_usage(command).map_or_else(
        |_| anyhow::anyhow!("unknown option {name:?}"),
        |usage| anyhow::anyhow!("unknown option {name:?}\n\n{usage}"),
    )
}

fn prompt_from(parsed: &Args, skip: usize) -> Result<Option<String>> {
    if parsed.flag("--stdin") {
        if parsed.positionals.len() > skip {
            bail!("--stdin and positional prompt are mutually exclusive");
        }
        let mut prompt = String::new();
        io::stdin().read_to_string(&mut prompt)?;
        return Ok(Some(prompt));
    }
    Ok((parsed.positionals.len() > skip).then(|| parsed.positionals[skip..].join(" ")))
}

fn parse_duration(value: &str) -> Result<Duration> {
    let split = value
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(value.len());
    let number = value[..split].parse::<u64>().context("invalid duration")?;
    if number == 0 {
        bail!("duration must be positive");
    }
    match &value[split..] {
        "ms" => Ok(Duration::from_millis(number)),
        "" | "s" => Ok(Duration::from_secs(number)),
        "m" => Ok(Duration::from_secs(number.saturating_mul(60))),
        "h" => Ok(Duration::from_secs(number.saturating_mul(3600))),
        unit => bail!("invalid duration unit {unit:?}; use ms, s, m, or h"),
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn launch_environment(parsed: &Args, profile: Option<&Value>) -> Result<Map<String, Value>> {
    let clean = parsed.flag("--clean-env")
        || profile
            .and_then(|value| value.get("clean_env"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
    let mut values = if clean {
        HashMap::new()
    } else {
        std::env::vars().collect()
    };
    if clean {
        for key in profile
            .into_iter()
            .filter_map(|value| value.get("pass_env"))
            .filter_map(Value::as_array)
            .flatten()
            .filter_map(Value::as_str)
        {
            if let Ok(value) = std::env::var(key) {
                values.insert(key.to_owned(), value);
            }
        }
        for key in parsed.many("--pass-env") {
            if let Ok(value) = std::env::var(key) {
                values.insert(key.to_owned(), value);
            }
        }
    }
    for assignment in parsed.many("--env") {
        let (key, value) = assignment
            .split_once('=')
            .context("--env requires KEY=VALUE")?;
        values.insert(key.to_owned(), value.to_owned());
    }
    for key in parsed.many("--unset-env") {
        values.remove(key);
    }
    Ok(values
        .into_iter()
        .map(|(key, value)| (key, Value::String(value)))
        .collect())
}

fn launch_cwd(parsed: &Args) -> Result<String> {
    let invocation_dir =
        std::env::current_dir().context("cannot determine the current directory")?;
    resolve_launch_cwd(parsed.one("--cwd"), &invocation_dir)
}

fn resolve_launch_cwd(argument: Option<&str>, invocation_dir: &Path) -> Result<String> {
    let Some(argument) = argument else {
        return Ok(invocation_dir.to_string_lossy().into_owned());
    };
    let resolved = invocation_dir
        .join(argument)
        .canonicalize()
        .with_context(|| format!("--cwd {argument:?} does not exist"))?;
    Ok(resolved.to_string_lossy().into_owned())
}

fn launch_harness_options(parsed: &Args, profile: Option<&Value>) -> Result<Vec<String>> {
    let mut options = Vec::new();
    if let Some(value) = profile.and_then(|profile| profile.get("harness_options")) {
        for option in value
            .as_array()
            .context("profile harness_options must be an array")?
        {
            options.push(
                option
                    .as_str()
                    .context("profile harness_options entries must be strings")?
                    .to_owned(),
            );
        }
    }
    options.extend(parsed.many("--harness-option").map(str::to_owned));
    Ok(options)
}

fn load_profiles() -> Result<Map<String, Value>> {
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
        Ok(value) => value,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()));
        }
    };
    let document = text
        .parse::<toml_edit::DocumentMut>()
        .context("invalid dlgt config TOML")?;
    let mut profiles = Map::new();
    if let Some(table) = document
        .get("profiles")
        .and_then(toml_edit::Item::as_table_like)
    {
        for (name, item) in table.iter() {
            if let Some(profile) = item.as_table_like() {
                let mut value = Map::new();
                for (key, item) in profile.iter() {
                    if let Some(string) = item.as_str() {
                        value.insert(key.to_owned(), json!(string));
                    } else if let Some(boolean) = item.as_bool() {
                        value.insert(key.to_owned(), json!(boolean));
                    } else if let Some(array) = item.as_array() {
                        value.insert(
                            key.to_owned(),
                            Value::Array(
                                array
                                    .iter()
                                    .filter_map(|item| item.as_str().map(|item| json!(item)))
                                    .collect(),
                            ),
                        );
                    }
                }
                profiles.insert(name.to_owned(), Value::Object(value));
            }
        }
    }
    Ok(profiles)
}

fn load_profile(name: &str) -> Result<Value> {
    load_profiles()?
        .remove(name)
        .with_context(|| format!("profile not found: {name}"))
}

fn print_success(value: Value, pretty: bool) -> Result<()> {
    let mut object = match value {
        Value::Object(object) => object,
        other => Map::from_iter([("value".to_owned(), other)]),
    };
    object.insert("ok".to_owned(), Value::Bool(true));
    if let Some(info) = client::take_info() {
        object.insert("info".to_owned(), info);
    }
    let value = Value::Object(object);
    println!(
        "{}",
        if pretty {
            serde_json::to_string_pretty(&value)?
        } else {
            serde_json::to_string(&value)?
        }
    );
    Ok(())
}

fn exit_status(code: &str) -> i32 {
    match code {
        "CANCEL_TIMEOUT" => 3,
        "SESSION_BLOCKED" => 4,
        "SESSION_BUSY" => 5,
        _ => 1,
    }
}

fn print_usage() {
    println!(
        "dlgt - local subagent runtime\n\nUSAGE\n  dlgt <COMMAND> [OPTIONS]\n\nDELEGATION\n  new          Create a Session with its first prompt\n  restart      Restart a Session\n  send         Send work to an existing idle Session or --resume a provider conversation\n  fetch        Read new state, results, events, and screen from a cursor\n  cancel       Interrupt the active execution\n\nSESSIONS\n  list, ls     List Sessions\n  show         Show Session state and latest result\n  attach       Attach to the Session screen\n  stop         Stop the Session\n\nOBSERVABILITY\n  scrollback   Read rendered terminal scrollback\n  logs         Read raw retained PTY bytes (requires --raw)\n\nCONFIGURATION\n  models       Discover Harness models\n  profiles     List or inspect Profiles\n  harnesses    List Harness capabilities\n  doctor       Diagnose the local dlgt installation\n  skill        Print the embedded dlgt skill\n\nRUNTIME\n  server       Run or stop the daemon\n  update       Install the latest release and embedded Skills\n  rpc          Use JSONL RPC"
    );
}

fn print_command_usage(command: &str) -> Result<()> {
    println!("{}", command_usage(command)?);
    Ok(())
}

fn command_usage(command: &str) -> Result<&'static str> {
    let usage = match command {
        "server" => {
            "dlgt server - run or stop the local daemon\n\nUSAGE\n  dlgt server [--foreground]\n  dlgt server stop\n\nOPTIONS\n  --foreground   Run in the foreground\n  -h, --help     Print this help"
        }
        "update" => {
            "dlgt update - install the latest release and embedded Skills\n\nUSAGE\n  dlgt update [--pretty]\n\nOPTIONS\n  --pretty     Pretty-print JSON output\n  -h, --help   Print this help"
        }
        "new" => {
            "dlgt new - create a Session and submit its first prompt\n\nUSAGE\n  dlgt new --title <TITLE> --request-id <ID> [OPTIONS] -- <PROMPT>\n  dlgt new --title <TITLE> --request-id <ID> [OPTIONS] --stdin\n\nOPTIONS\n  --title <TITLE>                 Human-readable Session title (required)\n  --alias <@ALIAS>               Exact active Session alias\n  --profile <PROFILE>            Reusable launch Profile\n  --harness <codex|claude>       Provider Harness (required without a Profile)\n  --model <MODEL>                 Provider model\n  --effort <LEVEL>               Provider reasoning effort\n  --cwd <DIR>                    Working directory (default: current directory)\n  --harness-option <KEY=VALUE>   Claude CLI option (repeatable)\n  --no-auto-approve              Keep the Harness's own approval prompts\n  --startup-timeout <DURATION>   Startup timeout (default: 60s)\n  --clean-env                    Start with an empty environment\n  --pass-env <KEY>               Pass a host variable with --clean-env (repeatable)\n  --env <KEY=VALUE>              Set an environment variable (repeatable)\n  --unset-env <KEY>              Remove an environment variable (repeatable)\n  --request-id <ID>              Idempotency key (required); a retry replays the receipt\n  --stdin                        Read the required prompt from stdin\n  --pretty                       Pretty-print JSON output\n  -h, --help                     Print this help"
        }
        "restart" => {
            "dlgt restart - replace a Session process and resume its provider conversation\n\nUSAGE\n  dlgt restart <SESSION_ID> [OPTIONS]\n\nOPTIONS\n  --startup-timeout <DURATION>   Startup timeout (default: 60s)\n  --clean-env                    Start with an empty environment\n  --pass-env <KEY>               Pass a host variable with --clean-env (repeatable)\n  --env <KEY=VALUE>              Set an environment variable (repeatable)\n  --unset-env <KEY>              Remove an environment variable (repeatable)\n  --pretty                       Pretty-print JSON output\n  -h, --help                     Print this help"
        }
        "send" => {
            "dlgt send - send work to an idle Session or explicitly resume a provider conversation\n\nUSAGE\n  dlgt send <SESSION_ID|@ALIAS> --request-id <ID> [OPTIONS] -- <PROMPT>\n  dlgt send <codex:ID|claude:ID> --resume --request-id <ID> [OPTIONS] -- <PROMPT>\n\nOPTIONS\n  --resume                       Resume a stopped provider conversation\n  --model <MODEL>                 Model override for resume\n  --effort <LEVEL>               Reasoning effort override for resume\n  --cwd <DIR>                    Working directory for resume (default: current directory)\n  --harness-option <KEY=VALUE>   Claude CLI option for resume (repeatable)\n  --no-auto-approve              Keep the Harness's own approval prompts on resume\n  --startup-timeout <DURATION>   Resume startup timeout (default: 60s)\n  --clean-env                    Resume with an empty environment\n  --pass-env <KEY>               Pass a host variable with --clean-env (repeatable)\n  --env <KEY=VALUE>              Set an environment variable (repeatable)\n  --unset-env <KEY>              Remove an environment variable (repeatable)\n  --request-id <ID>              Idempotency key (required); a retry replays the receipt\n  --stdin                        Read the required prompt from stdin\n  --pretty                       Pretty-print JSON output\n  -h, --help                     Print this help"
        }
        "fetch" => {
            "dlgt fetch - read one Session since a cursor in one call\n\nUSAGE\n  dlgt fetch <SESSION_ID|@ALIAS> [OPTIONS]\n\nOPTIONS\n  --cursor <N>            Observation position from a previous response\n  --wait <DURATION>       Wait for the active/latest execution's terminal result (max 24h)\n  --screen[=<LINES>]      Include the screen delta (default: on, 128 stable lines)\n  --no-screen             Omit the screen delta\n  --max-bytes <BYTES>     Serialized response budget (default: 32768, max: 262144)\n  --pretty                Pretty-print JSON output\n  -h, --help              Print this help\n\nEvery observation exits 0. Omit --cursor to recover a bounded baseline."
        }
        "cancel" => {
            "dlgt cancel - interrupt the active execution\n\nUSAGE\n  dlgt cancel <SESSION_ID|@ALIAS> [OPTIONS]\n\nOPTIONS\n  --timeout <DURATION>   Cancellation timeout (default: 30s)\n  --pretty               Pretty-print JSON output\n  -h, --help             Print this help"
        }
        "list" | "ls" => {
            "dlgt list - list Sessions\n\nUSAGE\n  dlgt list [--all] [--all-versions] [--pretty]\n  dlgt ls [--all] [--all-versions] [--pretty]\n\nOPTIONS\n  --all            Include terminal Sessions from live runtimes\n  --all-versions   Query every running dlgt version\n  --pretty         Pretty-print JSON output\n  -h, --help       Print this help"
        }
        "show" => {
            "dlgt show - show Session state and latest result\n\nUSAGE\n  dlgt show <SESSION_ID|@ALIAS> [--pretty]\n\nOPTIONS\n  --pretty     Pretty-print JSON output\n  -h, --help   Print this help"
        }
        "attach" => {
            "dlgt attach - attach to the Session screen\n\nUSAGE\n  dlgt attach <SESSION_ID|@ALIAS> [--steal]\n\nOPTIONS\n  --steal      Transfer an existing attach lease\n  -h, --help   Print this help"
        }
        "stop" => {
            "dlgt stop - stop a Session and its process group\n\nUSAGE\n  dlgt stop <SESSION_ID|@ALIAS> [OPTIONS]\n\nOPTIONS\n  --force      Force termination\n  --pretty     Pretty-print JSON output\n  -h, --help   Print this help"
        }
        "scrollback" => {
            "dlgt scrollback - read rendered terminal scrollback\n\nUSAGE\n  dlgt scrollback <SESSION_ID|@ALIAS> [OPTIONS]\n\nOPTIONS\n  --lines <COUNT>     Number of rendered lines (default: 100)\n  --before <CURSOR>   Read an older page before a cursor\n  --pretty            Pretty-print JSON output\n  -h, --help          Print this help"
        }
        "logs" => {
            "dlgt logs - read raw retained PTY bytes for diagnosis\n\nUSAGE\n  dlgt logs <SESSION_ID|@ALIAS> --raw [--json]\n\nOPTIONS\n  --raw        Required capability flag; write raw bytes to stdout\n  --json       Return the bytes as base64 JSON\n  -h, --help   Print this help"
        }
        "models" => {
            "dlgt models - discover models supported by a Harness\n\nUSAGE\n  dlgt models [OPTIONS]\n  dlgt models --harness <codex|claude> [OPTIONS]\n\nOPTIONS\n  --harness <codex|claude>   Query one Harness; omitted queries both\n  --include-hidden           Include hidden models\n  --pretty                   Pretty-print JSON output\n  -h, --help                 Print this help\n\nWithout --harness the response lists every Harness, and one that cannot be\nreached reports discovery: \"unavailable\" instead of failing the command."
        }
        "profiles" => {
            "dlgt profiles - list or inspect launch Profiles\n\nUSAGE\n  dlgt profiles list [--pretty]\n  dlgt profiles show <NAME> [--pretty]\n\nOPTIONS\n  --pretty     Pretty-print JSON output\n  -h, --help   Print this help"
        }
        "harnesses" => {
            "dlgt harnesses - list Harness capabilities\n\nUSAGE\n  dlgt harnesses [codex|claude] [--pretty]\n\nOPTIONS\n  --pretty     Pretty-print JSON output\n  -h, --help   Print this help"
        }
        "doctor" => {
            "dlgt doctor - diagnose the local dlgt installation\n\nUSAGE\n  dlgt doctor [OPTIONS]\n\nOPTIONS\n  --json      Emit the same checks as compact JSON\n  --probe     Initialize Codex app-server and check the published release\n  -h, --help  Print this help\n\nThe default checks are read-only and offline. --probe starts no model turn."
        }
        "skill" => {
            "dlgt skill - print the embedded dlgt skill\n\nUSAGE\n  dlgt skill\n\nOPTIONS\n  -h, --help   Print this help"
        }
        "rpc" => {
            "dlgt rpc - use the JSONL RPC interface\n\nUSAGE\n  dlgt rpc --stdio\n\nOPTIONS\n  --stdio      Read JSONL requests from stdin and write responses to stdout\n  -h, --help   Print this help"
        }
        "version" => {
            "dlgt version - print the dlgt version\n\nUSAGE\n  dlgt version\n\nOPTIONS\n  -h, --help   Print this help"
        }
        "help" => {
            "dlgt help - print top-level or command-specific help\n\nUSAGE\n  dlgt help [COMMAND]\n\nOPTIONS\n  -h, --help   Print this help"
        }
        _ => bail!("unknown help topic {command:?}; run `dlgt help`"),
    };
    Ok(usage)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_cwd_resolves_against_the_invocation_directory()
    -> Result<(), Box<dyn std::error::Error>> {
        let base = tempfile::tempdir()?;
        std::fs::create_dir(base.path().join("sub"))?;
        let expected = base
            .path()
            .join("sub")
            .canonicalize()?
            .to_string_lossy()
            .into_owned();

        assert_eq!(resolve_launch_cwd(Some("sub"), base.path())?, expected);
        assert_eq!(resolve_launch_cwd(Some("./sub"), base.path())?, expected);
        Ok(())
    }

    #[test]
    fn absolute_cwd_ignores_the_invocation_directory() -> Result<(), Box<dyn std::error::Error>> {
        let target = tempfile::tempdir()?;
        let unrelated = tempfile::tempdir()?;
        let expected = target.path().canonicalize()?.to_string_lossy().into_owned();

        assert_eq!(
            resolve_launch_cwd(Some(&expected), unrelated.path())?,
            expected
        );
        Ok(())
    }

    #[test]
    fn missing_cwd_fails_on_the_client() -> Result<(), Box<dyn std::error::Error>> {
        let base = tempfile::tempdir()?;

        match resolve_launch_cwd(Some("missing"), base.path()) {
            Ok(resolved) => return Err(format!("expected an error, got {resolved:?}").into()),
            Err(error) => {
                let message = format!("{error:#}");
                assert!(
                    message.contains("--cwd \"missing\" does not exist"),
                    "unexpected error message: {message}"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn omitted_cwd_uses_the_invocation_directory_verbatim() -> Result<(), Box<dyn std::error::Error>>
    {
        assert_eq!(
            resolve_launch_cwd(None, Path::new("/nonexistent-base"))?,
            "/nonexistent-base"
        );
        Ok(())
    }
}
