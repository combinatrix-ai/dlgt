use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::provider::CommandSpec;
use crate::reaper::Registration;

const RETAINED_OUTPUT_LIMIT: usize = 8 * 1024 * 1024;
const INPUT_READY_QUIET_WINDOW: Duration = Duration::from_millis(500);

type OutputCallback = Arc<dyn Fn(&[u8]) + Send + Sync>;
type ExitCallback = Arc<dyn Fn(u32) + Send + Sync>;

pub struct SessionRuntime {
    writer: Mutex<Box<dyn Write + Send>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    output: Arc<OutputState>,
    alive: Arc<AtomicBool>,
    pid: Option<u32>,
    reaper_registration: Mutex<Option<Registration>>,
}

struct OutputState {
    inner: Mutex<OutputInner>,
}

struct OutputInner {
    retained: VecDeque<u8>,
    subscribers: Vec<mpsc::Sender<Vec<u8>>>,
    closed: bool,
    last_output: Option<Instant>,
}

impl SessionRuntime {
    pub fn spawn(
        spec: &CommandSpec,
        size: PtySize,
        on_output: OutputCallback,
        on_exit: ExitCallback,
    ) -> Result<Arc<Self>> {
        let pair = native_pty_system()
            .openpty(size)
            .context("failed to allocate PTY")?;
        let mut command = CommandBuilder::new(&spec.program);
        command.cwd(&spec.cwd);
        command.env_clear();
        for (key, value) in &spec.environment {
            command.env(key, value);
        }
        // Desktop and service parents commonly export TERM=dumb. The child is
        // attached to a real PTY, so advertise a capable terminal explicitly;
        // otherwise interactive providers can stop for confirmation before
        // their lifecycle hooks are installed.
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        for argument in &spec.args {
            command.arg(argument);
        }

        let mut child = pair
            .slave
            .spawn_command(command)
            .with_context(|| format!("failed to spawn {}", spec.program.display()))?;
        let pid = child.process_id();
        let killer = child.clone_killer();
        let mut reader = pair
            .master
            .try_clone_reader()
            .context("failed to clone PTY reader")?;
        let writer = pair
            .master
            .take_writer()
            .context("failed to take PTY writer")?;
        drop(pair.slave);

        let output = Arc::new(OutputState::new());
        let output_reader = Arc::clone(&output);
        std::thread::Builder::new()
            .name("dlgt-pty-output".to_owned())
            .spawn(move || {
                let mut buffer = vec![0_u8; 8192];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            let chunk = &buffer[..read];
                            on_output(chunk);
                            output_reader.push(chunk);
                        }
                    }
                }
                output_reader.close();
            })
            .context("failed to start PTY output thread")?;

        let alive = Arc::new(AtomicBool::new(true));
        let wait_alive = Arc::clone(&alive);
        std::thread::Builder::new()
            .name("dlgt-child-wait".to_owned())
            .spawn(move || {
                let exit_code = child.wait().map_or(1, |status| status.exit_code());
                wait_alive.store(false, Ordering::Release);
                on_exit(exit_code);
            })
            .context("failed to start child wait thread")?;

        Ok(Arc::new(Self {
            writer: Mutex::new(writer),
            master: Mutex::new(pair.master),
            killer: Mutex::new(killer),
            output,
            alive,
            pid,
            reaper_registration: Mutex::new(None),
        }))
    }

    pub fn track_with(&self, registration: Registration) -> Result<()> {
        self.reaper_registration
            .lock()
            .map_err(|_| anyhow::anyhow!("reaper registration lock poisoned"))?
            .replace(registration);
        Ok(())
    }

    pub const fn pid(&self) -> Option<u32> {
        self.pid
    }

    pub fn write(&self, data: &[u8]) -> Result<()> {
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| anyhow::anyhow!("PTY writer lock poisoned"))?;
        writer
            .write_all(data)
            .context("failed to write PTY input")?;
        writer.flush().context("failed to flush PTY input")?;
        Ok(())
    }

    pub fn wait_for_input_ready(&self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            let canonical = {
                let master = self
                    .master
                    .lock()
                    .map_err(|_| anyhow::anyhow!("PTY master lock poisoned"))?;
                let fd = master
                    .as_raw_fd()
                    .context("PTY master has no Unix file descriptor")?;
                // SAFETY: tcgetattr only reads the kernel termios state for a
                // valid PTY file descriptor owned by this runtime.
                let mut attributes = unsafe { std::mem::zeroed::<libc::termios>() };
                if unsafe { libc::tcgetattr(fd, &raw mut attributes) } != 0 {
                    return Err(std::io::Error::last_os_error())
                        .context("failed to read PTY terminal mode");
                }
                attributes.c_lflag & libc::ICANON != 0
            };
            if !canonical && self.output.is_quiet_for(INPUT_READY_QUIET_WINDOW)? {
                return Ok(());
            }
            if self.output.is_closed() {
                bail!("agent exited before configuring interactive terminal input");
            }
            if Instant::now() >= deadline {
                bail!("timed out waiting for agent interactive terminal input mode");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        let master = self
            .master
            .lock()
            .map_err(|_| anyhow::anyhow!("PTY master lock poisoned"))?;
        master
            .resize(PtySize {
                rows: rows.max(1),
                cols: cols.max(1),
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("failed to resize PTY")
    }

    pub fn subscribe(&self) -> Result<(Vec<u8>, mpsc::Receiver<Vec<u8>>)> {
        self.output.subscribe()
    }

    pub fn stop(&self) -> Result<()> {
        self.killer
            .lock()
            .map_err(|_| anyhow::anyhow!("child killer lock poisoned"))?
            .kill()
            .context("failed to stop agent process")
    }

    pub fn force_stop(&self) -> Result<()> {
        let pid = self.pid.context("agent process has no pid")?;
        let pid = i32::try_from(pid).context("agent pid is too large")?;
        // SAFETY: kill has no memory-safety preconditions. portable-pty makes
        // the child a session leader, so the negative pid targets its process
        // group and avoids leaving provider subprocesses behind.
        let group_result = unsafe { libc::kill(-pid, libc::SIGKILL) };
        if group_result == 0 {
            return Ok(());
        }
        let group_error = std::io::Error::last_os_error();
        // Fall back to the leader pid if the process group no longer exists.
        // SAFETY: same rationale as the process-group kill above.
        if unsafe { libc::kill(pid, libc::SIGKILL) } == 0 {
            Ok(())
        } else {
            Err(group_error).context("failed to force-stop agent process group")
        }
    }
}

impl Drop for SessionRuntime {
    fn drop(&mut self) {
        // The daemon owns every runtime. Dropping the daemon therefore also
        // terminates the provider process group instead of orphaning it.
        if self.alive.swap(false, Ordering::AcqRel) {
            let _ = self.force_stop();
        }
    }
}

impl OutputState {
    fn new() -> Self {
        Self {
            inner: Mutex::new(OutputInner {
                retained: VecDeque::new(),
                subscribers: Vec::new(),
                closed: false,
                last_output: None,
            }),
        }
    }

    fn push(&self, data: &[u8]) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        inner.retained.extend(data);
        inner.last_output = Some(Instant::now());
        let overflow = inner.retained.len().saturating_sub(RETAINED_OUTPUT_LIMIT);
        inner.retained.drain(..overflow);
        inner
            .subscribers
            .retain(|subscriber| subscriber.send(data.to_vec()).is_ok());
    }

    fn subscribe(&self) -> Result<(Vec<u8>, mpsc::Receiver<Vec<u8>>)> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("PTY output lock poisoned"))?;
        let (sender, receiver) = mpsc::channel();
        let retained = inner.retained.iter().copied().collect();
        inner.subscribers.push(sender);
        Ok((retained, receiver))
    }

    fn close(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.closed = true;
            inner.subscribers.clear();
        }
    }

    fn is_closed(&self) -> bool {
        self.inner.lock().map_or(true, |inner| inner.closed)
    }

    fn is_quiet_for(&self, duration: Duration) -> Result<bool> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("PTY output lock poisoned"))?;
        Ok(inner
            .last_output
            .is_some_and(|last_output| last_output.elapsed() >= duration))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    use portable_pty::PtySize;

    use super::SessionRuntime;
    use crate::provider::CommandSpec;

    #[test]
    fn owns_pty_and_round_trips_input() {
        let spec = CommandSpec {
            program: PathBuf::from("/bin/cat"),
            args: Vec::new(),
            cwd: PathBuf::from("/tmp"),
            environment: std::collections::HashMap::new(),
        };
        let runtime = SessionRuntime::spawn(
            &spec,
            PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
            Arc::new(|_| {}),
            Arc::new(|_| {}),
        )
        .unwrap_or_else(|error| panic!("failed to spawn PTY: {error}"));
        let (_replay, output) = runtime
            .subscribe()
            .unwrap_or_else(|error| panic!("failed to subscribe: {error}"));
        runtime
            .write(b"hello\r")
            .unwrap_or_else(|error| panic!("failed to write: {error}"));
        let chunk = output
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or_else(|error| panic!("failed to read output: {error}"));
        assert!(String::from_utf8_lossy(&chunk).contains("hello"));
        let _ = runtime.stop();
    }

    #[test]
    fn waits_for_noncanonical_interactive_input_mode() {
        let spec = CommandSpec {
            program: PathBuf::from("/bin/sh"),
            args: vec![
                "-c".to_owned(),
                "sleep 0.05; stty -icanon -echo; printf agent-ready; sleep 1".to_owned(),
            ],
            cwd: PathBuf::from("/tmp"),
            environment: std::collections::HashMap::new(),
        };
        let runtime = SessionRuntime::spawn(
            &spec,
            PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
            Arc::new(|_| {}),
            Arc::new(|_| {}),
        )
        .unwrap_or_else(|error| panic!("failed to spawn PTY: {error}"));

        runtime
            .wait_for_input_ready(Duration::from_secs(2))
            .unwrap_or_else(|error| panic!("failed to observe terminal input mode: {error}"));
        let (replay, _output) = runtime
            .subscribe()
            .unwrap_or_else(|error| panic!("failed to subscribe: {error}"));
        assert!(String::from_utf8_lossy(&replay).contains("agent-ready"));
        let _ = runtime.force_stop();
    }
}
