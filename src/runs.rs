use crate::util;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeRecord {
    pub id: String,
    pub op: String,
    pub provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(rename = "artifactPath", skip_serializing_if = "Option::is_none")]
    pub artifact_path: Option<String>,
    #[serde(rename = "durationMs")]
    pub duration_ms: u128,
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u8,
    #[serde(rename = "runId")]
    pub run_id: String,
    #[serde(rename = "startedAt")]
    pub started_at: String,
    #[serde(rename = "endedAt", skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    pub status: String,
    pub tool: String,
    pub invocation: Value,
    pub nodes: Vec<NodeRecord>,
    #[serde(rename = "finalOutput", skip_serializing_if = "Option::is_none")]
    pub final_output: Option<String>,
    #[serde(rename = "totalDurationMs", skip_serializing_if = "Option::is_none")]
    pub total_duration_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub fn runs_root(output_dir: &std::path::Path) -> PathBuf {
    output_dir.join(".runs")
}

pub fn run_dir(output_dir: &std::path::Path, run_id: &str) -> anyhow::Result<PathBuf> {
    let valid = run_id.starts_with("run-")
        && run_id.len() > 8
        && run_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-');
    if !valid {
        anyhow::bail!("invalid runId");
    }
    Ok(runs_root(output_dir).join(run_id))
}

pub fn artifact_path(dir: &std::path::Path, node_id: &str) -> PathBuf {
    dir.join(format!("n{node_id}.png"))
}

pub async fn write_manifest(dir: &std::path::Path, manifest: &Manifest) -> anyhow::Result<()> {
    tokio::fs::create_dir_all(dir).await?;
    let body = serde_json::to_vec_pretty(manifest)?;
    util::write_atomic(&dir.join("manifest.json"), &body).await
}

/// Startup-only sweep so `.runs` does not grow without bound.
pub async fn sweep(output_dir: &std::path::Path, retention_hours: u32) {
    if retention_hours == 0 {
        return;
    }
    let root = runs_root(output_dir);
    let Ok(mut entries) = tokio::fs::read_dir(&root).await else {
        return;
    };
    let cutoff = std::time::SystemTime::now()
        - std::time::Duration::from_secs(u64::from(retention_hours) * 3600);
    while let Ok(Some(entry)) = entries.next_entry().await {
        let Ok(meta) = entry.metadata().await else { continue };
        let Ok(modified) = meta.modified() else { continue };
        if modified < cutoff {
            let _ = tokio::fs::remove_dir_all(entry.path()).await;
        }
    }
}
