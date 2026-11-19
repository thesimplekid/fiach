use std::{io, pin::Pin, process::Stdio};

use async_trait::async_trait;
use nix::{
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite},
    process::{Child, Command},
};

use crate::{LaunchSpec, Task};

/// Owned transport lifetime, separate from the ACP protocol.
#[async_trait]
pub trait Transport: Send {
    /// Stop owned execution and reap it. Must also clean up on Drop.
    async fn shutdown(&mut self) -> io::Result<()>;
}

pub struct Connection {
    pub reader: Pin<Box<dyn AsyncRead + Send>>,
    pub writer: Pin<Box<dyn AsyncWrite + Send>>,
    pub transport: Box<dyn Transport>,
}

#[async_trait]
pub trait Launcher: Send + Sync {
    async fn launch(&self, spec: &LaunchSpec, task: &Task) -> io::Result<Connection>;
}

#[derive(Debug, Default)]
pub struct ProcessLauncher;

struct Process {
    child: Child,
    group: Option<Pid>,
    stderr: tokio::task::JoinHandle<()>,
    _root: tempfile::TempDir,
}

impl Drop for Process {
    fn drop(&mut self) {
        if let Some(pid) = self.group {
            let _ = killpg(pid, Signal::SIGKILL);
        }
        self.stderr.abort();
    }
}

#[async_trait]
impl Transport for Process {
    async fn shutdown(&mut self) -> io::Result<()> {
        if let Some(pid) = self.group.take() {
            let _ = killpg(pid, Signal::SIGKILL);
        }
        let _ = self.child.start_kill();
        let result = self.child.wait().await.map(|_| ());
        self.stderr.abort();
        result
    }
}

#[async_trait]
impl Launcher for ProcessLauncher {
    async fn launch(&self, spec: &LaunchSpec, task: &Task) -> io::Result<Connection> {
        if !task.cwd.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "task cwd must be absolute",
            ));
        }
        let root = tempfile::Builder::new().prefix("harness-acp-").tempdir()?;
        let config = root.path().join("config");
        tokio::fs::create_dir_all(config.join("custom_providers")).await?;
        // Recipes supplement default builtins. Explicitly disable developer in
        // this private config; callers can opt it back in via recipe.extensions.
        tokio::fs::write(
            config.join("config.yaml"),
            br#"{
          "GOOSE_MODE":"auto", "extensions": {
            "developer":{"enabled":false,"type":"builtin","name":"developer"}
          }
        }"#,
        )
        .await?;
        for (index, definition) in spec.provider_definitions.iter().enumerate() {
            tokio::fs::write(
                config
                    .join("custom_providers")
                    .join(format!("{index}.json")),
                serde_json::to_vec(definition)?,
            )
            .await?;
        }
        let mut command = Command::new(&spec.program);
        command
            .args(&spec.args)
            .current_dir(&task.cwd)
            .env_clear()
            .envs(&spec.env)
            .env("GOOSE_PATH_ROOT", root.path())
            .env("HOME", root.path())
            .env("XDG_CONFIG_HOME", root.path().join("xdg-config"))
            .env("XDG_DATA_HOME", root.path().join("xdg-data"))
            .env("XDG_STATE_HOME", root.path().join("xdg-state"))
            .env("GOOSE_MODE", "auto")
            .env("GOOSE_DISABLE_KEYRING", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command.process_group(0);
        let mut child = command.spawn()?;
        let group = child.id().map(|id| Pid::from_raw(id as i32));
        let reader = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("missing child stdout"))?;
        let writer = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("missing child stdin"))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("missing child stderr"))?;
        // Drain without retaining provider logs: they may contain credentials or
        // hidden reasoning. ACP errors and visible updates are the evidence API.
        let stderr = tokio::spawn(async move {
            let mut buf = [0u8; 8192];
            loop {
                match stderr.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        });
        Ok(Connection {
            reader: Box::pin(reader),
            writer: Box::pin(writer),
            transport: Box::new(Process {
                child,
                group,
                stderr,
                _root: root,
            }),
        })
    }
}
