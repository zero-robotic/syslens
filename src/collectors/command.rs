//! Owned, nonblocking child pipes. Both one-shot queries and streaming sources
//! obey the same cancellation/deadline contract and clean up their own group.
use crate::framework::OperationContext;
use anyhow::{Context, Result, ensure};
use std::{
    io::Read,
    os::{fd::AsRawFd, unix::process::CommandExt},
    path::Path,
    process::{Child, ChildStdout, Command, Stdio},
    time::Duration,
};

pub(super) struct Pipe {
    child: Child,
    stdout: ChildStdout,
}
impl Pipe {
    pub fn spawn(executable: &Path, args: &[&str]) -> Result<Self> {
        let mut child = Command::new(executable)
            .args(args)
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("cannot start {}", executable.display()))?;
        let stdout = child.stdout.take().context("child stdout unavailable")?;
        let result = Self { child, stdout };
        let fd = result.stdout.as_raw_fd();
        // Only our owned descriptor is modified; the child has its own process group.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        ensure!(
            flags >= 0 && unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } >= 0,
            "cannot make child stdout nonblocking"
        );
        Ok(result)
    }
    pub fn read(&mut self, bytes: &mut Vec<u8>) -> Result<bool> {
        let mut buffer = [0u8; 8192];
        match self.stdout.read(&mut buffer) {
            Ok(0) => {
                if let Some(status) = self.child.try_wait()? {
                    ensure!(status.success(), "child query failed: {status}");
                    return Ok(true);
                }
            }
            Ok(length) => {
                ensure!(
                    bytes.len() + length <= 4 * 1024 * 1024,
                    "child output exceeds 4 MiB"
                );
                bytes.extend_from_slice(&buffer[..length]);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error.into()),
        }
        Ok(false)
    }
}
impl Drop for Pipe {
    fn drop(&mut self) {
        // The group was created for this child; never target an external tegrastats.
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let _ = self.child.wait();
    }
}
pub(super) fn command(
    executable: &Path,
    args: &[&str],
    context: &OperationContext,
) -> Result<String> {
    context.check()?;
    let mut pipe = Pipe::spawn(executable, args)?;
    let mut bytes = Vec::new();
    loop {
        context.check()?;
        if pipe.read(&mut bytes)? {
            return Ok(String::from_utf8(bytes)?);
        }
        context.wait(Duration::from_millis(2))?;
    }
}
