//! Managed-frame handshake validation shared by the browser and focused tests.

use serde_json::Value;
use url::Url;

pub const MAJOR: u64 = 1;
pub const MINOR: u64 = 0;
const MAX_CONFIG_BYTES: usize = 16 * 1024;
const MAX_ORIGINS: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Init {
    pub nonce: String,
    pub design_id: String,
    pub session_epoch: String,
}

#[derive(Clone, Debug)]
pub struct Policy {
    origins: Vec<String>,
    accepted: bool,
}

fn exact_keys(value: &Value, expected: &[&str]) -> bool {
    value.as_object().is_some_and(|o| o.len() == expected.len() && expected.iter().all(|key| o.contains_key(*key)))
}

fn bounded_id(value: Option<&Value>) -> Option<&str> {
    value.and_then(Value::as_str).filter(|v| !v.is_empty() && v.len() <= 128 && !v.chars().any(char::is_control))
}

/// Validate the v1 control envelope without admitting any operation.
pub fn valid_request(value: &Value, design_id: &str, epoch: &str) -> bool {
    let Some(object) = value.as_object() else { return false };
    let allowed = ["version", "kind", "requestId", "sessionEpoch", "designId", "operationId", "expectedGeneration", "payload"];
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return false;
    }
    let Some(version) = value.get("version") else { return false };
    let generation = value.get("expectedGeneration").is_none_or(|v| {
        v.as_str().is_some_and(|s| {
            !s.is_empty() && s.len() <= 20 && (s == "0" || !s.starts_with('0')) && s.bytes().all(|b| b.is_ascii_digit()) && s.parse::<u64>().is_ok()
        })
    });
    let operation = value.get("operationId").is_none_or(|v| bounded_id(Some(v)).is_some());
    exact_keys(version, &["major", "minor"])
        && version.get("major").and_then(Value::as_u64) == Some(MAJOR)
        && version.get("minor").and_then(Value::as_u64) == Some(MINOR)
        && value.get("kind").and_then(Value::as_str) == Some("request")
        && bounded_id(value.get("requestId")).is_some()
        && value.get("sessionEpoch").and_then(Value::as_str) == Some(epoch)
        && value.get("designId").and_then(Value::as_str) == Some(design_id)
        && value.get("payload").is_some_and(Value::is_object)
        && generation
        && operation
}

impl Policy {
    pub fn from_config(text: &str) -> Result<Self, &'static str> {
        if text.len() > MAX_CONFIG_BYTES {
            return Err("runtime config too large");
        }
        let config: Value = serde_json::from_str(text).map_err(|_| "invalid runtime config")?;
        let origins = config.get("allowedParentOrigins").and_then(Value::as_array).ok_or("missing parent origins")?;
        if origins.is_empty() || origins.len() > MAX_ORIGINS {
            return Err("invalid parent origin count");
        }
        let mut allowed = Vec::new();
        for value in origins {
            let origin = value.as_str().ok_or("invalid parent origin")?;
            let parsed = Url::parse(origin).map_err(|_| "invalid parent origin")?;
            let local_http = parsed.scheme() == "http" && matches!(parsed.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
            if parsed.origin().ascii_serialization() != origin
                || !parsed.username().is_empty()
                || parsed.password().is_some()
                || (parsed.scheme() != "https" && !local_http)
            {
                return Err("parent origins must be exact HTTP(S) origins");
            }
            if !allowed.iter().any(|item| item == origin) {
                allowed.push(origin.to_string());
            }
        }
        Ok(Self { origins: allowed, accepted: false })
    }

    pub fn origins(&self) -> &[String] {
        &self.origins
    }

    /// Acceptance is single-use: validation never changes state on rejection.
    pub fn accept(&mut self, origin: &str, is_parent: bool, port_count: usize, value: &Value) -> Result<Init, &'static str> {
        if self.accepted {
            return Err("handshake already accepted");
        }
        if !is_parent || !self.origins.iter().any(|allowed| allowed == origin) {
            return Err("parent not allowed");
        }
        if port_count != 1 {
            return Err("exactly one port required");
        }
        if !exact_keys(value, &["type", "version", "nonce", "designId", "sessionEpoch"]) {
            return Err("invalid handshake fields");
        }
        if value.get("type").and_then(Value::as_str) != Some("curie.cad.init") {
            return Err("invalid handshake type");
        }
        let version = value.get("version").ok_or("missing version")?;
        if !exact_keys(version, &["major", "minor"])
            || version.get("major").and_then(Value::as_u64) != Some(MAJOR)
            || version.get("minor").and_then(Value::as_u64) != Some(MINOR)
        {
            return Err("unsupported version");
        }
        let nonce = value
            .get("nonce")
            .and_then(Value::as_str)
            .filter(|n| (22..=128).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
            .ok_or("invalid nonce")?;
        let design_id = bounded_id(value.get("designId")).ok_or("invalid design id")?;
        let session_epoch = bounded_id(value.get("sessionEpoch")).ok_or("invalid session epoch")?;
        let init = Init { nonce: nonce.to_string(), design_id: design_id.to_string(), session_epoch: session_epoch.to_string() };
        self.accepted = true;
        Ok(init)
    }
}
