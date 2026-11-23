//! One-command MCP registration.
//!
//! MCP stdio servers are launched by the client on demand, so registering is
//! the only step needed for the tools to appear in every new session.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const SERVER_NAME: &str = "shenbi";

pub struct Outcome {
    pub target: String,
    pub detail: String,
}

fn current_exe() -> anyhow::Result<PathBuf> {
    Ok(std::env::current_exe()?.canonicalize().unwrap_or_else(|_| {
        std::env::current_exe().unwrap_or_default()
    }))
}

fn exe_for_json(path: &Path) -> String {
    // Client configs are JSON, so always emit forward slashes; drop the
    // Windows verbatim prefix canonicalize() adds, which some clients reject.
    path.display()
        .to_string()
        .replace('\\', "/")
        .trim_start_matches("//?/")
        .trim_start_matches("//./")
        .to_string()
}

fn claude_cli() -> bool {
    Command::new("claude")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn backup(path: &Path) -> anyhow::Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }
    let stamp = crate::util::iso_now().replace([':', '.'], "-");
    let target = path.with_extension(format!(
        "{}.bak-{stamp}",
        path.extension().and_then(|e| e.to_str()).unwrap_or("json")
    ));
    std::fs::copy(path, &target)?;
    Ok(Some(target))
}

fn read_json(path: &Path) -> anyhow::Result<Value> {
    match std::fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw)
            .map_err(|err| anyhow::anyhow!("{} is not valid JSON: {err}", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(err) => Err(err.into()),
    }
}

fn server_entry(exe: &Path) -> Value {
    json!({
        "type": "stdio",
        "command": exe_for_json(exe),
        "args": ["mcp"],
        "env": {}
    })
}

/// Merge the entry into a JSON file's `mcpServers`, preserving everything else.
fn write_entry(path: &Path, entry: Value, remove: bool) -> anyhow::Result<()> {
    let mut root = read_json(path)?;
    if !root.is_object() {
        anyhow::bail!("{} does not contain a JSON object", path.display());
    }

    let mut servers = root
        .get("mcpServers")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    if remove {
        servers.remove(SERVER_NAME);
    } else {
        servers.insert(SERVER_NAME.to_string(), entry);
    }

    root["mcpServers"] = Value::Object(servers);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(&root)?;
    crate::util::write_atomic_sync(path, body.as_bytes())?;
    Ok(())
}

fn user_config_path() -> Option<PathBuf> {
    crate::util::home_dir().map(|home| home.join(".claude.json"))
}

pub fn register(scope: &str, remove: bool) -> anyhow::Result<Outcome> {
    let exe = current_exe()?;
    match scope {
        "project" => {
            let path = std::env::current_dir()?.join(".mcp.json");
            if let Some(backup_path) = backup(&path)? {
                eprintln!("shenbi: backed up {}", backup_path.display());
            }
            write_entry(&path, server_entry(&exe), remove)?;
            Ok(Outcome {
                target: path.display().to_string(),
                detail: format!("项目级 .mcp.json（{}）", if remove { "已移除" } else { "已写入" }),
            })
        }
        "user" | "" => {
            // Prefer the official CLI: it validates and updates the right file
            // for this Claude Code version.
            if claude_cli() {
                if remove {
                    let _ = Command::new("claude")
                        .args(["mcp", "remove", SERVER_NAME, "--scope", "user"])
                        .status();
                } else {
                    let status = Command::new("claude")
                        .args([
                            "mcp",
                            "add",
                            "--scope",
                            "user",
                            SERVER_NAME,
                            "--",
                            &exe.to_string_lossy(),
                            "mcp",
                        ])
                        .status()?;
                    if status.success() {
                        return Ok(Outcome {
                            target: "~/.claude.json (user scope)".into(),
                            detail: "已通过 claude mcp add 注册，重开会话即可用".into(),
                        });
                    }
                }
            }

            let Some(path) = user_config_path() else {
                anyhow::bail!("cannot locate the home directory");
            };
            if let Some(backup_path) = backup(&path)? {
                eprintln!("shenbi: backed up {}", backup_path.display());
            }
            write_entry(&path, server_entry(&exe), remove)?;
            Ok(Outcome {
                target: path.display().to_string(),
                detail: format!("用户级配置（{}）", if remove { "已移除" } else { "已写入" }),
            })
        }
        other => anyhow::bail!("unknown scope '{other}'; use user or project"),
    }
}

/// Status for the UI: is this binary already registered, and where.
pub fn status() -> Value {
    let exe = current_exe().map(|p| exe_for_json(&p)).unwrap_or_default();
    let user_entry = user_config_path()
        .filter(|path| path.exists())
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|root| root.get("mcpServers")?.get(SERVER_NAME).cloned());

    let command = user_entry
        .as_ref()
        .and_then(|entry| entry.get("command"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .replace('\\', "/");

    json!({
        "registered": !command.is_empty(),
        "matchesThisBinary": !command.is_empty() && command.eq_ignore_ascii_case(&exe),
        "command": command,
        "exe": exe,
    })
}
