use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Runner configuration, read from `runner.toml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Docker CLI to invoke.
    #[serde(default = "default_docker")]
    pub docker: String,
    /// OCI runtime passed to `docker run --runtime`, e.g. `runsc` for gVisor.
    /// Docker's default runtime is used when unset.
    pub runtime: Option<String>,
    /// Where job directories are created. Must be visible to the Docker daemon.
    pub work_dir: Option<PathBuf>,
    #[serde(default)]
    pub limits: Limits,
    pub languages: HashMap<String, Language>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Language {
    /// Image name, e.g. `ghcr.io/bolli24/luga:playground`.
    pub image: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    /// Wall time for compiling and running together, including container startup.
    pub wall_time_ms: u64,
    pub memory_mb: u64,
    pub pids: u64,
    pub cpus: f64,
    /// Combined stdout and stderr size. The container is killed when it's exceeded.
    pub output_bytes: usize,
    pub build_tmpfs_mb: u64,
    pub tmp_tmpfs_mb: u64,
    pub max_files: usize,
    pub max_source_bytes: usize,
    pub max_stdin_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            wall_time_ms: 10_000,
            memory_mb: 256,
            pids: 64,
            cpus: 1.0,
            output_bytes: 64 * 1024,
            build_tmpfs_mb: 64,
            tmp_tmpfs_mb: 64,
            max_files: 32,
            max_source_bytes: 256 * 1024,
            max_stdin_bytes: 64 * 1024,
        }
    }
}

fn default_docker() -> String {
    "docker".to_string()
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("unable to read {}: {}", .0.display(), .1)]
    Io(PathBuf, std::io::Error),
    #[error("invalid config {}: {}", .0.display(), .1)]
    Parse(PathBuf, toml::de::Error),
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text =
            std::fs::read_to_string(path).map_err(|err| ConfigError::Io(path.into(), err))?;
        toml::from_str(&text).map_err(|err| ConfigError::Parse(path.into(), err))
    }
}
