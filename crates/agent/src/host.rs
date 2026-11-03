//! Commands on the host. Everything the agent runs (`systemctl`, `ipset`, `iptables`, the engine
//! CLIs) goes through [`CommandRunner`], so tests script the answers and never run a command.

use std::future::Future;
use std::io;
use std::process::Output;

pub trait CommandRunner: Send + Sync {
    /// Runs `program` with `args`, feeding `stdin` if given, and waits for it to end. A program
    /// that cannot be started is an `Err`; one that fails has a non-zero status in the `Output`.
    fn run(
        &self,
        program: &str,
        args: &[&str],
        stdin: Option<&[u8]>,
    ) -> impl Future<Output = io::Result<Output>> + Send;
}

/// Runs commands with `tokio::process`.
#[derive(Debug, Clone, Copy, Default)]
pub struct TokioRunner;

impl CommandRunner for TokioRunner {
    async fn run(&self, program: &str, args: &[&str], stdin: Option<&[u8]>) -> io::Result<Output> {
        use std::process::Stdio;

        use tokio::io::AsyncWriteExt;

        let mut command = tokio::process::Command::new(program);
        command
            .args(args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // A command that is still running when the agent stops is not left behind.
            .kill_on_drop(true);
        let mut child = command.spawn()?;
        let pipe = child.stdin.take();
        let input = stdin.unwrap_or_default();
        // Input and output move together, so a program that fills its output pipe before it
        // has read all its input cannot stall. Dropping the pipe closes the program's stdin.
        let write = async move {
            let Some(mut pipe) = pipe else {
                return Ok(());
            };
            match pipe.write_all(input).await {
                // The program may exit without reading everything: its status says why.
                Err(e) if e.kind() != io::ErrorKind::BrokenPipe => Err(e),
                _ => Ok(()),
            }
        };
        let (written, output) = tokio::join!(write, child.wait_with_output());
        let output = output?;
        written?;
        Ok(output)
    }
}

/// A runner that answers from a script, for tests of the modules that run host commands.
pub mod testing {
    use std::collections::VecDeque;
    use std::io;
    use std::process::Output;
    use std::sync::Mutex;

    use super::CommandRunner;

    /// One call the runner received.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Call {
        pub program: String,
        pub args: Vec<String>,
        pub stdin: Option<Vec<u8>>,
    }

    #[derive(Debug, Default)]
    pub struct ScriptedRunner {
        calls: Mutex<Vec<Call>>,
        answers: Mutex<VecDeque<io::Result<Output>>>,
    }

    impl ScriptedRunner {
        pub fn new() -> ScriptedRunner {
            ScriptedRunner::default()
        }
        /// The next call answers with this exit code and standard output.
        pub fn push_output(&self, code: i32, stdout: &[u8]) {
            use std::os::unix::process::ExitStatusExt;
            let output = Output {
                status: std::process::ExitStatus::from_raw(code << 8),
                stdout: stdout.to_vec(),
                stderr: Vec::new(),
            };
            lock(&self.answers).push_back(Ok(output));
        }
        /// The next call answers with this exit code and standard error.
        pub fn push_failure(&self, code: i32, stderr: &str) {
            use std::os::unix::process::ExitStatusExt;
            let output = Output {
                status: std::process::ExitStatus::from_raw(code << 8),
                stdout: Vec::new(),
                stderr: stderr.as_bytes().to_vec(),
            };
            lock(&self.answers).push_back(Ok(output));
        }
        /// The next call fails as if the program could not be started.
        pub fn push_error(&self, error: io::Error) {
            lock(&self.answers).push_back(Err(error));
        }
        pub fn calls(&self) -> Vec<Call> {
            lock(&self.calls).clone()
        }
    }

    impl CommandRunner for ScriptedRunner {
        async fn run(
            &self,
            program: &str,
            args: &[&str],
            stdin: Option<&[u8]>,
        ) -> io::Result<Output> {
            lock(&self.calls).push(Call {
                program: program.to_owned(),
                args: args.iter().map(|a| (*a).to_owned()).collect(),
                stdin: stdin.map(<[u8]>::to_vec),
            });
            lock(&self.answers).pop_front().unwrap_or_else(|| {
                Err(io::Error::other(format!(
                    "no scripted answer left for {program}"
                )))
            })
        }
    }

    fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
