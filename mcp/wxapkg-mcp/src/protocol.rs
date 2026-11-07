// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! JSON-RPC 2.0 message types.
//!
//! Only the subset the Model Context Protocol's stdio transport needs is modelled.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// An incoming request or notification.
#[derive(Debug, Clone, Deserialize)]
pub struct Request {
    /// Always `"2.0"`.
    #[serde(default = "default_jsonrpc")]
    pub jsonrpc: String,
    /// Correlator; absent for notifications, which must not be answered.
    #[serde(default)]
    pub id: Option<Value>,
    /// Method name.
    pub method: String,
    /// Method parameters.
    #[serde(default)]
    pub params: Option<Value>,
}

fn default_jsonrpc() -> String {
    "2.0".to_owned()
}

/// An outgoing response.
#[derive(Debug, Clone, Serialize)]
pub struct Response {
    /// Always `"2.0"`.
    pub jsonrpc: &'static str,
    /// Correlator copied from the request.
    pub id: Value,
    /// Result on success.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Error on failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Response {
    /// Builds a success response.
    pub fn success(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    /// Builds an error response.
    pub fn error(id: Value, error: RpcError) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(error),
        }
    }
}

/// A JSON-RPC error object.
#[derive(Debug, Clone, Serialize)]
pub struct RpcError {
    /// Numeric code.
    pub code: i32,
    /// Human readable message.
    pub message: String,
}

impl RpcError {
    /// `-32700`: the payload was not valid JSON.
    pub fn parse_error(message: impl Into<String>) -> Self {
        Self {
            code: -32700,
            message: message.into(),
        }
    }

    /// `-32600`: the payload was not a valid request.
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self {
            code: -32600,
            message: message.into(),
        }
    }

    /// `-32601`: unknown method.
    pub fn method_not_found(method: &str) -> Self {
        Self {
            code: -32601,
            message: format!("unknown method `{method}`"),
        }
    }

    /// `-32602`: invalid method parameters.
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: message.into(),
        }
    }

    /// `-32603`: unexpected server failure.
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            code: -32603,
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notifications_have_no_id() {
        let request: Request =
            serde_json::from_str(r#"{"jsonrpc":"2.0","method":"ping"}"#).unwrap();
        assert!(request.id.is_none());
    }

    #[test]
    fn responses_omit_the_unused_arm() {
        let ok = serde_json::to_string(&Response::success(
            serde_json::json!(1),
            serde_json::json!({}),
        ))
        .unwrap();
        assert!(!ok.contains("error"));
        let bad = serde_json::to_string(&Response::error(
            serde_json::json!(1),
            RpcError::method_not_found("x"),
        ))
        .unwrap();
        assert!(!bad.contains("result"));
    }
}
