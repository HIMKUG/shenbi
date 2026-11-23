use crate::config::Config;
use crate::error::AppError;
use crate::http;
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Size {
    Square,
    Landscape,
    Portrait,
}

impl Size {
    pub fn parse(value: Option<&str>) -> Result<Self, AppError> {
        match value.unwrap_or("square") {
            "square" => Ok(Size::Square),
            "landscape" => Ok(Size::Landscape),
            "portrait" => Ok(Size::Portrait),
            other => Err(AppError::constraint(format!(
                "unsupported size '{other}'; use square, landscape or portrait"
            ))),
        }
    }

    /// OpenAI image sizes in pixels.
    pub fn openai_dims(self) -> &'static str {
        match self {
            Size::Square => "1024x1024",
            Size::Landscape => "1536x1024",
            Size::Portrait => "1024x1536",
        }
    }

    pub fn aspect_ratio(self) -> &'static str {
        match self {
            Size::Square => "1:1",
            Size::Landscape => "16:9",
            Size::Portrait => "9:16",
        }
    }

    pub fn ideogram_ratio(self) -> &'static str {
        match self {
            Size::Square => "1x1",
            Size::Landscape => "16x9",
            Size::Portrait => "9x16",
        }
    }

    pub fn dims(self) -> (u32, u32) {
        match self {
            Size::Square => (1024, 1024),
            Size::Landscape => (1440, 960),
            Size::Portrait => (960, 1440),
        }
    }
}

pub struct GeneratedImage {
    pub bytes: Vec<u8>,
    pub model: String,
    pub revised_prompt: Option<String>,
}

pub fn supports_size(provider: &str) -> bool {
    provider != "grok"
}

pub fn default_model(config: &Config, provider: &str) -> String {
    config
        .provider(provider)
        .map(|p| p.model.trim().to_string())
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| match provider {
            "openai" => "gpt-image-1".into(),
            "gemini" => "gemini-3.1-flash-image".into(),
            "replicate" => "black-forest-labs/flux-1.1-pro".into(),
            "together" => "black-forest-labs/FLUX.1-schnell".into(),
            "grok" => "grok-imagine-image-2.0".into(),
            "ideogram" => "ideogram-v3-0".into(),
            _ => String::new(),
        })
}

fn require_key(config: &Config, provider: &str) -> Result<String, AppError> {
    config
        .provider(provider)
        .map(|p| p.api_key.trim().to_string())
        .filter(|k| !k.is_empty())
        .ok_or_else(|| {
            AppError::new(
                crate::error::ErrorCode::NotConfigured,
                format!("provider '{provider}' has no API key configured"),
                false,
            )
            .with_suggestion("Open the configuration UI at / and add the key.")
        })
}

fn timeout(config: &Config) -> Duration {
    Duration::from_millis(config.request_timeout_ms.max(5_000))
}

pub async fn generate(
    config: &Config,
    provider: &str,
    prompt: &str,
    size: Size,
    model_override: Option<&str>,
) -> Result<GeneratedImage, AppError> {
    let model = model_override
        .map(str::to_string)
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| default_model(config, provider));
    match provider {
        "openai" => openai_generate(config, &model, prompt, size).await,
        "gemini" => gemini_generate(config, &model, prompt, size).await,
        "replicate" => replicate_generate(config, &model, prompt, size).await,
        "together" => together_generate(config, &model, prompt, size).await,
        "grok" => grok_generate(config, &model, prompt).await,
        "ideogram" => ideogram_generate(config, &model, prompt, size).await,
        other => Err(AppError::constraint(format!("unknown provider '{other}'"))),
    }
}

async fn openai_generate(config: &Config, model: &str, prompt: &str, size: Size) -> Result<GeneratedImage, AppError> {
    let key = require_key(config, "openai")?;
    let url = format!("{}/v1/images/generations", openai_base(config));
    let body = json!({
        "model": model,
        "prompt": prompt,
        "n": 1,
        "size": size.openai_dims(),
    });
    let value = http::with_retries("openai generate", 2, |_| {
        let request = http::client()
            .post(&url)
            .bearer_auth(&key)
            .timeout(timeout(config))
            .json(&body);
        async move { http::json_or_error(request.send().await?).await }
    })
    .await?;
    let item = value.pointer("/data/0").ok_or_else(|| {
        AppError::provider("OpenAI returned no image (likely a content-policy block)", false)
    })?;
    let bytes = if let Some(b64) = item.get("b64_json").and_then(Value::as_str) {
        http::decode_b64(b64)?
    } else if let Some(url) = item.get("url").and_then(Value::as_str) {
        http::download_image(url, timeout(config)).await?
    } else {
        return Err(AppError::provider("OpenAI returned no image payload", false));
    };
    Ok(GeneratedImage {
        bytes,
        model: model.to_string(),
        revised_prompt: item.get("revised_prompt").and_then(Value::as_str).map(str::to_string),
    })
}

fn openai_base(config: &Config) -> String {
    config
        .provider("openai")
        .map(|p| p.base_url.trim().trim_end_matches('/').to_string())
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| "https://api.openai.com".to_string())
}

pub async fn openai_edit(
    config: &Config,
    model: &str,
    prompt: &str,
    size: Size,
    mime: &str,
    input: &[u8],
    prompt_limit: usize,
) -> Result<GeneratedImage, AppError> {
    let key = require_key(config, "openai")?;
    if prompt.chars().count() > prompt_limit {
        return Err(AppError::constraint(format!(
            "prompt exceeds {prompt_limit} characters"
        )));
    }
    let url = format!("{}/v1/images/edits", openai_base(config));
    let body = json!({
        "model": model,
        "images": [{ "image_url": http::data_url(mime, input) }],
        "prompt": prompt,
        "n": 1,
        "size": size.openai_dims(),
    });
    let value = http::with_retries("openai edit", 2, |_| {
        let request = http::client()
            .post(&url)
            .bearer_auth(&key)
            .timeout(timeout(config))
            .json(&body);
        async move { http::json_or_error(request.send().await?).await }
    })
    .await?;
    let item = value
        .pointer("/data/0")
        .ok_or_else(|| AppError::provider("OpenAI edit returned no image (likely a content-policy block)", false))?;
    let bytes = if let Some(b64) = item.get("b64_json").and_then(Value::as_str) {
        http::decode_b64(b64)?
    } else if let Some(url) = item.get("url").and_then(Value::as_str) {
        http::download_image(url, timeout(config)).await?
    } else {
        return Err(AppError::provider("OpenAI edit returned no image payload", false));
    };
    Ok(GeneratedImage {
        bytes,
        model: model.to_string(),
        revised_prompt: item.get("revised_prompt").and_then(Value::as_str).map(str::to_string),
    })
}

async fn gemini_generate(config: &Config, model: &str, prompt: &str, size: Size) -> Result<GeneratedImage, AppError> {
    let key = require_key(config, "gemini")?;
    let base = config
        .provider("gemini")
        .map(|p| p.base_url.trim().trim_end_matches('/').to_string())
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| "https://generativelanguage.googleapis.com".to_string());
    let url = format!("{base}/v1beta/models/{model}:generateContent");
    let body = json!({
        "contents": [{ "parts": [{ "text": prompt }] }],
        "generationConfig": {
            "responseModalities": ["TEXT", "IMAGE"],
            "imageConfig": { "aspectRatio": size.aspect_ratio() }
        }
    });
    let value = http::with_retries("gemini generate", 2, |_| {
        let request = http::client()
            .post(&url)
            .header("x-goog-api-key", &key)
            .timeout(timeout(config))
            .json(&body);
        async move { http::json_or_error(request.send().await?).await }
    })
    .await?;
    let parts = value
        .pointer("/candidates/0/content/parts")
        .and_then(Value::as_array)
        .ok_or_else(|| AppError::provider("Gemini returned no candidates", true))?;
    for part in parts {
        if let Some(data) = part.pointer("/inlineData/data").and_then(Value::as_str) {
            return Ok(GeneratedImage {
                bytes: http::decode_b64(data)?,
                model: model.to_string(),
                revised_prompt: None,
            });
        }
    }
    Err(AppError::provider("Gemini response contained no image data", true))
}

async fn replicate_generate(
    config: &Config,
    model: &str,
    prompt: &str,
    size: Size,
) -> Result<GeneratedImage, AppError> {
    let key = require_key(config, "replicate")?;
    let url = format!("https://api.replicate.com/v1/models/{model}/predictions");
    let body = json!({ "input": { "prompt": prompt, "aspect_ratio": size.aspect_ratio() } });
    let value = http::with_retries("replicate create", 1, |_| {
        let request = http::client()
            .post(&url)
            .bearer_auth(&key)
            .header("Prefer", "wait")
            .timeout(timeout(config))
            .json(&body);
        async move { http::json_or_error(request.send().await?).await }
    })
    .await?;
    let mut prediction = value;
    let get_url = prediction
        .pointer("/urls/get")
        .and_then(Value::as_str)
        .map(str::to_string);

    let deadline = tokio::time::Instant::now() + timeout(config);
    while matches!(
        prediction.get("status").and_then(Value::as_str),
        Some("starting") | Some("processing")
    ) {
        if tokio::time::Instant::now() >= deadline {
            if let Some(cancel) = prediction.pointer("/urls/cancel").and_then(Value::as_str) {
                let _ = http::client()
                    .post(cancel)
                    .bearer_auth(&key)
                    .timeout(Duration::from_secs(10))
                    .send()
                    .await;
            }
            return Err(AppError::timeout("Replicate prediction did not finish in time"));
        }
        tokio::time::sleep(Duration::from_millis(1_000)).await;
        let Some(url) = get_url.as_deref() else {
            return Err(AppError::provider("Replicate returned no polling URL", true));
        };
        let request = http::client()
            .get(url)
            .bearer_auth(&key)
            .timeout(Duration::from_secs(30));
        prediction = http::json_or_error(request.send().await?).await?;
    }

    match prediction.get("status").and_then(Value::as_str) {
        Some("succeeded") => {}
        Some(status) => {
            let detail = prediction
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("no error detail");
            return Err(AppError::provider(
                format!("Replicate prediction {status}: {detail}"),
                status == "failed",
            ));
        }
        None => return Err(AppError::provider("Replicate returned no prediction status", true)),
    }

    let output = prediction.get("output").cloned().unwrap_or(Value::Null);
    let first = match output {
        Value::Array(items) => items.first().cloned().unwrap_or(Value::Null),
        other => other,
    };
    let bytes = match first.as_str() {
        Some(url) if url.starts_with("http") => http::download_image(url, timeout(config)).await?,
        Some(url) if url.starts_with("data:") => http::decode_b64(url)?,
        _ => return Err(AppError::provider("Replicate returned no image URL", true)),
    };
    Ok(GeneratedImage { bytes, model: model.to_string(), revised_prompt: None })
}

async fn together_generate(
    config: &Config,
    model: &str,
    prompt: &str,
    size: Size,
) -> Result<GeneratedImage, AppError> {
    let key = require_key(config, "together")?;
    let (width, height) = size.dims();
    let body = json!({
        "model": model,
        "prompt": prompt,
        "width": width,
        "height": height,
        "n": 1,
        "response_format": "b64_json"
    });
    let value = http::with_retries("together generate", 2, |_| {
        let request = http::client()
            .post("https://api.together.xyz/v1/images/generations")
            .bearer_auth(&key)
            .timeout(timeout(config))
            .json(&body);
        async move { http::json_or_error(request.send().await?).await }
    })
    .await?;
    let b64 = value
        .pointer("/data/0/b64_json")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::provider("Together returned no image data", true))?;
    Ok(GeneratedImage { bytes: http::decode_b64(b64)?, model: model.to_string(), revised_prompt: None })
}

async fn grok_generate(config: &Config, model: &str, prompt: &str) -> Result<GeneratedImage, AppError> {
    let key = require_key(config, "grok")?;
    let body = json!({ "model": model, "prompt": prompt, "n": 1, "response_format": "b64_json" });
    let value = http::with_retries("grok generate", 2, |_| {
        let request = http::client()
            .post("https://api.x.ai/v1/images/generations")
            .bearer_auth(&key)
            .timeout(timeout(config))
            .json(&body);
        async move { http::json_or_error(request.send().await?).await }
    })
    .await?;
    let b64 = value
        .pointer("/data/0/b64_json")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::provider("xAI returned no image data", true))?;
    Ok(GeneratedImage { bytes: http::decode_b64(b64)?, model: model.to_string(), revised_prompt: None })
}

async fn ideogram_generate(
    config: &Config,
    model: &str,
    prompt: &str,
    size: Size,
) -> Result<GeneratedImage, AppError> {
    let key = require_key(config, "ideogram")?;
    let _ = model;
    let value = http::with_retries("ideogram generate", 1, |_| {
        let form = reqwest::multipart::Form::new()
            .text("prompt", prompt.to_string())
            .text("aspect_ratio", size.ideogram_ratio())
            .text("rendering_speed", "DEFAULT")
            .text("num_images", "1");
        let request = http::client()
            .post("https://api.ideogram.ai/v1/ideogram-v3/generate")
            .header("Api-Key", &key)
            .timeout(timeout(config))
            .multipart(form);
        async move { http::json_or_error(request.send().await?).await }
    })
    .await?;
    let url = value
        .pointer("/data/0/url")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::provider("Ideogram returned no image URL", true))?;
    Ok(GeneratedImage {
        bytes: http::download_image(url, timeout(config)).await?,
        model: "ideogram-v3-0".into(),
        revised_prompt: None,
    })
}
