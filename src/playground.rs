//! The `/playground` directory of an image: extracting it and reading its program indexes.
//! See "Image layout" and "Examples and fixtures" in CONTRACT.md.

use std::path::Path;
use std::process::Stdio;
use std::{fs, io};

use serde::Deserialize;
use tempfile::TempDir;
use tokio::process::Command;

use crate::executor::{Phase, container_name};
use crate::job::{SourceFile, is_valid_path};

#[derive(Debug, thiserror::Error)]
pub enum ExtractError {
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("docker {0} failed: {1}")]
    Docker(String, String),
}

/// `fixtures.toml` or `examples.toml`.
#[derive(Debug, Deserialize)]
struct Index {
    #[serde(default)]
    program: Vec<Program>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Program {
    pub id: String,
    pub title: Option<String>,
    pub files: Vec<String>,
    pub entry: String,
    #[serde(default)]
    pub stdin: String,
    #[serde(default)]
    pub expect: Expect,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Expect {
    pub phase: Option<Phase>,
    pub stdout: Option<String>,
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub timeout: bool,
    /// Views to request, each of which must be printed and not be empty.
    #[serde(default)]
    pub views: Vec<String>,
}

/// Copy `/playground` out of the image without running it.
pub async fn extract(docker: &str, image_id: &str) -> Result<TempDir, ExtractError> {
    let dir = tempfile::Builder::new()
        .prefix("playground-image-")
        .tempdir()?;
    let name = container_name();
    run_docker(docker, &["create", "--name", &name, image_id]).await?;
    let target = dir.path().display().to_string();
    let copied = run_docker(docker, &["cp", &format!("{name}:/playground/."), &target]).await;
    let removed = run_docker(docker, &["rm", "--force", &name]).await;
    copied.and(removed)?;
    Ok(dir)
}

async fn run_docker(docker: &str, args: &[&str]) -> Result<(), ExtractError> {
    let output = Command::new(docker)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await?;
    if output.status.success() {
        Ok(())
    } else {
        Err(ExtractError::Docker(
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ))
    }
}

/// Read `<root>/<set>/<set>.toml`, e.g. `examples/examples.toml`.
pub fn load_index(root: &Path, set: &str) -> Result<Vec<Program>, String> {
    let path = root.join(set).join(format!("{set}.toml"));
    let text = fs::read_to_string(&path).map_err(|err| err.to_string())?;
    let index: Index = toml::from_str(&text).map_err(|err| err.to_string())?;
    Ok(index.program)
}

pub fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-'))
}

/// Read a program's files from `<root>/<set>/`.
pub fn read_files(root: &Path, set: &str, program: &Program) -> Result<Vec<SourceFile>, String> {
    program
        .files
        .iter()
        .map(|path| {
            if !is_valid_path(path) {
                return Err(format!("invalid path {path}"));
            }
            let content = fs::read_to_string(root.join(set).join(path))
                .map_err(|err| format!("unable to read {path}: {err}"))?;
            Ok(SourceFile {
                path: path.clone(),
                content,
            })
        })
        .collect()
}
