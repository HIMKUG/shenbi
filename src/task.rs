use crate::capabilities::{self, OpOutput, OpRequest};
use crate::config::Config;
use crate::error::AppError;
use crate::http;
use crate::runs::{self, NodeRecord};
use crate::util;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Constraints {
    #[serde(default)]
    pub output_size: Option<String>,
    #[serde(default)]
    pub quality_tier: Option<String>,
    #[serde(default)]
    pub budget_cap_usd: Option<f64>,
    #[serde(default)]
    pub latency_cap_seconds: Option<f64>,
    #[serde(default)]
    pub style_refs: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PlanNode {
    pub id: String,
    pub op: String,
    pub provider: String,
    #[serde(default)]
    pub params: Value,
    #[serde(default, rename = "dependsOn")]
    pub depends_on: Vec<String>,
    #[serde(rename = "outputKind")]
    pub output_kind: String,
    #[serde(default, rename = "costUsd")]
    pub cost_usd: f64,
    #[serde(default, rename = "latencyMs")]
    pub latency_ms: f64,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Plan {
    pub version: u32,
    pub goal: String,
    pub nodes: Vec<PlanNode>,
    #[serde(rename = "terminalNodeId")]
    pub terminal_node_id: String,
    #[serde(rename = "estimatedTotalCostUsd", default)]
    pub estimated_cost: f64,
    #[serde(rename = "estimatedTotalLatencyMs", default)]
    pub estimated_latency: f64,
}

pub struct TaskRequest<'a> {
    pub config: &'a Config,
    pub goal: &'a str,
    pub input_images: Vec<String>,
    pub constraints: Constraints,
    pub dry_run: bool,
    pub output_path: Option<&'a str>,
    pub output_dir: Option<&'a str>,
}

#[derive(Default)]
pub struct TaskResult {
    pub plan: Option<Plan>,
    pub planner_method: String,
    pub nodes: Vec<TraceNode>,
    pub skips: Vec<Value>,
    pub outputs: HashMap<String, NodeOutput>,
    pub run_id: String,
    pub dry_run: bool,
}

pub struct TraceNode {
    pub id: String,
    pub op: String,
    pub provider: String,
    pub model: Option<String>,
    pub status: &'static str,
    pub output: Option<String>,
    pub data: Option<Value>,
    pub cost_usd: f64,
    pub latency_ms: u128,
    pub revised_prompt: Option<String>,
    pub error: Option<Value>,
}

#[derive(Clone)]
pub enum NodeOutput {
    Image(PathBuf),
    Data(Value),
}

pub fn input_refs(inputs: &[String]) -> HashMap<String, String> {
    inputs
        .iter()
        .enumerate()
        .map(|(index, path)| (format!("image_{index}"), path.clone()))
        .collect()
}

// ---------------------------------------------------------------- templates

fn first_input(refs: &HashMap<String, String>) -> Option<String> {
    refs.keys().next().map(|key| format!("$inputs.{key}"))
}

pub fn match_template(goal: &str, refs: &HashMap<String, String>, _constraints: &Constraints) -> Option<Plan> {
    let key = goal.trim().to_lowercase().replace([' ', '_'], "-");
    let canonical = key.replace('-', "_");
    let input = first_input(refs)?;

    if let Some(preset) = crate::presets::find(&canonical) {
        let ops = crate::presets::operations(preset.id);
        return Some(Plan {
            version: 1,
            goal: goal.to_string(),
            nodes: vec![PlanNode {
                id: "transform".into(),
                op: "transform".into(),
                provider: "sharp".into(),
                params: json!({ "input": input, "operations": ops }),
                depends_on: vec![],
                output_kind: "image".into(),
                cost_usd: 0.0,
                latency_ms: 200.0,
                reason: Some(format!("template:{}", preset.id)),
            }],
            terminal_node_id: "transform".into(),
            estimated_cost: 0.0,
            estimated_latency: 200.0,
        });
    }

    match key.as_str() {
        "logo-cleanup" => Some(Plan {
            version: 1,
            goal: goal.to_string(),
            nodes: vec![PlanNode {
                id: "extract".into(),
                op: "edit_prompt".into(),
                provider: "openai".into(),
                params: json!({ "input": input, "prompt": "isolate the subject on a transparent background, keep edges clean" }),
                depends_on: vec![],
                output_kind: "image".into(),
                cost_usd: 0.04,
                latency_ms: 12_000.0,
                reason: Some("template:logo-cleanup".into()),
            }],
            terminal_node_id: "extract".into(),
            estimated_cost: 0.04,
            estimated_latency: 12_000.0,
        }),
        "upscale-export" => Some(Plan {
            version: 1,
            goal: goal.to_string(),
            nodes: vec![PlanNode {
                id: "upscale".into(),
                op: "enhance_upscale".into(),
                provider: "replicate".into(),
                params: json!({ "input": input, "scale": 2 }),
                depends_on: vec![],
                output_kind: "image".into(),
                cost_usd: 0.01,
                latency_ms: 8_000.0,
                reason: Some("template:upscale-export".into()),
            }],
            terminal_node_id: "upscale".into(),
            estimated_cost: 0.01,
            estimated_latency: 8_000.0,
        }),
        _ => None,
    }
}

// ------------------------------------------------------------------ planner

const PLANNER_SYSTEM: &str = "You are the image_task planner for an MCP image pipeline.\n\
Return only a single JSON object matching this shape:\n\
{\"version\":1,\"goal\":\"...\",\"nodes\":[{\"id\":\"node_id\",\"op\":\"generate\",\"provider\":\"openai\",\"params\":{\"prompt\":\"...\"},\"dependsOn\":[],\"outputKind\":\"image\",\"costUsd\":0.04,\"latencyMs\":12000,\"reason\":\"...\"}],\"terminalNodeId\":\"node_id\",\"estimatedTotalCostUsd\":0.04,\"estimatedTotalLatencyMs\":12000}\n\
Use only $inputs.<name> or $nodes.<id>.output references. Choose only registered capabilities from the snapshot.\n\
Required params: generate {prompt}; edit_prompt {input, prompt}; extract_subject {input}; transform {input, operations}; composite_layers {canvas:{width,height,background?}, layers:[{input,x?,y?,scale?,opacity?,anchor?}]}; enhance_upscale {input, scale?}; analyze_* {input}.\n\
Prefer local sharp capabilities when they satisfy the goal; generate only when new pixels are needed.";

async fn plan_with_llm(config: &Config, request: &TaskRequest<'_>, refs: &HashMap<String, String>) -> Result<Plan, AppError> {
    let api_key = config
        .provider("anthropic")
        .map(|p| p.api_key.trim().to_string())
        .filter(|k| !k.is_empty())
        .ok_or_else(|| {
            AppError::constraint("image_task needs an Anthropic API key for planning (or use a template goal)")
                .with_suggestion("Add the key under Providers → Anthropic in the config UI.")
        })?;
    let model = config
        .provider("anthropic")
        .map(|p| p.model.clone())
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| "claude-haiku-4-5".into());
    let base = config
        .provider("anthropic")
        .map(|p| p.base_url.trim().trim_end_matches('/').to_string())
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| "https://api.anthropic.com".into());
    let capabilities = capabilities::registered_pairs(config).await;
    let body = json!({
        "model": model,
        "max_tokens": 2048,
        "system": format!("{PLANNER_SYSTEM}\nCapability snapshot:\n{}", serde_json::to_string(&capabilities).unwrap_or_default()),
        "messages": [{
            "role": "user",
            "content": json!({
                "goal": request.goal,
                "input_images": refs.keys().collect::<Vec<_>>(),
                "constraints": request.constraints,
            }).to_string()
        }]
    });
    let response = http::client()
        .post(format!("{base}/v1/messages"))
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .timeout(std::time::Duration::from_secs(60))
        .json(&body)
        .send()
        .await?;
    let value = http::json_or_error(response).await?;
    let text = value
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    parse_plan_json(&text)
}

fn parse_plan_json(text: &str) -> Result<Plan, AppError> {
    let trimmed = text.trim();
    let candidate = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .and_then(|rest| rest.strip_suffix("```"))
        .map(str::trim)
        .unwrap_or(trimmed);
    let json_slice = if candidate.starts_with('{') {
        candidate.to_string()
    } else {
        match (candidate.find('{'), candidate.rfind('}')) {
            (Some(start), Some(end)) if end > start => candidate[start..=end].to_string(),
            _ => {
                return Err(AppError::provider("planner response contained no JSON object", false))
            }
        }
    };
    let plan: Plan = serde_json::from_str(&json_slice)
        .map_err(|err| AppError::provider(format!("planner returned an invalid plan: {err}"), false))?;
    if plan.nodes.is_empty() {
        return Err(AppError::provider("planner returned an empty plan", false));
    }
    Ok(plan)
}

pub fn validate_plan(plan: &Plan, refs: &HashMap<String, String>) -> Result<f64, AppError> {
    let ids: HashSet<&str> = plan.nodes.iter().map(|n| n.id.as_str()).collect();
    if ids.len() != plan.nodes.len() {
        return Err(AppError::constraint("plan contains duplicate node ids"));
    }
    if !ids.contains(plan.terminal_node_id.as_str()) {
        return Err(AppError::constraint("terminalNodeId does not exist in the plan"));
    }
    let mut cost = 0.0;
    for node in &plan.nodes {
        for dep in &node.depends_on {
            if !ids.contains(dep.as_str()) {
                return Err(AppError::constraint(format!(
                    "node '{}' depends on unknown node '{dep}'",
                    node.id
                )));
            }
        }
        let expected = if node.op.starts_with("analyze_") { "data" } else { "image" };
        if node.output_kind != expected {
            return Err(AppError::constraint(format!(
                "node '{}' with op '{}' must declare outputKind '{expected}'",
                node.id, node.op
            )));
        }
        if node.op != "analyze_dimensions" && node.op != "analyze_palette" {
            let input = match node.op.as_str() {
                "composite_layers" => node
                    .params
                    .get("layers")
                    .and_then(Value::as_array)
                    .and_then(|l| l.first())
                    .and_then(|l| l.get("input"))
                    .and_then(Value::as_str),
                "generate" => None,
                _ => node.params.get("input").and_then(Value::as_str),
            };
            if let Some(value) = input {
                if let Some(name) = value.strip_prefix("$inputs.") {
                    if !refs.contains_key(name) {
                        return Err(AppError::constraint(format!(
                            "node '{}' references unknown input '{value}'",
                            node.id
                        )));
                    }
                } else if let Some(rest) = value.strip_prefix("$nodes.") {
                    let node_id = rest.split('.').next().unwrap_or_default();
                    if !ids.contains(node_id) {
                        return Err(AppError::constraint(format!(
                            "node '{}' references unknown node '{value}'",
                            node.id
                        )));
                    }
                }
            }
        }
        cost += node.cost_usd;
    }

    // Cycle check by Kahn's algorithm.
    let mut indegree: HashMap<&str, usize> =
        plan.nodes.iter().map(|n| (n.id.as_str(), n.depends_on.len())).collect();
    let mut queue: Vec<&str> = indegree
        .iter()
        .filter(|(_, degree)| **degree == 0)
        .map(|(id, _)| *id)
        .collect();
    let mut visited = 0;
    while let Some(id) = queue.pop() {
        visited += 1;
        for node in plan.nodes.iter().filter(|n| n.depends_on.iter().any(|d| d == id)) {
            let degree = indegree.get_mut(node.id.as_str()).expect("degree");
            *degree -= 1;
            if *degree == 0 {
                queue.push(node.id.as_str());
            }
        }
    }
    if visited != plan.nodes.len() {
        return Err(AppError::constraint("plan contains a dependency cycle"));
    }
    Ok(cost)
}

/// Longest dependency chain, measured in the planner's per-node latency
/// estimates. This is what a `latency_cap_seconds` gate has to compare
/// against — total latency would reject any plan that runs work in parallel.
fn critical_path_ms(plan: &Plan) -> f64 {
    let by_id: HashMap<&str, &PlanNode> = plan.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    let mut memo: HashMap<&str, f64> = HashMap::new();

    fn walk<'a>(id: &'a str, by_id: &HashMap<&'a str, &'a PlanNode>, memo: &mut HashMap<&'a str, f64>) -> f64 {
        if let Some(value) = memo.get(id) {
            return *value;
        }
        let Some(node) = by_id.get(id) else { return 0.0 };
        let upstream = node
            .depends_on
            .iter()
            .map(|dep| walk(dep, by_id, memo))
            .fold(0.0_f64, f64::max);
        let value = upstream + node.latency_ms;
        memo.insert(id, value);
        value
    }

    plan.nodes
        .iter()
        .map(|node| walk(&node.id, &by_id, &mut memo))
        .fold(0.0_f64, f64::max)
}

// ----------------------------------------------------------------- executor

fn resolve_refs(value: &Value, outputs: &HashMap<String, NodeOutput>, refs: &HashMap<String, String>) -> Result<Value, AppError> {
    match value {
        Value::String(text) => {
            if let Some(name) = text.strip_prefix("$inputs.") {
                return refs
                    .get(name)
                    .map(|path| Value::String(path.clone()))
                    .ok_or_else(|| AppError::constraint(format!("unknown input reference '{text}'")));
            }
            if let Some(rest) = text.strip_prefix("$nodes.") {
                let node_id = rest.trim_end_matches(".output");
                return match outputs.get(node_id) {
                    Some(NodeOutput::Image(path)) => Ok(Value::String(path.display().to_string())),
                    Some(NodeOutput::Data(data)) => Ok(data.clone()),
                    None => Err(AppError::constraint(format!("cannot resolve '{text}'"))),
                };
            }
            Ok(value.clone())
        }
        Value::Array(items) => items
            .iter()
            .map(|item| resolve_refs(item, outputs, refs))
            .collect(),
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (key, nested) in map {
                out.insert(key.clone(), resolve_refs(nested, outputs, refs)?);
            }
            Ok(Value::Object(out))
        }
        other => Ok(other.clone()),
    }
}

pub async fn execute(request: TaskRequest<'_>) -> Result<TaskResult, AppError> {
    let refs = input_refs(&request.input_images);
    let run_id = util::new_run_id();
    let output_dir = request.config.output_dir_resolved();
    let run_dir = runs::run_dir(&output_dir, &run_id)
        .map_err(|err| AppError::constraint(err.to_string()))?;
    tokio::fs::create_dir_all(&run_dir).await.ok();

    let mut result = TaskResult { run_id: run_id.clone(), ..Default::default() };
    result.planner_method = "template".into();

    let plan = match match_template(request.goal, &refs, &request.constraints) {
        Some(plan) => plan,
        None => {
            result.planner_method = "llm".into();
            plan_with_llm(request.config, &request, &refs).await?
        }
    };
    let recomputed = validate_plan(&plan, &refs)?;

    if let Some(cap) = request.constraints.budget_cap_usd {
        if recomputed > cap {
            return Err(AppError::constraint(format!(
                "estimated cost ${recomputed:.4} exceeds the ${cap:.4} budget cap"
            )));
        }
    }
    if let Some(cap) = request.constraints.latency_cap_seconds {
        let path_ms = critical_path_ms(&plan);
        if path_ms > cap * 1000.0 {
            return Err(AppError::constraint(format!(
                "critical-path latency {path_ms:.0}ms exceeds the {:.0}ms latency cap",
                cap * 1000.0
            )));
        }
    }
    let mut outputs: HashMap<String, NodeOutput> = HashMap::new();

    if request.dry_run {
        result.plan = Some(plan);
        result.dry_run = true;
        result.outputs = outputs;
        return Ok(result);
    }

    let mut pending: Vec<&PlanNode> = plan.nodes.iter().collect();
    let mut done: HashSet<String> = HashSet::new();
    let mut failed: HashSet<String> = HashSet::new();

    while !pending.is_empty() {
        let mut progressed = false;
        let mut still_pending = Vec::new();
        for node in pending {
            if node.depends_on.iter().any(|dep| failed.contains(dep)) {
                result.skips.push(json!({ "nodeId": node.id, "reason": "blocked by failed dependency" }));
                failed.insert(node.id.clone());
                progressed = true;
                continue;
            }
            if !node.depends_on.iter().all(|dep| done.contains(dep)) {
                still_pending.push(node);
                continue;
            }
            progressed = true;
            let started = util::unix_ms();
            let resolved = resolve_refs(&node.params, &outputs, &refs)?;
            let outcome = capabilities::invoke(OpRequest {
                config: request.config,
                op: &node.op,
                provider: &node.provider,
                params: &resolved,
            })
            .await;
            let latency = util::unix_ms() - started;

            match outcome {
                Ok(OpOutput::Image { bytes, model, revised_prompt, .. }) => {
                    let artifact = runs::artifact_path(&run_dir, &node.id);
                    util::write_atomic(&artifact, &bytes)
                        .await
                        .map_err(|err| AppError::provider(err.to_string(), false))?;
                    outputs.insert(node.id.clone(), NodeOutput::Image(artifact.clone()));
                    done.insert(node.id.clone());
                    result.nodes.push(TraceNode {
                        id: node.id.clone(),
                        op: node.op.clone(),
                        provider: node.provider.clone(),
                        model: Some(model),
                        status: "success",
                        output: Some(artifact.display().to_string()),
                        data: None,
                        cost_usd: node.cost_usd,
                        latency_ms: latency,
                        revised_prompt,
                        error: None,
                    });
                }
                Ok(OpOutput::Data { data, model, .. }) => {
                    outputs.insert(node.id.clone(), NodeOutput::Data(data.clone()));
                    done.insert(node.id.clone());
                    result.nodes.push(TraceNode {
                        id: node.id.clone(),
                        op: node.op.clone(),
                        provider: node.provider.clone(),
                        model: Some(model),
                        status: "success",
                        output: None,
                        data: Some(data),
                        cost_usd: node.cost_usd,
                        latency_ms: latency,
                        revised_prompt: None,
                        error: None,
                    });
                }
                Err(err) => {
                    failed.insert(node.id.clone());
                    result.nodes.push(TraceNode {
                        id: node.id.clone(),
                        op: node.op.clone(),
                        provider: node.provider.clone(),
                        model: None,
                        status: "error",
                        output: None,
                        data: None,
                        cost_usd: 0.0,
                        latency_ms: latency,
                        revised_prompt: None,
                        error: Some(err.to_json()),
                    });
                }
            }
        }
        if !progressed {
            return Err(AppError::constraint("plan cannot make progress; check dependencies"));
        }
        pending = still_pending;
    }

    // Publish the terminal image with the caller's naming rules.
    if let Some(NodeOutput::Image(path)) = outputs.get(&plan.terminal_node_id).cloned() {
        let bytes = tokio::fs::read(&path).await.map_err(|err| AppError::provider(err.to_string(), false))?;
        let target = crate::output::resolve(
            request.config,
            &crate::output::SaveRequest {
                output_path: request.output_path,
                output_dir: request.output_dir,
                asset_id: None,
                prompt: &plan.goal,
                provider: "image_task",
            },
        )
        .map_err(|err| AppError::constraint(err.to_string()))?;
        crate::output::save(&bytes, &target)
            .await
            .map_err(|err| AppError::provider(err.to_string(), false))?;
        outputs.insert(plan.terminal_node_id.clone(), NodeOutput::Image(target));
        if let Some(node) = result.nodes.iter_mut().find(|n| n.id == plan.terminal_node_id) {
            node.output = outputs.get(&plan.terminal_node_id).and_then(|o| match o {
                NodeOutput::Image(p) => Some(p.display().to_string()),
                _ => None,
            });
        }
    }

    result.plan = Some(plan.clone());
    result.outputs = outputs;

    let manifest = runs::Manifest {
        schema_version: 1,
        run_id: run_id.clone(),
        started_at: util::iso_now(),
        ended_at: Some(util::iso_now()),
        status: if failed.is_empty() { "success".into() } else { "error".into() },
        tool: "image_task".into(),
        invocation: json!({
            "goal": request.goal,
            "inputImages": refs,
            "constraints": request.constraints,
        }),
        nodes: result
            .nodes
            .iter()
            .map(|node| NodeRecord {
                id: node.id.clone(),
                op: node.op.clone(),
                provider: node.provider.clone(),
                model: node.model.clone(),
                artifact_path: node.output.clone(),
                duration_ms: node.latency_ms,
                outcome: node.status.to_string(),
                error: node.error.clone(),
            })
            .collect(),
        final_output: result.nodes.iter().find(|n| n.id == plan.terminal_node_id).and_then(|n| n.output.clone()),
        total_duration_ms: Some(result.nodes.iter().map(|n| n.latency_ms).sum()),
        error: None,
    };
    if let Err(err) = runs::write_manifest(&run_dir, &manifest).await {
        eprintln!("image_task: manifest write failed: {err}");
    }

    Ok(result)
}

pub fn plan_to_json(plan: &Plan) -> Value {
    json!({
        "version": plan.version,
        "goal": plan.goal,
        "terminalNodeId": plan.terminal_node_id,
        "steps": plan.nodes.iter().map(|n| json!({
            "id": n.id, "op": n.op, "provider": n.provider, "dependsOn": n.depends_on
        })).collect::<Vec<_>>(),
        "estimatedTotalCostUsd": plan.estimated_cost,
        "estimatedTotalLatencyMs": plan.estimated_latency,
    })
}
