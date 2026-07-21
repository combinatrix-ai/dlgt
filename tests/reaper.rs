use std::io::Write as _;
use std::os::unix::process::CommandExt as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn reaper_kills_registered_process_group_when_daemon_pipe_closes() {
    let mut provider = Command::new("/bin/sh")
        .args(["-c", "sleep 30"])
        .process_group(0)
        .spawn()
        .unwrap_or_else(|error| panic!("failed to start provider fixture: {error}"));
    let mut reaper = Command::new(env!("CARGO_BIN_EXE_dlgt"))
        .args(["server", "--reaper"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap_or_else(|error| panic!("failed to start reaper: {error}"));
    let mut writer = reaper
        .stdin
        .take()
        .unwrap_or_else(|| panic!("reaper had no stdin"));
    writeln!(writer, "+{}", provider.id())
        .unwrap_or_else(|error| panic!("failed to register provider: {error}"));
    std::thread::sleep(Duration::from_millis(500));
    let reaper_group = i32::try_from(reaper.id())
        .unwrap_or_else(|error| panic!("reaper pid is too large: {error}"));
    // SAFETY: kill has no memory-safety preconditions. The test created the
    // reaper as this process-group leader.
    assert_eq!(unsafe { libc::kill(-reaper_group, libc::SIGINT) }, 0);
    std::thread::sleep(Duration::from_millis(100));
    if let Some(status) = reaper
        .try_wait()
        .unwrap_or_else(|error| panic!("failed to inspect signaled reaper: {error}"))
    {
        panic!("reaper did not survive its process-group SIGINT: {status}");
    }
    drop(writer);

    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if provider
            .try_wait()
            .unwrap_or_else(|error| panic!("failed to inspect provider: {error}"))
            .is_some()
        {
            break;
        }
        if Instant::now() >= deadline {
            let _ = provider.kill();
            panic!("registered provider survived reaper EOF");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let status = reaper
        .wait()
        .unwrap_or_else(|error| panic!("failed to wait for reaper: {error}"));
    assert!(status.success());
}
