use crate::capabilities::{self, OpOutput, OpRequest};
use crate::config::Config;
use crate::error::AppError;
use crate::output;
use crate::presets;
use crate::providers::{self, Size};
use crate::runs;
use crate::task::{self, Constraints, TaskRequest};
use crate::util;
use serde_json::{json, Map, Value};
use std::sync::Arc;

/// JSON-RPC 2.0 over newline-delimited stdio, implemented against the MCP
/// revision 2025-06-18 method set (initialize, tools/list, tools/call).
pub async fn serve_stdio(config: Arc<Config>) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();

    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Value>(&line) {
            Ok(request) => handle(&config, request).await,
            Err(err) => Some(error_response(Value::Null, -32700, &format!("parse error: {err}"))),
        };
        if let Some(payload) = response {
            let mut stdout = tokio::io::stdout();
            let _ = stdout.write_all(format!("{payload}\n").as_bytes()).await;
            let _ = stdout.flush().await;
        }
    }
}

fn error_response(id: Value, code: i32, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

pub async fn handle(config: &Config, request: Value) -> Option<Value> {
    let id = request.get("id").cloned();
    let method = request.get("method").and_then(Value::as_str).unwrap_or_default();
    let params = request.get("params").cloned().unwrap_or(Value::Null);

    // Notifications (no id) never get a reply.
    let id = id?;

    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": "2025-06-18",
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": "shenbi", "version": env!("CARGO_PKG_VERSION") },
            "instructions": "Multi-provider image generation, editing, compositing and goal-shaped image tasks. Configure providers in the local UI.",
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_definitions() })),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
            let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            match call_tool(config, &name, &args).await {
                Ok(text) => Ok(json!({ "content": [{ "type": "text", "text": text }] })),
                Err(err) => Ok(json!({
                    "isError": true,
                    "content": [{ "type": "text", "text": json!({
                        "success": false,
                        "error": err.to_json()
                    }).to_string() }]
                })),
            }
        }
        other => Err((-32601, format!("method not found: {other}"))),
    };

    Some(match result {
        Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
        Err((code, message)) => error_response(id, code, &message),
    })
}

fn tool_definitions() -> Vec<Value> {
    let provider_enum = json!(crate::config::GENERATE_PROVIDERS);
    let output_path = json!({ "type": "string", "description": "Exact output file path (must end in .png)" });
    let output_dir = json!({ "type": "string", "description": "Output directory (filename auto-generated)" });

    vec![
        json!({
            "name": "generate_image",
            "description": "Generate an image from a text prompt. Supports style modifiers, size presets and a reference image that anchors geometry and lighting (routes through the OpenAI edit model).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "prompt": { "type": "string", "description": "Text description of the image" },
                    "provider": { "type": "string", "enum": provider_enum },
                    "model": { "type": "string", "description": "Provider-specific model override" },
                    "size": { "type": "string", "enum": ["square", "landscape", "portrait"] },
                    "style": { "type": "string", "description": "Style modifier prepended to the prompt" },
                    "reference_image": { "type": "string", "description": "Absolute path to a PNG/JPEG/WebP that anchors the composition" },
                    "outputPath": output_path,
                    "outputDir": output_dir
                },
                "required": ["prompt"]
            }
        }),
        json!({
            "name": "generate_batch",
            "description": "Generate several images in one call. Shared provider, style and size; per-item failures are isolated and reported inline.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "items": {
                        "type": "array", "minItems": 1, "maxItems": 50,
                        "items": { "type": "object", "properties": {
                            "prompt": { "type": "string" },
                            "outputPath": { "type": "string" }
                        }, "required": ["prompt"] }
                    },
                    "provider": { "type": "string", "enum": provider_enum },
                    "style": { "type": "string" },
                    "size": { "type": "string", "enum": ["square", "landscape", "portrait"] },
                    "reference_image": { "type": "string" },
                    "max_concurrent": { "type": "integer", "minimum": 1, "maximum": 8 },
                    "outputDir": output_dir
                },
                "required": ["items"]
            }
        }),
        json!({
            "name": "process_image",
            "description": "Process an existing image with resize, crop, aspect crop and circle mask operations, applied in order.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "inputPath": { "type": "string" },
                    "operations": { "type": "array", "items": { "type": "object" } },
                    "outputPath": output_path
                },
                "required": ["inputPath", "operations"]
            }
        }),
        json!({
            "name": "generate_asset",
            "description": "Generate an image and post-process it into a ready-to-use asset: profile_pic, post_image, hero_photo, avatar, scene, avery_8293, avery_5160, avery_22830, avery_8164.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "prompt": { "type": "string" },
                    "assetType": { "type": "string", "enum": presets::PRESETS.iter().map(|p| p.id).collect::<Vec<_>>() },
                    "provider": { "type": "string", "enum": provider_enum },
                    "model": { "type": "string" },
                    "style": { "type": "string" },
                    "assetId": { "type": "string", "description": "Clean output filename base (a-zA-Z0-9_-)" },
                    "outputPath": output_path,
                    "outputDir": output_dir
                },
                "required": ["prompt", "assetType"]
            }
        }),
        json!({
            "name": "image_op",
            "description": "Invoke one registered capability directly. Image ops save a PNG; analyze ops return data. Use list_capabilities to discover (op, provider) pairs.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "op": { "type": "string", "enum": [
                        "extract_subject", "edit_prompt", "composite_layers", "transform",
                        "enhance_upscale", "analyze_dimensions", "analyze_palette", "generate"
                    ] },
                    "provider": { "type": "string" },
                    "params": { "type": "object" },
                    "outputPath": output_path,
                    "outputDir": output_dir
                },
                "required": ["op", "provider"]
            }
        }),
        json!({
            "name": "image_task",
            "description": "Hand off a natural-language image goal. The server plans a DAG of capabilities (template fast path or Claude planner), validates it and executes it, returning the final image plus a trace.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "goal": { "type": "string" },
                    "input_images": { "type": "array", "items": { "type": "string" } },
                    "constraints": { "type": "object", "properties": {
                        "output_size": { "type": "string", "enum": ["square", "landscape", "portrait"] },
                        "quality_tier": { "type": "string", "enum": ["fast", "balanced", "best"] },
                        "budget_cap_usd": { "type": "number" },
                        "latency_cap_seconds": { "type": "number" },
                        "style_refs": { "type": "array", "items": { "type": "string" } }
                    } },
                    "dry_run": { "type": "boolean" },
                    "outputPath": output_path,
                    "outputDir": output_dir
                },
                "required": ["goal"]
            }
        }),
        json!({
            "name": "list_capabilities",
            "description": "List the capability (op, provider) pairs that are usable with the current configuration, with cost and input requirements.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
    ]
}

fn text_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|v| !v.trim().is_empty())
}

fn size_arg(args: &Value) -> Result<Size, AppError> {
    Size::parse(args.get("size").and_then(Value::as_str))
}

pub async fn call_tool(config: &Config, name: &str, args: &Value) -> Result<String, AppError> {
    match name {
        "generate_image" => generate_image(config, args).await,
        "generate_batch" => generate_batch(config, args).await,
        "process_image" => process_image(config, args).await,
        "generate_asset" => generate_asset(config, args).await,
        "image_op" => image_op(config, args).await,
        "image_task" => image_task(config, args).await,
        "list_capabilities" => {
            let pairs = capabilities::registered_pairs(config).await;
            Ok(json!({
                "capabilities": pairs,
                "config": {
                    "defaultProvider": config.default_provider,
                    "configuredProviders": config.available_generate_providers(),
                    "outputDir": config.output_dir_resolved().display().to_string(),
                }
            })
            .to_string())
        }
        other => Err(AppError::constraint(format!("unknown tool '{other}'"))),
    }
}

pub fn resolve_provider(config: &Config, requested: Option<&str>, needs_size: bool) -> Result<(String, bool), AppError> {
    let available = config.available_generate_providers();
    if available.is_empty() {
        return Err(AppError::constraint(
            "no image providers are configured; add an API key in the config UI",
        ));
    }
    let default = if config.is_configured(&config.default_provider) && config.default_provider != "anthropic" {
        config.default_provider.clone()
    } else {
        available[0].to_string()
    };

    match requested {
        Some(name) if !name.is_empty() => {
            if !available.contains(&name) {
                return Err(AppError::constraint(format!(
                    "provider '{name}' is not configured; available: {}",
                    available.join(", ")
                )));
            }
            let dropped = needs_size && !providers::supports_size(name);
            Ok((name.to_string(), dropped))
        }
        _ => {
            if needs_size && !providers::supports_size(&default) {
                if let Some(fallback) = available.iter().find(|p| providers::supports_size(p)) {
                    return Ok((fallback.to_string(), false));
                }
            }
            Ok((default, false))
        }
    }
}

pub fn effective_prompt(prompt: &str, style: Option<&str>) -> String {
    match style.filter(|s| !s.trim().is_empty()) {
        Some(style) => format!("{style}, {prompt}"),
        None => prompt.to_string(),
    }
}

async fn generate_image(config: &Config, args: &Value) -> Result<String, AppError> {
    let prompt = text_arg(args, "prompt").ok_or_else(|| AppError::constraint("prompt is required"))?;
    let style = text_arg(args, "style");
    let effective = effective_prompt(&prompt, style.as_deref());
    let output_path = text_arg(args, "outputPath");
    let output_dir = text_arg(args, "outputDir");

    // Reference-image branch: anchor through the OpenAI edit model.
    if let Some(reference) = text_arg(args, "reference_image") {
        output::assert_within_input_root(config, &reference)
            .await
            .map_err(|err| AppError::constraint(err.to_string()))?;
        let bytes = output::read_input(&reference)?;
        let info = crate::imaging::info(&bytes)?;
        let mime = util::mime_for_format(Some(&info.format))
            .ok_or_else(|| AppError::unsupported("reference_image must be PNG, JPEG or WebP"))?;
        let model = config
            .provider("openai")
            .map(|p| p.edit_model.clone())
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| "gpt-image-1.5".into());
        let image = providers::openai_edit(config, &model, &effective, size_arg(args)?, mime, &bytes, 32_000).await?;
        let path = output::resolve(
            config,
            &output::SaveRequest {
                output_path: output_path.as_deref(),
                output_dir: output_dir.as_deref(),
                asset_id: None,
                prompt: &prompt,
                provider: "openai",
            },
        )
        .map_err(|err| AppError::constraint(err.to_string()))?;
        let path = output::save(&image.bytes, &path)
            .await
            .map_err(|err| AppError::provider(err.to_string(), false))?;
        return Ok(json!({
            "success": true,
            "path": path.display().to_string(),
            "provider": "openai",
            "model": image.model,
            "routedVia": "edit_prompt",
            "referenceImage": reference,
            "revisedPrompt": image.revised_prompt,
        })
        .to_string());
    }

    let requested = text_arg(args, "provider");
    let (provider, size_dropped) = resolve_provider(config, requested.as_deref(), args.get("size").is_some())?;
    let size = size_arg(args)?;
    let model = text_arg(args, "model");
    let image = providers::generate(config, &provider, &effective, size, model.as_deref()).await?;
    let path = output::resolve(
        config,
        &output::SaveRequest {
            output_path: output_path.as_deref(),
            output_dir: output_dir.as_deref(),
            asset_id: None,
            prompt: &prompt,
            provider: &provider,
        },
    )
    .map_err(|err| AppError::constraint(err.to_string()))?;
    let path = output::save(&image.bytes, &path)
        .await
        .map_err(|err| AppError::provider(err.to_string(), false))?;

    let mut body = json!({
        "success": true,
        "path": path.display().to_string(),
        "provider": provider,
        "model": image.model,
        "revisedPrompt": image.revised_prompt,
    });
    if size_dropped {
        body["warning"] = json!(format!("provider '{provider}' ignores the size parameter"));
    }
    Ok(body.to_string())
}

async fn generate_batch(config: &Config, args: &Value) -> Result<String, AppError> {
    let items = args
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| AppError::constraint("items must be an array"))?
        .clone();
    if items.is_empty() || items.len() > 50 {
        return Err(AppError::constraint("items must contain 1-50 entries"));
    }
    let style = text_arg(args, "style");
    let reference = text_arg(args, "reference_image");
    let requested = text_arg(args, "provider");
    let output_dir = text_arg(args, "outputDir");
    let (provider, size_dropped) = resolve_provider(config, requested.as_deref(), args.get("size").is_some())?;
    let size = size_arg(args)?;
    let concurrency = args
        .get("max_concurrent")
        .and_then(Value::as_u64)
        .unwrap_or(config.max_concurrent.max(1) as u64)
        .clamp(1, 8) as usize;

    let run_id = util::new_run_id();
    let mut results: Vec<Value> = Vec::with_capacity(items.len());
    let started = util::unix_ms();

    for chunk in items.chunks(concurrency) {
        let mut handles = Vec::new();
        for item in chunk {
            let prompt = item.get("prompt").and_then(Value::as_str).unwrap_or_default().to_string();
            let effective = effective_prompt(&prompt, style.as_deref());
            let item_output = item.get("outputPath").and_then(Value::as_str).map(str::to_string);
            let provider = provider.clone();
            let reference = reference.clone();
            let output_dir = output_dir.clone();
            let config = config.clone();
            handles.push(tokio::spawn(async move {
                let started = util::unix_ms();
                let outcome = batch_item(
                    &config,
                    &provider,
                    &prompt,
                    &effective,
                    size,
                    reference.as_deref(),
                    item_output.as_deref(),
                    output_dir.as_deref(),
                )
                .await;
                (prompt, util::unix_ms() - started, outcome)
            }));
        }
        for handle in handles {
            let (prompt, duration, outcome) = handle
                .await
                .map_err(|err| AppError::provider(err.to_string(), false))?;
            let index = results.len();
            let duration_ms = duration as u64;
            match outcome {
                Ok(payload) => {
                    let mut map: Map<String, Value> = payload.as_object().cloned().unwrap_or_default();
                    map.insert("index".into(), index.into());
                    map.insert("success".into(), true.into());
                    map.insert("prompt".into(), prompt.into());
                    map.insert("durationMs".into(), duration_ms.into());
                    results.push(Value::Object(map));
                }
                Err(err) => {
                    results.push(json!({
                        "index": index,
                        "success": false,
                        "prompt": prompt,
                        "provider": provider,
                        "durationMs": duration_ms,
                        "error": err.to_json(),
                    }));
                }
            }
        }
    }

    let succeeded = results.iter().filter(|r| r["success"] == json!(true)).count();
    let failed = results.len() - succeeded;
    let status = if failed == 0 {
        "success"
    } else if succeeded == 0 {
        "error"
    } else {
        "partial"
    };
    let mut body = json!({
        "batchRunId": run_id,
        "status": status,
        "summary": { "total": results.len(), "succeeded": succeeded, "failed": failed },
        "elapsedMs": util::unix_ms() - started,
        "items": results,
    });
    if size_dropped {
        body["warning"] = json!(format!("provider '{provider}' ignores the size parameter"));
    }
    Ok(body.to_string())
}

async fn batch_item(
    config: &Config,
    provider: &str,
    prompt: &str,
    effective: &str,
    size: Size,
    reference: Option<&str>,
    item_output: Option<&str>,
    output_dir: Option<&str>,
) -> Result<Value, AppError> {
    if let Some(reference) = reference {
        let bytes = output::read_input(reference)?;
        let info = crate::imaging::info(&bytes)?;
        let mime = util::mime_for_format(Some(&info.format))
            .ok_or_else(|| AppError::unsupported("reference_image must be PNG, JPEG or WebP"))?;
        let model = config
            .provider("openai")
            .map(|p| p.edit_model.clone())
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| "gpt-image-1.5".into());
        let image = providers::openai_edit(config, &model, effective, size, mime, &bytes, 32_000).await?;
        let path = output::resolve(
            config,
            &output::SaveRequest {
                output_path: item_output,
                output_dir,
                asset_id: None,
                prompt,
                provider: "openai",
            },
        )
        .map_err(|err| AppError::constraint(err.to_string()))?;
        let path = output::save(&image.bytes, &path)
            .await
            .map_err(|err| AppError::provider(err.to_string(), false))?;
        return Ok(json!({
            "path": path.display().to_string(),
            "provider": "openai",
            "model": image.model,
            "routedVia": "edit_prompt"
        }));
    }

    let image = providers::generate(config, provider, effective, size, None).await?;
    let path = output::resolve(
        config,
        &output::SaveRequest {
            output_path: item_output,
            output_dir,
            asset_id: None,
            prompt,
            provider,
        },
    )
    .map_err(|err| AppError::constraint(err.to_string()))?;
    let path = output::save(&image.bytes, &path)
        .await
        .map_err(|err| AppError::provider(err.to_string(), false))?;
    Ok(json!({
        "path": path.display().to_string(),
        "provider": provider,
        "model": image.model,
        "revisedPrompt": image.revised_prompt
    }))
}

async fn process_image(config: &Config, args: &Value) -> Result<String, AppError> {
    let input = text_arg(args, "inputPath").ok_or_else(|| AppError::constraint("inputPath is required"))?;
    output::assert_within_input_root(config, &input)
        .await
        .map_err(|err| AppError::constraint(err.to_string()))?;
    let operations: Vec<crate::imaging::Operation> = serde_json::from_value(
        args.get("operations")
            .cloned()
            .ok_or_else(|| AppError::constraint("operations is required"))?,
    )
    .map_err(|err| AppError::constraint(format!("invalid operations: {err}")))?;
    let bytes = output::read_input(&input)?;
    let processed = crate::imaging::apply(&bytes, &operations)?;

    let target = match text_arg(args, "outputPath") {
        Some(path) if !path.to_ascii_lowercase().ends_with(".png") => {
            return Err(AppError::constraint("outputPath must end with .png"))
        }
        Some(path) => util::expand_tilde(&path),
        None => {
            let mut path = util::expand_tilde(&input);
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("image").to_string();
            path.set_file_name(format!("{stem}_processed.png"));
            path
        }
    };
    let path = output::save(&processed.bytes, &target)
        .await
        .map_err(|err| AppError::provider(err.to_string(), false))?;
    Ok(json!({
        "success": true,
        "inputPath": input,
        "outputPath": path.display().to_string(),
        "originalSize": { "width": processed.original.width, "height": processed.original.height },
        "outputSize": { "width": processed.output.width, "height": processed.output.height },
        "operationsApplied": processed.applied,
    })
    .to_string())
}

async fn generate_asset(config: &Config, args: &Value) -> Result<String, AppError> {
    let prompt = text_arg(args, "prompt").ok_or_else(|| AppError::constraint("prompt is required"))?;
    let asset_type = text_arg(args, "assetType").ok_or_else(|| AppError::constraint("assetType is required"))?;
    let preset = presets::find(&asset_type).ok_or_else(|| {
        AppError::constraint(format!("unknown assetType '{asset_type}'"))
    })?;
    let style = text_arg(args, "style");
    let effective = effective_prompt(&prompt, style.as_deref());
    let requested = text_arg(args, "provider");
    let (provider, size_dropped) = resolve_provider(config, requested.as_deref(), true)?;
    let size = Size::parse(Some(preset.size))?;
    let model = text_arg(args, "model");
    let image = providers::generate(config, &provider, &effective, size, model.as_deref()).await?;

    let output_path = text_arg(args, "outputPath");
    let output_dir = text_arg(args, "outputDir");
    let asset_id = text_arg(args, "assetId");

    let processed = crate::imaging::apply(&image.bytes, &presets::operations(preset.id));
    let (bytes, size_info, applied) = match processed {
        Ok(processed) => (processed.bytes, processed.output, processed.applied),
        Err(err) => {
            // Keep the raw render rather than losing a paid generation.
            let info = crate::imaging::info(&image.bytes)?;
            let path = output::resolve(
                config,
                &output::SaveRequest {
                    output_path: output_path.as_deref(),
                    output_dir: output_dir.as_deref(),
                    asset_id: asset_id.as_deref(),
                    prompt: &prompt,
                    provider: &provider,
                },
            )
            .map_err(|e| AppError::constraint(e.to_string()))?;
            let raw_name = format!(
                "{}{}",
                path.file_stem().and_then(|s| s.to_str()).unwrap_or("asset"),
                "_raw.png"
            );
            let raw_path = path.with_file_name(raw_name);
            let saved = output::save(&image.bytes, &raw_path)
                .await
                .map_err(|e| AppError::provider(e.to_string(), false))?;
            return Ok(json!({
                "success": true,
                "path": saved.display().to_string(),
                "provider": provider,
                "model": image.model,
                "assetType": asset_type,
                "outputSize": { "width": info.width, "height": info.height },
                "warning": format!("post-processing failed, saved raw image: {}", err.message),
            })
            .to_string());
        }
    };

    let path = output::resolve(
        config,
        &output::SaveRequest {
            output_path: output_path.as_deref(),
            output_dir: output_dir.as_deref(),
            asset_id: asset_id.as_deref(),
            prompt: &prompt,
            provider: &provider,
        },
    )
    .map_err(|e| AppError::constraint(e.to_string()))?;
    let path = output::save(&bytes, &path)
        .await
        .map_err(|e| AppError::provider(e.to_string(), false))?;

    let mut body = json!({
        "success": true,
        "path": path.display().to_string(),
        "provider": provider,
        "model": image.model,
        "assetType": asset_type,
        "outputSize": { "width": size_info.width, "height": size_info.height },
        "operationsApplied": applied,
    });
    if size_dropped {
        body["warning"] = json!(format!("provider '{provider}' ignores the size parameter"));
    }
    Ok(body.to_string())
}

async fn image_op(config: &Config, args: &Value) -> Result<String, AppError> {
    let op = text_arg(args, "op").ok_or_else(|| AppError::constraint("op is required"))?;
    let provider = text_arg(args, "provider").ok_or_else(|| AppError::constraint("provider is required"))?;
    let params = args.get("params").cloned().unwrap_or_else(|| json!({}));
    let output_path = text_arg(args, "outputPath");
    let output_dir = text_arg(args, "outputDir");

    let run_id = util::new_run_id();
    let run_dir = runs::run_dir(&config.output_dir_resolved(), &run_id).ok();

    let outcome = capabilities::invoke(OpRequest { config, op: &op, provider: &provider, params: &params }).await;
    match outcome {
        Ok(OpOutput::Image { bytes, model, revised_prompt, metadata }) => {
            if let Some(dir) = &run_dir {
                let artifact = runs::artifact_path(dir, "1");
                let _ = tokio::fs::create_dir_all(dir).await;
                let _ = util::write_atomic(&artifact, &bytes).await;
            }
            let path = output::resolve(
                config,
                &output::SaveRequest {
                    output_path: output_path.as_deref(),
                    output_dir: output_dir.as_deref(),
                    asset_id: None,
                    prompt: &format!("{op}-{provider}"),
                    provider: &provider,
                },
            )
            .map_err(|err| AppError::constraint(err.to_string()))?;
            let path = output::save(&bytes, &path)
                .await
                .map_err(|err| AppError::provider(err.to_string(), false))?;
            Ok(json!({
                "success": true,
                "output": path.display().to_string(),
                "runId": run_id,
                "trace": {
                    "runId": run_id,
                    "nodes": [{
                        "id": "n1", "op": op, "provider": provider, "model": model,
                        "status": "success", "output_path": path.display().to_string(),
                        "metadata": metadata, "revisedPrompt": revised_prompt,
                        "latency_ms": 0, "cost_usd": 0
                    }]
                }
            })
            .to_string())
        }
        Ok(OpOutput::Data { data, model, metadata }) => Ok(json!({
            "success": true,
            "data": data,
            "runId": run_id,
            "trace": {
                "runId": run_id,
                "nodes": [{
                    "id": "n1", "op": op, "provider": provider, "model": model,
                    "status": "success", "data": data, "metadata": metadata,
                    "latency_ms": 0, "cost_usd": 0
                }]
            }
        })
        .to_string()),
        Err(err) => Err(err),
    }
}

async fn image_task(config: &Config, args: &Value) -> Result<String, AppError> {
    let goal = text_arg(args, "goal").ok_or_else(|| AppError::constraint("goal is required"))?;
    let input_images: Vec<String> = args
        .get("input_images")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    let constraints: Constraints = args
        .get("constraints")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|err| AppError::constraint(format!("invalid constraints: {err}")))?
        .unwrap_or_default();

    let result = task::execute(TaskRequest {
        config,
        goal: &goal,
        input_images,
        constraints,
        dry_run: args.get("dry_run").and_then(Value::as_bool).unwrap_or(false),
        output_path: text_arg(args, "outputPath").as_deref(),
        output_dir: text_arg(args, "outputDir").as_deref(),
    })
    .await?;

    let plan = result.plan.as_ref().map(task::plan_to_json).unwrap_or(Value::Null);
    let trace: Vec<Value> = result
        .nodes
        .iter()
        .map(|node| {
            json!({
                "id": format!("n{}", node.id),
                "op": node.op,
                "provider": node.provider,
                "model": node.model,
                "status": node.status,
                "output_path": node.output,
                "data": node.data,
                "cost_usd": node.cost_usd,
                "latency_ms": node.latency_ms,
                "revisedPrompt": node.revised_prompt,
                "error": node.error,
            })
        })
        .collect();
    let total_cost: f64 = result.nodes.iter().map(|n| n.cost_usd).sum();
    let total_latency: u128 = result.nodes.iter().map(|n| n.latency_ms).sum();
    let terminal = result
        .plan
        .as_ref()
        .and_then(|plan| result.outputs.get(&plan.terminal_node_id))
        .and_then(|output| match output {
            task::NodeOutput::Image(path) => Some(path.display().to_string()),
            _ => None,
        });
    let failed = result.nodes.iter().any(|n| n.status == "error");

    Ok(json!({
        "success": !failed,
        "output": terminal.map(|path| json!({ "path": path, "mimeType": "image/png" })),
        "runId": result.run_id,
        "plannerMethod": result.planner_method,
        "dry_run": result.dry_run,
        "total_cost_usd": total_cost,
        "total_latency_ms": total_latency,
        "plan": plan,
        "trace": trace,
        "skips": result.skips,
    })
    .to_string())
}

