//! OpenAI-compatible custom image endpoint support.
//!
//! Relays (one-api / new-api / SiliconFlow / LiteLLM and friends) all speak a
//! `/images/generations` POST but disagree on the request field for the size
//! and on where the bytes come back. Both are handled here so the rest of the
//! code base only sees a `GeneratedImage`.

use crate::config::Config;
use crate::error::AppError;
use crate::http;
use crate::providers::Size;
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireFormat {
    /// OpenAI: `{"size": "1024x1024"}`, image in `data[0].b64_json` or `data[0].url`.
    OpenAi,
    /// SiliconFlow: `{"image_size": "1024x1024"}`, image in `images[0].url`.
    SiliconFlow,
}

impl WireFormat {
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "siliconflow" | "silicon_flow" | "sf" => WireFormat::SiliconFlow,
            _ => WireFormat::OpenAi,
        }
    }

}

/// Accepts a base (`https://host/v1`), a prefix (`https://host`) or the full
/// endpoint and always returns the endpoint.
pub fn endpoint(base_url: &str) -> String {
    let base = base_url.trim().trim_end_matches('/');
    if base.ends_with("/images/generations") {
        base.to_string()
    } else if base.ends_with("/v1") {
        format!("{base}/images/generations")
    } else if base.is_empty() {
        String::new()
    } else {
        format!("{base}/v1/images/generations")
    }
}

/// Model-discovery URL used by the connectivity probe.
pub fn models_url(base_url: &str) -> Option<String> {
    let base = base_url.trim().trim_end_matches('/');
    if base.is_empty() {
        return None;
    }
    let root = base
        .strip_suffix("/images/generations")
        .unwrap_or(base)
        .trim_end_matches('/');
    if root.ends_with("/v1") {
        Some(format!("{root}/models"))
    } else {
        Some(format!("{root}/v1/models"))
    }
}

fn size_for(format: WireFormat, size: Size) -> &'static str {
    match format {
        WireFormat::OpenAi => size.openai_dims(),
        // Every resolution here is on SiliconFlow's documented list.
        WireFormat::SiliconFlow => match size {
            Size::Square => "1024x1024",
            Size::Landscape => "1024x576",
            Size::Portrait => "576x1024",
        },
    }
}

/// Relays differ: b64 JSON, a URL, or a bare string URL.
fn extract_payload(value: &Value) -> Option<String> {
    let first = value
        .pointer("/data/0")
        .or_else(|| value.pointer("/images/0"))
        .or_else(|| value.pointer("/data"))
        .or_else(|| value.pointer("/images"))?;

    let candidate = match first {
        Value::Array(items) => items.first()?,
        other => other,
    };

    if let Some(b64) = candidate.get("b64_json").and_then(Value::as_str) {
        return Some(format!("b64:{b64}"));
    }
    if let Some(url) = candidate.get("url").and_then(Value::as_str) {
        // Some relays inline the image instead of hosting it.
        return Some(if url.starts_with("data:") {
            format!("b64:{url}")
        } else {
            format!("url:{url}")
        });
    }
    if let Some(text) = candidate.as_str() {
        if text.starts_with("data:") {
            return Some(format!("b64:{text}"));
        }
        if text.starts_with("http") {
            return Some(format!("url:{text}"));
        }
    }
    None
}

pub async fn generate(
    config: &Config,
    provider: &str,
    model: &str,
    prompt: &str,
    size: Size,
) -> Result<super::providers::GeneratedImage, AppError> {
    let entry = config
        .provider(provider)
        .ok_or_else(|| AppError::constraint("custom provider is not configured"))?;
    let api_key = entry.api_key.trim();
    if api_key.is_empty() {
        return Err(AppError::constraint(
            "custom endpoint has no API key; add it in the config UI",
        ));
    }
    let url = endpoint(&entry.base_url);
    if url.is_empty() {
        return Err(AppError::constraint(
            "custom endpoint has no base URL; add e.g. https://api.example.com/v1 in the config UI",
        ));
    }
    if model.trim().is_empty() {
        return Err(AppError::constraint(
            "custom endpoint has no model name; add it in the config UI",
        ));
    }

    let format = WireFormat::parse(&entry.format);
    let dimension = size_for(format, size);
    let body = match format {
        WireFormat::OpenAi => json!({
            "model": model,
            "prompt": prompt,
            "n": 1,
            "size": dimension,
        }),
        WireFormat::SiliconFlow => json!({
            "model": model,
            "prompt": prompt,
            "image_size": dimension,
            "batch_size": 1,
        }),
    };

    let timeout = Duration::from_millis(config.request_timeout_ms.max(5_000));
    let value = http::with_retries("custom generate", 2, |_| {
        let request = http::client()
            .post(&url)
            .bearer_auth(api_key)
            .timeout(timeout)
            .json(&body);
        async move { http::json_or_error(request.send().await?).await }
    })
    .await?;

    let payload = extract_payload(&value).ok_or_else(|| {
        AppError::provider(
            "custom endpoint returned no image; expected data[0].b64_json, data[0].url or images[0].url",
            false,
        )
    })?;

    let bytes = if let Some(b64) = payload.strip_prefix("b64:") {
        http::decode_b64(b64)?
    } else if let Some(url) = payload.strip_prefix("url:") {
        http::download_image(url, timeout).await?
    } else {
        return Err(AppError::provider("custom endpoint returned an unusable payload", false));
    };

    Ok(super::providers::GeneratedImage {
        bytes,
        model: model.to_string(),
        revised_prompt: value
            .pointer("/data/0/revised_prompt")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// Probe `/models` on the relay. Auth-less relays still answer 200/401
/// meaningfully, so surface the HTTP result rather than guessing.
pub async fn probe(api_key: &str, base_url: &str) -> Result<String, String> {
    let Some(url) = models_url(base_url) else {
        return Err("请先填写接口地址".into());
    };
    let request = http::client()
        .get(&url)
        .timeout(Duration::from_secs(15));
    let request = if api_key.trim().is_empty() {
        request
    } else {
        request.bearer_auth(api_key)
    };
    let response = request.send().await.map_err(|err| {
        if err.is_timeout() {
            "请求超时，检查接口地址或代理".to_string()
        } else if err.is_connect() {
            "无法建立连接，检查接口地址".to_string()
        } else {
            err.to_string()
        }
    })?;
    if response.status().is_success() {
        return Ok("接口可达".into());
    }
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let detail = http::error_message(&parsed)
        .unwrap_or_else(|| body.chars().take(200).collect::<String>());
    Err(if detail.trim().is_empty() {
        format!("HTTP {}", status.as_u16())
    } else {
        format!("HTTP {}：{detail}", status.as_u16())
    })
}
