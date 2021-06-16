use crate::{CmdResult, FunResult};
use log::{error, info};
use os_pipe::PipeReader;
use std::io::{BufRead, BufReader, Error, ErrorKind, Read, Result};
use std::process::{Child, ExitStatus};
use std::thread::JoinHandle;

/// Representation of running or exited children processes, connected with pipes
/// optionally.
///
/// Calling `spawn!` or `spawn_with_output!` macro will return `Result<CmdChildren>`
pub struct CmdChildren(Vec<CmdChild>);
impl CmdChildren {
    pub(crate) fn from(children: Vec<CmdChild>) -> Self {
        Self(children)
    }

    pub fn wait_cmd_result(&mut self) -> CmdResult {
        let ret = self.wait_cmd_result_nolog();
        if let Err(ref err) = ret {
            error!(
                "Running {} failed, Error: {}",
                CmdChild::get_full_cmd(&self.0),
                err
            );
        }
        ret
    }

    pub(crate) fn wait_cmd_result_nolog(&mut self) -> CmdResult {
        // wait last process result
        let handle = self.0.pop().unwrap();
        handle.wait(true)?;
        Self::wait_children(&mut self.0)
    }

    fn wait_children(children: &mut Vec<CmdChild>) -> CmdResult {
        while !children.is_empty() {
            let child_handle = children.pop().unwrap();
            child_handle.wait(false)?;
        }
        Ok(())
    }

    pub fn wait_fun_result(&mut self) -> FunResult {
        let ret = self.wait_fun_result_nolog();
        if let Err(ref err) = ret {
            error!(
                "Running {} failed, Error: {}",
                CmdChild::get_full_cmd(&self.0),
                err
            );
        }
        ret
    }

    pub(crate) fn wait_fun_result_nolog(&mut self) -> FunResult {
        // wait last process result
        let handle = self.0.pop().unwrap();
        let wait_last = handle.wait_with_output();
        match wait_last {
            Err(e) => {
                let _ = CmdChildren::wait_children(&mut self.0);
                Err(e)
            }
            Ok(output) => {
                let mut ret = String::from_utf8_lossy(&output).to_string();
                if ret.ends_with('\n') {
                    ret.pop();
                }
                CmdChildren::wait_children(&mut self.0)?;
                Ok(ret)
            }
        }
    }

    pub fn wait_with_pipe(&mut self, f: &mut dyn FnMut(Box<dyn Read>)) {
        let handle = self.0.pop().unwrap();
        match handle {
            CmdChild::Proc {
                mut child,
                mut stderr,
                ..
            } => {
                if let Some(stdout) = child.stdout.take() {
                    f(Box::new(stdout));
                    let _ = child.kill();
                }
                CmdChild::log_stderr_output(&mut stderr);
            }
            CmdChild::ThreadFn { .. } => {
                panic!("should not wait pipe on thread");
            }
            CmdChild::SyncFn {
                mut stderr, stdout, ..
            } => {
                CmdChild::log_stderr_output(&mut stderr);
                if let Some(stdout) = stdout {
                    f(Box::new(stdout));
                }
            }
        };
        let _ = Self::wait_children(&mut self.0);
    }
}

#[derive(Debug)]
pub(crate) enum CmdChild {
    Proc {
        child: Child,
        cmd: String,
        stderr: Option<PipeReader>,
        ignore_error: bool,
    },
    ThreadFn {
        child: JoinHandle<CmdResult>,
        cmd: String,
        stdout: Option<PipeReader>,
        stderr: Option<PipeReader>,
        ignore_error: bool,
    },
    SyncFn {
        cmd: String,
        stdout: Option<PipeReader>,
        stderr: Option<PipeReader>,
    },
}

impl CmdChild {
    fn wait(self, is_last: bool) -> CmdResult {
        let pipefail = std::env::var("CMD_LIB_PIPEFAIL") != Ok("0".into());
        let check_result = |result| {
            if let Err(e) = result {
                if is_last || pipefail {
                    return Err(e);
                }
            }
            Ok(())
        };
        match self {
            CmdChild::Proc {
                mut child,
                mut stderr,
                cmd,
                ignore_error,
            } => {
                Self::log_stderr_output(&mut stderr);
                Self::print_stdout_output(&mut child.stdout);
                let status = child.wait()?;
                Self::log_stderr_output(&mut stderr);
                Self::print_stdout_output(&mut child.stdout);
                if !ignore_error && !status.success() && (is_last || pipefail) {
                    return Err(Self::status_to_io_error(
                        status,
                        &format!("{} exited with error", cmd),
                    ));
                }
            }
            CmdChild::ThreadFn {
                child,
                cmd,
                mut stderr,
                ignore_error,
                ..
            } => {
                Self::log_stderr_output(&mut stderr);
                let status = child.join();
                Self::log_stderr_output(&mut stderr);
                if ignore_error {
                    return Ok(());
                }
                match status {
                    Err(e) => {
                        if is_last || pipefail {
                            return Err(Error::new(
                                ErrorKind::Other,
                                format!("{} thread exited with error: {:?}", cmd, e),
                            ));
                        }
                    }
                    Ok(result) => {
                        check_result(result)?;
                    }
                }
            }
            CmdChild::SyncFn {
                mut stdout,
                mut stderr,
                ..
            } => {
                Self::log_stderr_output(&mut stderr);
                Self::print_stdout_output(&mut stdout);
            }
        }
        Ok(())
    }

    fn wait_with_output(self) -> Result<Vec<u8>> {
        match self {
            CmdChild::Proc {
                child,
                cmd,
                mut stderr,
                ignore_error,
            } => {
                Self::log_stderr_output(&mut stderr);
                let output = child.wait_with_output()?;
                Self::log_stderr_output(&mut stderr);
                if !ignore_error && !output.status.success() {
                    return Err(Self::status_to_io_error(
                        output.status,
                        &format!("{} exited with error", cmd),
                    ));
                } else {
                    Ok(output.stdout)
                }
            }
            CmdChild::ThreadFn { cmd, .. } => {
                panic!("{} thread should not be waited for output", cmd);
            }
            CmdChild::SyncFn {
                stdout, mut stderr, ..
            } => {
                Self::log_stderr_output(&mut stderr);
                if let Some(mut out) = stdout {
                    let mut buf = vec![];
                    out.read_to_end(&mut buf)?;
                    return Ok(buf);
                }
                Ok(vec![])
            }
        }
    }

    fn print_stdout_output(stdout: &mut Option<impl Read>) {
        if let Some(stdout) = stdout {
            BufReader::new(stdout)
                .lines()
                .filter_map(|line| line.ok())
                .for_each(|line| println!("{}", line));
        }
    }

    fn log_stderr_output(stderr: &mut Option<impl Read>) {
        if let Some(stderr) = stderr {
            BufReader::new(stderr)
                .lines()
                .filter_map(|line| line.ok())
                .for_each(|line| info!("{}", line));
        }
    }

    fn status_to_io_error(status: ExitStatus, command: &str) -> Error {
        if let Some(code) = status.code() {
            Error::new(
                ErrorKind::Other,
                format!("{}; status code: {}", command, code),
            )
        } else {
            Error::new(
                ErrorKind::Other,
                format!("{}; terminated by {}", command, status),
            )
        }
    }

    fn get_full_cmd(children: &[Self]) -> String {
        children
            .iter()
            .map(|child| match child {
                CmdChild::Proc { cmd, .. } => cmd.to_owned(),
                CmdChild::ThreadFn { cmd, .. } => cmd.to_owned(),
                CmdChild::SyncFn { cmd, .. } => cmd.to_owned(),
            })
            .collect::<Vec<_>>()
            .join(" | ")
    }
}
