use crate::error::AppError;
use base64::Engine;
use serde_json::Value;
use std::time::Duration;

const BASE_BACKOFF_MS: u64 = 1_000;

pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent("shenbi/1.0")
        .connect_timeout(Duration::from_secs(20))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

pub fn data_url(mime: &str, bytes: &[u8]) -> String {
    format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes))
}

/// Read a JSON 200 body, or turn a non-2xx into a provider error carrying the
/// upstream message. 5xx and 429 stay retryable.
pub async fn json_or_error(response: reqwest::Response) -> Result<Value, AppError> {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    if status.is_success() {
        return Ok(parsed);
    }
    let message = error_message(&parsed).unwrap_or_else(|| {
        if body.trim().is_empty() {
            format!("HTTP {status}")
        } else {
            body.chars().take(500).collect()
        }
    });
    Err(AppError::provider(
        message,
        status.is_server_error() || status.as_u16() == 429,
    ))
}

pub fn error_message(body: &Value) -> Option<String> {
    let candidates = [
        body.pointer("/error/message"),
        body.pointer("/error"),
        body.get("message"),
        body.get("detail"),
        body.get("error"),
    ];
    candidates
        .into_iter()
        .flatten()
        .find_map(|value| match value {
            Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
            Value::Object(map) => map
                .get("message")
                .and_then(|m| m.as_str())
                .map(str::to_string),
            _ => None,
        })
}

pub fn decode_b64(value: &str) -> Result<Vec<u8>, AppError> {
    let payload = value.split_once(";base64,").map(|(_, rest)| rest).unwrap_or(value);
    base64::engine::general_purpose::STANDARD
        .decode(payload.trim())
        .map_err(|err| AppError::provider(format!("invalid base64 image payload: {err}"), false))
}

pub async fn download_image(url: &str, timeout: Duration) -> Result<Vec<u8>, AppError> {
    // Relays sometimes hand back an inline data URL instead of a hosted file.
    if url.starts_with("data:") {
        return decode_b64(url);
    }
    for attempt in 0..=1 {
        let result = client().get(url).timeout(timeout).send().await;
        match result {
            Ok(response) if response.status().is_success() => {
                return response
                    .bytes()
                    .await
                    .map(|b| b.to_vec())
                    .map_err(|err| AppError::provider(format!("image download failed: {err}"), true));
            }
            Ok(response) => {
                return Err(AppError::provider(
                    format!("image download returned {}", response.status()),
                    true,
                ))
            }
            Err(err) if attempt == 0 => {
                eprintln!("download retry after error: {err}");
                tokio::time::sleep(Duration::from_millis(800)).await;
            }
            Err(err) => return Err(AppError::timeout(format!("image download failed: {err}"))),
        }
    }
    Err(AppError::provider("image download failed", true))
}

pub async fn with_retries<F, Fut, T>(label: &str, max_retries: usize, mut attempt_fn: F) -> Result<T, AppError>
where
    F: FnMut(usize) -> Fut,
    Fut: std::future::Future<Output = Result<T, AppError>>,
{
    let mut last: Option<AppError> = None;
    for attempt in 0..=max_retries {
        match attempt_fn(attempt).await {
            Ok(value) => return Ok(value),
            Err(err) => {
                let retryable = err.retryable;
                last = Some(err);
                if !retryable || attempt == max_retries {
                    break;
                }
                let delay = BASE_BACKOFF_MS * (1 << attempt);
                eprintln!("{label}: attempt {} failed, retrying in {delay}ms", attempt + 1);
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
        }
    }
    Err(last.unwrap_or_else(|| AppError::provider(format!("{label} failed"), false)))
}
