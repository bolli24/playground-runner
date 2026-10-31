use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{fs, io};

use serde::{Deserialize, Serialize};
use tempfile::TempDir;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::Notify;

use crate::config::Config;
use crate::image::{self, ImageError, ImageInfo};
use crate::job::{Job, JobError};

/// Written by the adapter to stdout and stderr between the compile and run phases.
pub const MARKER: &[u8] = b"\x1ePLAYGROUND-RUN\x1e\n";

/// Exit code the adapter uses for compile errors in the user's program.
const EXIT_COMPILE_ERROR: i32 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Phase {
    Compile,
    Run,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Outcome {
    /// The program ran and exited, see `exit_code`.
    Exited,
    /// The program doesn't compile; the compile output explains why.
    CompileError,
    /// The image or toolchain is broken.
    ToolchainError,
    TimedOut,
    OutOfMemory,
    /// Output exceeded the limit and the container was killed.
    OutputLimit,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunResult {
    pub outcome: Outcome,
    /// The phase the container was in when it ended.
    pub phase: Phase,
    /// Container exit code when it exited on its own. For `Exited`, the program's exit code.
    pub exit_code: Option<i32>,
    pub compile: Output,
    /// Present when the run phase was reached.
    pub run: Option<Output>,
    /// Commit the image was built from.
    pub revision: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Output {
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error("unknown language '{0}'")]
    UnknownLanguage(String),
    #[error(transparent)]
    Image(#[from] ImageError),
    #[error(transparent)]
    Job(#[from] JobError),
    #[error("unable to prepare job: {0}")]
    Io(#[from] io::Error),
    #[error("docker run failed: {0}")]
    Docker(String),
}

pub struct Executor {
    config: Config,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    Exited,
    TimedOut,
    OutputLimit,
}

#[derive(Default)]
struct Captured {
    buf: Vec<u8>,
    marker_at: Option<Instant>,
}

impl Executor {
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub async fn image(&self, language: &str) -> Result<ImageInfo, ExecError> {
        let lang = self
            .config
            .languages
            .get(language)
            .ok_or_else(|| ExecError::UnknownLanguage(language.into()))?;
        Ok(image::inspect(&self.config.docker, &lang.image).await?)
    }

    pub async fn run(&self, job: &Job) -> Result<RunResult, ExecError> {
        let image = self.image(&job.language).await?;
        self.run_on(&image, job).await
    }

    /// Run a job on a specific image instead of the one configured for its language.
    pub async fn run_on(&self, image: &ImageInfo, job: &Job) -> Result<RunResult, ExecError> {
        job.validate(&image.extensions, &self.config.limits)?;

        let dir = self.prepare(job)?;
        let name = container_name();
        let result = self
            .run_container(image, &name, dir.path(), &job.entry)
            .await;
        self.docker(&["rm", "--force", &name]).await;
        result
    }

    /// Write the job's files to a fresh directory:
    /// `src/` (mounted at `/work/src`) and `stdin` (mounted at `/work/stdin`).
    fn prepare(&self, job: &Job) -> io::Result<TempDir> {
        let mut builder = tempfile::Builder::new();
        builder.prefix("playground-");
        let dir = match &self.config.work_dir {
            Some(work_dir) => builder.tempdir_in(work_dir)?,
            None => builder.tempdir()?,
        };

        let src = dir.path().join("src");
        fs::create_dir(&src)?;
        for file in &job.files {
            let path = src.join(&file.path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&path, &file.content)?;
        }
        fs::write(dir.path().join("stdin"), &job.stdin)?;

        make_readable(dir.path())?;
        Ok(dir)
    }

    fn docker_args(&self, image: &ImageInfo, name: &str, dir: &Path, entry: &str) -> Vec<String> {
        let limits = &self.config.limits;
        let mut args = vec!["run".to_string(), format!("--name={name}")];
        if let Some(runtime) = &self.config.runtime {
            args.push(format!("--runtime={runtime}"));
        }
        args.extend([
            // An init process as PID 1, so the program isn't: signals with default handlers are
            // ignored for PID 1, which turns e.g. abort() into a crash with the wrong exit code
            "--init".to_string(),
            "--network=none".to_string(),
            "--read-only".to_string(),
            "--cap-drop=ALL".to_string(),
            "--security-opt=no-new-privileges".to_string(),
            "--user=65534:65534".to_string(),
            format!("--memory={}m", limits.memory_mb),
            format!("--memory-swap={}m", limits.memory_mb),
            format!("--pids-limit={}", limits.pids),
            format!("--cpus={}", limits.cpus),
            format!("--tmpfs=/work/build:exec,size={}m", limits.build_tmpfs_mb),
            format!("--tmpfs=/tmp:size={}m", limits.tmp_tmpfs_mb),
            format!("--volume={}:/work/src:ro", dir.join("src").display()),
            format!("--volume={}:/work/stdin:ro", dir.join("stdin").display()),
            format!("--env=PLAYGROUND_ENTRY={entry}"),
            "--env=HOME=/tmp".to_string(),
            "--env=TMPDIR=/tmp".to_string(),
            "--env=NO_COLOR=1".to_string(),
            "--env=TERM=dumb".to_string(),
            image.id.clone(),
        ]);
        args
    }

    async fn run_container(
        &self,
        image: &ImageInfo,
        name: &str,
        dir: &Path,
        entry: &str,
    ) -> Result<RunResult, ExecError> {
        let limits = &self.config.limits;
        let started = Instant::now();
        let mut child = Command::new(&self.config.docker)
            .args(self.docker_args(image, name, dir, entry))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;

        let total = Arc::new(AtomicUsize::new(0));
        let over = Arc::new(Notify::new());
        let stdout = tokio::spawn(capture(
            child.stdout.take().expect("stdout is piped"),
            total.clone(),
            limits.output_bytes,
            over.clone(),
        ));
        let stderr = tokio::spawn(capture(
            child.stderr.take().expect("stderr is piped"),
            total,
            limits.output_bytes,
            over.clone(),
        ));

        let end = tokio::select! {
            status = child.wait() => {
                status?;
                End::Exited
            }
            () = tokio::time::sleep(Duration::from_millis(limits.wall_time_ms)) => End::TimedOut,
            () = over.notified() => End::OutputLimit,
        };
        if end != End::Exited {
            self.docker(&["kill", name]).await;
            child.wait().await?;
        }
        let finished = Instant::now();

        let stdout = stdout.await.expect("capture task panicked")?;
        let stderr = stderr.await.expect("capture task panicked")?;

        // The container doesn't exist if docker couldn't create it, e.g. an invalid flag.
        // Docker's own error message is then on stderr.
        let Some((exit_code, oom_killed)) = self.container_state(name).await else {
            return Err(ExecError::Docker(
                String::from_utf8_lossy(&stderr.buf).trim().to_string(),
            ));
        };

        let phase = match stdout.marker_at {
            Some(_) => Phase::Run,
            None => Phase::Compile,
        };
        let outcome = match end {
            End::TimedOut => Outcome::TimedOut,
            End::OutputLimit => Outcome::OutputLimit,
            End::Exited if oom_killed => Outcome::OutOfMemory,
            End::Exited if phase == Phase::Run => Outcome::Exited,
            End::Exited if exit_code == EXIT_COMPILE_ERROR => Outcome::CompileError,
            End::Exited => Outcome::ToolchainError,
        };

        let (compile_stdout, run_stdout) = split_at_marker(&stdout.buf);
        let (compile_stderr, run_stderr) = split_at_marker(&stderr.buf);
        let compile_end = stdout.marker_at.unwrap_or(finished);

        Ok(RunResult {
            outcome,
            phase,
            exit_code: (end == End::Exited).then_some(exit_code),
            compile: Output {
                stdout: lossy(compile_stdout),
                stderr: lossy(compile_stderr),
                duration_ms: millis(compile_end - started),
            },
            run: stdout.marker_at.map(|marker_at| Output {
                stdout: lossy(run_stdout.unwrap_or_default()),
                stderr: lossy(run_stderr.unwrap_or_default()),
                duration_ms: millis(finished - marker_at),
            }),
            revision: image.revision.clone(),
        })
    }

    /// Exit code and whether the container was killed for running out of memory, or `None` if
    /// the container doesn't exist.
    async fn container_state(&self, name: &str) -> Option<(i32, bool)> {
        let output = Command::new(&self.config.docker)
            .args([
                "container",
                "inspect",
                "--format={{.State.ExitCode}} {{.State.OOMKilled}}",
                name,
            ])
            .stdin(Stdio::null())
            .output()
            .await
            .ok()
            .filter(|output| output.status.success())?;
        let text = String::from_utf8_lossy(&output.stdout);
        let (code, oom) = text.trim().split_once(' ')?;
        Some((code.parse().ok()?, oom == "true"))
    }

    /// Run a docker command for cleanup, ignoring failures.
    async fn docker(&self, args: &[&str]) {
        let _ = Command::new(&self.config.docker)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
}

/// Read a stream until it ends, keeping at most `limit` bytes shared with the other stream.
/// Once the limit is reached, `over` is notified and the rest is drained and discarded, so
/// docker never blocks on a full pipe.
async fn capture(
    mut reader: impl AsyncRead + Unpin,
    total: Arc<AtomicUsize>,
    limit: usize,
    over: Arc<Notify>,
) -> io::Result<Captured> {
    let mut captured = Captured::default();
    let mut chunk = vec![0; 8192];
    let mut full = false;
    loop {
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            return Ok(captured);
        }
        if full {
            continue;
        }

        // The marker may be split across reads, so search a little before the new bytes too
        let search_from = captured.buf.len().saturating_sub(MARKER.len() - 1);
        let room = limit.saturating_sub(total.fetch_add(n, Ordering::Relaxed));
        captured.buf.extend_from_slice(&chunk[..n.min(room)]);
        if captured.marker_at.is_none() && find(&captured.buf[search_from..], MARKER).is_some() {
            captured.marker_at = Some(Instant::now());
        }
        if n > room {
            full = true;
            over.notify_one();
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Split output into the part before the first marker and the part after it, if there is one.
fn split_at_marker(buf: &[u8]) -> (&[u8], Option<&[u8]>) {
    match find(buf, MARKER) {
        Some(i) => (&buf[..i], Some(&buf[i + MARKER.len()..])),
        None => (buf, None),
    }
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn millis(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

pub fn container_name() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let started = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|time| time.as_millis())
        .unwrap_or_default();
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("playground-{started}-{}-{count}", std::process::id())
}

/// The container runs as an unprivileged user that must be able to read the job's files.
#[cfg(unix)]
fn make_readable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mode = if path.is_dir() { 0o755 } else { 0o644 };
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    if path.is_dir() {
        for entry in fs::read_dir(path)? {
            make_readable(&entry?.path())?;
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn make_readable(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_at_first_marker() {
        let buf = b"compiling\n\x1ePLAYGROUND-RUN\x1e\nhello\n\x1ePLAYGROUND-RUN\x1e\n";
        let (compile, run) = split_at_marker(buf);
        assert_eq!(compile, b"compiling\n");
        assert_eq!(run, Some(&b"hello\n\x1ePLAYGROUND-RUN\x1e\n"[..]));
    }

    #[test]
    fn no_marker_is_all_compile_output() {
        let (compile, run) = split_at_marker(b"error\n");
        assert_eq!(compile, b"error\n");
        assert_eq!(run, None);
    }

    #[tokio::test]
    async fn capture_finds_split_marker_and_caps_output() {
        let (mut writer, reader) = tokio::io::duplex(4);
        let over = Arc::new(Notify::new());
        let task = tokio::spawn(capture(reader, Arc::new(AtomicUsize::new(0)), 30, over));
        tokio::io::AsyncWriteExt::write_all(
            &mut writer,
            b"ab\x1ePLAYGROUND-RUN\x1e\nprogram output that is too long",
        )
        .await
        .unwrap();
        drop(writer);

        let captured = task.await.unwrap().unwrap();
        assert!(captured.marker_at.is_some());
        assert_eq!(captured.buf.len(), 30);
    }
}
