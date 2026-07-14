use std::process::Command;

fn dlgt(args: &[&str]) -> std::io::Result<std::process::Output> {
    Command::new(env!("CARGO_BIN_EXE_dlgt")).args(args).output()
}

#[test]
fn every_public_command_supports_both_help_spellings() -> Result<(), Box<dyn std::error::Error>> {
    let commands = [
        "server",
        "new",
        "send",
        "wait",
        "cancel",
        "list",
        "ls",
        "show",
        "attach",
        "stop",
        "events",
        "scrollback",
        "logs",
        "models",
        "profiles",
        "harnesses",
        "skill",
        "rpc",
        "version",
        "help",
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
fn prompt_named_help_is_not_treated_as_a_help_flag() -> Result<(), Box<dyn std::error::Error>> {
    let output = dlgt(&["new", "--", "--help"])?;

    assert!(!output.status.success());
    assert_ne!(output.stdout, dlgt(&["new", "--help"])?.stdout);
    Ok(())
}
