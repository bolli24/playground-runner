use std::collections::BTreeMap;
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

/// Starts a view in the compile phase's stdout: `\x1ePLAYGROUND-VIEW <name>\x1e\n`, followed by
/// the view's text up to the next header or the end of the compile output.
const VIEW_HEADER: &[u8] = b"\x1ePLAYGROUND-VIEW ";
const VIEW_HEADER_END: &[u8] = b"\x1e\n";

/// Longer views are cut and marked as truncated.
pub const VIEW_BYTES: usize = 256 * 1024;

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
    /// The program compiled and wasn't run, because the job asked not to.
    Compiled,
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
    /// The requested views the adapter printed. A view is missing when the compiler failed
    /// before producing it.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub views: BTreeMap<String, View>,
    /// Commit the image was built from.
    pub revision: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct View {
    pub content: String,
    /// The view was longer than `VIEW_BYTES` and was cut.
    pub truncated: bool,
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

/// Output limits shared by stdout and stderr, one for each phase.
#[derive(Clone)]
struct Budget {
    compile: Arc<AtomicUsize>,
    run: Arc<AtomicUsize>,
    compile_limit: usize,
    run_limit: usize,
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
        job.validate(image, &self.config.limits)?;

        let dir = self.prepare(job)?;
        let name = container_name();
        let result = self.run_container(image, &name, dir.path(), job).await;
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

    fn docker_args(&self, image: &ImageInfo, name: &str, dir: &Path, job: &Job) -> Vec<String> {
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
            format!("--env=PLAYGROUND_ENTRY={}", job.entry),
            format!("--env=PLAYGROUND_VIEWS={}", job.views.join(",")),
            format!("--env=PLAYGROUND_OPTIONS={}", job.options.join(",")),
            format!("--env=PLAYGROUND_RUN={}", u8::from(job.run)),
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
        job: &Job,
    ) -> Result<RunResult, ExecError> {
        let limits = &self.config.limits;
        let started = Instant::now();
        let mut child = Command::new(&self.config.docker)
            .args(self.docker_args(image, name, dir, job))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;

        let budget = Budget {
            compile: Arc::new(AtomicUsize::new(0)),
            run: Arc::new(AtomicUsize::new(0)),
            compile_limit: limits.compile_output_bytes,
            run_limit: limits.output_bytes,
        };
        let over = Arc::new(Notify::new());
        let stdout = tokio::spawn(capture(
            child.stdout.take().expect("stdout is piped"),
            budget.clone(),
            over.clone(),
        ));
        let stderr = tokio::spawn(capture(
            child.stderr.take().expect("stderr is piped"),
            budget,
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
            End::Exited if exit_code == 0 && !job.run => Outcome::Compiled,
            End::Exited => Outcome::ToolchainError,
        };

        let (compile_stdout, run_stdout) = split_at_marker(&stdout.buf);
        let (compile_stdout, views) = split_views(compile_stdout, &job.views);
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
            views,
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

/// Read a stream until it ends, keeping at most as many bytes as the budget of the current phase
/// allows, shared with the other stream. Once a limit is reached, `over` is notified and the rest
/// is drained and discarded, so docker never blocks on a full pipe.
async fn capture(
    mut reader: impl AsyncRead + Unpin,
    budget: Budget,
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

        // A read containing the marker counts against the compile budget as a whole, so the run
        // phase can get up to one read's worth of extra room
        let (total, limit) = match captured.marker_at {
            None => (&budget.compile, budget.compile_limit),
            Some(_) => (&budget.run, budget.run_limit),
        };
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

/// Split compile stdout into the compiler's own output and the views printed after it. Views that
/// weren't requested are dropped, and each view is cut to `VIEW_BYTES`.
fn split_views<'a>(buf: &'a [u8], requested: &[String]) -> (&'a [u8], BTreeMap<String, View>) {
    let mut views = BTreeMap::new();
    let Some(first) = find(buf, VIEW_HEADER) else {
        return (buf, views);
    };

    let mut rest = &buf[first..];
    while let Some(header) = rest.strip_prefix(VIEW_HEADER) {
        // A header cut off by the output limit ends the views
        let Some(name_end) = find(header, VIEW_HEADER_END) else {
            break;
        };
        let body = &header[name_end + VIEW_HEADER_END.len()..];
        let body_end = find(body, VIEW_HEADER).unwrap_or(body.len());
        let content = &body[..body_end];
        if let Ok(name) = std::str::from_utf8(&header[..name_end])
            && requested.iter().any(|view| view == name)
        {
            views.insert(
                name.to_string(),
                View {
                    content: lossy(&content[..content.len().min(VIEW_BYTES)]),
                    truncated: content.len() > VIEW_BYTES,
                },
            );
        }
        rest = &body[body_end..];
    }
    (&buf[..first], views)
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

    #[test]
    fn splits_views_from_compile_output() {
        let buf = b"warning\n\x1ePLAYGROUND-VIEW hir\x1e\nfn main\n\
            \x1ePLAYGROUND-VIEW other\x1e\nx\n\x1ePLAYGROUND-VIEW mir\x1e\nbb0\n";
        let requested = vec!["mir".to_string(), "hir".to_string(), "asm".to_string()];
        let (compile, views) = split_views(buf, &requested);
        assert_eq!(compile, b"warning\n");
        assert_eq!(views.keys().collect::<Vec<_>>(), ["hir", "mir"]);
        assert_eq!(views["hir"].content, "fn main\n");
        assert_eq!(views["mir"].content, "bb0\n");
        assert!(!views["mir"].truncated);
    }

    #[test]
    fn truncates_long_views_and_ignores_cut_headers() {
        let mut buf = b"\x1ePLAYGROUND-VIEW mir\x1e\n".to_vec();
        buf.extend(vec![b'x'; VIEW_BYTES + 1]);
        buf.extend(b"\x1ePLAYGROUND-VIEW h");
        let (compile, views) = split_views(&buf, &["mir".to_string(), "hir".to_string()]);
        assert!(compile.is_empty());
        assert_eq!(views.len(), 1);
        assert_eq!(views["mir"].content.len(), VIEW_BYTES);
        assert!(views["mir"].truncated);
    }

    #[tokio::test]
    async fn capture_finds_split_marker_and_caps_output_per_phase() {
        let (mut writer, reader) = tokio::io::duplex(4);
        let budget = Budget {
            compile: Arc::new(AtomicUsize::new(0)),
            run: Arc::new(AtomicUsize::new(0)),
            compile_limit: 1024,
            run_limit: 10,
        };
        let task = tokio::spawn(capture(reader, budget, Arc::new(Notify::new())));
        let compile_output = b"ab\x1ePLAYGROUND-RUN\x1e\n";
        tokio::io::AsyncWriteExt::write_all(
            &mut writer,
            &[&compile_output[..], b"program output that is too long"].concat(),
        )
        .await
        .unwrap();
        drop(writer);

        let captured = task.await.unwrap().unwrap();
        assert!(captured.marker_at.is_some());
        // The read containing the end of the marker may include up to 3 bytes of program output
        let program_output = captured.buf.len() - compile_output.len();
        assert!((10..=13).contains(&program_output), "{program_output}");
    }
}
