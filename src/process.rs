use std::{future::Future, process::Output, time::Duration};

use anyhow::{Context, Result};
use tokio::process::Command;

pub const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
pub const LONG_COMMAND_TIMEOUT: Duration = Duration::from_secs(600);

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
}
