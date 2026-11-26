use crate::config::{Config, ProviderConfig};
use crate::http;
use axum::extract::State;
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

const INDEX_HTML: &str = include_str!("index.html");

/// Display order for the settings page. Anything the config holds that is not
/// listed here is appended alphabetically, so a provider added on the Rust side
/// can never disappear from the UI.
/// Everything the settings page needs to know about a provider lives here, so
/// the page never keeps a second copy of the order, the names or the abilities.
struct ProviderMeta {
    name: &'static str,
    label: &'static str,
    role: &'static str,
    group: &'static str,
}

const PROVIDERS: [ProviderMeta; 10] = [
    ProviderMeta { name: "openai", label: "OpenAI", role: "生成 · 改图", group: "生成" },
    ProviderMeta { name: "gemini", label: "Google Gemini", role: "生成", group: "生成" },
    ProviderMeta { name: "replicate", label: "Replicate", role: "生成 · 放大", group: "生成" },
    ProviderMeta { name: "together", label: "Together AI", role: "生成", group: "生成" },
    ProviderMeta { name: "grok", label: "xAI Grok", role: "生成", group: "生成" },
    ProviderMeta { name: "ideogram", label: "Ideogram", role: "生成", group: "生成" },
    ProviderMeta { name: "custom", label: "自定义接口", role: "生成 · OpenAI 兼容", group: "生成" },
    ProviderMeta { name: "photoroom", label: "Photoroom", role: "抠图 · 合成", group: "图像处理" },
    ProviderMeta { name: "fal", label: "fal.ai", role: "生成 · 改图", group: "图像处理" },
    ProviderMeta { name: "anthropic", label: "Anthropic", role: "规划（不生成图片）", group: "规划" },
];

/// Providers whose credentials can be checked without spending money.
const TESTABLE: [&str; 7] = ["openai", "gemini", "replicate", "together", "grok", "anthropic", "custom"];

fn meta_of(name: &str) -> Option<&'static ProviderMeta> {
    PROVIDERS.iter().find(|m| m.name == name)
}

#[derive(Clone)]
pub struct AppState {
    config: Arc<RwLock<Config>>,
    /// Random per-process token handed to the page and required back on every
    /// state-changing request. Without it, any other local process (or a page
    /// that guesses the port) could rewrite the user's config.
    token: Arc<String>,
}

fn new_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut out = String::with_capacity(32);
    for salt in [1u64, 2] {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u64(std::process::id() as u64 ^ salt);
        out.push_str(&format!("{:016x}", hasher.finish()));
    }
    out
}

pub async fn serve(config: Arc<Config>, port: u16) -> anyhow::Result<()> {
    let state = AppState {
        config: Arc::new(RwLock::new((*config).clone())),
        token: Arc::new(new_token()),
    };
    let app = Router::new()
        .route("/", get(index))
        .route("/api/config", get(read_config).put(write_config))
        .route("/api/providers/{name}/test", post(test_provider))
        .route("/api/install", get(install_status).post(install_register))
        .layer(axum::middleware::from_fn_with_state(state.clone(), guard))
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

/// Two guards in one: the Host header must be loopback (blocks DNS rebinding),
/// and every state-changing request must carry the per-process token cookie
/// (blocks any other local page or process from writing config or registering).
async fn guard(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let host_name = host.rsplit_once(':').map(|(name, _)| name).unwrap_or(host);
    if !matches!(host_name, "127.0.0.1" | "localhost" | "[::1]") {
        return (StatusCode::FORBIDDEN, "loopback only").into_response();
    }

    if request.method() != Method::GET && request.method() != Method::HEAD {
        if !has_token(request.headers(), &state.token) {
            return (StatusCode::FORBIDDEN, "missing session token; reopen the settings page").into_response();
        }
        if let Some(origin) = request.headers().get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
            let origin_host = origin.split("://").nth(1).unwrap_or("");
            let origin_name = origin_host.rsplit_once(':').map(|(n, _)| n).unwrap_or(origin_host);
            if !matches!(origin_name, "127.0.0.1" | "localhost" | "[::1]") {
                return (StatusCode::FORBIDDEN, "cross-origin").into_response();
            }
        }
    }

    next.run(request).await
}

fn has_token(headers: &header::HeaderMap, token: &str) -> bool {
    let Some(cookie) = headers.get(header::COOKIE).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    cookie.split(';').any(|pair| {
        let mut parts = pair.splitn(2, '=');
        matches!(parts.next().map(str::trim), Some("shenbi_session"))
            && parts.next().map(str::trim) == Some(token)
    })
}

async fn index(State(state): State<AppState>) -> Response {
    let cookie = format!(
        "shenbi_session={}; Path=/; HttpOnly; SameSite=Strict",
        state.token
    );
    let mut response = Html(INDEX_HTML).into_response();
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    response
}

/// Group order on the page: what generates first, then the processing helpers,
/// then the planner. Unknown names trail at the end.
fn group_rank(group: &str) -> u8 {
    match group {
        "生成" => 0,
        "图像处理" => 1,
        "规划" => 2,
        _ => 3,
    }
}

fn provider_names(config: &Config) -> Vec<String> {
    let mut names: Vec<(u8, usize, String)> = PROVIDERS
        .iter()
        .enumerate()
        .filter(|(_, meta)| config.providers.contains_key(meta.name))
        .map(|(index, meta)| (group_rank(meta.group), index, meta.name.to_string()))
        .collect();
    let mut rest: Vec<(u8, usize, String)> = config
        .providers
        .keys()
        .filter(|name| meta_of(name).is_none())
        .enumerate()
        .map(|(index, name)| (3, 100 + index, name.clone()))
        .collect();
    rest.sort_by(|a, b| a.2.cmp(&b.2));
    names.append(&mut rest);
    names.sort_by_key(|(rank, index, _)| (*rank, *index));
    names.into_iter().map(|(_, _, name)| name).collect()
}

fn snapshot(config: &Config) -> Value {
    let providers: Vec<Value> = provider_names(config)
        .into_iter()
        .map(|name| {
            let entry = config.provider(&name);
            let env_key = config.env_keys.contains(&name);
            let meta = meta_of(&name);
            json!({
                "name": name,
                "label": meta.map(|m| m.label).unwrap_or(name.as_str()),
                "role": meta.map(|m| m.role).unwrap_or(""),
                "group": meta.map(|m| m.group).unwrap_or("其他"),
                "configured": config.is_configured(&name),
                "envKey": env_key,
                "testable": TESTABLE.contains(&name.as_str()),
                // Only these two providers have a separate editing model.
                "supportsEdit": matches!(name.as_str(), "openai" | "fal"),
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
    // Client configs are JSON, so always emit forward slashes.
    let exe = exe.replace('\\', "/");
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "defaultProvider": config.default_provider,
        "outputDir": config.output_dir,
        "inputRoot": config.input_root,
        "runRetentionHours": config.run_retention_hours,
        "maxConcurrent": config.max_concurrent,
        "requestTimeoutSeconds": config.request_timeout_ms / 1000,
        "providers": providers,
        "savedAtMs": config.saved_at_ms().unwrap_or(0),
        // Shown as-is in the footer; the page abbreviates the home directory.
        "configPath": config.path.as_ref().map(|p| crate::util::display_path(p)).unwrap_or_default(),
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
    clear_api_key: Option<bool>,
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
    request_timeout_seconds: Option<u64>,
    #[serde(default)]
    providers: BTreeMap<String, ProviderPatch>,
}

/// An empty text field means "leave the stored value alone": clearing a model
/// name by accident used to silently wipe it, and the defaults refill it on the
/// next load anyway.
fn non_empty(value: String) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

async fn write_config(State(state): State<AppState>, Json(patch): Json<ConfigPatch>) -> Response {
    let mut guard = state.config.write().expect("config lock");
    let mut next = guard.clone();
    let known = provider_names(&next);

    if let Some(value) = patch.default_provider.filter(|v| known.iter().any(|k| k == v.trim())) {
        next.default_provider = value.trim().to_string();
    }
    if let Some(value) = non_empty(patch.output_dir.unwrap_or_default()) {
        next.output_dir = value;
    }
    if let Some(value) = patch.input_root {
        next.input_root = value.trim().to_string();
    }
    if let Some(value) = patch.run_retention_hours {
        next.run_retention_hours = value.min(720);
    }
    if let Some(value) = patch.max_concurrent {
        next.max_concurrent = value.clamp(1, 8);
    }
    if let Some(value) = patch.request_timeout_seconds {
        next.request_timeout_ms = value.clamp(5, 600) * 1000;
    }

    for (name, patch) in patch.providers {
        // Only names the binary already knows; a tampered DOM cannot invent a
        // provider and have it written to disk.
        if !known.contains(&name) {
            continue;
        }
        let mut changed = false;
        let entry = next.providers.entry(name.clone()).or_insert_with(ProviderConfig::default);
        if let Some(clear) = patch.clear_api_key {
            if clear && !next.env_keys.contains(&name) {
                entry.api_key.clear();
                changed = true;
            }
        }
        if let Some(key) = patch.api_key.and_then(non_empty) {
            if !next.env_keys.contains(&name) {
                entry.api_key = key;
                changed = true;
            }
        }
        if let Some(model) = patch.model.and_then(non_empty) {
            entry.model = model;
            changed = true;
        }
        if let Some(edit) = patch.edit_model.and_then(non_empty) {
            entry.edit_model = edit;
            changed = true;
        }
        if let Some(base) = patch.base_url.and_then(non_empty) {
            entry.base_url = base;
            changed = true;
        }
        if let Some(format) = patch.format.and_then(non_empty) {
            entry.format = format;
            changed = true;
        }
        let _ = changed;
    }

    if let Err(err) = next.save() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "error": format!(
                    "写配置失败：{err}。检查 {} 所在目录是否可写。",
                    next.path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "配置目录".into())
                )
            })),
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
    format: Option<String>,
}

async fn test_provider(
    axum::extract::Path(name): axum::extract::Path<String>,
    State(state): State<AppState>,
    Json(body): Json<TestRequest>,
) -> Json<Value> {
    if !TESTABLE.contains(&name.as_str()) {
        return Json(json!({ "ok": false, "error": format!("{name} 暂不支持在线测试，保存后由 AI 调用时才会校验") }));
    }
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
            return Json(json!({ "ok": false, "error": "先填写接口地址再测试" }));
        };
        let format = body.format.filter(|f| !f.trim().is_empty()).unwrap_or_else(|| {
            if stored.format.is_empty() { "openai".to_string() } else { stored.format.clone() }
        });
        let result = crate::custom::probe_with_format(&key, &base, &format).await;
        return Json(match result {
            Ok(detail) => json!({ "ok": true, "detail": detail }),
            Err(error) => json!({ "ok": false, "error": error }),
        });
    }

    if key.trim().is_empty() {
        return Json(json!({ "ok": false, "error": "还没有填写 API 密钥" }));
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
    classify(response, "密钥有效").await
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
    let known = models.contains(&model);
    Ok(if !has_image_model {
        "密钥有效，但账户未开放图像模型".into()
    } else if known {
        format!("密钥有效 · 模型 {model} 可用")
    } else {
        format!("密钥有效 · 模型 {model} 不在列表里（可能需要组织验证）")
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
        format!("密钥有效 · {model} 可用于规划")
    } else {
        format!("密钥有效 · 列表里没有 {model}，规划前请确认模型名")
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
    classify(response, "密钥有效").await
}

async fn classify(response: reqwest::Response, ok_detail: &str) -> Result<String, String> {
    if response.status().is_success() {
        Ok(ok_detail.to_string())
    } else {
        Err(status_message(response).await)
    }
}

/// Upstream error bodies sometimes echo the submitted key. Never let that reach
/// the page.
fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for word in text.split_inclusive(|c: char| c.is_whitespace() || ",;\"'".contains(c)) {
        let trimmed = word.trim();
        let looks_secret = (trimmed.len() >= 12)
            && (trimmed.starts_with("sk-")
                || trimmed.starts_with("AIza")
                || trimmed.starts_with("xai-")
                || trimmed.starts_with("r8_")
                || (trimmed.chars().filter(|c| c.is_ascii_alphanumeric()).count() >= 24
                    && trimmed.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))));
        if looks_secret {
            out.push_str("[已隐藏]");
            if let Some(last) = word.chars().last() {
                if !last.is_alphanumeric() {
                    out.push(last);
                }
            }
        } else {
            out.push_str(word);
        }
    }
    out.chars().take(240).collect()
}

async fn status_message(response: reqwest::Response) -> String {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let detail = http::error_message(&parsed).unwrap_or_else(|| body.chars().take(240).collect::<String>());
    let detail = redact(detail.trim());
    let label = match status.as_u16() {
        401 => "密钥无效",
        403 => "无权限或需要验证",
        429 => "额度受限，稍后再试",
        _ => "连接失败",
    };
    if detail.is_empty() {
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

/// Registration writes outside the app's own files, so it stays a deliberate
/// POST from the token-bearing page.
async fn install_register() -> Json<Value> {
    match tokio::task::spawn_blocking(|| crate::install::register("user", false)).await {
        Ok(Ok(outcome)) => Json(json!({ "ok": true, "detail": outcome.detail, "target": outcome.target })),
        Ok(Err(err)) => Json(json!({ "ok": false, "error": err.to_string() })),
        Err(err) => Json(json!({ "ok": false, "error": err.to_string() })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_hides_key_shaped_tokens() {
        let text = "invalid key sk-abcdefghijklmnop for request";
        assert!(!redact(text).contains("sk-abcdefghijklmnop"));
        assert!(redact(text).contains("invalid key"));
    }

    #[test]
    fn redact_keeps_normal_prose() {
        let text = "连接失败（HTTP 401）：invalid api key";
        assert_eq!(redact(text), text);
    }

    #[test]
    fn token_check_requires_exact_cookie() {
        let mut headers = header::HeaderMap::new();
        headers.insert(header::COOKIE, HeaderValue::from_static("shenbi_session=abc; x=1"));
        assert!(has_token(&headers, "abc"));
        assert!(!has_token(&headers, "abcd"));
    }
}
