use serde::Serialize;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    ConstraintViolation,
    ProviderFailure,
    Timeout,
    Unsupported,
    InputTooLarge,
    NotConfigured,
}

#[derive(Debug)]
pub struct AppError {
    pub code: ErrorCode,
    pub message: String,
    pub retryable: bool,
    pub suggestion: Option<String>,
}

impl AppError {
    pub fn new(code: ErrorCode, message: impl Into<String>, retryable: bool) -> Self {
        Self { code, message: message.into(), retryable, suggestion: None }
    }

    pub fn constraint(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::ConstraintViolation, message, false)
    }

    pub fn provider(message: impl Into<String>, retryable: bool) -> Self {
        Self::new(ErrorCode::ProviderFailure, message, retryable)
    }

    pub fn timeout(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Timeout, message, true)
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Unsupported, message, false)
    }

    pub fn with_suggestion(mut self, suggestion: impl Into<String>) -> Self {
        self.suggestion = Some(suggestion.into());
        self
    }

    pub fn to_json(&self) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        map.insert("message".into(), self.message.clone().into());
        map.insert("code".into(), serde_json::to_value(self.code).unwrap_or_default());
        map.insert("retryable".into(), self.retryable.into());
        if let Some(s) = &self.suggestion {
            map.insert("suggestion".into(), s.clone().into());
        }
        serde_json::Value::Object(map)
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for AppError {}

impl From<std::io::Error> for AppError {
    fn from(err: std::io::Error) -> Self {
        AppError::new(ErrorCode::ProviderFailure, err.to_string(), false)
    }
}

impl From<reqwest::Error> for AppError {
    fn from(err: reqwest::Error) -> Self {
        if err.is_timeout() {
            AppError::timeout(err.to_string())
        } else {
            AppError::provider(format!("network error: {err}"), err.is_connect() || err.is_request())
        }
    }
}

impl From<image::ImageError> for AppError {
    fn from(err: image::ImageError) -> Self {
        AppError::new(ErrorCode::Unsupported, format!("image decode failed: {err}"), false)
    }
}
