use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt as _;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex, Weak};

use anyhow::{Context, Result, anyhow, bail};

pub struct Reaper {
    writer: Mutex<Option<ChildStdin>>,
    child: Mutex<Child>,
}

pub struct Registration {
    process_group: i32,
    reaper: Weak<Reaper>,
}

impl Reaper {
    pub fn spawn() -> Result<Arc<Self>> {
        let executable = std::env::current_exe().context("failed to locate dlgt executable")?;
        let mut child = Command::new(executable)
            .args(["server", "--reaper"])
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("failed to start provider process reaper")?;
        let writer = child
            .stdin
            .take()
            .context("provider process reaper had no stdin")?;
        Ok(Arc::new(Self {
            writer: Mutex::new(Some(writer)),
            child: Mutex::new(child),
        }))
    }

    pub fn watch(self: &Arc<Self>, pid: u32) -> Result<Registration> {
        let process_group = i32::try_from(pid).context("provider pid is too large")?;
        self.send('+', process_group)?;
        Ok(Registration {
            process_group,
            reaper: Arc::downgrade(self),
        })
    }

    fn send(&self, operation: char, process_group: i32) -> Result<()> {
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| anyhow!("provider process reaper lock poisoned"))?;
        let writer = writer
            .as_mut()
            .context("provider process reaper is unavailable")?;
        writeln!(writer, "{operation}{process_group}")
            .context("failed to update provider process reaper")?;
        writer
            .flush()
            .context("failed to flush provider process reaper command")
    }
}

impl Drop for Reaper {
    fn drop(&mut self) {
        if let Ok(writer) = self.writer.get_mut() {
            writer.take();
        }
        if let Ok(child) = self.child.get_mut() {
            let _ = child.wait();
        }
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        if let Some(reaper) = self.reaper.upgrade() {
            let _ = reaper.send('-', self.process_group);
        }
    }
}

pub fn run() -> Result<()> {
    // The reaper is intentionally outside the daemon's process group and
    // survives ordinary terminal/service shutdown signals long enough to
    // observe control-pipe EOF and enforce the ownership boundary.
    // SAFETY: signal installs process-wide dispositions before worker threads
    // exist in this small helper process.
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }
    let mut process_groups = HashSet::new();
    for line in BufReader::new(std::io::stdin().lock()).lines() {
        let line = line.context("failed to read provider process reaper command")?;
        let (operation, value) = line.split_at_checked(1).context("empty reaper command")?;
        let process_group = value
            .parse::<i32>()
            .context("invalid reaper process group")?;
        if process_group <= 0 {
            bail!("invalid reaper process group {process_group}");
        }
        match operation {
            "+" => {
                process_groups.insert(process_group);
            }
            "-" => {
                process_groups.remove(&process_group);
            }
            _ => bail!("invalid reaper operation {operation:?}"),
        }
    }

    for process_group in process_groups {
        // SAFETY: kill has no memory-safety preconditions. Every registered
        // child is created as a process-group leader by the owning daemon.
        unsafe {
            libc::kill(-process_group, libc::SIGKILL);
        }
    }
    Ok(())
}
