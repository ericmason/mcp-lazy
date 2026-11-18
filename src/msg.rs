//! Just enough JSON-RPC to route MCP messages. Messages are forwarded as the
//! original bytes; parsing only classifies them.

use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::Value;

pub const PROXY_ID_PREFIX: &str = "mcp-lazy:";

#[derive(Deserialize)]
pub struct Msg<'a> {
    /// Some for any id that is present, including `null`, so a request with
    /// a null id is still a request and still gets an answer.
    #[serde(borrow, default, deserialize_with = "present")]
    pub id: Option<&'a RawValue>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(borrow, default)]
    pub params: Option<&'a RawValue>,
    #[serde(borrow, default)]
    pub result: Option<&'a RawValue>,
    #[serde(borrow, default)]
    pub error: Option<&'a RawValue>,
}

fn present<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<&'de RawValue>, D::Error> {
    <&RawValue>::deserialize(d).map(Some)
}

/// Returns None for anything that isn't a single JSON object (batches, garbage).
pub fn parse(line: &[u8]) -> Option<Msg<'_>> {
    let text = std::str::from_utf8(line).ok()?;
    // serde would also accept an array as a struct's fields in order, so a
    // batch must be turned away before it gets that far.
    if !text.trim_start().starts_with('{') {
        return None;
    }
    serde_json::from_str(text).ok()
}

/// True when a line that `parse` rejected is a JSON-RPC batch.
pub fn is_batch(line: &[u8]) -> bool {
    std::str::from_utf8(line).is_ok_and(|t| t.trim_start().starts_with('['))
}

/// A canonical form of an id, so `1` and `1.0`, or differently escaped
/// strings, match when the server echoes them back. Integers keep every digit.
pub fn id_key(id: &RawValue) -> String {
    let raw = id.get().trim();
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::Number(n)) => {
            if let Some(i) = n.as_i64() {
                return i.to_string();
            }
            if let Some(u) = n.as_u64() {
                return u.to_string();
            }
            // An integer too big for 64 bits: its digits are already canonical,
            // and going through f64 would merge neighbors.
            if raw.bytes().enumerate().all(|(i, b)| b.is_ascii_digit() || (i == 0 && b == b'-')) {
                return raw.to_string();
            }
            match n.as_f64() {
                Some(f) if f.fract() == 0.0 && f.abs() < 9_007_199_254_740_992.0 => (f as i64).to_string(),
                _ => raw.to_string(),
            }
        }
        Ok(v) => v.to_string(),
        Err(_) => raw.to_string(),
    }
}

/// True for a string id in the range the proxy uses for its own requests.
pub fn is_proxy_id(id: &RawValue) -> bool {
    serde_json::from_str::<String>(id.get()).is_ok_and(|s| s.starts_with(PROXY_ID_PREFIX))
}

pub fn response(id: &str, result: &str) -> Vec<u8> {
    format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{result}}}"#).into_bytes()
}

pub fn error(id: &str, code: i64, message: &str) -> Vec<u8> {
    let message = serde_json::to_string(message).unwrap();
    format!(r#"{{"jsonrpc":"2.0","id":{id},"error":{{"code":{code},"message":{message}}}}}"#).into_bytes()
}

pub fn request(id: &str, method: &str, params: Option<&str>) -> Vec<u8> {
    let method = serde_json::to_string(method).unwrap();
    match params {
        Some(p) => format!(r#"{{"jsonrpc":"2.0","id":{id},"method":{method},"params":{p}}}"#),
        None => format!(r#"{{"jsonrpc":"2.0","id":{id},"method":{method}}}"#),
    }
    .into_bytes()
}

pub fn notification(method: &str) -> Vec<u8> {
    let method = serde_json::to_string(method).unwrap();
    format!(r#"{{"jsonrpc":"2.0","method":{method}}}"#).into_bytes()
}

fn params_value(params: Option<&RawValue>) -> Option<Value> {
    params.and_then(|p| serde_json::from_str(p.get()).ok())
}

/// True when a list request asks for a page after the first.
pub fn has_cursor(params: Option<&RawValue>) -> bool {
    params_value(params)
        .and_then(|v| v.get("cursor").map(|c| !c.is_null()))
        .unwrap_or(false)
}

pub fn protocol_version(params: Option<&RawValue>) -> String {
    params_value(params)
        .and_then(|v| v.get("protocolVersion").and_then(|p| p.as_str()).map(String::from))
        .unwrap_or_default()
}

#[derive(Deserialize)]
struct Cancelled<'a> {
    #[serde(borrow, rename = "requestId")]
    request_id: &'a RawValue,
}

/// The canonical id of the request a `notifications/cancelled` refers to.
pub fn cancelled_request_key(params: Option<&RawValue>) -> Option<String> {
    serde_json::from_str::<Cancelled>(params?.get()).ok().map(|c| id_key(c.request_id))
}

/// The raw requestId of a `notifications/cancelled`, for checking whether it
/// names one of the proxy's own requests.
pub fn cancelled_request_id(params: Option<&RawValue>) -> Option<&RawValue> {
    serde_json::from_str::<Cancelled>(params?.get()).ok().map(|c| c.request_id)
}

/// List methods to refresh, given an initialize result's capabilities.
pub fn advertised_lists(init_result: &RawValue) -> Vec<&'static str> {
    let caps = serde_json::from_str::<Value>(init_result.get())
        .ok()
        .and_then(|v| v.get("capabilities").cloned())
        .unwrap_or(Value::Null);
    let mut out = Vec::new();
    if caps.get("tools").is_some() {
        out.push("tools/list");
    }
    if caps.get("prompts").is_some() {
        out.push("prompts/list");
    }
    if caps.get("resources").is_some() {
        out.push("resources/list");
        out.push("resources/templates/list");
    }
    out
}

/// The notification that tells the client a cached list changed.
pub fn list_changed_for(method: &str) -> Option<&'static str> {
    match method {
        "tools/list" => Some("notifications/tools/list_changed"),
        "prompts/list" => Some("notifications/prompts/list_changed"),
        "resources/list" | "resources/templates/list" => Some("notifications/resources/list_changed"),
        _ => None,
    }
}

/// The list methods a server's list_changed notification invalidates.
pub fn lists_for_changed(notification: &str) -> &'static [&'static str] {
    match notification {
        "notifications/tools/list_changed" => &["tools/list"],
        "notifications/prompts/list_changed" => &["prompts/list"],
        "notifications/resources/list_changed" => &["resources/list", "resources/templates/list"],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_messages() {
        let m = parse(br#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"x"}}"#).unwrap();
        assert_eq!(m.method.as_deref(), Some("tools/call"));
        assert_eq!(id_key(m.id.unwrap()), "7");
        let n = parse(br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).unwrap();
        assert!(n.id.is_none());
        let r = parse(br#"{"jsonrpc":"2.0","id":"a","result":{}}"#).unwrap();
        assert!(r.method.is_none() && r.result.is_some());
        assert!(parse(br#"[{"jsonrpc":"2.0","id":1,"method":"x"}]"#).is_none());
        assert!(parse(b"not json").is_none());
    }

    fn key_of(line: &str) -> String {
        id_key(parse(line.as_bytes()).unwrap().id.unwrap())
    }

    #[test]
    fn canonical_ids_match_across_spellings() {
        assert_eq!(key_of(r#"{"id":1}"#), key_of(r#"{"id":1.0}"#));
        assert_eq!(key_of(r#"{"id":1}"#), key_of(r#"{"id":1e0}"#));
        assert_eq!(key_of(r#"{"id":"a"}"#), key_of(r#"{"id":"a"}"#));
        assert_ne!(key_of(r#"{"id":1}"#), key_of(r#"{"id":"1"}"#));
        assert_ne!(key_of(r#"{"id":1}"#), key_of(r#"{"id":1.5}"#));
        // Integers past 2^53 keep every digit.
        assert_ne!(key_of(r#"{"id":12345678901234567890123}"#), key_of(r#"{"id":12345678901234567890124}"#));
        let p = parse(br#"{"params":{"requestId":1.0}}"#).unwrap();
        assert_eq!(cancelled_request_key(p.params).unwrap(), key_of(r#"{"id":1}"#));
    }

    #[test]
    fn a_null_id_is_present() {
        let m = parse(br#"{"jsonrpc":"2.0","id":null,"method":"tools/call"}"#).unwrap();
        assert_eq!(m.id.map(|i| i.get()), Some("null"));
        assert!(parse(br#"{"jsonrpc":"2.0","method":"x"}"#).unwrap().id.is_none());
        assert!(is_batch(b" [{}]") && !is_batch(b"nope"));
    }

    #[test]
    fn builds_valid_json() {
        let e: Value = serde_json::from_slice(&error("\"x\"", -32603, "quote \" here")).unwrap();
        assert_eq!(e["error"]["message"], "quote \" here");
        let r: Value = serde_json::from_slice(&response("3", r#"{"tools":[]}"#)).unwrap();
        assert_eq!(r["id"], 3);
    }

    #[test]
    fn reads_params() {
        let p = parse(br#"{"params":{"cursor":"abc"}}"#).unwrap();
        assert!(has_cursor(p.params));
        let p = parse(br#"{"params":{"cursor":null}}"#).unwrap();
        assert!(!has_cursor(p.params));
        let p = parse(br#"{"params":{"protocolVersion":"2025-06-18"}}"#).unwrap();
        assert_eq!(protocol_version(p.params), "2025-06-18");
        let p = parse(br#"{"params":{"requestId":5}}"#).unwrap();
        assert_eq!(cancelled_request_key(p.params).as_deref(), Some("5"));
    }

    #[test]
    fn lists_from_capabilities() {
        let r = parse(br#"{"result":{"capabilities":{"tools":{},"resources":{}}}}"#).unwrap();
        assert_eq!(advertised_lists(r.result.unwrap()), ["tools/list", "resources/list", "resources/templates/list"]);
    }
}
