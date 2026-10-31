//! The languages the server offers: for each, the image in use and its extracted `/playground`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde::Serialize;
use tempfile::TempDir;

use crate::config::Config;
use crate::image::{self, ImageInfo};
use crate::job::SourceFile;
use crate::playground;

/// A language's image and what was extracted from it. Replaced as a whole when the image changes,
/// so requests that hold an `Arc<Snapshot>` keep a consistent view.
pub struct Snapshot {
    pub image: ImageInfo,
    pub examples: Vec<Example>,
    pub has_linter: bool,
    dir: TempDir,
}

#[derive(Debug, Clone, Serialize)]
pub struct Example {
    pub id: String,
    pub title: String,
    pub entry: String,
    pub files: Vec<SourceFile>,
}

impl Snapshot {
    /// The file at `path` below `linter/`, or `None` if there is none.
    pub fn linter_file(&self, path: &str) -> Option<PathBuf> {
        let mut file = self.dir.path().join("linter");
        for segment in path.split('/') {
            if matches!(segment, "" | "." | "..") || segment.contains('\\') {
                return None;
            }
            file.push(segment);
        }
        file.is_file().then_some(file)
    }
}

pub struct Registry {
    docker: String,
    /// Language → configured image name.
    images: HashMap<String, String>,
    snapshots: RwLock<HashMap<String, Arc<Snapshot>>>,
}

impl Registry {
    pub fn new(config: &Config) -> Self {
        Self {
            docker: config.docker.clone(),
            images: config
                .languages
                .iter()
                .map(|(name, language)| (name.clone(), language.image.clone()))
                .collect(),
            snapshots: RwLock::default(),
        }
    }

    pub fn get(&self, language: &str) -> Option<Arc<Snapshot>> {
        self.snapshots
            .read()
            .expect("lock is never poisoned")
            .get(language)
            .cloned()
    }

    /// All available languages, sorted by name.
    pub fn all(&self) -> Vec<Arc<Snapshot>> {
        let mut snapshots: Vec<_> = self
            .snapshots
            .read()
            .expect("lock is never poisoned")
            .values()
            .cloned()
            .collect();
        snapshots.sort_by(|a, b| a.image.language.cmp(&b.image.language));
        snapshots
    }

    /// Check every language's image and switch to it if it changed. A language whose new image
    /// is unusable keeps its previous one.
    pub async fn refresh_all(&self) {
        for (language, image) in &self.images {
            if let Err(err) = self.refresh(language, image).await {
                eprintln!("{language}: keeping the previous image: {err}");
            }
        }
    }

    pub fn spawn_refresh(self: Arc<Self>, interval: Duration) {
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                self.refresh_all().await;
            }
        });
    }

    async fn refresh(&self, language: &str, image: &str) -> Result<(), String> {
        let info = image::inspect(&self.docker, image)
            .await
            .map_err(|err| err.to_string())?;
        if self
            .get(language)
            .is_some_and(|current| current.image.id == info.id)
        {
            return Ok(());
        }
        if info.language != language {
            return Err(format!(
                "{image} is a {} image, expected {language}",
                info.language
            ));
        }

        let dir = playground::extract(&self.docker, &info.id)
            .await
            .map_err(|err| err.to_string())?;
        let examples = playground::load_index(dir.path(), "examples")?
            .into_iter()
            .filter_map(|program| {
                // The contract test already rejects broken examples; skip any that slip through
                match playground::read_files(dir.path(), "examples", &program) {
                    Ok(files) => Some(Example {
                        title: program.title.unwrap_or_else(|| program.id.clone()),
                        id: program.id,
                        entry: program.entry,
                        files,
                    }),
                    Err(err) => {
                        eprintln!("{language}: skipping example {}: {err}", program.id);
                        None
                    }
                }
            })
            .collect();

        eprintln!(
            "{language}: using {image} (revision {}, image {})",
            info.revision, info.id
        );
        let snapshot = Snapshot {
            has_linter: dir.path().join("linter/index.js").is_file(),
            image: info,
            examples,
            dir,
        };
        self.snapshots
            .write()
            .expect("lock is never poisoned")
            .insert(language.to_string(), Arc::new(snapshot));
        Ok(())
    }
}
