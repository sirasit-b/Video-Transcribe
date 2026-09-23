//! Spawning ffmpeg so a job can cancel it.
//!
//! Every child is registered with its job, so cancelling a trim kills the encodes
//! it has in flight instead of letting them run to completion in the background.
//! stderr is always drained on its own thread: a process that fills that pipe while
//! we read its stdout would deadlock.

use std::io::Read;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::jobs::{Job, WorkError, WorkResult};

pub struct Ffmpeg {
    child: Arc<Mutex<Child>>,
    stderr: JoinHandle<String>,
    /// Taken by the caller when it asked for a piped stdout/stdin.
    pub stdout: Option<ChildStdout>,
    pub stdin: Option<ChildStdin>,
}

impl Ffmpeg {
    /// Spawn `command`, tracked by `job`. The caller decides whether stdout and
    /// stdin are pipes; stderr always is.
    pub fn spawn(command: &mut Command, job: &Job) -> Result<Ffmpeg, String> {
        command.stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|err| format!("failed to run ffmpeg: {err}"))?;

        let stdout = child.stdout.take();
        let stdin = child.stdin.take();
        let stderr = drain(child.stderr.take());
        let child = Arc::new(Mutex::new(child));
        job.register_child(Arc::clone(&child));

        Ok(Ffmpeg {
            child,
            stderr,
            stdout,
            stdin,
        })
    }

    pub fn kill(&self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
        }
    }

    /// Wait for the process, turning a non-zero exit into the message it printed.
    pub fn wait(self, job: &Job) -> WorkResult<()> {
        let status = {
            let mut child = self.child.lock().expect("child poisoned");
            child
                .wait()
                .map_err(|err| WorkError::Failed(format!("ffmpeg did not exit cleanly: {err}")))?
        };
        job.forget_child(&self.child);
        let message = self.stderr.join().unwrap_or_default();

        if job.is_canceled() {
            return Err(WorkError::Canceled);
        }
        if !status.success() {
            let trimmed = message.trim();
            return Err(WorkError::Failed(if trimmed.is_empty() {
                format!("ffmpeg exited with {status}")
            } else {
                trimmed.to_string()
            }));
        }
        Ok(())
    }

    /// Wait, and hand back what the process printed.
    ///
    /// For the few runs where the output *is* stderr: a meter reports its reading
    /// there, and the file it was asked about is written to nowhere.
    pub fn wait_for_stderr(self, job: &Job) -> WorkResult<String> {
        let status = {
            let mut child = self.child.lock().expect("child poisoned");
            child
                .wait()
                .map_err(|err| WorkError::Failed(format!("ffmpeg did not exit cleanly: {err}")))?
        };
        job.forget_child(&self.child);
        let message = self.stderr.join().unwrap_or_default();

        if job.is_canceled() {
            return Err(WorkError::Canceled);
        }
        if !status.success() {
            let trimmed = message.trim();
            return Err(WorkError::Failed(if trimmed.is_empty() {
                format!("ffmpeg exited with {status}")
            } else {
                trimmed.to_string()
            }));
        }
        Ok(message)
    }

    /// Reap a process we killed on purpose: its exit status carries no news.
    pub fn discard(self, job: &Job) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.wait();
        }
        job.forget_child(&self.child);
        let _ = self.stderr.join();
    }
}

fn drain<R: Read + Send + 'static>(stream: Option<R>) -> JoinHandle<String> {
    std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(mut stream) = stream {
            let _ = stream.read_to_string(&mut text);
        }
        text
    })
}
