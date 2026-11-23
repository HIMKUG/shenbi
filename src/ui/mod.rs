use crate::config::{Config, ProviderConfig};
use crate::http;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

const INDEX_HTML: &str = include_str!("index.html");

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<RwLock<Config>>,
}

pub async fn serve(config: Arc<Config>, port: u16) -> anyhow::Result<()> {
    let state = AppState { config: Arc::new(RwLock::new((*config).clone())) };
    let app = Router::new()
        .route("/", get(index))
        .route("/api/config", get(read_config).put(write_config))
        .route("/api/providers/{name}/test", post(test_provider))
        .route("/api/install", get(install_status).post(install_register))
        .layer(axum::middleware::from_fn(loopback_only))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    let url = format!("http://127.0.0.1:{port}");
    println!("神笔 config UI → {url}");
    if std::env::var_os("SHENBI_NO_BROWSER").is_none() {
        open_browser(&url);
    }
    axum::serve(listener, app).await?;
    Ok(())
}

/// Best-effort: open the URL in the user's default browser. Failure is
/// silent because the URL is already printed.
fn open_browser(url: &str) {
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}

/// Reject requests whose Host header is not loopback: blocks DNS rebinding
/// from a page the user happens to have open.
async fn loopback_only(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let host = request
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let host_name = host.rsplit_once(':').map(|(name, _)| name).unwrap_or(host);
    let allowed = matches!(host_name, "127.0.0.1" | "localhost" | "[::1]");
    if allowed {
        next.run(request).await
    } else {
        (StatusCode::FORBIDDEN, "loopback only").into_response()
    }
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

const UI_ORDER: [&str; 10] = [
    "openai", "gemini", "replicate", "together", "grok", "ideogram", "custom", "photoroom", "fal", "anthropic",
];

fn snapshot(config: &Config) -> Value {
    let providers: Vec<Value> = UI_ORDER
        .iter()
        .map(|name| {
            let entry = config.provider(name);
            json!({
                "name": name,
                "configured": config.is_configured(name),
                "model": entry.map(|p| p.model.clone()).unwrap_or_default(),
                "editModel": entry.map(|p| p.edit_model.clone()).unwrap_or_default(),
                "baseUrl": entry.map(|p| p.base_url.clone()).unwrap_or_default(),
                "format": entry.map(|p| p.format.clone()).unwrap_or_default(),
            })
        })
        .collect();

    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "shenbi".into());
    let exe = exe.replace('\\', "/");
    json!({
        "defaultProvider": config.default_provider,
        "outputDir": config.output_dir,
        "inputRoot": config.input_root,
        "runRetentionHours": config.run_retention_hours,
        "maxConcurrent": config.max_concurrent,
        "requestTimeoutMs": config.request_timeout_ms,
        "providers": providers,
        "savedAt": saved_at(config),
        "configPath": config.path.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
        "mcp": {
            "mcpServers": {
                "shenbi": {
                    "command": exe,
                    "args": ["mcp"]
                }
            }
        }
    })
}

fn saved_at(config: &Config) -> String {
    let Some(path) = &config.path else { return String::new() };
    let Ok(meta) = std::fs::metadata(path) else { return String::new() };
    let Ok(modified) = meta.modified() else { return String::new() };
    let Ok(elapsed) = modified.elapsed() else { return String::new() };
    let secs = elapsed.as_secs();
    match secs {
        0..=59 => "刚刚".to_string(),
        60..=3599 => format!("{} 分钟前", secs / 60),
        3600..=86399 => format!("{} 小时前", secs / 3600),
        _ => format!("{} 天前", secs / 86400),
    }
}

async fn read_config(State(state): State<AppState>) -> Json<Value> {
    let config = state.config.read().expect("config lock");
    Json(snapshot(&config))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProviderPatch {
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    edit_model: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    format: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfigPatch {
    default_provider: Option<String>,
    output_dir: Option<String>,
    input_root: Option<String>,
    run_retention_hours: Option<u32>,
    max_concurrent: Option<usize>,
    request_timeout_ms: Option<u64>,
    #[serde(default)]
    providers: BTreeMap<String, ProviderPatch>,
}

async fn write_config(State(state): State<AppState>, Json(patch): Json<ConfigPatch>) -> Response {
    let mut guard = state.config.write().expect("config lock");
    let mut next = guard.clone();

    if let Some(value) = patch.default_provider.filter(|v| !v.trim().is_empty()) {
        next.default_provider = value;
    }
    if let Some(value) = patch.output_dir.filter(|v| !v.trim().is_empty()) {
        next.output_dir = value;
    }
    if let Some(value) = patch.input_root {
        next.input_root = value;
    }
    if let Some(value) = patch.run_retention_hours {
        next.run_retention_hours = value.min(720);
    }
    if let Some(value) = patch.max_concurrent {
        next.max_concurrent = value.clamp(1, 8);
    }
    if let Some(value) = patch.request_timeout_ms {
        next.request_timeout_ms = value.clamp(5_000, 600_000);
    }
    for (name, patch) in patch.providers {
        let entry = next.providers.entry(name).or_insert_with(ProviderConfig::default);
        if let Some(key) = patch.api_key {
            let key = key.trim().to_string();
            // An empty field means "leave the stored key alone".
            if !key.is_empty() {
                entry.api_key = key;
            }
        }
        if let Some(model) = patch.model {
            entry.model = model.trim().to_string();
        }
        if let Some(edit) = patch.edit_model {
            entry.edit_model = edit.trim().to_string();
        }
        if let Some(base) = patch.base_url {
            entry.base_url = base.trim().to_string();
        }
        if let Some(format) = patch.format {
            entry.format = format.trim().to_string();
        }
    }

    if let Err(err) = next.save() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("failed to write config: {err}") })),
        )
            .into_response();
    }
    *guard = next;
    Json(snapshot(&guard)).into_response()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TestRequest {
    api_key: Option<String>,
    model: Option<String>,
    base_url: Option<String>,
}

async fn test_provider(
    axum::extract::Path(name): axum::extract::Path<String>,
    State(state): State<AppState>,
    Json(body): Json<TestRequest>,
) -> Json<Value> {
    let config = state.config.read().expect("config lock").clone();
    let stored = config.provider(&name).cloned().unwrap_or_default();
    let key = body
        .api_key
        .filter(|k| !k.trim().is_empty())
        .unwrap_or(stored.api_key);
    let model = body.model.filter(|m| !m.trim().is_empty()).unwrap_or(stored.model);
    let base = body
        .base_url
        .filter(|b| !b.trim().is_empty())
        .map(|b| b.trim_end_matches('/').to_string())
        .or_else(|| Some(stored.base_url).filter(|b| !b.trim().is_empty()));

    if name == crate::config::CUSTOM_PROVIDER {
        let Some(base) = base else {
            return Json(json!({ "ok": false, "error": "请先填写接口地址" }));
        };
        let result = crate::custom::probe(&key, &base).await;
        return Json(match result {
            Ok(detail) => json!({ "ok": true, "detail": detail }),
            Err(error) => json!({ "ok": false, "error": error }),
        });
    }

    if key.trim().is_empty() {
        return Json(json!({ "ok": false, "error": "尚未填写 API Key" }));
    }

    let result = match name.as_str() {
        "openai" => probe_openai(&key, &model, base.as_deref()).await,
        "anthropic" => probe_anthropic(&key, &model, base.as_deref()).await,
        "gemini" => probe_gemini(&key).await,
        "replicate" => probe_bearer("https://api.replicate.com/v1/account", &key).await,
        "together" => probe_bearer("https://api.together.xyz/v1/models", &key).await,
        "grok" => probe_bearer("https://api.x.ai/v1/models", &key).await,
        other => Err(format!("{other} 暂不支持在线测试")),
    };

    Json(match result {
        Ok(detail) => json!({ "ok": true, "detail": detail }),
        Err(error) => json!({ "ok": false, "error": error }),
    })
}

async fn probe_bearer(url: &str, key: &str) -> Result<String, String> {
    let response = http::client()
        .get(url)
        .bearer_auth(key)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|err| friendly_net_error(&err))?;
    classify(response, "凭据有效").await
}

async fn probe_openai(key: &str, model: &str, base: Option<&str>) -> Result<String, String> {
    let base = base.unwrap_or("https://api.openai.com");
    let url = format!("{base}/v1/models");
    let response = http::client()
        .get(url)
        .bearer_auth(key)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|err| friendly_net_error(&err))?;
    if !response.status().is_success() {
        return Err(status_message(response).await);
    }
    let body: Value = response.json().await.map_err(|err| err.to_string())?;
    let models: Vec<&str> = body
        .get("data")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(|m| m.get("id").and_then(Value::as_str)).collect())
        .unwrap_or_default();
    let model = if model.trim().is_empty() { "gpt-image-1" } else { model };
    let has_image_model = models.iter().any(|id| id.contains("image") || id.starts_with("dall-e"));
    let known = models.iter().any(|id| *id == model);
    Ok(if !has_image_model {
        "凭据有效，但账户未开放图像模型".into()
    } else if known {
        format!("凭据有效 · 模型 {model} 可用")
    } else {
        format!("凭据有效 · 模型 {model} 未在列表中（可能需要组织验证）")
    })
}

async fn probe_anthropic(key: &str, model: &str, base: Option<&str>) -> Result<String, String> {
    let base = base.unwrap_or("https://api.anthropic.com");
    let url = format!("{base}/v1/models?limit=20");
    let response = http::client()
        .get(url)
        .header("x-api-key", key)
        .header("anthropic-version", "2023-06-01")
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|err| friendly_net_error(&err))?;
    if !response.status().is_success() {
        return Err(status_message(response).await);
    }
    let body: Value = response.json().await.map_err(|err| err.to_string())?;
    let models: Vec<&str> = body
        .get("data")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(|m| m.get("id").and_then(Value::as_str)).collect())
        .unwrap_or_default();
    let model = if model.trim().is_empty() { "claude-haiku-4-5" } else { model };
    Ok(if models.contains(&model) {
        format!("凭据有效 · {model} 可用于规划")
    } else {
        format!("凭据有效 · 未发现 {model}，规划前请确认模型名")
    })
}

async fn probe_gemini(key: &str) -> Result<String, String> {
    let url = "https://generativelanguage.googleapis.com/v1beta/models?pageSize=1";
    let response = http::client()
        .get(url)
        .header("x-goog-api-key", key)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|err| friendly_net_error(&err))?;
    classify(response, "凭据有效").await
}

async fn classify(response: reqwest::Response, ok_detail: &str) -> Result<String, String> {
    if response.status().is_success() {
        Ok(ok_detail.to_string())
    } else {
        Err(status_message(response).await)
    }
}

async fn status_message(response: reqwest::Response) -> String {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let detail = http::error_message(&parsed)
        .unwrap_or_else(|| body.chars().take(200).collect::<String>());
    let label = match status.as_u16() {
        401 => "API Key 无效",
        403 => "无权限或需要验证",
        429 => "额度受限，请稍后再试",
        _ => "连接失败",
    };
    if detail.trim().is_empty() {
        format!("{label}（HTTP {}）", status.as_u16())
    } else {
        format!("{label}（HTTP {}）：{detail}", status.as_u16())
    }
}

fn friendly_net_error(err: &reqwest::Error) -> String {
    if err.is_timeout() {
        "请求超时，检查网络或代理".to_string()
    } else if err.is_connect() {
        "无法建立连接，检查网络或代理".to_string()
    } else {
        err.to_string()
    }
}

async fn install_status() -> Json<Value> {
    Json(crate::install::status())
}

/// Registration writes outside the app's own files, so keep it to a
/// deliberate POST from the loopback page.
async fn install_register() -> Json<Value> {
    match tokio::task::spawn_blocking(|| crate::install::register("user", false)).await {
        Ok(Ok(outcome)) => Json(json!({ "ok": true, "detail": outcome.detail, "target": outcome.target })),
        Ok(Err(err)) => Json(json!({ "ok": false, "error": err.to_string() })),
        Err(err) => Json(json!({ "ok": false, "error": err.to_string() })),
    }
}
