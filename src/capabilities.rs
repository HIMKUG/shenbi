use crate::config::Config;
use crate::error::AppError;
use crate::http;
use crate::imaging;
use crate::output;
use crate::providers::{self, GeneratedImage, Size};
use crate::util;
use serde_json::{json, Value};
use std::time::Duration;

pub struct OpRequest<'a> {
    pub config: &'a Config,
    pub op: &'a str,
    pub provider: &'a str,
    pub params: &'a Value,
}

pub enum OpOutput {
    Image { bytes: Vec<u8>, model: String, revised_prompt: Option<String>, metadata: Value },
    Data { data: Value, model: String, metadata: Value },
}

fn param_str<'a>(params: &'a Value, key: &str) -> Result<&'a str, AppError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| AppError::constraint(format!("missing required parameter '{key}'")))
}

fn param_size(params: &Value) -> Result<Size, AppError> {
    Size::parse(params.get("size").and_then(Value::as_str))
}

fn provider_timeout(config: &Config) -> Duration {
    Duration::from_millis(config.request_timeout_ms.max(5_000))
}

pub async fn registered_pairs(config: &Config) -> Vec<Value> {
    let mut out = Vec::new();
    let push = |list: &mut Vec<Value>, op: &str, provider: &str, model: String, cost: f64, needs_input: bool, sizes: bool| {
        list.push(json!({
            "op": op,
            "provider": provider,
            "model": model,
            "costUsd": cost,
            "requiresInput": needs_input,
            "sizes": sizes,
        }));
    };

    for provider in ["openai", "gemini", "replicate", "together", "grok", "ideogram"] {
        if config.is_configured(provider) {
            let model = providers::default_model(config, provider);
            push(&mut out, "generate", provider, model, cost_of(provider), false, true);
        }
    }
    if config.is_configured("openai") {
        let model = config
            .provider("openai")
            .map(|p| p.edit_model.clone())
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| "gpt-image-1.5".into());
        push(&mut out, "edit_prompt", "openai", model, 0.04, true, true);
    }
    if config.is_configured("fal") {
        push(&mut out, "edit_prompt", "fal", "fal-ai/flux-pro/kontext".into(), 0.04, true, true);
    }
    if config.is_configured("photoroom") {
        push(&mut out, "extract_subject", "photoroom", "remove-bg-v2".into(), 0.02, true, false);
        push(&mut out, "composite_layers", "photoroom", "image-editing-v1".into(), 0.05, true, false);
    }
    if config.is_configured("replicate") {
        push(&mut out, "enhance_upscale", "replicate", "nightmareai/real-esrgan".into(), 0.01, true, false);
    }
    push(&mut out, "transform", "sharp", "local-transform".into(), 0.0, true, false);
    push(&mut out, "composite_layers", "sharp", "local-composite".into(), 0.0, false, false);
    push(&mut out, "analyze_dimensions", "sharp", "local-metadata".into(), 0.0, true, false);
    push(&mut out, "analyze_palette", "sharp", "local-palette".into(), 0.0, true, false);
    out
}

fn cost_of(provider: &str) -> f64 {
    match provider {
        "openai" => 0.04,
        "gemini" => 0.03,
        "replicate" => 0.004,
        "together" => 0.003,
        "grok" => 0.02,
        "ideogram" => 0.08,
        _ => 0.0,
    }
}

pub async fn invoke(request: OpRequest<'_>) -> Result<OpOutput, AppError> {
    let OpRequest { config, op, provider, params } = request;
    match (op, provider) {
        ("generate", provider) => {
            let prompt = param_str(params, "prompt")?;
            let size = param_size(params)?;
            let model = params.get("model").and_then(Value::as_str);
            let image = providers::generate(config, provider, prompt, size, model).await?;
            Ok(image_output(image, json!({ "provider": provider, "prompt": prompt })))
        }
        ("edit_prompt", "openai") => {
            let input = param_str(params, "input")?;
            let prompt = param_str(params, "prompt")?;
            output::assert_within_input_root(config, input)
                .await
                .map_err(|err| AppError::constraint(err.to_string()))?;
            let bytes = output::read_input(input)?;
            let mime = imaging::info(&bytes)
                .ok()
                .and_then(|info| util::mime_for_format(Some(&info.format)))
                .ok_or_else(|| AppError::unsupported("edit_prompt needs a PNG, JPEG or WebP input"))?;
            let model = config
                .provider("openai")
                .map(|p| p.edit_model.clone())
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| "gpt-image-1.5".into());
            let image = providers::openai_edit(config, &model, prompt, param_size(params)?, mime, &bytes, 4000).await?;
            Ok(image_output(image, json!({ "input": input })))
        }
        ("edit_prompt", "fal") => fal_edit(config, params).await,
        ("transform", "sharp") => {
            let input = param_str(params, "input")?;
            output::assert_within_input_root(config, input)
                .await
                .map_err(|err| AppError::constraint(err.to_string()))?;
            let operations: Vec<imaging::Operation> = serde_json::from_value(
                params
                    .get("operations")
                    .cloned()
                    .ok_or_else(|| AppError::constraint("transform requires an operations array"))?,
            )
            .map_err(|err| AppError::constraint(format!("invalid operations: {err}")))?;
            let bytes = output::read_input(input)?;
            let processed = imaging::apply(&bytes, &operations)?;
            Ok(OpOutput::Image {
                bytes: processed.bytes,
                model: "sharp-transform@1".into(),
                revised_prompt: None,
                metadata: json!({
                    "input": input,
                    "originalInfo": processed.original,
                    "outputInfo": processed.output,
                    "operationsApplied": processed.applied,
                }),
            })
        }
        ("composite_layers", "sharp") => {
            let canvas = params
                .get("canvas")
                .ok_or_else(|| AppError::constraint("composite_layers requires canvas"))?;
            let width = canvas.get("width").and_then(Value::as_u64).unwrap_or(0) as u32;
            let height = canvas.get("height").and_then(Value::as_u64).unwrap_or(0) as u32;
            let background = background_of(canvas.get("background"));
            let layers: Vec<imaging::CompositeLayer> = serde_json::from_value(
                params
                    .get("layers")
                    .cloned()
                    .ok_or_else(|| AppError::constraint("composite_layers requires layers"))?,
            )
            .map_err(|err| AppError::constraint(format!("invalid layers: {err}")))?;
            for layer in &layers {
                output::assert_within_input_root(config, &layer.input)
                    .await
                    .map_err(|err| AppError::constraint(err.to_string()))?;
            }
            let bytes = imaging::composite(width, height, background, &layers)?;
            Ok(OpOutput::Image {
                bytes,
                model: "sharp-composite@1".into(),
                revised_prompt: None,
                metadata: json!({ "canvasWidth": width, "canvasHeight": height, "layerCount": layers.len() }),
            })
        }
        ("composite_layers", "photoroom") => photoroom_composite(config, params).await,
        ("extract_subject", "photoroom") => photoroom_extract(config, params).await,
        ("enhance_upscale", "replicate") => upscale(config, params).await,
        ("analyze_dimensions", "sharp") => {
            let input = param_str(params, "input")?;
            output::assert_within_input_root(config, input)
                .await
                .map_err(|err| AppError::constraint(err.to_string()))?;
            let bytes = output::read_input(input)?;
            let info = imaging::info(&bytes)?;
            Ok(OpOutput::Data {
                data: json!({
                    "type": "dimensions",
                    "width": info.width,
                    "height": info.height,
                    "format": info.format,
                    "channels": info.channels,
                    "hasAlpha": info.has_alpha,
                }),
                model: "sharp-metadata@1".into(),
                metadata: json!({ "input": input }),
            })
        }
        ("analyze_palette", "sharp") => {
            let input = param_str(params, "input")?;
            output::assert_within_input_root(config, input)
                .await
                .map_err(|err| AppError::constraint(err.to_string()))?;
            let count = params.get("count").and_then(Value::as_u64).unwrap_or(5) as usize;
            let bytes = output::read_input(input)?;
            let colors = imaging::palette(&bytes, count, 16)?;
            Ok(OpOutput::Data {
                data: json!({ "type": "palette", "colors": colors }),
                model: "sharp-palette@1".into(),
                metadata: json!({ "input": input, "count": count }),
            })
        }
        (op, provider) => Err(AppError::constraint(format!(
            "capability '{op}' is not available for provider '{provider}'"
        ))),
    }
}

fn image_output(image: GeneratedImage, extra: Value) -> OpOutput {
    let mut metadata = extra;
    if let Value::Object(map) = &mut metadata {
        map.insert("modelVersion".into(), image.model.clone().into());
    }
    OpOutput::Image { bytes: image.bytes, model: image.model, revised_prompt: image.revised_prompt, metadata }
}

fn background_of(value: Option<&Value>) -> [u8; 4] {
    let Some(value) = value else { return [0, 0, 0, 0] };
    if let Some(text) = value.as_str() {
        let hex = text.trim().trim_start_matches('#');
        if hex.len() >= 6 {
            let parse = |range: std::ops::Range<usize>| u8::from_str_radix(&hex[range], 16).unwrap_or(0);
            return [parse(0..2), parse(2..4), parse(4..6), 255];
        }
        return [0, 0, 0, 0];
    }
    let component = |key: &str| value.get(key).and_then(Value::as_f64).unwrap_or(0.0).clamp(0.0, 255.0) as u8;
    let alpha = value.get("alpha").and_then(Value::as_f64).unwrap_or(1.0).clamp(0.0, 1.0);
    [component("r"), component("g"), component("b"), (alpha * 255.0).round() as u8]
}

fn require_key(config: &Config, provider: &str) -> Result<String, AppError> {
    config
        .provider(provider)
        .map(|p| p.api_key.trim().to_string())
        .filter(|k| !k.is_empty())
        .ok_or_else(|| AppError::constraint(format!("provider '{provider}' has no API key configured")))
}

async fn photoroom_extract(config: &Config, params: &Value) -> Result<OpOutput, AppError> {
    let input = param_str(params, "input")?;
    let key = require_key(config, "photoroom")?;
    output::assert_within_input_root(config, input)
        .await
        .map_err(|err| AppError::constraint(err.to_string()))?;
    let bytes = output::read_input(input)?;
    let mime = imaging::info(&bytes)
        .ok()
        .and_then(|info| util::mime_for_format(Some(&info.format)))
        .ok_or_else(|| AppError::unsupported("extract_subject needs a PNG, JPEG or WebP input"))?;
    let form = reqwest::multipart::Form::new()
        .part(
            "image_file",
            reqwest::multipart::Part::bytes(bytes)
                .file_name("input")
                .mime_str(mime)
                .map_err(|err| AppError::constraint(err.to_string()))?,
        )
        .text("format", "png")
        .text("channels", "rgba");
    let response = http::client()
        .post("https://sdk.photoroom.com/v1/segment")
        .header("x-api-key", key)
        .timeout(provider_timeout(config))
        .multipart(form)
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(AppError::provider(
            format!("Photoroom segment failed: {}", body.chars().take(400).collect::<String>()),
            status.is_server_error() || status.as_u16() == 429,
        ));
    }
    let bytes = response.bytes().await?.to_vec();
    Ok(OpOutput::Image {
        bytes,
        model: "photoroom-remove-bg-v2".into(),
        revised_prompt: None,
        metadata: json!({ "input": input }),
    })
}

async fn photoroom_composite(config: &Config, params: &Value) -> Result<OpOutput, AppError> {
    let key = require_key(config, "photoroom")?;
    let canvas = params
        .get("canvas")
        .ok_or_else(|| AppError::constraint("composite_layers requires canvas"))?;
    let width = canvas.get("width").and_then(Value::as_u64).unwrap_or(0);
    let height = canvas.get("height").and_then(Value::as_u64).unwrap_or(0);
    let layers = params
        .get("layers")
        .and_then(Value::as_array)
        .filter(|l| l.len() == 1)
        .ok_or_else(|| {
            AppError::constraint(
                "composite_layers:photoroom handles exactly one subject layer; use composite_layers:sharp for more",
            )
        })?;
    let input = layers[0]
        .get("input")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::constraint("layers[0].input is required"))?;
    output::assert_within_input_root(config, input)
        .await
        .map_err(|err| AppError::constraint(err.to_string()))?;
    let bytes = output::read_input(input)?;
    let mime = imaging::info(&bytes)
        .ok()
        .and_then(|info| util::mime_for_format(Some(&info.format)))
        .ok_or_else(|| AppError::unsupported("composite_layers needs a PNG, JPEG or WebP input"))?;

    let mut form = reqwest::multipart::Form::new()
        .part(
            "imageFile",
            reqwest::multipart::Part::bytes(bytes)
                .file_name("input")
                .mime_str(mime)
                .map_err(|err| AppError::constraint(err.to_string()))?,
        )
        .text("removeBackground", "true")
        .text("export.format", "png")
        .text("outputSize", format!("{width}x{height}"))
        .text("padding", "0");
    if params.pointer("/shadow/enabled").and_then(Value::as_bool) == Some(true) {
        let mode = params
            .pointer("/shadow/mode")
            .and_then(Value::as_str)
            .unwrap_or("ai.soft")
            .to_string();
        form = form.text("shadow.mode", mode);
    }
    if let Some(color) = params
        .pointer("/background/color")
        .and_then(Value::as_str)
        .map(|c| c.trim_start_matches('#').to_string())
    {
        form = form.text("background.color", color);
    } else {
        let bg = background_of(canvas.get("background"));
        if bg[3] > 0 {
            form = form.text("background.color", format!("{:02x}{:02x}{:02x}", bg[0], bg[1], bg[2]));
        }
    }
    if let Some(prompt) = params.pointer("/background/prompt").and_then(Value::as_str) {
        form = form.text("background.prompt", prompt.to_string());
    }

    let response = http::client()
        .post("https://image-api.photoroom.com/v2/edit")
        .header("x-api-key", key)
        .timeout(provider_timeout(config))
        .multipart(form)
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(AppError::provider(
            format!("Photoroom edit failed: {}", body.chars().take(400).collect::<String>()),
            status.is_server_error() || status.as_u16() == 429,
        ));
    }
    let bytes = response.bytes().await?.to_vec();
    Ok(OpOutput::Image {
        bytes,
        model: "photoroom-image-editing-v1".into(),
        revised_prompt: None,
        metadata: json!({
            "shadowApplied": params.pointer("/shadow/enabled").and_then(Value::as_bool).unwrap_or(false)
        }),
    })
}

async fn fal_edit(config: &Config, params: &Value) -> Result<OpOutput, AppError> {
    let key = require_key(config, "fal")?;
    let input = param_str(params, "input")?;
    let prompt = param_str(params, "prompt")?;
    if prompt.chars().count() > 4000 {
        return Err(AppError::constraint("prompt exceeds 4000 characters"));
    }
    output::assert_within_input_root(config, input)
        .await
        .map_err(|err| AppError::constraint(err.to_string()))?;
    let bytes = output::read_input(input)?;
    let mime = imaging::info(&bytes)
        .ok()
        .and_then(|info| util::mime_for_format(Some(&info.format)))
        .ok_or_else(|| AppError::unsupported("edit_prompt needs a PNG, JPEG or WebP input"))?;

    let body = json!({
        "image_url": http::data_url(mime, &bytes),
        "prompt": prompt,
        "output_format": "png",
    });
    let submit = http::client()
        .post("https://queue.fal.run/fal-ai/flux-pro/kontext")
        .header("Authorization", format!("Key {key}"))
        .timeout(Duration::from_secs(60))
        .json(&body)
        .send()
        .await?;
    let submit_body = http::json_or_error(submit).await?;

    let image_url = if let Some(url) = submit_body.pointer("/images/0/url").and_then(Value::as_str) {
        url.to_string()
    } else {
        let status_url = submit_body
            .get("status_url")
            .and_then(Value::as_str)
            .ok_or_else(|| AppError::provider("fal returned no queue status URL", true))?;
        let response_url = submit_body
            .get("response_url")
            .and_then(Value::as_str)
            .ok_or_else(|| AppError::provider("fal returned no result URL", true))?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
        loop {
            if tokio::time::Instant::now() >= deadline {
                return Err(AppError::timeout("fal queue timed out after 90s"));
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
            let poll = http::client()
                .get(status_url)
                .header("Authorization", format!("Key {key}"))
                .timeout(Duration::from_secs(30))
                .send()
                .await?;
            let poll_body = http::json_or_error(poll).await?;
            match poll_body.get("status").and_then(Value::as_str) {
                Some("COMPLETED") => {
                    let result = http::client()
                        .get(response_url)
                        .header("Authorization", format!("Key {key}"))
                        .timeout(Duration::from_secs(30))
                        .send()
                        .await?;
                    let result_body = http::json_or_error(result).await?;
                    let url = result_body
                        .pointer("/images/0/url")
                        .and_then(Value::as_str)
                        .ok_or_else(|| AppError::provider("fal returned no image URL", true))?;
                    break url.to_string();
                }
                Some("FAILED") | Some("ERROR") => {
                    return Err(AppError::provider("fal queue reported failure", true))
                }
                _ => {}
            }
        }
    };

    Ok(OpOutput::Image {
        bytes: http::download_image(&image_url, provider_timeout(config)).await?,
        model: "fal-ai/flux-pro/kontext".into(),
        revised_prompt: None,
        metadata: json!({ "input": input }),
    })
}

async fn upscale(config: &Config, params: &Value) -> Result<OpOutput, AppError> {
    let key = require_key(config, "replicate")?;
    let input = param_str(params, "input")?;
    output::assert_within_input_root(config, input)
        .await
        .map_err(|err| AppError::constraint(err.to_string()))?;
    let bytes = output::read_input(input)?;
    let info = imaging::info(&bytes)?;
    let mime = util::mime_for_format(Some(&info.format))
        .ok_or_else(|| AppError::unsupported("enhance_upscale needs a PNG, JPEG or WebP input"))?;
    let scale = params.get("scale").and_then(Value::as_u64).unwrap_or(2);
    if scale != 2 && scale != 4 {
        return Err(AppError::constraint("scale must be 2 or 4"));
    }
    let face_enhance = params.get("face_enhance").and_then(Value::as_bool).unwrap_or(false);

    let input_pixels = u64::from(info.width) * u64::from(info.height);
    if input_pixels > 4_000_000 {
        return Err(AppError::new(
            crate::error::ErrorCode::InputTooLarge,
            "enhance_upscale input exceeds the 4MP cap",
            false,
        ));
    }
    if input_pixels * scale * scale > 16_000_000 {
        return Err(AppError::new(
            crate::error::ErrorCode::InputTooLarge,
            "enhance_upscale output would exceed the 16MP cap",
            false,
        ));
    }

    let body = json!({
        "input": {
            "image": http::data_url(mime, &bytes),
            "scale": scale,
            "face_enhance": face_enhance,
        }
    });
    let create = http::client()
        .post("https://api.replicate.com/v1/predictions")
        .bearer_auth(&key)
        .timeout(Duration::from_secs(60))
        .json(&body)
        .send()
        .await?;
    let mut prediction = http::json_or_error(create).await?;
    let get_url = prediction
        .pointer("/urls/get")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| AppError::provider("Replicate returned no polling URL", true))?;

    let deadline = tokio::time::Instant::now() + provider_timeout(config);
    while matches!(
        prediction.get("status").and_then(Value::as_str),
        Some("starting") | Some("processing")
    ) {
        if tokio::time::Instant::now() >= deadline {
            if let Some(cancel) = prediction.pointer("/urls/cancel").and_then(Value::as_str) {
                let _ = http::client().post(cancel).bearer_auth(&key).send().await;
            }
            return Err(AppError::timeout("Replicate upscale did not finish in time"));
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        let poll = http::client()
            .get(&get_url)
            .bearer_auth(&key)
            .timeout(Duration::from_secs(30))
            .send()
            .await?;
        prediction = http::json_or_error(poll).await?;
    }

    if prediction.get("status").and_then(Value::as_str) != Some("succeeded") {
        let detail = prediction
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("prediction did not succeed");
        return Err(AppError::provider(detail, true));
    }
    let output = prediction.get("output").cloned().unwrap_or(Value::Null);
    let first = match output {
        Value::Array(items) => items.first().cloned().unwrap_or(Value::Null),
        other => other,
    };
    let url = first
        .as_str()
        .ok_or_else(|| AppError::provider("Replicate returned no output URL", true))?;
    Ok(OpOutput::Image {
        bytes: http::download_image(url, provider_timeout(config)).await?,
        model: "nightmareai/real-esrgan".into(),
        revised_prompt: None,
        metadata: json!({ "input": input, "scale": scale, "face_enhance": face_enhance }),
    })
}
