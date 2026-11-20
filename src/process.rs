use std::{future::Future, process::ExitStatus, process::Output, process::Stdio, time::Duration};

use anyhow::{Context, Result};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

/// Own nspawn in a separate task: dropping the caller requests orderly shutdown
/// instead of SIGKILLing the supervisor and orphaning its PID namespace.
pub(crate) struct NspawnChild {
    cancel: tokio_util::sync::CancellationToken,
    result: tokio::sync::oneshot::Receiver<Result<ExitStatus>>,
    shutdown_timeout: Duration,
    reaped: bool,
}

impl NspawnChild {
    pub(crate) fn spawn(command: &mut Command) -> Result<Self> {
        Self::spawn_with_grace(command, Duration::from_secs(5))
    }

    fn spawn_with_grace(command: &mut Command, grace: Duration) -> Result<Self> {
        let mut child = command
            .kill_on_drop(false)
            .spawn()
            .context("Starting nspawn supervisor")?;
        let cancel = tokio_util::sync::CancellationToken::new();
        let stop = cancel.clone();
        let (send, result) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let mut teardown = NspawnSignalGuard(child.id());
            let outcome = async {
                tokio::select! {
                    result = child.wait() => return result.context("Waiting for nspawn"),
                    _ = stop.cancelled() => {}
                }
                signal_nspawn(&child)?;
                if let Ok(result) = tokio::time::timeout(grace, child.wait()).await {
                    return result.context("Waiting for graceful nspawn shutdown");
                }
                // nspawn's first TERM forwards --kill-signal to its init. The
                // second *same* signal exits its event loop, which kills and
                // reaps the container init (and thereby its PID namespace).
                // SIGKILL to nspawn skips that cleanup and must never be used.
                signal_nspawn(&child)?;
                child
                    .wait()
                    .await
                    .context("Waiting for forced nspawn cleanup")
            }
            .await;
            if child.id().is_none() {
                teardown.0 = None;
            }
            let _ = send.send(outcome);
        });
        Ok(Self {
            cancel,
            result,
            shutdown_timeout: grace * 2 + Duration::from_secs(1),
            reaped: false,
        })
    }

    pub(crate) fn is_reaped(&self) -> bool {
        self.reaped
    }

    pub(crate) async fn wait(&mut self) -> Result<ExitStatus> {
        let result = (&mut self.result)
            .await
            .context("nspawn supervisor task stopped")?;
        self.reaped = result.is_ok();
        result
    }

    pub(crate) async fn shutdown(&mut self) -> Result<ExitStatus> {
        self.cancel.cancel();
        tokio::time::timeout(self.shutdown_timeout, self.wait())
            .await
            .context("nspawn cleanup did not finish; supervisor retained to reap container")?
    }
}

impl Drop for NspawnChild {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

// Runtime teardown cannot await cleanup; at least ask nspawn to forward the
// graceful signal. Normal caller cancellation retains the full cleanup task.
struct NspawnSignalGuard(Option<u32>);
impl Drop for NspawnSignalGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGTERM,
            );
        }
    }
}

fn signal_nspawn(child: &tokio::process::Child) -> Result<()> {
    if let Some(pid) = child.id() {
        // Exclusively owned, unreaped Child: PID cannot have been reused.
        match nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid as i32),
            nix::sys::signal::Signal::SIGTERM,
        ) {
            Ok(()) | Err(nix::errno::Errno::ESRCH) => {}
            Err(error) => return Err(error).context("Signalling nspawn shutdown"),
        }
    }
    Ok(())
}

pub const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
pub const LONG_COMMAND_TIMEOUT: Duration = Duration::from_secs(600);
const STDERR_CAPTURE_LIMIT: usize = 64 * 1024;

pub struct LimitedOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_truncated: bool,
}

pub trait CommandExt {
    fn output_bounded(&mut self, operation: &str) -> impl Future<Output = Result<Output>> + Send;

    fn output_with_timeout(
        &mut self,
        operation: &str,
        command_timeout: Duration,
    ) -> impl Future<Output = Result<Output>> + Send;
}

impl CommandExt for Command {
    async fn output_bounded(&mut self, operation: &str) -> Result<Output> {
        self.output_with_timeout(operation, DEFAULT_COMMAND_TIMEOUT)
            .await
    }

    async fn output_with_timeout(
        &mut self,
        operation: &str,
        command_timeout: Duration,
    ) -> Result<Output> {
        self.kill_on_drop(true);
        tokio::time::timeout(command_timeout, self.output())
            .await
            .with_context(|| format!("Timed out while {operation}"))?
            .with_context(|| format!("Failed while {operation}"))
    }
}

pub async fn output_limited(
    command: &mut Command,
    operation: &str,
    command_timeout: Duration,
    stdout_limit: usize,
) -> Result<LimitedOutput> {
    command
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .with_context(|| format!("Failed while {operation}"))?;
    let stdout = child
        .stdout
        .take()
        .context("Bounded command stdout was not piped")?;
    let stderr = child
        .stderr
        .take()
        .context("Bounded command stderr was not piped")?;
    let stdout_read_limit = stdout_limit.saturating_add(1);
    let mut stdout_task = tokio::spawn(async move {
        let mut bytes = Vec::new();
        stdout
            .take(stdout_read_limit as u64)
            .read_to_end(&mut bytes)
            .await?;
        Ok::<_, std::io::Error>(bytes)
    });
    let stderr_task = tokio::spawn(read_capped(stderr, STDERR_CAPTURE_LIMIT));
    let deadline = tokio::time::Instant::now() + command_timeout;

    let (status, mut stdout) = tokio::select! {
        stdout = &mut stdout_task => {
            let stdout = stdout.context("Bounded stdout reader task failed")??;
            if stdout.len() > stdout_limit {
                let _ = child.kill().await;
                let status = child.wait().await.context("Failed to reap truncated command")?;
                (status, stdout)
            } else {
                let status = match tokio::time::timeout_at(deadline, child.wait()).await {
                    Ok(status) => status.with_context(|| format!("Failed while {operation}"))?,
                    Err(_) => {
                        let _ = child.kill().await;
                        let _ = child.wait().await;
                        anyhow::bail!("Timed out while {operation}");
                    }
                };
                (status, stdout)
            }
        }
        status = tokio::time::timeout_at(deadline, child.wait()) => {
            let status = match status {
                Ok(status) => status.with_context(|| format!("Failed while {operation}"))?,
                Err(_) => {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    anyhow::bail!("Timed out while {operation}");
                }
            };
            let stdout = stdout_task
                .await
                .context("Bounded stdout reader task failed")??;
            (status, stdout)
        }
    };

    let stdout_truncated = stdout.len() > stdout_limit;
    stdout.truncate(stdout_limit);
    let (stderr, stderr_truncated) = stderr_task
        .await
        .context("Bounded stderr reader task failed")??;
    if stderr_truncated {
        tracing::debug!(operation, "Truncated captured command stderr");
    }

    Ok(LimitedOutput {
        status,
        stdout,
        stderr,
        stdout_truncated,
    })
}

async fn read_capped(
    mut reader: impl AsyncRead + Unpin,
    limit: usize,
) -> std::io::Result<(Vec<u8>, bool)> {
    let mut output = Vec::new();
    let mut buffer = [0u8; 8192];
    let mut truncated = false;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(output.len());
        let retained = remaining.min(read);
        output.extend_from_slice(&buffer[..retained]);
        truncated |= retained < read;
    }
    Ok((output, truncated))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn supervised_fixture(grace: Duration) -> (super::NspawnChild, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("signals");
        let script = r#"
import signal, sys, time
path = sys.argv[1]
count = 0
def stop(signum, frame):
    global count
    count += 1
    with open(path, 'a') as out: out.write('TERM\n')
    if count == 2: sys.exit(0)
signal.signal(signal.SIGTERM, stop)
with open(path, 'w') as out: out.write('ready\n')
while True: time.sleep(0.01)
"#;
        let child = super::NspawnChild::spawn_with_grace(
            Command::new("python3").args(["-c", script]).arg(&marker),
            grace,
        )
        .unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while !marker.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        (child, dir)
    }

    #[tokio::test]
    async fn nspawn_shutdown_escalates_with_the_same_signal_and_reaps() {
        let grace = Duration::from_millis(100);
        let (mut child, dir) = supervised_fixture(grace).await;
        let started = std::time::Instant::now();
        let status = tokio::time::timeout(Duration::from_secs(3), child.shutdown())
            .await
            .unwrap()
            .unwrap();
        assert!(status.success());
        assert!(started.elapsed() >= grace);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("signals")).unwrap(),
            "ready\nTERM\nTERM\n"
        );
    }

    #[tokio::test]
    async fn dropping_nspawn_owner_still_runs_native_cleanup() {
        let (child, dir) = supervised_fixture(Duration::from_millis(100)).await;
        drop(child);
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if std::fs::read_to_string(dir.path().join("signals")).unwrap()
                    == "ready\nTERM\nTERM\n"
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn captures_command_output() {
        let output = Command::new("sh")
            .args(["-c", "printf 'bounded output'"])
            .output_bounded("capturing test output")
            .await
            .unwrap();

        assert!(output.status.success());
        assert_eq!(output.stdout, b"bounded output");
    }

    #[tokio::test]
    async fn reports_timeout() {
        let error = Command::new("sh")
            .args(["-c", "exec sleep 60"])
            .output_with_timeout("running a slow test command", Duration::from_millis(20))
            .await
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            "Timed out while running a slow test command"
        );
    }

    #[tokio::test]
    async fn truncates_large_stdout_without_waiting_for_more_output() {
        let output = output_limited(
            Command::new("sh").args(["-c", "while :; do printf x; done"]),
            "running a noisy test command",
            Duration::from_secs(3),
            64,
        )
        .await
        .unwrap();

        assert!(output.stdout_truncated);
        assert_eq!(output.stdout.len(), 64);
    }
}
