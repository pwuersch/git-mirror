/*
 * Copyright (c) 2018 Pascal Bach
 *
 * SPDX-License-Identifier:     MIT
 */

use std::io;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::Duration;
use thiserror::Error;

use log::{debug, warn};
use wait_timeout::ChildExt;

/// An error occurring during git command execution
#[derive(Debug, Error)]
pub enum GitError {
    #[error("Command {cmd_str} failed with system error: {err}")]
    CommandError { cmd_str: String, err: io::Error },
    #[error("Command {cmd_str} failed with exit code: {code}, Stderr: {stderr}")]
    GitCommandError {
        code: i32,
        stderr: String,
        cmd_str: String,
    },
    #[error("Command {cmd_str} timed out after {timeout:?}, Stderr: {stderr}")]
    GitCommandTimeout {
        cmd_str: String,
        timeout: Duration,
        stderr: String,
    },
}

#[derive(Debug, Error)]
pub enum CommandExecutionError {
    #[error("Unknown system IO error: {0}")]
    SystemIOError(#[from] io::Error),
    #[error("Timeout has been reached: {0:?}")]
    TimeoutReachedError(Duration, String),
}

impl From<(CommandExecutionError, String)> for GitError {
    fn from(value: (CommandExecutionError, String)) -> Self {
        match value {
            (CommandExecutionError::SystemIOError(err), cmd_str) => {
                GitError::CommandError { cmd_str, err }
            }
            (CommandExecutionError::TimeoutReachedError(timeout, stderr), cmd_str) => {
                GitError::GitCommandTimeout {
                    cmd_str,
                    timeout,
                    stderr,
                }
            }
        }
    }
}

/// Common interface to different git backends
/// - [x] git command line
/// - [ ] libgit2
/// - [ ] gitoxide
///
pub trait GitWrapper {
    /// Get the git version
    fn git_version(&self) -> Result<(), Box<GitError>>;
    fn git_lfs_version(&self) -> Result<(), Box<GitError>>;
    fn git_clone_mirror(
        &self,
        origin: &str,
        repo_dir: &Path,
        lfs: bool,
    ) -> Result<(), Box<GitError>>;
    fn git_update_mirror(
        &self,
        origin: &str,
        repo_dir: &Path,
        lfs: bool,
    ) -> Result<(), Box<GitError>>;
    fn git_push_mirror(
        &self,
        dest: &str,
        repo_dir: &Path,
        refspec: &Option<Vec<String>>,
        lfs: bool,
    ) -> Result<(), Box<GitError>>;
}

/// Git command line wrapper
pub struct Git {
    executable: String,
    lfs_enabled: bool,
    timeout: Option<Duration>,
    retries: u32,
    retry_delay: Duration,
}

impl Git {
    pub fn new(executable: String, lfs_enabled: bool, timeout: Option<Duration>) -> Git {
        Git {
            executable,
            lfs_enabled,
            timeout,
            retries: 2,
            retry_delay: Duration::from_secs(30),
        }
    }

    fn git_base_cmd(&self) -> Command {
        let mut git = Command::new(self.executable.clone());
        git.env("GIT_TERMINAL_PROMPT", "0");
        git
    }

    fn run_cmd<F, C>(&self, mut command: F, mut cleanup: C) -> Result<(), Box<GitError>>
    where
        F: FnMut() -> Command,
        C: FnMut() -> io::Result<()>,
    {
        let mut retries = 0;

        loop {
            let mut cmd = command();
            let cmd_str = format!("{:?}", cmd);
            let result: Result<Output, CommandExecutionError> = match self.timeout {
                Some(timeout) => self.run_cmd_with_timeout(cmd, timeout),
                None => cmd.output().map_err(From::from),
            };

            let error = match result {
                Ok(o) => {
                    let stdout = String::from_utf8_lossy(&o.stdout).to_string();
                    if !stdout.is_empty() {
                        debug!("Stdout: {stdout}");
                    }
                    let stderr = String::from_utf8_lossy(&o.stderr).to_string();
                    if !stderr.is_empty() {
                        debug!("Stderr: {stderr}");
                    }
                    if o.status.success() {
                        return Ok(());
                    }
                    GitError::GitCommandError {
                        cmd_str,
                        code: o.status.code().unwrap_or_default(),
                        stderr,
                    }
                }
                Err(err) => (err, cmd_str).into(),
            };

            if retries < self.retries && is_retryable(&error) {
                retries += 1;
                if let Err(cleanup_error) = cleanup() {
                    warn!("Unable to clean up before retrying Git command: {cleanup_error}");
                }
                warn!(
                    "Retrying Git command after transient failure (attempt {}/{}): {}",
                    retries, self.retries, error
                );
                thread::sleep(self.retry_delay);
                continue;
            }

            return Err(Box::new(error));
        }
    }

    fn run_cmd_with_timeout(
        &self,
        mut cmd: Command,
        timeout: Duration,
    ) -> Result<Output, CommandExecutionError> {
        let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;

        match child.wait_timeout(timeout)? {
            Some(_) => Ok(child.wait_with_output()?),
            None => {
                child.kill()?;
                let output = child.wait_with_output()?;
                Err(CommandExecutionError::TimeoutReachedError(
                    timeout,
                    String::from_utf8_lossy(&output.stderr).to_string(),
                ))
            }
        }
    }
}

fn is_retryable(error: &GitError) -> bool {
    match error {
        GitError::GitCommandTimeout { .. } => true,
        GitError::GitCommandError { stderr, .. } => {
            let stderr = stderr.to_ascii_lowercase();
            [
                "connection reset by peer",
                "connection timed out",
                "connection refused",
                "could not connect to server",
                "could not resolve host",
                "early eof",
                "gnutls recv error",
                "http/2 stream",
                "remote end hung up unexpectedly",
                "the tls connection was non-properly terminated",
                "unable to rewind rpc post data",
                "unexpected disconnect while reading sideband packet",
            ]
            .iter()
            .any(|pattern| stderr.contains(pattern))
                || stderr.contains("the requested url returned error: 5")
        }
        GitError::CommandError { .. } => false,
    }
}

impl GitWrapper for Git {
    fn git_version(&self) -> Result<(), Box<GitError>> {
        self.run_cmd(
            || {
                let mut cmd = self.git_base_cmd();
                cmd.arg("--version");
                cmd
            },
            || Ok(()),
        )
    }

    fn git_lfs_version(&self) -> Result<(), Box<GitError>> {
        self.run_cmd(
            || {
                let mut cmd = self.git_base_cmd();
                cmd.args(["lfs", "version"]);
                cmd
            },
            || Ok(()),
        )
    }

    fn git_clone_mirror(
        &self,
        origin: &str,
        repo_dir: &Path,
        lfs: bool,
    ) -> Result<(), Box<GitError>> {
        let repo_dir = repo_dir.to_path_buf();
        let result = self.run_cmd(
            || {
                let mut cmd = self.git_base_cmd();
                cmd.args(["clone", "--mirror"]).arg(origin).arg(&repo_dir);
                cmd
            },
            || {
                if repo_dir.exists() {
                    std::fs::remove_dir_all(&repo_dir)?;
                }
                Ok(())
            },
        );
        if result.is_err() && repo_dir.exists() {
            let _ = std::fs::remove_dir_all(&repo_dir);
        }
        result?;

        if self.lfs_enabled && lfs {
            self.run_cmd(
                || {
                    let mut cmd = self.git_base_cmd();
                    cmd.args(["lfs", "fetch"]).current_dir(&repo_dir);
                    cmd
                },
                || Ok(()),
            )
        } else {
            Ok(())
        }
    }

    fn git_update_mirror(
        &self,
        origin: &str,
        repo_dir: &Path,
        lfs: bool,
    ) -> Result<(), Box<GitError>> {
        self.run_cmd(
            || {
                let mut cmd = self.git_base_cmd();
                cmd.current_dir(repo_dir)
                    .args(["remote", "set-url", "origin"])
                    .arg(origin);
                cmd
            },
            || Ok(()),
        )?;

        self.run_cmd(
            || {
                let mut cmd = self.git_base_cmd();
                cmd.current_dir(repo_dir)
                    .args(["remote", "update", "--prune"]);
                cmd
            },
            || Ok(()),
        )?;

        if self.lfs_enabled && lfs {
            self.run_cmd(
                || {
                    let mut cmd = self.git_base_cmd();
                    cmd.args(["lfs", "fetch"]).current_dir(repo_dir);
                    cmd
                },
                || Ok(()),
            )
        } else {
            Ok(())
        }
    }

    fn git_push_mirror(
        &self,
        dest: &str,
        repo_dir: &Path,
        refspec: &Option<Vec<String>>,
        lfs: bool,
    ) -> Result<(), Box<GitError>> {
        if self.lfs_enabled && lfs {
            self.run_cmd(
                || {
                    let mut cmd = self.git_base_cmd();
                    cmd.args(["lfs", "install"]).current_dir(repo_dir);
                    cmd
                },
                || Ok(()),
            )?;
        }

        self.run_cmd(
            || {
                let mut cmd = self.git_base_cmd();
                cmd.current_dir(repo_dir);
                // Override the LFS URL when pushing, in case .lfsconfig contains another URL.
                cmd.args(["-c", &format!("lfs.url={dest}")]);
                cmd.args(["push", "-f"]);
                if let Some(r) = &refspec {
                    cmd.arg(dest);
                    for spec in r.iter() {
                        cmd.arg(spec);
                    }
                } else {
                    cmd.args(["--mirror", dest]);
                }
                cmd
            },
            || Ok(()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{GitError, is_retryable};
    use std::time::Duration;

    #[test]
    fn retries_timeouts() {
        assert!(is_retryable(&GitError::GitCommandTimeout {
            cmd_str: "git push".to_string(),
            timeout: Duration::from_secs(1),
            stderr: String::new(),
        }));
    }

    #[test]
    fn retries_transient_network_errors() {
        assert!(is_retryable(&GitError::GitCommandError {
            code: 128,
            cmd_str: "git fetch".to_string(),
            stderr: "fatal: early EOF".to_string(),
        }));
    }

    #[test]
    fn does_not_retry_deterministic_remote_errors() {
        assert!(!is_retryable(&GitError::GitCommandError {
            code: 1,
            cmd_str: "git push".to_string(),
            stderr: "remote: You can't push code to an archived project.".to_string(),
        }));
    }
}
