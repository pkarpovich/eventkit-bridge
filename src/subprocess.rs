use std::io;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, MutexGuard};
use tokio::time;

/// The most stdout bytes kept before the child is killed.
pub const STDOUT_CAP: usize = 8 * 1024 * 1024;
/// How many trailing stderr bytes are kept.
pub const STDERR_TAIL: usize = 500;
/// How many bytes one pipe read asks for.
pub const READ_CHUNK: usize = 64 * 1024;

/// The lock every EventKit CLI call holds, shared by the `ekctl` and `remindctl` runners.
#[derive(Debug, Clone, Default)]
pub struct StoreLock(Arc<Mutex<()>>);

impl StoreLock {
    /// Waits until no other holder has the lock and takes it.
    pub async fn acquire(&self) -> MutexGuard<'_, ()> {
        self.0.lock().await
    }
}

/// Why a child process produced no output to inspect.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    /// The child could not be started.
    #[error("could not start: {0}")]
    Spawn(#[source] io::Error),
    /// Reading the child's output or waiting for it failed.
    #[error("i/o failed: {0}")]
    Io(#[source] io::Error),
    /// The child ran past its deadline and was killed.
    #[error("timed out")]
    Timeout,
    /// The child wrote more than [`STDOUT_CAP`] and was killed.
    #[error("output too large")]
    OutputTooLarge,
}

/// What a child that ran to its end left behind.
#[derive(Debug)]
pub struct Output {
    /// Everything written to stdout.
    pub stdout: Vec<u8>,
    /// The last [`STDERR_TAIL`] bytes written to stderr.
    pub stderr: Vec<u8>,
    /// How the child exited.
    pub status: ExitStatus,
}

/// Runs `program` with `args` directly, never through a shell, killing it once `timeout` passes.
pub async fn run(program: &Path, args: &[String], timeout: Duration) -> Result<Output, RunError> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(RunError::Spawn)?;
    let collected = match time::timeout(timeout, collect(&mut child)).await {
        Ok(collected) => collected,
        Err(_elapsed) => Err(RunError::Timeout),
    };
    match collected {
        Ok(output) => Ok(output),
        Err(err) => {
            kill(&mut child, program).await;
            Err(err)
        }
    }
}

async fn collect(child: &mut Child) -> Result<Output, RunError> {
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        return Err(RunError::Io(io::Error::other("output pipes are missing")));
    };
    let (stdout, stderr) = tokio::try_join!(read_capped(stdout), read_tail(stderr))?;
    let status = child.wait().await.map_err(RunError::Io)?;
    Ok(Output {
        stdout,
        stderr,
        status,
    })
}

async fn kill(child: &mut Child, program: &Path) {
    if let Err(err) = child.kill().await {
        tracing::warn!(error = %err, program = %program.display(), "could not kill child");
    }
}

async fn read_capped(mut pipe: impl AsyncRead + Unpin) -> Result<Vec<u8>, RunError> {
    let mut output = Vec::new();
    let mut chunk = vec![0; READ_CHUNK];
    loop {
        let read = pipe.read(&mut chunk).await.map_err(RunError::Io)?;
        if read == 0 {
            return Ok(output);
        }
        output.extend_from_slice(&chunk[..read]);
        if output.len() > STDOUT_CAP {
            return Err(RunError::OutputTooLarge);
        }
    }
}

async fn read_tail(mut pipe: impl AsyncRead + Unpin) -> Result<Vec<u8>, RunError> {
    let mut tail = Vec::new();
    let mut chunk = vec![0; READ_CHUNK];
    loop {
        let read = pipe.read(&mut chunk).await.map_err(RunError::Io)?;
        if read == 0 {
            return Ok(tail);
        }
        tail.extend_from_slice(&chunk[..read]);
        if tail.len() > STDERR_TAIL {
            let excess = tail.len() - STDERR_TAIL;
            tail.drain(..excess);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        let mut owned = Vec::new();
        for value in values {
            owned.push((*value).to_owned());
        }
        owned
    }

    #[tokio::test]
    async fn runs_without_a_shell() {
        let output = run(
            Path::new("printf"),
            &args(&["%s|", "$HOME", "a b"]),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"$HOME|a b|");
    }

    #[tokio::test]
    async fn keeps_the_stderr_tail() {
        let script = format!("head -c {} /dev/zero | tr '\\0' x >&2; echo end >&2", 2000);
        let output = run(
            Path::new("sh"),
            &args(&["-c", &script]),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_eq!(output.stderr.len(), STDERR_TAIL);
        assert!(output.stderr.ends_with(b"xxend\n"));
    }

    #[tokio::test]
    async fn times_out() {
        let err = run(
            Path::new("sleep"),
            &args(&["5"]),
            Duration::from_millis(100),
        )
        .await
        .unwrap_err();
        let RunError::Timeout = err else {
            panic!("unexpected error: {err:?}");
        };
    }

    #[tokio::test]
    async fn missing_program() {
        let dir = tempfile::tempdir().unwrap();
        let err = run(&dir.path().join("none"), &[], Duration::from_secs(5))
            .await
            .unwrap_err();
        let RunError::Spawn(source) = err else {
            panic!("unexpected error: {err:?}");
        };
        assert_eq!(source.kind(), io::ErrorKind::NotFound);
    }

    #[tokio::test]
    async fn lock_is_shared_between_clones() {
        let lock = StoreLock::default();
        let other = lock.clone();
        let guard = lock.acquire().await;
        assert!(other.0.try_lock().is_err());
        drop(guard);
        assert!(other.0.try_lock().is_ok());
    }
}
