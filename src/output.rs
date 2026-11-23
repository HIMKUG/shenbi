use crate::config::Config;
use crate::util;
use anyhow::Context;
use std::path::{Path, PathBuf};

pub struct SaveRequest<'a> {
    pub output_path: Option<&'a str>,
    pub output_dir: Option<&'a str>,
    pub asset_id: Option<&'a str>,
    pub prompt: &'a str,
    pub provider: &'a str,
}

fn sanitize_asset_id(asset_id: &str) -> Result<String, anyhow::Error> {
    let base = Path::new(asset_id)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let safe: String = base
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    if safe.is_empty() {
        anyhow::bail!("invalid assetId '{asset_id}': needs at least one of a-zA-Z0-9_-");
    }
    Ok(safe)
}

/// Mirrors the original precedence: explicit path, dir + assetId, dir, default
/// dir + assetId, default dir — auto-named with date, provider and prompt slug.
pub fn resolve(config: &Config, request: &SaveRequest<'_>) -> anyhow::Result<PathBuf> {
    if let Some(path) = request.output_path.filter(|p| !p.trim().is_empty()) {
        if !path.to_ascii_lowercase().ends_with(".png") {
            anyhow::bail!("outputPath must end with .png");
        }
        return Ok(util::expand_tilde(path));
    }

    let asset_id = match request.asset_id {
        Some(id) if !id.trim().is_empty() => Some(sanitize_asset_id(id)?),
        _ => None,
    };
    let dir = match request.output_dir.filter(|d| !d.trim().is_empty()) {
        Some(dir) => util::expand_tilde(dir),
        None => config.output_dir_resolved(),
    };
    let name = match asset_id {
        Some(id) => format!("{id}.png"),
        None => auto_name(request.prompt, request.provider),
    };
    Ok(dir.join(name))
}

pub fn auto_name(prompt: &str, provider: &str) -> String {
    let date = &util::iso_now()[..10];
    let slug = util::slugify(prompt);
    let slug = if slug.is_empty() { "image".to_string() } else { slug };
    let provider: String = provider
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect();
    format!("{date}-{provider}-{slug}-{}.png", util::pseudo_hex(3))
}

pub async fn save(bytes: &[u8], path: &Path) -> anyhow::Result<PathBuf> {
    util::write_atomic(path, bytes)
        .await
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(path.to_path_buf())
}

/// Enforce the optional containment root the same way the original did.
pub async fn assert_within_input_root(config: &Config, path: &str) -> anyhow::Result<()> {
    let root = config.input_root.trim();
    if root.is_empty() {
        return Ok(());
    }
    let root = util::expand_tilde(root);
    let root = tokio::fs::canonicalize(&root)
        .await
        .with_context(|| format!("input root does not exist: {}", root.display()))?;
    if !Path::new(path).is_absolute() {
        anyhow::bail!("input path must be absolute: {path}");
    }
    let candidate = util::expand_tilde(path);
    let candidate = tokio::fs::canonicalize(&candidate).await?;
    if !candidate.starts_with(&root) {
        anyhow::bail!(
            "input path resolves outside the configured input root: {}",
            candidate.display()
        );
    }
    Ok(())
}

pub fn read_input(path: &str) -> Result<Vec<u8>, crate::error::AppError> {
    std::fs::read(util::expand_tilde(path))
        .map_err(|err| crate::error::AppError::constraint(format!("cannot read input '{path}': {err}")))
}
