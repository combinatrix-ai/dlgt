use std::process::Command;

fn dlgt(args: &[&str]) -> std::io::Result<std::process::Output> {
    cli_command().args(args).output()
}

fn cli_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dlgt"));
    command
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_AUTH_TOKEN")
        .env("DLGT_CONFIG", "/dev/null");
    command
}

#[test]
fn every_public_command_supports_both_help_spellings() -> Result<(), Box<dyn std::error::Error>> {
    let commands = [
        "server", "update", "new", "restart", "send", "fetch", "answer", "cancel", "list", "ls",
        "show", "watch", "stop", "models", "profiles", "doctor", "skill", "rpc", "version", "help",
    ];

    for command in commands {
        let flag_form = dlgt(&[command, "--help"])?;
        let short_form = dlgt(&[command, "-h"])?;
        let help_form = dlgt(&["help", command])?;

        assert!(flag_form.status.success(), "{command} --help failed");
        assert!(short_form.status.success(), "{command} -h failed");
        assert!(help_form.status.success(), "help {command} failed");
        assert_eq!(flag_form.stdout, short_form.stdout, "{command} -h differed");
        assert_eq!(
            flag_form.stdout, help_form.stdout,
            "help {command} differed"
        );

        let help = String::from_utf8(flag_form.stdout)?;
        assert!(help.contains("USAGE"), "{command} help had no usage");
        assert!(
            help.contains(&format!(
                "dlgt {}",
                if command == "ls" { "list" } else { command }
            )),
            "{command} help named the wrong command"
        );
    }
    Ok(())
}

#[test]
fn list_alias_uses_list_help() -> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(
        dlgt(&["list", "--help"])?.stdout,
        dlgt(&["ls", "--help"])?.stdout
    );
    Ok(())
}

#[test]
fn fetch_help_is_single_session_and_wait_binds_the_result() -> Result<(), Box<dyn std::error::Error>>
{
    let help = String::from_utf8(dlgt(&["fetch", "--help"])?.stdout)?;
    assert!(help.contains("dlgt fetch <SESSION_ID|@ALIAS>"));
    assert!(help.contains("--wait <DURATION>"));
    assert!(!help.contains("--all"));
    assert!(!help.contains("--until"));

    let cases: &[&[&str]] = &[
        &["fetch", "--all"],
        &["fetch", "session", "--until", "result"],
    ];
    for args in cases {
        let output = dlgt(args)?;
        assert!(
            !output.status.success(),
            "removed fetch option was accepted: {args:?}"
        );
    }
    Ok(())
}

#[test]
fn harness_option_help_is_provider_neutral() -> Result<(), Box<dyn std::error::Error>> {
    for command in ["new", "send"] {
        let help = String::from_utf8(dlgt(&[command, "--help"])?.stdout)?;
        assert!(help.contains("--harness-option <KEY[=VALUE]>"));
        assert!(!help.contains("Claude CLI option"));
    }
    Ok(())
}

#[test]
fn acceptance_help_documents_prompt_files() -> Result<(), Box<dyn std::error::Error>> {
    for command in ["new", "send"] {
        let help = String::from_utf8(dlgt(&[command, "--help"])?.stdout)?;
        assert!(help.contains("--prompt-file <PATH>"));
        assert!(help.contains("Read the required prompt from a file"));
    }
    Ok(())
}

#[test]
fn auth_policy_help_covers_new_and_resume() -> Result<(), Box<dyn std::error::Error>> {
    let new_help = String::from_utf8(cli_command().arg("new").arg("--help").output()?.stdout)?;
    let send_help = String::from_utf8(cli_command().arg("send").arg("--help").output()?.stdout)?;
    assert!(new_help.contains("--auth <subscription|any>"));
    assert!(send_help.contains("--auth <subscription|any>"));
    assert!(send_help.contains("--profile <PROFILE>"));
    Ok(())
}

#[test]
fn prompt_named_help_is_not_treated_as_a_help_flag() -> Result<(), Box<dyn std::error::Error>> {
    let output = dlgt(&["new", "--", "--help"])?;

    assert!(!output.status.success());
    assert_ne!(output.stdout, dlgt(&["new", "--help"])?.stdout);
    Ok(())
}

#[test]
fn unknown_long_options_are_named_with_the_command_usage() -> Result<(), Box<dyn std::error::Error>>
{
    let home = tempfile::tempdir()?;
    let output = cli_command()
        .env("DLGT_HOME", home.path())
        .args(["show", "codex:test-session", "--json"])
        .output()?;

    assert!(!output.status.success());
    let stdout = String::from_utf8(output.stdout)?;
    assert!(
        stdout.contains("unknown option") && stdout.contains("--json"),
        "unexpected output: {stdout}"
    );
    assert!(stdout.contains("USAGE"), "unexpected output: {stdout}");
    assert!(
        !stdout.contains("missing value"),
        "unknown option consumed the next token: {stdout}"
    );
    assert!(!home.path().join("run").exists());
    Ok(())
}

#[test]
fn acceptance_requires_an_idempotency_key() -> Result<(), Box<dyn std::error::Error>> {
    let home = tempfile::tempdir()?;
    for command in [
        vec!["new", "--title", "t", "--harness", "claude", "--", "hello"],
        vec!["send", "codex:test-session", "--", "hello"],
    ] {
        let output = cli_command()
            .env("DLGT_HOME", home.path())
            .args(&command)
            .output()?;

        assert!(!output.status.success(), "{command:?} was accepted");
        let stdout = String::from_utf8(output.stdout)?;
        assert!(
            stdout.contains("--request-id"),
            "the error must name the flag: {stdout}"
        );
        assert!(stdout.contains("USAGE"), "unexpected output: {stdout}");
    }
    assert!(!home.path().join("run").exists());
    Ok(())
}

#[test]
fn an_idempotency_key_is_validated_before_the_prompt_is_read()
-> Result<(), Box<dyn std::error::Error>> {
    let home = tempfile::tempdir()?;
    let workdir = tempfile::tempdir()?;
    let at_limit = "k".repeat(128);
    let over_limit = "k".repeat(129);
    let cases = [
        ("", true),
        (at_limit.as_str(), false),
        (over_limit.as_str(), true),
    ];
    for (key, rejected) in cases {
        let output = cli_command()
            .env("DLGT_HOME", home.path())
            .current_dir(workdir.path())
            // A missing --cwd fails after the key check, so it marks how far
            // the invocation got without needing a daemon.
            .args([
                "new",
                "--title",
                "t",
                "--harness",
                "claude",
                "--request-id",
                key,
                "--cwd",
                "./missing",
                "--",
                "hello",
            ])
            .output()?;

        assert!(!output.status.success());
        let stdout = String::from_utf8(output.stdout)?;
        if rejected {
            assert!(
                stdout.contains("--request-id"),
                "key of {} bytes should be rejected: {stdout}",
                key.len()
            );
        } else {
            assert!(
                stdout.contains("./missing"),
                "key of {} bytes should be accepted: {stdout}",
                key.len()
            );
        }
    }
    assert!(!home.path().join("run").exists());
    Ok(())
}

#[test]
fn prompt_file_errors_before_starting_a_daemon() -> Result<(), Box<dyn std::error::Error>> {
    let home = tempfile::tempdir()?;
    let workdir = tempfile::tempdir()?;
    let output = cli_command()
        .env("DLGT_HOME", home.path())
        .current_dir(workdir.path())
        .args([
            "new",
            "--title",
            "t",
            "--harness",
            "claude",
            "--request-id",
            "r1",
            "--prompt-file",
            "missing-prompt.md",
        ])
        .output()?;

    assert!(!output.status.success());
    let stdout = String::from_utf8(output.stdout)?;
    assert!(
        stdout.contains("failed to read prompt file missing-prompt.md"),
        "unexpected output: {stdout}"
    );
    assert!(!home.path().join("run").exists());
    Ok(())
}

#[test]
fn prompt_sources_are_mutually_exclusive() -> Result<(), Box<dyn std::error::Error>> {
    let home = tempfile::tempdir()?;
    let prompt = home.path().join("prompt.md");
    std::fs::write(&prompt, "from file")?;
    let output = cli_command()
        .env("DLGT_HOME", home.path())
        .args([
            "new",
            "--title",
            "t",
            "--harness",
            "claude",
            "--request-id",
            "r1",
            "--prompt-file",
            prompt.to_str().ok_or("non-UTF-8 temporary path")?,
            "--",
            "inline",
        ])
        .output()?;

    assert!(!output.status.success());
    let stdout = String::from_utf8(output.stdout)?;
    assert!(
        stdout.contains("--prompt-file, --stdin, and positional prompt are mutually exclusive"),
        "unexpected output: {stdout}"
    );
    assert!(!home.path().join("run").exists());
    Ok(())
}

#[test]
fn resuming_also_requires_an_idempotency_key() -> Result<(), Box<dyn std::error::Error>> {
    let home = tempfile::tempdir()?;
    let output = cli_command()
        .env("DLGT_HOME", home.path())
        .args(["send", "claude:some-session", "--resume", "--", "hello"])
        .output()?;

    assert!(!output.status.success());
    assert!(String::from_utf8(output.stdout)?.contains("--request-id"));
    assert!(!home.path().join("run").exists());
    Ok(())
}

#[test]
fn send_no_longer_accepts_the_removed_wait_flags() -> Result<(), Box<dyn std::error::Error>> {
    let home = tempfile::tempdir()?;
    for flag in [["--wait", "--timeout"], ["--timeout", "1s"]] {
        let output = cli_command()
            .env("DLGT_HOME", home.path())
            .args([
                "send",
                "codex:test-session",
                flag[0],
                flag[1],
                "--",
                "hello",
            ])
            .output()?;

        assert!(!output.status.success());
        let stdout = String::from_utf8(output.stdout)?;
        assert!(
            stdout.contains("unknown option"),
            "unexpected output: {stdout}"
        );
    }
    assert!(!home.path().join("run").exists());
    Ok(())
}

#[test]
fn new_rejects_a_missing_relative_cwd_before_starting_a_daemon()
-> Result<(), Box<dyn std::error::Error>> {
    let home = tempfile::tempdir()?;
    let workdir = tempfile::tempdir()?;
    let output = cli_command()
        .env("DLGT_HOME", home.path())
        .current_dir(workdir.path())
        .args([
            "new",
            "--title",
            "t",
            "--harness",
            "claude",
            "--request-id",
            "r1",
            "--cwd",
            "./missing",
            "--",
            "hello",
        ])
        .output()?;

    assert!(!output.status.success());
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("--cwd"), "unexpected output: {stdout}");
    assert!(stdout.contains("./missing"), "unexpected output: {stdout}");
    assert!(
        stdout.contains("does not exist"),
        "unexpected output: {stdout}"
    );
    assert!(!home.path().join("run").exists());
    Ok(())
}

/// Run a command that must fail before it reaches a daemon.
fn rejected(args: &[&str]) -> Result<String, Box<dyn std::error::Error>> {
    let home = tempfile::tempdir()?;
    let output = cli_command()
        .env("DLGT_HOME", home.path())
        .args(args)
        .output()?;
    assert!(!output.status.success(), "{args:?} was accepted");
    assert!(
        !home.path().join("run").exists(),
        "{args:?} started a daemon before failing"
    );
    Ok(String::from_utf8(output.stdout)?)
}

#[test]
fn the_terminal_commands_and_options_are_gone() -> Result<(), Box<dyn std::error::Error>> {
    let usage = String::from_utf8(dlgt(&["help"])?.stdout)?;
    for command in ["attach", "scrollback", "logs", "harnesses"] {
        assert!(
            !usage.contains(command),
            "top-level help still lists {command}"
        );
        let stdout = rejected(&[command, "codex:test-session"])?;
        assert!(
            stdout.contains("unknown command"),
            "unexpected output for {command}: {stdout}"
        );
        assert!(!dlgt(&["help", command])?.status.success());
    }
    for command in ["answer", "watch"] {
        assert!(usage.contains(command), "top-level help omits {command}");
    }
    assert!(!usage.to_lowercase().contains("screen"));

    for args in [
        &["fetch", "codex:test-session", "--screen"][..],
        &["fetch", "codex:test-session", "--no-screen"],
        &[
            "new",
            "--title",
            "t",
            "--harness",
            "claude",
            "--request-id",
            "r1",
            "--no-auto-approve",
            "--",
            "hello",
        ],
    ] {
        let stdout = rejected(args)?;
        assert!(
            stdout.contains("unknown option"),
            "unexpected output for {args:?}: {stdout}"
        );
    }
    Ok(())
}

#[test]
fn approvals_are_documented_and_validated_before_launch() -> Result<(), Box<dyn std::error::Error>>
{
    for command in ["new", "send"] {
        let help = String::from_utf8(dlgt(&[command, "--help"])?.stdout)?;
        assert!(help.contains("--approvals <auto|ask|bypass>"));
        assert!(!help.contains("auto-approve"));
    }
    let stdout = rejected(&[
        "new",
        "--title",
        "t",
        "--harness",
        "claude",
        "--approvals",
        "yolo",
        "--request-id",
        "r1",
        "--",
        "hello",
    ])?;
    assert!(
        stdout.contains("invalid approvals mode") && stdout.contains("INVALID_ARGUMENT"),
        "unexpected output: {stdout}"
    );
    Ok(())
}

#[test]
fn steer_is_a_send_mode_that_cannot_launch() -> Result<(), Box<dyn std::error::Error>> {
    let help = String::from_utf8(dlgt(&["send", "--help"])?.stdout)?;
    assert!(help.contains("--steer"));

    let stdout = rejected(&[
        "send",
        "claude:test-session",
        "--steer",
        "--resume",
        "--request-id",
        "r1",
        "--",
        "hello",
    ])?;
    assert!(
        stdout.contains("--steer and --resume are mutually exclusive"),
        "unexpected output: {stdout}"
    );
    for option in [["--model", "haiku"], ["--approvals", "ask"]] {
        let stdout = rejected(&[
            "send",
            "claude:test-session",
            option[0],
            option[1],
            "--request-id",
            "r1",
            "--",
            "hello",
        ])?;
        assert!(
            stdout.contains("accepted only with --resume"),
            "unexpected output for {option:?}: {stdout}"
        );
    }
    Ok(())
}

#[test]
fn answer_takes_exactly_one_kind_of_answer() -> Result<(), Box<dyn std::error::Error>> {
    let help = String::from_utf8(dlgt(&["answer", "--help"])?.stdout)?;
    for option in [
        "--allow",
        "--deny",
        "--message <TEXT>",
        "--text <TEXT>",
        "--choice <N>",
    ] {
        assert!(help.contains(option), "answer help omits {option}");
    }
    for (args, expected) in [
        (&["answer", "claude:s", "r1"][..], "exactly one of"),
        (
            &["answer", "claude:s", "r1", "--allow", "--deny"],
            "exactly one of",
        ),
        (
            &["answer", "claude:s", "r1", "--allow", "--message", "x"],
            "only with --deny",
        ),
        (
            &["answer", "claude:s", "r1", "--choice", "0"],
            "invalid --choice",
        ),
        (&["answer", "claude:s", "--allow"], "Request ID"),
    ] {
        let stdout = rejected(args)?;
        assert!(
            stdout.contains(expected),
            "unexpected output for {args:?}: {stdout}"
        );
    }
    Ok(())
}

#[test]
fn fetch_help_documents_messages_and_requests() -> Result<(), Box<dyn std::error::Error>> {
    let help = String::from_utf8(dlgt(&["fetch", "--help"])?.stdout)?;
    assert!(help.contains("--messages[=<MAX>]"));
    assert!(help.contains("--no-messages"));
    assert!(help.contains("dlgt answer"));
    let stdout = rejected(&["fetch", "claude:s", "--messages", "--no-messages"])?;
    assert!(
        stdout.contains("mutually exclusive"),
        "unexpected output: {stdout}"
    );
    Ok(())
}
