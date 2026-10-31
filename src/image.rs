use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tokio::process::Command;

/// Contract versions this runner understands, see CONTRACT.md.
pub const SUPPORTED_CONTRACTS: &[u32] = &[1];

/// What an image declares about itself through its labels.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageInfo {
    /// Image ID. Containers are started from the ID, so a concurrent pull can't swap the image
    /// between inspecting and running it.
    pub id: String,
    pub contract: u32,
    pub language: String,
    pub extensions: Vec<String>,
    pub capabilities: Vec<String>,
    pub revision: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ImageError {
    #[error("unable to run docker: {0}")]
    Spawn(std::io::Error),
    #[error("docker image inspect {0} failed: {1}")]
    Inspect(String, String),
    #[error("unexpected docker inspect output: {0}")]
    Output(serde_json::Error),
    #[error("image {0} is missing label {1}")]
    MissingLabel(String, &'static str),
    #[error("image {0} has invalid label {1}={2}")]
    InvalidLabel(String, &'static str, String),
    #[error("image {0} implements contract {1}, this runner supports {SUPPORTED_CONTRACTS:?}")]
    UnsupportedContract(String, u32),
}

#[derive(Deserialize)]
struct Inspect {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "Config")]
    config: InspectConfig,
}

#[derive(Deserialize)]
struct InspectConfig {
    #[serde(rename = "Labels", default)]
    labels: Option<HashMap<String, String>>,
}

pub async fn inspect(docker: &str, image: &str) -> Result<ImageInfo, ImageError> {
    let output = Command::new(docker)
        .args(["image", "inspect", "--format", "{{json .}}", image])
        .output()
        .await
        .map_err(ImageError::Spawn)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(ImageError::Inspect(image.into(), stderr));
    }
    let inspect: Inspect = serde_json::from_slice(&output.stdout).map_err(ImageError::Output)?;
    parse_labels(image, inspect.id, inspect.config.labels.unwrap_or_default())
}

fn parse_labels(
    image: &str,
    id: String,
    labels: HashMap<String, String>,
) -> Result<ImageInfo, ImageError> {
    let label = |name: &'static str| {
        labels
            .get(name)
            .map(String::as_str)
            .ok_or_else(|| ImageError::MissingLabel(image.into(), name))
    };
    let list = |value: &str| -> Vec<String> {
        value
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(String::from)
            .collect()
    };

    let contract_label = label("playground.contract")?;
    let contract = contract_label.parse().map_err(|_| {
        ImageError::InvalidLabel(image.into(), "playground.contract", contract_label.into())
    })?;
    if !SUPPORTED_CONTRACTS.contains(&contract) {
        return Err(ImageError::UnsupportedContract(image.into(), contract));
    }

    let extensions = list(label("playground.extensions")?);
    if extensions.is_empty() || extensions.iter().any(|ext| !ext.starts_with('.')) {
        return Err(ImageError::InvalidLabel(
            image.into(),
            "playground.extensions",
            label("playground.extensions")?.into(),
        ));
    }

    Ok(ImageInfo {
        id,
        contract,
        language: label("playground.language")?.into(),
        extensions,
        capabilities: list(label("playground.capabilities")?),
        revision: label("org.opencontainers.image.revision")?.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn luga_labels() -> HashMap<String, String> {
        labels(&[
            ("playground.contract", "1"),
            ("playground.language", "luga"),
            ("playground.extensions", ".luga"),
            ("playground.capabilities", ""),
            ("org.opencontainers.image.revision", "abc"),
        ])
    }

    #[test]
    fn parses_labels() {
        let info = parse_labels("img", "sha256:1".into(), luga_labels()).unwrap();
        assert_eq!(info.extensions, vec![".luga"]);
        assert!(info.capabilities.is_empty());
        assert_eq!(info.revision, "abc");
    }

    #[test]
    fn rejects_unknown_contract() {
        let mut labels = luga_labels();
        labels.insert("playground.contract".into(), "2".into());
        assert!(matches!(
            parse_labels("img", "id".into(), labels),
            Err(ImageError::UnsupportedContract(_, 2))
        ));
    }

    #[test]
    fn rejects_missing_label() {
        let mut labels = luga_labels();
        labels.remove("playground.language");
        assert!(matches!(
            parse_labels("img", "id".into(), labels),
            Err(ImageError::MissingLabel(_, "playground.language"))
        ));
    }
}
