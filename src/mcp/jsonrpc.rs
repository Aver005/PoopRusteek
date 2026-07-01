//! JSON-RPC 2.0 message shapes used by every MCP transport, plus the pure
//! helpers (`ids_match`, `is_notification_or_request`) that transports use
//! to tell a real response to *our* request apart from a notification, a
//! server-initiated request, or the response to some other in-flight call.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: u64,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub _meta: Option<Value>,
}

impl JsonRpcRequest {
    pub fn new(id: u64, method: &str, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            method: method.to_string(),
            params,
            _meta: None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    /// JSON-RPC allows number or string ids (and `null` for parse-error
    /// responses), so this is a raw `Value` rather than `u64`. Requests we
    /// send always use numeric ids (see `JsonRpcRequest::new`), but servers
    /// may echo back whatever id they received or send unsolicited
    /// notifications/requests with no id at all — use `ids_match` /
    /// `is_notification_or_request` to interpret this field rather than
    /// comparing it directly.
    #[serde(default)]
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub _meta: Option<Value>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub _meta: Option<Value>,
}

impl JsonRpcNotification {
}

/// Canonical equality between a request id (`u64`, always numeric — we mint
/// our own request ids) and a response's `id` field (`Option<Value>`, which
/// may be numeric, a string, or absent/null).
///
/// `serde_json::Value` numbers can deserialize as i64/u64/f64 depending on
/// the wire representation (e.g. `42` vs `42.0`), so a naive `Value ==
/// Value` comparison can spuriously fail for numerically-equal ids sent by a
/// spec-compliant but differently-serializing server. This compares numbers
/// via `as_u64()` first and only falls back to raw equality for non-numeric
/// ids (strings), which JSON-RPC also permits.
pub fn ids_match(request_id: u64, response_id: &Value) -> bool {
    if let Some(n) = response_id.as_u64() {
        return n == request_id;
    }
    if let Some(f) = response_id.as_f64() {
        // Integral floats (e.g. a server that encodes ids as `42.0`) still
        // count as a match; fractional ids can never equal our integer id.
        return f.fract() == 0.0 && f >= 0.0 && f as u64 == request_id;
    }
    false
}

/// A parsed JSON-RPC message on the wire is a *notification or server
/// request* — not the answer to one of our calls — when it carries a
/// `method` field. Per the JSON-RPC 2.0 spec, responses never have `method`;
/// only requests and notifications do. Notifications additionally lack an
/// `id`, while server-initiated requests (rare, but legal — e.g.
/// `sampling/createMessage`) have both `method` and an `id` in the server's
/// own id-space, which is why checking for `method` (rather than "id is
/// absent") is the correct discriminator here.
pub fn is_notification_or_request(value: &Value) -> bool {
    value.get("method").is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ids_match_numeric_equal() {
        assert!(ids_match(42, &json!(42)));
    }

    #[test]
    fn ids_match_numeric_mismatch() {
        assert!(!ids_match(42, &json!(43)));
    }

    #[test]
    fn ids_match_integral_float() {
        // Some servers may round-trip ids through a float-typed JSON number.
        assert!(ids_match(42, &json!(42.0)));
    }

    #[test]
    fn ids_match_fractional_float_never_matches() {
        assert!(!ids_match(42, &json!(42.5)));
    }

    #[test]
    fn ids_match_string_id_does_not_match_numeric_request() {
        // We only ever mint numeric ids, so a string id (even "42") is never
        // "our" response.
        assert!(!ids_match(42, &json!("42")));
    }

    #[test]
    fn ids_match_null_or_missing_never_matches() {
        assert!(!ids_match(1, &Value::Null));
    }

    #[test]
    fn is_notification_or_request_true_for_method_field() {
        assert!(is_notification_or_request(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/progress",
            "params": {}
        })));
    }

    #[test]
    fn is_notification_or_request_true_even_with_id() {
        // Server-initiated requests (e.g. sampling/createMessage) have both
        // `method` and `id` — still not one of our responses.
        assert!(is_notification_or_request(&json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "sampling/createMessage",
            "params": {}
        })));
    }

    #[test]
    fn is_notification_or_request_false_for_plain_response() {
        assert!(!is_notification_or_request(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {}
        })));
    }

    #[test]
    fn is_notification_or_request_false_for_error_response() {
        assert!(!is_notification_or_request(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": { "code": -32000, "message": "boom" }
        })));
    }
}
