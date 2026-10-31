//! Own a private process group until its caller has confirmed completion.

use std::io;
use std::os::unix::process::CommandExt;
use tokio::process::{Child, Command};

/// Keep the leader unreaped while descendant I/O can still keep an operation
/// pending. Callers wait last, then immediately mark completion. This keeps
/// the group identity reserved until cancellation can no longer signal it.
pub(crate) struct ProcessGroupChild {
    child: Option<Child>,
    pid: u32,
    label: String,
}

impl ProcessGroupChild {
    pub(crate) fn spawn(mut command: Command, label: &str) -> io::Result<Self> {
        command.kill_on_drop(true).as_std_mut().process_group(0);
        let child = command.spawn()?;
        let pid = child.id().expect("a newly spawned child has a process id");
        Ok(Self {
            child: Some(child),
            pid,
            label: label.into(),
        })
    }

    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }

    pub(crate) fn child_mut(&mut self) -> &mut Child {
        self.child
            .as_mut()
            .expect("child is owned until completion")
    }

    /// Call only after wait has succeeded and all owned I/O has completed.
    pub(crate) fn completed(&mut self) {
        debug_assert!(self.child_mut().id().is_none(), "child must be reaped");
        self.child.take();
    }
}

impl Drop for ProcessGroupChild {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        // A cancelled future must stop the whole group, including children
        // holding inherited output/control FDs after their leader has exited.
        let _ = super::signal_process_group(self.pid, libc::SIGKILL);
        let _ = child.start_kill();
        let label = self.label.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if let Err(error) = child.wait().await {
                    log::warn!("wait for cancelled {label}: {error}");
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, BufReader};

    #[tokio::test]
    async fn dropping_owner_terminates_the_group_and_reaps_its_leader() {
        let mut command = Command::new("sh");
        command
            .args(["-c", "sleep 30 & echo ready; wait"])
            .stdout(std::process::Stdio::piped());
        let mut process = ProcessGroupChild::spawn(command, "test process group").unwrap();
        let pid = process.pid();
        let mut output = BufReader::new(process.child_mut().stdout.take().unwrap());
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(3), output.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(line, "ready\n");
        drop(process);
        tokio::time::timeout(Duration::from_secs(3), async {
            // EOF proves that the descendant holding stdout is gone too.
            assert_eq!(output.read_line(&mut String::new()).await.unwrap(), 0);
            while unsafe { libc::kill(pid as i32, 0) } == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("cancelled process group was not cleaned up");
    }
}
