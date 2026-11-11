//! Translate ACP model/thought-level options into the Codex model picker.
use crate::{error::GatewayError, translation::state::GatewayState};
use agent_client_protocol::{
    Agent, NewSessionRequest, SetSessionConfigOptionRequest, SetSessionModelRequest,
};
use serde_json::{Value, json};

fn options(state: &GatewayState, session: &str) -> Vec<Value> {
    state
        .session_config
        .get(session)
        .map(|v| {
            v.iter()
                .filter_map(|o| serde_json::to_value(o).ok())
                .collect()
        })
        .unwrap_or_default()
}
fn category<'a>(options: &'a [Value], kind: &str) -> Option<&'a Value> {
    options.iter().find(|o| {
        if kind == "model" {
            o["category"] == "model" || o["id"] == "model"
        } else {
            matches!(
                o["category"].as_str(),
                Some("thought_level" | "reasoning_effort")
            ) || matches!(
                o["id"].as_str(),
                Some("thinking_level" | "reasoning_effort")
            )
        }
    })
}
fn choices(option: &Value) -> Vec<(String, String)> {
    fn collect(values: &Value, out: &mut Vec<(String, String)>) {
        if let Some(values) = values.as_array() {
            for v in values {
                if let Some(id) = v["value"].as_str() {
                    out.push((id.into(), v["name"].as_str().unwrap_or(id).into()));
                }
                collect(&v["options"], out);
            }
        }
    }
    let mut out = vec![];
    collect(&option["options"], &mut out);
    out
}
fn allowed(id: &str) -> bool {
    std::env::var("ACP_ALLOWED_MODELS")
        .ok()
        .filter(|s| !s.is_empty())
        .is_none_or(|s| s.split(',').any(|m| m.trim() == id))
}
fn valid_effort(e: &str) -> bool {
    matches!(
        e,
        "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
    )
}
pub fn current(state: &GatewayState, session: &str) -> (String, Option<String>) {
    let opts = options(state, session);
    let model = category(&opts, "model")
        .and_then(|o| o["currentValue"].as_str())
        .map(String::from)
        .or_else(|| {
            state
                .model_state
                .as_ref()
                .map(|m| m.current_model_id.to_string())
        })
        .unwrap_or_else(|| "acp-agent".into());
    let effort = category(&opts, "effort")
        .and_then(|o| o["currentValue"].as_str())
        .filter(|e| valid_effort(e))
        .map(String::from);
    (model, effort)
}
pub async fn list(
    agent: &(impl Agent + ?Sized),
    state: &mut GatewayState,
    cwd: &std::path::Path,
) -> Result<Value, GatewayError> {
    if let Some(cached) = &state.catalog_cache {
        return Ok(cached.clone());
    }
    // Always use a dedicated discovery session; never change a conversation's model.
    let discovered = agent
        .new_session(NewSessionRequest::new(cwd.to_path_buf()))
        .await
        .map_err(|e| GatewayError::Acp(format!("catalog discovery: {e:?}")))?;
    let session = discovered.session_id.to_string();
    if let Some(options) = discovered.config_options {
        state.session_config.insert(session.clone(), options);
    }
    if let Some(models) = discovered.models {
        state.model_state = Some(models);
    }
    let opts = options(state, &session);
    let (selected, effort) = current(state, &session);
    let models = if let Some(o) = category(&opts, "model") {
        choices(o)
    } else {
        state
            .model_state
            .as_ref()
            .map(|m| {
                m.available_models
                    .iter()
                    .map(|m| (m.model_id.to_string(), m.name.clone()))
                    .collect()
            })
            .unwrap_or_else(|| vec![("acp-agent".into(), "ACP Agent".into())])
    };
    let efforts: Vec<_> = category(&opts, "effort")
        .map(choices)
        .unwrap_or_default()
        .into_iter()
        .filter(|(e, _)| valid_effort(e))
        .map(|(e, n)| json!({"reasoningEffort":e,"description":n}))
        .collect();
    let mut model_efforts = category(&opts, "model")
        .map(|o| o["_meta"]["modelEfforts"].clone())
        .unwrap_or(Value::Null);
    if model_efforts.is_null() {
        model_efforts = json!({});
        if let Some(option) = category(&opts, "model") {
            for (id, _) in models.iter().filter(|(id, _)| allowed(id)) {
                let live = options(state, &session);
                let live_option = category(&live, "model").unwrap_or(option);
                set_option(agent, state, &session, live_option, id).await?;
                let current_options = options(state, &session);
                let values = category(&current_options, "effort")
                    .map(choices)
                    .unwrap_or_default();
                let variants: Vec<_> = values
                    .into_iter()
                    .filter(|(e, _)| valid_effort(e))
                    .map(|(e, _)| e)
                    .collect();
                let default = category(&current_options, "effort")
                    .and_then(|o| o["currentValue"].as_str())
                    .filter(|e| valid_effort(e))
                    .unwrap_or_else(|| {
                        if variants.iter().any(|e| e == "medium") {
                            "medium"
                        } else {
                            "high"
                        }
                    });
                model_efforts[id] = json!({"variants":variants,"default":default});
            }
            // Restore explicitly: the original option still describes its old current value.
            let restore = options(state, &session);
            if let Some(option) = category(&restore, "model") {
                set_option(agent, state, &session, option, &selected).await?;
            }
        }
    }
    let mut rows:Vec<_>=models.into_iter().filter(|(id,_)|allowed(id)).map(|(id,name)|{
        let specific=&model_efforts[&id];
        let efforts=specific["variants"].as_array().map(|v|v.iter().map(|e|json!({"reasoningEffort":e,"description":e})).collect::<Vec<_>>()).unwrap_or_else(||efforts.clone());
        let default=specific["default"].as_str().unwrap_or(effort.as_deref().unwrap_or("medium"));
        json!({
        "id":id,"model":id,"displayName":name,"description":"ACP agent model", "hidden":false,
        "supportedReasoningEfforts":efforts,"defaultReasoningEffort":default,
        "isDefault":id==selected,"inputModalities":["text"],"supportsPersonality":false
    })}).collect();
    if !rows.iter().any(|m| m["isDefault"] == true) {
        if let Some(first) = rows.first_mut() {
            first["isDefault"] = json!(true);
        }
    }
    let result = json!({"data":rows,"nextCursor":null});
    state.catalog_cache = Some(result.clone());
    Ok(result)
}
async fn set_option(
    agent: &(impl Agent + ?Sized),
    state: &mut GatewayState,
    session: &str,
    option: &Value,
    value: &str,
) -> Result<(), GatewayError> {
    let id = option["id"]
        .as_str()
        .ok_or_else(|| GatewayError::Translation("ACP option lacks id".into()))?;
    if !choices(option).iter().any(|(v, _)| v == value) {
        return Err(GatewayError::Translation(format!(
            "Unsupported {id}: {value}"
        )));
    }
    if option["currentValue"] == value {
        return Ok(());
    }
    let response = agent
        .set_session_config_option(SetSessionConfigOptionRequest::new(
            session.to_string(),
            id.to_string(),
            value.to_string(),
        ))
        .await
        .map_err(|e| GatewayError::Acp(format!("set {id}={value}: {e:?}")))?;
    state
        .session_config
        .insert(session.into(), response.config_options);
    Ok(())
}
pub async fn select(
    agent: &(impl Agent + ?Sized),
    state: &mut GatewayState,
    session: &str,
    params: &Value,
) -> Result<(), GatewayError> {
    let settings = &params["collaborationMode"]["settings"];
    if let Some(model) = params["model"]
        .as_str()
        .or_else(|| settings["model"].as_str())
    {
        if !allowed(model) {
            return Err(GatewayError::Translation(format!(
                "Model not enabled: {model}"
            )));
        }
        let opts = options(state, session);
        if let Some(option) = category(&opts, "model") {
            set_option(agent, state, session, option, model).await?;
        } else if model != "acp-agent" {
            agent
                .set_session_model(SetSessionModelRequest::new(
                    session.to_string(),
                    model.to_string(),
                ))
                .await
                .map_err(|e| GatewayError::Acp(format!("set model: {e:?}")))?;
            if let Some(models) = state.model_state.as_mut() {
                models.current_model_id = model.to_string().into();
            }
        }
    }
    if let Some(effort) = params["effort"]
        .as_str()
        .or_else(|| params["reasoningEffort"].as_str())
        .or_else(|| params["config"]["model_reasoning_effort"].as_str())
        .or_else(|| settings["reasoning_effort"].as_str())
    {
        let opts = options(state, session);
        if let Some(option) = category(&opts, "effort") {
            set_option(agent, state, session, option, effort).await?;
        } else {
            return Err(GatewayError::Translation(
                "Agent does not advertise a configurable reasoning effort".into(),
            ));
        }
    }
    Ok(())
}

/// Preserve the ACP session id across gateway restarts.
pub fn save_session(state: &GatewayState, thread: &str, session: &str) -> Result<(), GatewayError> {
    uuid::Uuid::parse_str(thread).map_err(|e| GatewayError::Translation(e.to_string()))?;
    let dir = state.rollout.gateway_home().join("acp-sessions");
    std::fs::create_dir_all(&dir)
        .and_then(|_| {
            std::fs::write(
                dir.join(format!("{thread}.json")),
                json!({"sessionId":session}).to_string(),
            )
        })
        .map_err(|e| GatewayError::Translation(format!("save ACP session: {e}")))
}
pub fn load_session(state: &GatewayState, thread: &str) -> Option<String> {
    uuid::Uuid::parse_str(thread).ok()?;
    let text = std::fs::read_to_string(
        state
            .rollout
            .gateway_home()
            .join("acp-sessions")
            .join(format!("{thread}.json")),
    )
    .ok()?;
    serde_json::from_str::<Value>(&text).ok()?["sessionId"]
        .as_str()
        .map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn grouped_options_and_efforts_are_preserved() {
        let option = json!({"options":[{"group":"Gemini","options":[{"value":"gemini-4-carbon","name":"Carbon"}]}]});
        assert_eq!(
            choices(&option),
            vec![("gemini-4-carbon".into(), "Carbon".into())]
        );
        assert!(valid_effort("high"));
        assert!(!valid_effort("invented"));
    }
    #[test]
    fn session_mapping_survives_new_state_and_rejects_paths() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = GatewayState::new();
        state.rollout = crate::rollout::recorder::RolloutRecorder::new(Some(dir.path().into()));
        let thread = uuid::Uuid::now_v7().to_string();
        save_session(&state, &thread, "different-acp-session").unwrap();
        assert_eq!(
            load_session(&state, &thread).as_deref(),
            Some("different-acp-session")
        );
        assert!(save_session(&state, "../../outside", "id").is_err());
    }
}

/// Only acknowledge settings that this adapter actually applies.
pub async fn update_settings(
    agent: &(impl Agent + ?Sized),
    state: &mut GatewayState,
    session: &str,
    params: &Value,
) -> Result<Value, GatewayError> {
    if let Some(fields) = params.as_object() {
        for (key, value) in fields {
            let inherited_tier = key == "serviceTier"
                && (params["model"].is_string() || params["effort"].is_string());
            if !value.is_null()
                && !inherited_tier
                && !matches!(
                    key.as_str(),
                    "threadId" | "model" | "effort" | "collaborationMode"
                )
            {
                return Err(GatewayError::Translation(format!(
                    "ACP gateway does not support changing {key}; configure the ACP agent directly"
                )));
            }
        }
    }
    if let Some(mode) = params["collaborationMode"]["mode"].as_str() {
        if mode != "default" {
            return Err(GatewayError::Translation(format!(
                "ACP gateway does not implement Codex collaboration mode {mode}"
            )));
        }
    }
    select(agent, state, session, params).await?;
    Ok(json!({}))
}
