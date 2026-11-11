//! Current Codex app-server response fields absent in the vendored protocol.
use serde_json::{Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

pub fn response(method: &str, value: &mut Value) {
    if method == "item/started" {
        value["startedAtMs"] =
            json!((OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64);
    }
    if method == "item/completed" {
        value["completedAtMs"] =
            json!((OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64);
    }
    if method == "initialize" {
        value["codexHome"] = json!(std::env::var("CODEX_HOME").unwrap_or_else(|_| {
            dirs::home_dir()
                .unwrap()
                .join(".codex")
                .to_string_lossy()
                .into_owned()
        }));
        value["platformOs"] = json!(std::env::consts::OS);
        value["platformFamily"] = json!(std::env::consts::FAMILY);
    }
    if let Some(thread) = value.get_mut("thread") {
        normalize_thread(thread);
    }
    if method == "thread/list" {
        if let Some(threads) = value["data"].as_array_mut() {
            for thread in threads {
                normalize_thread(thread);
            }
        }
    }
    if matches!(
        method,
        "thread/start" | "thread/resume" | "thread/fork" | "thread/rollback"
    ) {
        value["approvalsReviewer"] = json!("user");
    }
}
fn normalize_thread(thread: &mut Value) {
    let Some(o) = thread.as_object_mut() else {
        return;
    };
    o.entry("ephemeral").or_insert(json!(false));
    o.entry("projectId").or_insert(Value::Null);
    let id = o.get("id").cloned().unwrap_or(Value::Null);
    o.entry("sessionId").or_insert(id);
    for key in ["createdAt", "updatedAt"] {
        if let Some(s) = o.get(key).and_then(Value::as_str) {
            let timestamp = OffsetDateTime::parse(s, &Rfc3339)
                .map(|t| t.unix_timestamp())
                .unwrap_or(0);
            o.insert(key.into(), json!(timestamp));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn upgrades_legacy_thread_fields() {
        let mut value =
            json!({"thread":{"id":"abc","createdAt":"2026-11-11T00:00:00Z","updatedAt":42}});
        response("thread/start", &mut value);
        assert!(value["thread"]["createdAt"].is_i64());
        assert_eq!(value["thread"]["updatedAt"], 42);
        assert_eq!(value["thread"]["sessionId"], "abc");
        assert_eq!(value["thread"]["ephemeral"], false);
        assert_eq!(value["approvalsReviewer"], "user");
    }
}

pub fn read_config(state: &crate::translation::state::GatewayState) -> Value {
    let path = state.rollout.gateway_home().join("client-config.json");
    let config = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .unwrap_or(json!({}));
    json!({"config":config,"origins":{},"layers":[]})
}
pub fn write_config(
    state: &crate::translation::state::GatewayState,
    params: &Value,
) -> Result<Value, crate::error::GatewayError> {
    use crate::error::GatewayError;
    let edits = params["edits"]
        .as_array()
        .cloned()
        .unwrap_or_else(|| vec![params.clone()]);
    let mut config = read_config(state)["config"].clone();
    for edit in edits {
        let key = edit["keyPath"].as_str().unwrap_or("");
        if !matches!(key, "model" | "model_reasoning_effort" | "service_tier") {
            return Err(GatewayError::Translation(format!(
                "Gateway settings support model, model_reasoning_effort and service_tier only; unsupported: {key}"
            )));
        }
        config[key] = edit["value"].clone();
    }
    let home = state.rollout.gateway_home();
    let path = home.join("client-config.json");
    std::fs::create_dir_all(home)
        .and_then(|_| std::fs::write(&path, serde_json::to_vec_pretty(&config).unwrap()))
        .map_err(|e| GatewayError::Translation(format!("save gateway settings: {e}")))?;
    Ok(
        json!({"filePath":path,"status":"ok","version":OffsetDateTime::now_utc().unix_timestamp_nanos().to_string()}),
    )
}
