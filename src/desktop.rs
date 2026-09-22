//! Optional macOS Desktop backend. Python/Laya is isolated behind JSONL so the
//! decision engine can later be replaced without changing the Session API.
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use crate::reaper::{Reaper, Registration};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

const WORKER: &str = include_str!("../assets/desktop/worker.py");
const NATIVE: &str = include_str!("../assets/desktop/claude.swift");
const SETUP: &str = include_str!("../assets/desktop/setup.sh");

fn directory() -> Result<PathBuf> {
    Ok(crate::paths::home_dir()?
        .join("desktop")
        .join(env!("CARGO_PKG_VERSION")))
}

pub fn setup() -> Result<Value> {
    if !cfg!(target_os = "macos") {
        bail!("claude-desktop requires macOS");
    }
    let path = directory()?;
    fs::create_dir_all(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
    fs::write(path.join("worker.py"), WORKER)?;
    fs::write(path.join("claude.swift"), NATIVE)?;
    fs::write(path.join("setup.sh"), SETUP)?;
    let status = Command::new("/bin/sh")
        .arg(path.join("setup.sh"))
        .arg(&path)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;
    if !status.success() {
        bail!("desktop setup failed");
    }
    Ok(json!({"harness":"claude-desktop", "installed":true, "directory":path}))
}

pub struct Runtime {
    child: Mutex<Child>,
    input: Mutex<ChildStdin>,
    _registration: Registration,
}

impl Runtime {
    pub fn spawn(
        init: &Value,
        environment: &serde_json::Map<String, Value>,
        timeout: Duration,
        reaper: &Arc<Reaper>,
        event: Arc<dyn Fn(Value) + Send + Sync>,
    ) -> Result<Arc<Self>> {
        if !cfg!(target_os = "macos") {
            bail!("claude-desktop requires macOS");
        }
        let path = directory()?;
        let python = path.join("venv/bin/python");
        if !python.exists() || !path.join("claude-ax").exists() {
            bail!("DESKTOP_SETUP_REQUIRED: run `dlgt desktop-setup` first");
        }
        // A running binary must use its own embedded protocol, never stale code.
        if fs::read_to_string(path.join("worker.py"))? != WORKER
            || fs::read_to_string(path.join("claude.swift"))? != NATIVE
        {
            bail!("DESKTOP_SETUP_REQUIRED: assets changed; run `dlgt desktop-setup`");
        }
        let environment = environment
            .iter()
            .map(|(key, value)| {
                Ok((
                    key,
                    value.as_str().context("environment value must be text")?,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut child = Command::new(python)
            .arg("-u")
            .arg(path.join("worker.py"))
            .arg(path.join("claude-ax"))
            .env_clear()
            .envs(environment)
            .env("HF_HOME", path.join("hf"))
            .env("USE_TF", "0")
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let registration = match reaper.watch(child.id()) {
            Ok(value) => value,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        let input = child.stdin.take().context("desktop stdin missing")?;
        let output = child.stdout.take().context("desktop stdout missing")?;
        let runtime = Arc::new(Self {
            child: Mutex::new(child),
            input: Mutex::new(input),
            _registration: registration,
        });
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let event_thread = std::thread::Builder::new()
            .name("dlgt-desktop-events".into())
            .spawn(move || {
                let mut ready = false;
                for line in BufReader::new(output).lines() {
                    let Ok(line) = line else { break };
                    let Ok(value) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    if ready {
                        event(value);
                    } else if value["event"] == "ready" {
                        ready = true;
                        let _ = ready_tx.send(Ok(()));
                    } else if value["event"] == "error" {
                        let _ = ready_tx.send(Err(value["message"]
                            .as_str()
                            .unwrap_or("desktop startup failed")
                            .to_owned()));
                        return;
                    }
                }
                if ready {
                    event(json!({"event":"exit"}));
                }
            });
        if let Err(error) = event_thread {
            let _ = runtime.stop();
            return Err(error.into());
        }
        if let Err(error) = runtime.send(init) {
            let _ = runtime.stop();
            return Err(error);
        }
        match ready_rx.recv_timeout(timeout) {
            Ok(Ok(())) => Ok(runtime),
            result => {
                let _ = runtime.stop();
                let message = match result { Ok(Err(message)) => message, _ => "Desktop startup timed out or helper exited; inspect Claude (folder confirmation may be pending)".into() };
                bail!("{message}")
            }
        }
    }

    pub fn send(&self, value: &Value) -> Result<()> {
        let mut input = self
            .input
            .lock()
            .map_err(|_| anyhow!("desktop input lock poisoned"))?;
        writeln!(input, "{value}")?;
        input.flush()?;
        Ok(())
    }
    pub fn pid(&self) -> Option<u32> {
        self.child.lock().ok().map(|child| child.id())
    }
    pub fn stop(&self) -> Result<()> {
        let mut child = self
            .child
            .lock()
            .map_err(|_| anyhow!("desktop child lock poisoned"))?;
        if child.try_wait()?.is_none() {
            let pid = i32::try_from(child.id())?;
            // SAFETY: the helper owns this process group; Claude.app is not in it.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
            child.wait()?;
        }
        Ok(())
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
