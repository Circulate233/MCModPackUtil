use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use thiserror::Error;

/// Complete, shell-free description of a child process invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessSpec {
    /// Executable path passed directly to the operating system.
    pub executable: PathBuf,
    /// Individual arguments passed without shell expansion.
    pub args: Vec<String>,
    /// Working directory inherited by the child.
    pub working_dir: PathBuf,
    /// Whether Windows must suppress creation of a separate console window.
    pub suppress_console_window: bool,
}

/// Identifies the standard stream that produced one supervised process log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessOutputStream {
    /// Standard output emitted by the child process.
    Stdout,
    /// Standard error emitted by the child process.
    Stderr,
}

impl ProcessSpec {
    /// Formats an invocation for diagnostics without changing execution semantics.
    pub fn display(&self) -> String {
        format!("{} {}", self.executable.display(), self.args.join(" "))
    }
}

/// Errors from starting or controlling a supervised child process.
#[derive(Debug, Error)]
pub enum ProcessError {
    /// The operating system rejected process creation.
    #[error("failed to start process '{command}': {source}")]
    Start {
        /// Human-readable process invocation.
        command: String,
        /// Operating-system error.
        #[source]
        source: io::Error,
    },
    /// A running process operation failed.
    #[error("failed to {operation} process {pid}: {source}")]
    Control {
        /// Requested operation.
        operation: &'static str,
        /// Child process identifier.
        pid: u32,
        /// Operating-system error.
        #[source]
        source: io::Error,
    },
    /// Graceful shutdown was requested after stdin became unavailable.
    #[error("process {pid} has no writable stdin for graceful shutdown")]
    StdinUnavailable {
        /// Child process identifier.
        pid: u32,
    },
    /// A spawned child did not expose an output pipe requested by the runner.
    #[error("process '{command}' has no {stream:?} pipe")]
    OutputPipe {
        /// Human-readable process invocation.
        command: String,
        /// Stream that was unavailable.
        stream: ProcessOutputStream,
    },
    /// Reading a child output stream failed.
    #[error("failed to read {stream:?} from process {pid}: {source}")]
    OutputRead {
        /// Child process identifier.
        pid: u32,
        /// Stream that failed.
        stream: ProcessOutputStream,
        /// Underlying I/O error.
        #[source]
        source: io::Error,
    },
    /// An output-reader thread terminated unexpectedly.
    #[error("{stream:?} reader thread panicked for process {pid}")]
    OutputReaderPanicked {
        /// Child process identifier.
        pid: u32,
        /// Stream whose reader panicked.
        stream: ProcessOutputStream,
    },
}

/// Starts processes in a form that can be supervised without blocking the caller.
pub trait ProcessRunner {
    /// Spawns one child with piped stdin and returns its control handle.
    fn spawn(&self, spec: &ProcessSpec) -> Result<Box<dyn RunningProcess>, ProcessError>;
}

/// Exposes the lifecycle operations required by the run supervisor.
pub trait RunningProcess: Send {
    /// Returns the operating-system process identifier.
    fn pid(&self) -> u32;
    /// Returns an exit status if the child has exited without blocking.
    fn try_wait(&mut self) -> Result<Option<ExitStatus>, ProcessError>;
    /// Waits for process exit.
    fn wait(&mut self) -> Result<ExitStatus, ProcessError>;
    /// Writes bytes to the child's standard input and flushes them.
    fn write_stdin(&mut self, bytes: &[u8]) -> Result<(), ProcessError>;
    /// Requests Minecraft's normal shutdown through its `stop` console command.
    fn graceful_stop(&mut self) -> Result<(), ProcessError> {
        self.write_stdin(b"stop\n")
    }
    /// Forces process termination through the operating system.
    fn terminate(&mut self) -> Result<(), ProcessError>;
    /// Forwards every output line currently available without blocking.
    ///
    /// Test or embedded implementations that do not expose process output can retain the default
    /// no-op behavior. The system implementation always captures both streams.
    fn drain_output(
        &mut self,
        _output: &mut dyn FnMut(ProcessOutputStream, String),
    ) -> Result<(), ProcessError> {
        Ok(())
    }
    /// Drains remaining output after process exit and waits for output readers to finish.
    fn finish_output(
        &mut self,
        output: &mut dyn FnMut(ProcessOutputStream, String),
    ) -> Result<(), ProcessError> {
        self.drain_output(output)
    }
}

/// Uses `std::process` to start and supervise child processes.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemProcessRunner;

impl ProcessRunner for SystemProcessRunner {
    fn spawn(&self, spec: &ProcessSpec) -> Result<Box<dyn RunningProcess>, ProcessError> {
        let mut command = Command::new(&spec.executable);
        command
            .args(&spec.args)
            .current_dir(&spec.working_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_console_window(&mut command, spec.suppress_console_window);
        let mut child = command.spawn().map_err(|source| ProcessError::Start {
            command: spec.display(),
            source,
        })?;
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => return unavailable_output_pipe(child, spec, ProcessOutputStream::Stdout),
        };
        let stderr = match child.stderr.take() {
            Some(stderr) => stderr,
            None => return unavailable_output_pipe(child, spec, ProcessOutputStream::Stderr),
        };
        let (sender, receiver) = mpsc::sync_channel(PROCESS_OUTPUT_CHANNEL_CAPACITY);
        let output_readers = vec![
            (
                ProcessOutputStream::Stdout,
                spawn_output_reader(stdout, ProcessOutputStream::Stdout, sender.clone()),
            ),
            (
                ProcessOutputStream::Stderr,
                spawn_output_reader(stderr, ProcessOutputStream::Stderr, sender),
            ),
        ];
        Ok(Box::new(SystemRunningProcess {
            child,
            output_receiver: receiver,
            output_readers: Some(output_readers),
        }))
    }
}

const PROCESS_OUTPUT_CHANNEL_CAPACITY: usize = 512;
/// Quiet period after which the final output drain stops.
const FINISH_OUTPUT_SILENCE: Duration = Duration::from_millis(250);
/// Hard limit for the final output drain when output keeps arriving.
const FINISH_OUTPUT_DEADLINE: Duration = Duration::from_secs(2);
const PROCESS_OUTPUT_READ_BUFFER_BYTES: usize = 8 * 1024;
const MAX_PROCESS_OUTPUT_LINE_BYTES: usize = 64 * 1024;

fn unavailable_output_pipe(
    mut child: Child,
    spec: &ProcessSpec,
    stream: ProcessOutputStream,
) -> Result<Box<dyn RunningProcess>, ProcessError> {
    let _ = child.kill();
    let _ = child.wait();
    Err(ProcessError::OutputPipe {
        command: spec.display(),
        stream,
    })
}

/// Applies the process-window policy without changing standard-handle routing.
pub(crate) fn configure_console_window(command: &mut Command, suppress: bool) {
    #[cfg(windows)]
    if suppress {
        use std::os::windows::process::CommandExt;

        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }

    #[cfg(not(windows))]
    let _ = (command, suppress);
}

struct SystemRunningProcess {
    child: Child,
    output_receiver: Receiver<ProcessOutputMessage>,
    output_readers: Option<Vec<(ProcessOutputStream, thread::JoinHandle<()>)>>,
}

impl Drop for SystemRunningProcess {
    fn drop(&mut self) {
        match self.child.try_wait() {
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }
}

enum ProcessOutputMessage {
    Line {
        stream: ProcessOutputStream,
        line: String,
    },
    ReadError {
        stream: ProcessOutputStream,
        source: io::Error,
    },
}

fn spawn_output_reader<R>(
    reader: R,
    stream: ProcessOutputStream,
    sender: mpsc::SyncSender<ProcessOutputMessage>,
) -> thread::JoinHandle<()>
where
    R: io::Read + Send + 'static,
{
    thread::spawn(move || {
        let mut reader = io::BufReader::new(reader);
        let mut read_buffer = [0_u8; PROCESS_OUTPUT_READ_BUFFER_BYTES];
        let mut line_buffer = Vec::new();
        let mut line_truncated = false;
        loop {
            match io::Read::read(&mut reader, &mut read_buffer) {
                Ok(0) => {
                    if (!line_buffer.is_empty() || line_truncated)
                        && send_output_line(&sender, stream, &mut line_buffer, line_truncated)
                            .is_err()
                    {
                        return;
                    }
                    return;
                }
                Ok(read) => {
                    for byte in &read_buffer[..read] {
                        if *byte == b'\n' {
                            if send_output_line(&sender, stream, &mut line_buffer, line_truncated)
                                .is_err()
                            {
                                return;
                            }
                            line_truncated = false;
                        } else if line_buffer.len() < MAX_PROCESS_OUTPUT_LINE_BYTES {
                            line_buffer.push(*byte);
                        } else {
                            line_truncated = true;
                        }
                    }
                }
                Err(source) => {
                    let _ = sender.send(ProcessOutputMessage::ReadError { stream, source });
                    return;
                }
            }
        }
    })
}

fn send_output_line(
    sender: &mpsc::SyncSender<ProcessOutputMessage>,
    stream: ProcessOutputStream,
    line_buffer: &mut Vec<u8>,
    truncated: bool,
) -> Result<(), mpsc::SendError<ProcessOutputMessage>> {
    while matches!(line_buffer.last(), Some(b'\r')) {
        line_buffer.pop();
    }
    let mut line = String::from_utf8_lossy(line_buffer).into_owned();
    if truncated {
        line.push_str(" [line truncated]");
    }
    line_buffer.clear();
    sender.send(ProcessOutputMessage::Line { stream, line })
}

impl RunningProcess for SystemRunningProcess {
    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn try_wait(&mut self) -> Result<Option<ExitStatus>, ProcessError> {
        let pid = self.pid();
        self.child
            .try_wait()
            .map_err(|source| ProcessError::Control {
                operation: "query",
                pid,
                source,
            })
    }

    fn wait(&mut self) -> Result<ExitStatus, ProcessError> {
        let pid = self.pid();
        self.child.wait().map_err(|source| ProcessError::Control {
            operation: "wait for",
            pid,
            source,
        })
    }

    fn write_stdin(&mut self, bytes: &[u8]) -> Result<(), ProcessError> {
        let pid = self.pid();
        let stdin = self
            .child
            .stdin
            .as_mut()
            .ok_or(ProcessError::StdinUnavailable { pid })?;
        stdin
            .write_all(bytes)
            .and_then(|_| stdin.flush())
            .map_err(|source| ProcessError::Control {
                operation: "write to stdin of",
                pid,
                source,
            })
    }

    fn terminate(&mut self) -> Result<(), ProcessError> {
        let pid = self.pid();
        self.child.kill().map_err(|source| ProcessError::Control {
            operation: "terminate",
            pid,
            source,
        })
    }

    fn drain_output(
        &mut self,
        output: &mut dyn FnMut(ProcessOutputStream, String),
    ) -> Result<(), ProcessError> {
        loop {
            match self.output_receiver.try_recv() {
                Ok(ProcessOutputMessage::Line { stream, line }) => output(stream, line),
                Ok(ProcessOutputMessage::ReadError { stream, source }) => {
                    return Err(ProcessError::OutputRead {
                        pid: self.pid(),
                        stream,
                        source,
                    });
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return Ok(()),
            }
        }
    }

    fn finish_output(
        &mut self,
        output: &mut dyn FnMut(ProcessOutputStream, String),
    ) -> Result<(), ProcessError> {
        let read_error = drain_bounded(&self.output_receiver, output);
        // Readers are detached rather than joined: they end when the pipes finally close, which
        // may be long after this run has been reported as finished.
        self.output_readers.take();
        if let Some((stream, source)) = read_error {
            return Err(ProcessError::OutputRead {
                pid: self.pid(),
                stream,
                source,
            });
        }
        Ok(())
    }
}

/// Collects queued and newly arriving output for a bounded time.
///
/// The supervised process has already exited, so collecting its remaining output is best effort:
/// its pipes may still be open in another process that inherited them (a loader wrapper or a
/// helper the game started), and that process never closes them on our schedule. Waiting for the
/// readers to finish would therefore hold this run — and the UI reporting it — open indefinitely,
/// so the drain stops after a short silence and in any case at a fixed deadline.
fn drain_bounded(
    receiver: &Receiver<ProcessOutputMessage>,
    output: &mut dyn FnMut(ProcessOutputStream, String),
) -> Option<(ProcessOutputStream, io::Error)> {
    let started = Instant::now();
    let mut read_error = None;
    loop {
        match receiver.recv_timeout(FINISH_OUTPUT_SILENCE) {
            Ok(ProcessOutputMessage::Line { stream, line }) => output(stream, line),
            Ok(ProcessOutputMessage::ReadError { stream, source }) => {
                if read_error.is_none() {
                    read_error = Some((stream, source));
                }
            }
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
        }
        if started.elapsed() >= FINISH_OUTPUT_DEADLINE {
            break;
        }
    }
    read_error
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Queued output must still be delivered, but an open pipe must not hold the drain open.
    #[test]
    fn bounded_drain_delivers_queued_output_and_gives_up_on_an_open_pipe() {
        let (sender, receiver) = mpsc::sync_channel(4);
        sender
            .send(ProcessOutputMessage::Line {
                stream: ProcessOutputStream::Stdout,
                line: "last words".to_string(),
            })
            .unwrap();

        let started = Instant::now();
        let mut lines = Vec::new();
        let read_error = drain_bounded(&receiver, &mut |_, line| lines.push(line));
        let elapsed = started.elapsed();

        // `sender` stays alive for the whole call, standing in for a reader thread blocked on a
        // pipe that another process still holds open after the supervised process exited.
        assert_eq!(lines, ["last words"]);
        assert!(read_error.is_none());
        assert!(
            elapsed < FINISH_OUTPUT_DEADLINE,
            "the drain waited {elapsed:?} for a pipe that never closed"
        );
    }
}
