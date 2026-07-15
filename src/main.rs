mod client;
mod codex;
mod daemon;
mod paths;
mod protocol;
mod provider;
mod raw_mode;
mod session;
mod skill;
mod store;

use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Write};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use serde_json::{Map, Value, json};

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
            if let Some(provider_session_id) = &failure.provider_session_id {
                error_object.insert("provider_session_id".to_owned(), json!(provider_session_id));
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
        "wait" => command_wait(&args[1..]),
        "cancel" => command_cancel(&args[1..]),
        "list" | "ls" => command_list(&args[1..]),
        "show" => command_show(&args[1..]),
        "attach" => command_attach(&args[1..]),
        "stop" => command_stop(&args[1..]),
        "events" => command_events(&args[1..]),
        "scrollback" => command_scrollback(&args[1..]),
        "logs" => command_logs(&args[1..]),
        "models" => command_models(&args[1..]),
        "profiles" => command_profiles(&args[1..]),
        "harnesses" => command_harnesses(&args[1..]),
        "rpc" => command_rpc(&args[1..]),
        "hook" => command_hook(&args[1..]),
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
    let parsed = Args::parse(args, &["--foreground", "--daemon-child"])?;
    parsed.no_positionals()?;
    daemon::run()
}

fn command_new(args: &[String]) -> Result<()> {
    let parsed = Args::parse(args, &["--wait", "--stdin", "--pretty", "--clean-env"])?;
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
    let prompt = prompt_from(&parsed, 0)?;
    if parsed.flag("--wait") && prompt.is_none() {
        bail!("--wait requires an initial prompt");
    }
    let timeout_ms = if parsed.flag("--wait") {
        Some(parse_duration(parsed.required("--timeout")?)?.as_millis())
    } else {
        None
    };
    let timeout_ms = timeout_ms.map(|value| u64::try_from(value).unwrap_or(u64::MAX));
    let cwd = parsed.one("--cwd").map(str::to_owned).map_or_else(
        || std::env::current_dir().map(|path| path.to_string_lossy().into_owned()),
        Ok,
    )?;
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
    let environment = launch_environment(&parsed, profile.as_ref())?;
    let (rows, cols) = raw_mode::terminal_size(libc::STDIN_FILENO);
    let mut result = client::call(
        "session.create",
        json!({
            "title": title,
            "alias": parsed.one("--alias"),
            "harness": harness,
            "cwd": cwd,
            "model": model,
            "effort": effort,
            "prompt": prompt,
            "startup_timeout_ms": parsed.one("--startup-timeout")
                .map(parse_duration).transpose()?.unwrap_or(Duration::from_secs(60)).as_millis(),
            "environment": environment,
            "rows": rows,
            "cols": cols,
        }),
    )?;
    if let Some(timeout_ms) = timeout_ms {
        let session_id = result
            .pointer("/session/id")
            .and_then(Value::as_str)
            .context("session.create response had no Session ID")?;
        result = client::call(
            "session.wait",
            json!({"session":session_id,"timeout_ms":timeout_ms}),
        )?;
        return print_execution(result, parsed.flag("--pretty"));
    }
    print_success(result, parsed.flag("--pretty"))
}

fn command_send(args: &[String]) -> Result<()> {
    let parsed = Args::parse(args, &["--wait", "--stdin", "--pretty"])?;
    let session = parsed
        .positionals
        .first()
        .context("missing Session selector")?;
    let prompt = prompt_from(&parsed, 1)?.context("missing prompt; use --stdin or -- PROMPT")?;
    let mut result = client::call("session.send", json!({"session":session,"prompt":prompt}))?;
    if parsed.flag("--wait") {
        let timeout = parse_duration(parsed.required("--timeout")?)?;
        result = client::call(
            "session.wait",
            json!({"session":session,"timeout_ms":duration_ms(timeout)}),
        )?;
        return print_execution(result, parsed.flag("--pretty"));
    } else if parsed.one("--timeout").is_some() {
        bail!("--timeout requires --wait");
    }
    print_success(result, parsed.flag("--pretty"))
}

fn command_restart(args: &[String]) -> Result<()> {
    let parsed = Args::parse(args, &["--pretty", "--clean-env"])?;
    let session = parsed.one_positional("Session selector")?;
    let environment = launch_environment(&parsed, None)?;
    let (rows, cols) = raw_mode::terminal_size(libc::STDIN_FILENO);
    let result = client::call(
        "session.restart",
        json!({
            "session": session,
            "startup_timeout_ms": parsed.one("--startup-timeout")
                .map(parse_duration).transpose()?.unwrap_or(Duration::from_secs(60)).as_millis(),
            "environment": environment,
            "rows": rows,
            "cols": cols,
        }),
    )?;
    print_success(result, parsed.flag("--pretty"))
}

fn command_wait(args: &[String]) -> Result<()> {
    let parsed = Args::parse(args, &["--pretty"])?;
    let session = parsed.one_positional("Session selector")?;
    let timeout = parse_duration(parsed.required("--timeout")?)?;
    let result = client::call(
        "session.wait",
        json!({"session":session,"timeout_ms":duration_ms(timeout)}),
    )?;
    print_execution(result, parsed.flag("--pretty"))
}

fn command_cancel(args: &[String]) -> Result<()> {
    let parsed = Args::parse(args, &["--pretty"])?;
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
    let parsed = Args::parse(args, &["--all", "--pretty"])?;
    parsed.no_positionals()?;
    print_success(
        json!({"sessions":client::call("session.list", json!({"all":parsed.flag("--all")}))?}),
        parsed.flag("--pretty"),
    )
}

fn command_show(args: &[String]) -> Result<()> {
    let parsed = Args::parse(args, &["--pretty"])?;
    let result = client::call(
        "session.read",
        json!({"session":parsed.one_positional("Session selector")?}),
    )?;
    print_success(result, parsed.flag("--pretty"))
}

fn command_attach(args: &[String]) -> Result<()> {
    let parsed = Args::parse(args, &["--steal"])?;
    client::attach(
        parsed.one_positional("Session selector")?,
        parsed.flag("--steal"),
    )
}

fn command_stop(args: &[String]) -> Result<()> {
    let parsed = Args::parse(args, &["--force", "--pretty"])?;
    let result = client::call(
        "session.stop",
        json!({
            "session":parsed.one_positional("Session selector")?, "force":parsed.flag("--force")
        }),
    )?;
    print_success(result, parsed.flag("--pretty"))
}

fn command_events(args: &[String]) -> Result<()> {
    let parsed = Args::parse(args, &["--follow", "--pretty"])?;
    if parsed.positionals.len() > 1 {
        bail!("events accepts at most one Session selector");
    }
    let after = parsed
        .one("--after")
        .map(str::parse::<i64>)
        .transpose()
        .context("invalid --after")?
        .unwrap_or(0);
    if parsed.flag("--follow") {
        return client::follow_events(parsed.positionals.first().map(String::as_str), after);
    }
    let value = client::call(
        "event.read",
        json!({"session":parsed.positionals.first(),"after":after}),
    )?;
    let events = value.as_array().context("invalid event response")?;
    print_success(json!({"events":events}), parsed.flag("--pretty"))
}

fn command_scrollback(args: &[String]) -> Result<()> {
    let parsed = Args::parse(args, &["--pretty"])?;
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
    let parsed = Args::parse(args, &["--raw", "--json"])?;
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
    let parsed = Args::parse(args, &["--include-hidden", "--pretty"])?;
    parsed.no_positionals()?;
    let result = client::call(
        "model.list",
        json!({"harness":parsed.required("--harness")?,"include_hidden":parsed.flag("--include-hidden")}),
    )?;
    print_success(result, parsed.flag("--pretty"))
}

fn command_profiles(args: &[String]) -> Result<()> {
    let parsed = Args::parse(args, &["--pretty"])?;
    let action = parsed.positionals.first().map_or("list", String::as_str);
    let profiles = load_profiles()?;
    let result = match action {
        "list" if parsed.positionals.len() == 1 => json!({"profiles":profiles}),
        "show" if parsed.positionals.len() == 2 => {
            let name = &parsed.positionals[1];
            json!({"name":name,"profile":profiles.get(name).with_context(|| format!("profile not found: {name}"))?})
        }
        _ => bail!("usage: dlgt profiles list | profiles show NAME"),
    };
    print_success(result, parsed.flag("--pretty"))
}

fn command_harnesses(args: &[String]) -> Result<()> {
    let parsed = Args::parse(args, &["--pretty"])?;
    if parsed.positionals.len() > 1 {
        bail!("harnesses accepts at most one Harness name");
    }
    let result = client::call(
        "harness.list",
        json!({"harness":parsed.positionals.first()}),
    )?;
    print_success(json!({"harnesses":result}), parsed.flag("--pretty"))
}

fn command_rpc(args: &[String]) -> Result<()> {
    let parsed = Args::parse(args, &["--stdio"])?;
    parsed.no_positionals()?;
    if !parsed.flag("--stdio") {
        bail!("rpc requires --stdio");
    }
    client::rpc_stdio()
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

#[derive(Default)]
struct Args {
    positionals: Vec<String>,
    options: HashMap<String, Vec<String>>,
    flags: HashSet<String>,
}

impl Args {
    fn parse(args: &[String], flags: &[&str]) -> Result<Self> {
        let known_flags = flags.iter().copied().collect::<HashSet<_>>();
        let mut parsed = Self::default();
        let mut index = 0;
        let mut positional = false;
        while index < args.len() {
            let value = &args[index];
            if positional {
                parsed.positionals.push(value.clone());
            } else if value == "--" {
                positional = true;
            } else if value.starts_with("--") {
                if known_flags.contains(value.as_str()) {
                    parsed.flags.insert(value.clone());
                } else {
                    index += 1;
                    let option = args
                        .get(index)
                        .with_context(|| format!("missing value for {value}"))?;
                    parsed
                        .options
                        .entry(value.clone())
                        .or_default()
                        .push(option.clone());
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

fn print_execution(value: Value, pretty: bool) -> Result<()> {
    let failed = value
        .pointer("/result/status")
        .and_then(Value::as_str)
        .is_some_and(|status| status != "completed");
    print_success(value, pretty)?;
    if failed {
        std::process::exit(2);
    }
    Ok(())
}

fn exit_status(code: &str) -> i32 {
    match code {
        "PROVIDER_FAILED" => 2,
        "WAIT_TIMEOUT" | "CANCEL_TIMEOUT" => 3,
        "SESSION_BLOCKED" => 4,
        "SESSION_BUSY" => 5,
        _ => 1,
    }
}

fn print_usage() {
    println!(
        "dlgt - persistent local subagent runtime\n\nUSAGE\n  dlgt <COMMAND> [OPTIONS]\n\nDELEGATION\n  new          Create a Session, optionally with its first prompt\n  restart      Restart a Session\n  send         Send work to an existing idle Session\n  wait         Wait for the current or latest execution\n  cancel       Interrupt the active execution\n\nSESSIONS\n  list, ls     List Sessions\n  show         Show Session state and latest result\n  attach       Attach to the Session screen\n  stop         Stop the Session\n\nOBSERVABILITY\n  events       Read or follow lifecycle events\n  scrollback   Read rendered terminal scrollback\n  logs         Read raw retained PTY bytes (requires --raw)\n\nCONFIGURATION\n  models       Discover Harness models\n  profiles     List or inspect Profiles\n  harnesses    List Harness capabilities\n  skill        Print the embedded dlgt skill\n\nRUNTIME\n  server       Run or stop the daemon\n  rpc          Use JSONL RPC"
    );
}

fn print_command_usage(command: &str) -> Result<()> {
    let usage = match command {
        "server" => {
            "dlgt server - run or stop the local daemon\n\nUSAGE\n  dlgt server [--foreground]\n  dlgt server stop\n\nOPTIONS\n  --foreground   Run in the foreground\n  -h, --help     Print this help"
        }
        "new" => {
            "dlgt new - create a Session, optionally with its first prompt\n\nUSAGE\n  dlgt new --title <TITLE> [OPTIONS] [-- <PROMPT>]\n\nOPTIONS\n  --title <TITLE>                 Human-readable Session title (required)\n  --alias <@ALIAS>               Exact active Session alias\n  --profile <PROFILE>            Reusable launch Profile\n  --harness <codex|claude>       Provider Harness (required without a Profile)\n  --model <MODEL>                 Provider model\n  --effort <LEVEL>               Provider reasoning effort\n  --cwd <DIR>                    Working directory (default: current directory)\n  --harness-option <KEY=VALUE>   Harness-specific option (repeatable)\n  --startup-timeout <DURATION>   Startup timeout (default: 60s)\n  --clean-env                    Start with an empty environment\n  --pass-env <KEY>               Pass a host variable with --clean-env (repeatable)\n  --env <KEY=VALUE>              Set an environment variable (repeatable)\n  --unset-env <KEY>              Remove an environment variable (repeatable)\n  --wait                         Wait for the initial prompt to finish\n  --timeout <DURATION>           Required with --wait\n  --stdin                        Read the initial prompt from stdin\n  --pretty                       Pretty-print JSON output\n  -h, --help                     Print this help"
        }
        "restart" => {
            "dlgt restart - replace a Session process and resume its provider conversation\n\nUSAGE\n  dlgt restart <SESSION_ID> [OPTIONS]\n\nOPTIONS\n  --startup-timeout <DURATION>   Startup timeout (default: 60s)\n  --clean-env                    Start with an empty environment\n  --pass-env <KEY>               Pass a host variable with --clean-env (repeatable)\n  --env <KEY=VALUE>              Set an environment variable (repeatable)\n  --unset-env <KEY>              Remove an environment variable (repeatable)\n  --pretty                       Pretty-print JSON output\n  -h, --help                     Print this help"
        }
        "send" => {
            "dlgt send - send work to an existing idle Session\n\nUSAGE\n  dlgt send <SESSION_ID|@ALIAS> [OPTIONS] [-- <PROMPT>]\n\nOPTIONS\n  --wait                 Wait for the prompt to finish\n  --timeout <DURATION>   Required with --wait\n  --stdin                Read the prompt from stdin\n  --pretty               Pretty-print JSON output\n  -h, --help             Print this help"
        }
        "wait" => {
            "dlgt wait - wait for the current or latest execution\n\nUSAGE\n  dlgt wait <SESSION_ID|@ALIAS> --timeout <DURATION> [--pretty]\n\nOPTIONS\n  --timeout <DURATION>   Positive wait timeout (required)\n  --pretty               Pretty-print JSON output\n  -h, --help             Print this help"
        }
        "cancel" => {
            "dlgt cancel - interrupt the active execution\n\nUSAGE\n  dlgt cancel <SESSION_ID|@ALIAS> [OPTIONS]\n\nOPTIONS\n  --timeout <DURATION>   Cancellation timeout (default: 30s)\n  --pretty               Pretty-print JSON output\n  -h, --help             Print this help"
        }
        "list" | "ls" => {
            "dlgt list - list Sessions\n\nUSAGE\n  dlgt list [--all] [--pretty]\n  dlgt ls [--all] [--pretty]\n\nOPTIONS\n  --all        Include terminal historical Sessions\n  --pretty     Pretty-print JSON output\n  -h, --help   Print this help"
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
        "events" => {
            "dlgt events - read or follow lifecycle events\n\nUSAGE\n  dlgt events [SESSION_ID|@ALIAS] [OPTIONS]\n\nOPTIONS\n  --after <SEQ>   Read events after a sequence number (default: 0)\n  --follow        Follow events as NDJSON\n  --pretty        Pretty-print JSON output when not following\n  -h, --help      Print this help"
        }
        "scrollback" => {
            "dlgt scrollback - read rendered terminal scrollback\n\nUSAGE\n  dlgt scrollback <SESSION_ID|@ALIAS> [OPTIONS]\n\nOPTIONS\n  --lines <COUNT>     Number of rendered lines (default: 100)\n  --before <CURSOR>   Read an older page before a cursor\n  --pretty            Pretty-print JSON output\n  -h, --help          Print this help"
        }
        "logs" => {
            "dlgt logs - read raw retained PTY bytes for diagnosis\n\nUSAGE\n  dlgt logs <SESSION_ID|@ALIAS> --raw [--json]\n\nOPTIONS\n  --raw        Required capability flag; write raw bytes to stdout\n  --json       Return the bytes as base64 JSON\n  -h, --help   Print this help"
        }
        "models" => {
            "dlgt models - discover models supported by a Harness\n\nUSAGE\n  dlgt models --harness <codex|claude> [OPTIONS]\n\nOPTIONS\n  --harness <codex|claude>   Harness to query (required)\n  --include-hidden           Include hidden models\n  --pretty                   Pretty-print JSON output\n  -h, --help                 Print this help"
        }
        "profiles" => {
            "dlgt profiles - list or inspect launch Profiles\n\nUSAGE\n  dlgt profiles list [--pretty]\n  dlgt profiles show <NAME> [--pretty]\n\nOPTIONS\n  --pretty     Pretty-print JSON output\n  -h, --help   Print this help"
        }
        "harnesses" => {
            "dlgt harnesses - list Harness capabilities\n\nUSAGE\n  dlgt harnesses [codex|claude] [--pretty]\n\nOPTIONS\n  --pretty     Pretty-print JSON output\n  -h, --help   Print this help"
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
    println!("{usage}");
    Ok(())
}
