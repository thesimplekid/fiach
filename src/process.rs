use std::{future::Future, process::ExitStatus, process::Output, process::Stdio, time::Duration};

use anyhow::{Context, Result};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

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
