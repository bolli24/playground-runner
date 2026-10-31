use std::collections::HashSet;

use serde::Deserialize;

use crate::config::Limits;

/// A program to compile and run.
#[derive(Debug, Clone, Deserialize)]
pub struct Job {
    pub language: String,
    pub files: Vec<SourceFile>,
    /// Path of the entry file, one of `files`.
    pub entry: String,
    #[serde(default)]
    pub stdin: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SourceFile {
    /// Source path as defined by the contract, e.g. `modules/vec.luga`.
    pub path: String,
    pub content: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum JobError {
    #[error("no files")]
    NoFiles,
    #[error("too many files ({0}, at most {1})")]
    TooManyFiles(usize, usize),
    #[error("invalid path '{0}'")]
    InvalidPath(String),
    #[error("'{0}' doesn't have one of the extensions {1}")]
    WrongExtension(String, String),
    #[error("duplicate path '{0}'")]
    DuplicatePath(String),
    #[error("'{0}' is used as both a file and a folder")]
    FileFolderConflict(String),
    #[error("entry '{0}' is not one of the files")]
    MissingEntry(String),
    #[error("sources are too large ({0} bytes, at most {1})")]
    SourcesTooLarge(usize, usize),
    #[error("stdin is too large ({0} bytes, at most {1})")]
    StdinTooLarge(usize, usize),
}

const MAX_SEGMENTS: usize = 3;
const MAX_SEGMENT_LEN: usize = 64;

/// Check a source path against the contract: 1 to 3 `/`-separated segments, each matching
/// `^[A-Za-z0-9_][A-Za-z0-9_.-]{0,63}$`.
pub fn is_valid_path(path: &str) -> bool {
    let segments: Vec<&str> = path.split('/').collect();
    segments.len() <= MAX_SEGMENTS && segments.iter().all(|segment| is_valid_segment(segment))
}

fn is_valid_segment(segment: &str) -> bool {
    let mut chars = segment.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    segment.len() <= MAX_SEGMENT_LEN
        && (first.is_ascii_alphanumeric() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

impl Job {
    /// Validate the job against the contract and the limits. `extensions` are the image's
    /// `playground.extensions`, including the dot.
    pub fn validate(&self, extensions: &[String], limits: &Limits) -> Result<(), JobError> {
        if self.files.is_empty() {
            return Err(JobError::NoFiles);
        }
        if self.files.len() > limits.max_files {
            return Err(JobError::TooManyFiles(self.files.len(), limits.max_files));
        }

        let mut paths = HashSet::new();
        for file in &self.files {
            if !is_valid_path(&file.path) {
                return Err(JobError::InvalidPath(file.path.clone()));
            }
            if !extensions
                .iter()
                .any(|ext| file.path.ends_with(ext.as_str()))
            {
                return Err(JobError::WrongExtension(
                    file.path.clone(),
                    extensions.join(", "),
                ));
            }
            if !paths.insert(file.path.as_str()) {
                return Err(JobError::DuplicatePath(file.path.clone()));
            }
        }

        // `a.luga` and `a.luga/b.luga` can't both exist on disk
        for path in &paths {
            let mut prefix = *path;
            while let Some((parent, _)) = prefix.rsplit_once('/') {
                if paths.contains(parent) {
                    return Err(JobError::FileFolderConflict(parent.to_string()));
                }
                prefix = parent;
            }
        }

        if !paths.contains(self.entry.as_str()) {
            return Err(JobError::MissingEntry(self.entry.clone()));
        }

        let source_bytes: usize = self.files.iter().map(|file| file.content.len()).sum();
        if source_bytes > limits.max_source_bytes {
            return Err(JobError::SourcesTooLarge(
                source_bytes,
                limits.max_source_bytes,
            ));
        }
        if self.stdin.len() > limits.max_stdin_bytes {
            return Err(JobError::StdinTooLarge(
                self.stdin.len(),
                limits.max_stdin_bytes,
            ));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(paths: &[&str], entry: &str) -> Job {
        Job {
            language: "luga".into(),
            files: paths
                .iter()
                .map(|path| SourceFile {
                    path: path.to_string(),
                    content: String::new(),
                })
                .collect(),
            entry: entry.into(),
            stdin: String::new(),
        }
    }

    fn validate(job: &Job) -> Result<(), JobError> {
        job.validate(&[".luga".into()], &Limits::default())
    }

    #[test]
    fn valid_paths() {
        for path in [
            "main.luga",
            "modules/vec.luga",
            "a/b/c.luga",
            "_x.luga",
            "a-b.c.luga",
        ] {
            assert!(is_valid_path(path), "{path}");
        }
    }

    #[test]
    fn invalid_paths() {
        for path in [
            "",
            "/main.luga",
            "main.luga/",
            "a//b.luga",
            "./main.luga",
            "../main.luga",
            "a/../b.luga",
            ".hidden.luga",
            "-x.luga",
            "a/b/c/d.luga",
            "a\\b.luga",
            "a b.luga",
            "ä.luga",
        ] {
            assert!(!is_valid_path(path), "{path}");
        }
        assert!(!is_valid_path(&format!("{}.luga", "a".repeat(64))));
    }

    #[test]
    fn accepts_valid_job() {
        assert_eq!(
            validate(&job(&["main.luga", "lib/a.luga"], "main.luga")),
            Ok(())
        );
    }

    #[test]
    fn rejects_bad_jobs() {
        assert_eq!(validate(&job(&[], "main.luga")), Err(JobError::NoFiles));
        assert_eq!(
            validate(&job(&["main.fl"], "main.fl")),
            Err(JobError::WrongExtension("main.fl".into(), ".luga".into()))
        );
        assert_eq!(
            validate(&job(&["a.luga", "a.luga"], "a.luga")),
            Err(JobError::DuplicatePath("a.luga".into()))
        );
        assert_eq!(
            validate(&job(&["a.luga", "a.luga/b.luga"], "a.luga")),
            Err(JobError::FileFolderConflict("a.luga".into()))
        );
        assert_eq!(
            validate(&job(&["a.luga"], "b.luga")),
            Err(JobError::MissingEntry("b.luga".into()))
        );
    }
}
