//! Native Muse MSP backend. ACP types are only the gateway's internal agent interface;
//! the host runs `muse serve` and all wire traffic uses the official Rust MSP SDK.
use crate::acp::AcpEvent;
use agent_client_protocol::*;
use futures::StreamExt;
use muse_code_sdk::{
    connection::{Connection, StdioTransport},
    spawn::{MuseChild, SpawnOptions, spawn_muse},
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::{cell::RefCell, collections::HashMap, path::Path};
use tokio::sync::mpsc;

fn error(e: impl std::fmt::Display) -> Error {
    Error::internal_error().data(e.to_string())
}
fn decode<T: DeserializeOwned>(v: Value) -> Result<T> {
    serde_json::from_value(v).map_err(error)
}
fn command(mut v: Value) -> Value {
    v["commandId"] = json!(uuid::Uuid::now_v7().to_string());
    v
}
async fn rpc(c: &Connection, method: &str, v: Value) -> Result<Value> {
    c.request(method, v.as_object().cloned())
        .await
        .map(Value::Object)
        .map_err(error)
}
fn approval_options(choices: &[Value]) -> Vec<Value> {
    choices.iter().filter(|c|c["scope"]=="once" && matches!(c["decision"].as_str(),Some("approved"|"denied"|"abort")))
        .map(|c|json!({"optionId":c["choiceId"],"name":c["label"].as_str().unwrap_or("Review"),"kind":if c["decision"]=="approved" {"allow_once"}else{"reject_once"}})).collect()
}
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Session {
    model: String,
    effort: String,
    turn: Option<String>,
    #[serde(default)]
    cancel_requested: bool,
}
pub struct MuseAgent {
    connection: Connection,
    events: mpsc::UnboundedSender<AcpEvent>,
    models: RefCell<Vec<Value>>,
    sessions: RefCell<HashMap<String, Session>>,
}
pub async fn connect(
    cmd: &str,
    cwd: &Path,
) -> anyhow::Result<(
    MuseAgent,
    (Connection, MuseChild),
    mpsc::UnboundedReceiver<AcpEvent>,
)> {
    let mut options = SpawnOptions::default();
    options.muse_bin = Some(cmd.into());
    options.cwd = Some(cwd.into());
    let mut host = spawn_muse(options)?;
    let (connection, driver) = Connection::new(StdioTransport::new(
        host.take_stdin().unwrap(),
        host.take_stdout().unwrap(),
    ));
    tokio::spawn(driver.run());
    let (events, rx) = mpsc::unbounded_channel();
    Ok((
        MuseAgent {
            connection: connection.clone(),
            events,
            models: RefCell::new(vec![]),
            sessions: RefCell::new(HashMap::new()),
        },
        (connection, host),
        rx,
    ))
}
impl MuseAgent {
    fn settings_path(id: &str) -> Result<std::path::PathBuf> {
        uuid::Uuid::parse_str(id).map_err(error)?;
        Ok(crate::rollout::recorder::RolloutRecorder::default()
            .gateway_home()
            .join("msp-sessions")
            .join(format!("{id}.json")))
    }
    fn save(&self, id: &str) -> Result<()> {
        let path = Self::settings_path(id)?;
        std::fs::create_dir_all(path.parent().unwrap()).map_err(error)?;
        let tmp = path.with_extension("tmp");
        let sessions = self.sessions.borrow();
        std::fs::write(
            &tmp,
            serde_json::to_vec(
                sessions
                    .get(id)
                    .ok_or_else(|| error("Unknown Muse session"))?,
            )
            .map_err(error)?,
        )
        .map_err(error)?;
        std::fs::rename(tmp, path).map_err(error)
    }
    fn default_effort(&self, model: &str) -> String {
        self.models
            .borrow()
            .iter()
            .find(|m| m["modelId"] == model)
            .and_then(|m| m["defaultReasoningEffort"].as_str())
            .unwrap_or("high")
            .into()
    }
    fn options(&self, session: &str) -> Result<Value> {
        let sessions = self.sessions.borrow();
        let s = sessions
            .get(session)
            .ok_or_else(|| error("Unknown Muse session"))?;
        let models = self.models.borrow();
        let row = models
            .iter()
            .find(|m| m["modelId"] == s.model)
            .ok_or_else(|| error("Unknown Muse model"))?;
        let choices: Vec<_> = models
            .iter()
            .map(|m| json!({"value":m["modelId"],"name":m["displayLabel"]}))
            .collect();
        let efforts: Vec<_> = row["variants"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|e| json!({"value":e,"name":e}))
            .collect();
        let per_model: serde_json::Map<String, Value> = models
            .iter()
            .map(|m| {
                (
                    m["modelId"].as_str().unwrap().into(),
                    json!({"variants":m["variants"],"default":m["defaultReasoningEffort"]}),
                )
            })
            .collect();
        Ok(json!([
            {"id":"model","name":"Model","category":"model","type":"select","currentValue":s.model,"options":choices,"_meta":{"modelEfforts":per_model}},
            {"id":"reasoning_effort","name":"Reasoning effort","category":"thought_level","type":"select","currentValue":s.effort,"options":efforts}
        ]))
    }
    fn update(&self, id: &str, update: Value) -> Result<()> {
        self.events
            .send(AcpEvent::SessionNotification(decode(
                json!({"sessionId":id,"update":update}),
            )?))
            .map_err(error)
    }
    async fn approval(&self, p: Value) -> Result<()> {
        // Only offer choices the host supplied. Never infer an allow decision.
        let choices = p["availableChoices"]
            .as_array()
            .ok_or_else(|| error("Muse approval missing choices"))?;
        let options = approval_options(choices);
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.events.send(AcpEvent::PermissionRequest{request:decode(json!({"sessionId":p["sessionId"],"toolCall":{"toolCallId":p["itemId"],"title":format!("{} {}",p["toolName"].as_str().unwrap_or("Muse tool"),p["subject"]["command"].as_str().or_else(||p["rawArgs"].as_str()).unwrap_or("")),"kind":"other","status":"pending","rawInput":p["rawArgs"]},"options":options}))?,response_tx:tx}).map_err(error)?;
        let response = serde_json::to_value(rx.await.map_err(error)?).map_err(error)?;
        let choice = response["outcome"]["optionId"]
            .as_str()
            .map(String::from)
            .or_else(|| {
                choices
                    .iter()
                    .find(|c| matches!(c["decision"].as_str(), Some("denied" | "abort")))
                    .and_then(|c| c["choiceId"].as_str())
                    .map(String::from)
            })
            .ok_or_else(|| error("Muse approval cancelled; no deny choice"))?;
        if !options.iter().any(|c| c["optionId"] == choice) {
            return Err(error("Unknown approval choice"));
        }
        rpc(&self.connection,"approval/decide",command(json!({"sessionId":p["sessionId"],"approvalId":p["approvalId"],"requirementId":p["currentRequirementId"],"choiceId":choice}))).await?;
        Ok(())
    }
}
#[async_trait::async_trait(?Send)]
impl Agent for MuseAgent {
    async fn initialize(&self, _: InitializeRequest) -> Result<InitializeResponse> {
        let init=muse_code_sdk::handshake::Handshake::new(self.connection.clone()).initialize(
            decode(json!({"clientInfo":{"name":"codex_acp_gateway","version":env!("CARGO_PKG_VERSION")},"capabilities":{}}))?
        ).await.map_err(error)?;
        if let Some(warning) = init.fingerprint_warning {
            tracing::warn!(expected=%warning.expected,actual=%warning.actual,"Muse SDK schema differs from installed host");
        }
        let models = rpc(&self.connection, "model/list", json!({})).await?;
        *self.models.borrow_mut() = models["models"]
            .as_array()
            .cloned()
            .ok_or_else(|| error("Muse returned no model catalog"))?;
        decode(
            json!({"protocolVersion":1,"agentCapabilities":{"loadSession":true},"agentInfo":{"name":"muse-msp","version":"1.4"}}),
        )
    }
    async fn authenticate(&self, _: AuthenticateRequest) -> Result<AuthenticateResponse> {
        Err(error("Run muse login in your terminal"))
    }
    async fn new_session(&self, args: NewSessionRequest) -> Result<NewSessionResponse> {
        let r = rpc(
            &self.connection,
            "session/start",
            command(json!({"workspaceRoots":[args.cwd.canonicalize().map_err(error)?]})),
        )
        .await?;
        let id = r["session"]["sessionId"]
            .as_str()
            .ok_or_else(|| error(format!("Missing Muse session: {r}")))?
            .to_string();
        let model = r["session"]["modelId"]
            .as_str()
            .map(String::from)
            .unwrap_or_else(|| self.models.borrow()[0]["modelId"].as_str().unwrap().into());
        let effort = self.default_effort(&model);
        rpc(
            &self.connection,
            "session/setReasoningEffort",
            command(json!({"sessionId":id,"reasoningEffort":effort})),
        )
        .await?;
        self.sessions.borrow_mut().insert(
            id.clone(),
            Session {
                model,
                effort,
                turn: None,
                cancel_requested: false,
            },
        );
        self.save(&id)?;
        decode(json!({"sessionId":id,"configOptions":self.options(&id)?}))
    }
    async fn load_session(&self, args: LoadSessionRequest) -> Result<LoadSessionResponse> {
        let id = args.session_id.to_string();
        let r = rpc(
            &self.connection,
            "session/resume",
            command(json!({"sessionId":id,"excludeItems":true})),
        )
        .await?;
        let model: String = r["session"]["modelId"]
            .as_str()
            .ok_or_else(|| error("Muse resume missing model"))?
            .into();
        // MSP snapshots omit effort. Reapply our last acknowledged setting on resume.
        let saved = std::fs::read(Self::settings_path(&id)?)
            .ok()
            .and_then(|b| serde_json::from_slice::<Session>(&b).ok());
        let effort = saved
            .map(|s| s.effort)
            .unwrap_or_else(|| self.default_effort(&model));
        rpc(
            &self.connection,
            "session/setReasoningEffort",
            command(json!({"sessionId":id,"reasoningEffort":effort})),
        )
        .await?;
        self.sessions.borrow_mut().insert(
            id.clone(),
            Session {
                model,
                effort,
                turn: None,
                cancel_requested: false,
            },
        );
        decode(json!({"configOptions":self.options(&id)?}))
    }
    async fn set_session_config_option(
        &self,
        args: SetSessionConfigOptionRequest,
    ) -> Result<SetSessionConfigOptionResponse> {
        let v = serde_json::to_value(args).map_err(error)?;
        let id = v["sessionId"].as_str().unwrap();
        let value = v["value"].as_str().unwrap();
        match v["configId"].as_str() {
            Some("model") => {
                rpc(
                    &self.connection,
                    "session/setModel",
                    command(json!({"sessionId":id,"model":{"modelId":value}})),
                )
                .await?;
                self.sessions
                    .borrow_mut()
                    .get_mut(id)
                    .ok_or_else(|| error("Unknown Muse session"))?
                    .model = value.into();
            }
            Some("reasoning_effort") => {
                rpc(
                    &self.connection,
                    "session/setReasoningEffort",
                    command(json!({"sessionId":id,"reasoningEffort":value})),
                )
                .await?;
                self.sessions
                    .borrow_mut()
                    .get_mut(id)
                    .ok_or_else(|| error("Unknown Muse session"))?
                    .effort = value.into();
            }
            _ => return Err(error("Unsupported Muse option")),
        }
        self.save(id)?;
        decode(json!({"configOptions":self.options(id)?}))
    }
    async fn prompt(&self, args: PromptRequest) -> Result<PromptResponse> {
        let id = args.session_id.to_string();
        let input: Vec<Value> = args
            .prompt
            .into_iter()
            .map(|c| serde_json::to_value(c).map_err(error))
            .collect::<Result<_>>()?;
        if input.iter().any(|c| c["type"] != "text") {
            return Err(error("Muse MSP gateway currently accepts text input only"));
        }
        let mut notifications = self.connection.subscribe_notifications();
        let ack = rpc(
            &self.connection,
            "turn/start",
            command(json!({"sessionId":id,"input":input})),
        )
        .await?;
        let turn = ack["turnId"]
            .as_str()
            .ok_or_else(|| error(format!("Muse turn not accepted: {ack}")))?
            .to_string();
        self.sessions.borrow_mut().get_mut(&id).unwrap().turn = Some(turn.clone());
        let cancelled = self
            .sessions
            .borrow()
            .get(&id)
            .is_some_and(|s| s.cancel_requested);
        if cancelled {
            rpc(
                &self.connection,
                "turn/cancel",
                command(json!({"sessionId":id,"turnId":turn})),
            )
            .await?;
        }
        let mut kinds = HashMap::<String, String>::new();
        let mut streamed = HashMap::<String, String>::new();
        let mut approvals = futures::stream::FuturesUnordered::new();
        let outcome=async {loop {
            // Fail visibly on notification loss; never report a partial turn as successful.
            let n=tokio::select! {
                result=approvals.next(), if !approvals.is_empty()=>{result.unwrap()?;continue;},
                result=notifications.recv()=>result.map_err(error)?,
            };
            let p=Value::Object(n.params.unwrap_or_default());
            if p["sessionId"]!=id {continue;}
            match n.method.as_str(){
                "item/started"|"item/updated"|"item/completed"=>{
                    let item=&p["item"];let item_id=item["itemId"].as_str().unwrap_or("");let kind=item["kind"].as_str().unwrap_or("");
                    kinds.insert(item_id.into(),kind.into());
                    if kind=="toolCall" {
                        let update=if n.method=="item/started"{"tool_call"}else{"tool_call_update"};
                        let status=match item["status"].as_str(){Some("inProgress")=>"in_progress",Some("completed")=>"completed",_=>"failed"};
                        self.update(&id,json!({"sessionUpdate":update,"toolCallId":item_id,"title":item["tool"].as_str().unwrap_or("Muse tool"),"kind":"other","status":status,"rawInput":item["args"].as_str().and_then(|s|serde_json::from_str::<Value>(s).ok()).unwrap_or(item["args"].clone()),"rawOutput":item["visibleOutput"],"content":[]}))?;
                    }
                    if n.method=="item/completed" && matches!(kind,"agentMessage"|"reasoning") {
                        let full=item["text"].as_str().unwrap_or("");let prior=streamed.get(item_id).map(String::as_str).unwrap_or("");
                        if let Some(tail)=full.strip_prefix(prior).filter(|s|!s.is_empty()){
                            self.update(&id,json!({"sessionUpdate":if kind=="agentMessage"{"agent_message_chunk"}else{"agent_thought_chunk"},"content":{"type":"text","text":tail}}))?;
                        } else if !full.is_empty() && !full.starts_with(prior) {return Err(error("Muse final text differed from streamed text"));}
                    }
                }
                "item/delta"=>{
                    let item=p["itemId"].as_str().unwrap_or("");let kind=kinds.get(item).map(String::as_str).unwrap_or("");
                    if matches!(kind,"agentMessage"|"reasoning") && p["field"].as_str().unwrap_or("text")=="text" {
                        let delta=p["delta"].as_str().unwrap_or("");streamed.entry(item.into()).or_default().push_str(delta);
                        self.update(&id,json!({"sessionUpdate":if kind=="agentMessage"{"agent_message_chunk"}else{"agent_thought_chunk"},"content":{"type":"text","text":delta}}))?;
                    }
                }
                "approval/requested"|"approval/request"=>{approvals.push(Box::pin(self.approval(p)));}
                "turn/completed" if p["turnId"]==turn=>{
                    return match p["terminal"].as_str(){Some("completed")=>Ok(PromptResponse::new(StopReason::EndTurn)),Some("cancelled")=>Ok(PromptResponse::new(StopReason::Cancelled)),_=>Err(error(format!("Muse turn failed: {}",p["error"])))};
                }
                _=>{}
            }
        }}.await;
        {
            let mut sessions = self.sessions.borrow_mut();
            let session = sessions.get_mut(&id).unwrap();
            session.turn = None;
            session.cancel_requested = false;
        }
        if outcome.is_err() {
            let _ = rpc(
                &self.connection,
                "turn/cancel",
                command(json!({"sessionId":id,"turnId":turn})),
            )
            .await;
        }
        outcome
    }
    async fn cancel(&self, args: CancelNotification) -> Result<()> {
        let id = args.session_id.to_string();
        let turn = {
            let mut sessions = self.sessions.borrow_mut();
            let session = sessions
                .get_mut(&id)
                .ok_or_else(|| error("Unknown Muse session"))?;
            session.cancel_requested = true;
            session.turn.clone()
        };
        if let Some(turn) = turn {
            rpc(
                &self.connection,
                "turn/cancel",
                command(json!({"sessionId":id,"turnId":turn})),
            )
            .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn approval_only_offers_host_supplied_once_choices() {
        let choices = json!([
            {"choiceId":"once","label":"Allow once","scope":"once","decision":"approved"},
            {"choiceId":"persistent","label":"Always","scope":"localPersistent","decision":"approvedPolicyAmendment"},
            {"choiceId":"reject","label":"Reject","scope":"once","decision":"denied"},
            {"choiceId":"future","label":"Unknown","scope":"once","decision":"newDecision"}
        ]);
        let options = approval_options(choices.as_array().unwrap());
        assert_eq!(options.len(), 2);
        assert_eq!(options[0]["kind"], "allow_once");
        assert_eq!(options[1]["optionId"], "reject");
        let typed: Vec<PermissionOption> = decode(json!(options)).unwrap();
        let response = crate::translation::approval::translate_codex_approval_to_acp(
            &json!({"decision":"decline"}),
            &typed,
        );
        assert_eq!(
            serde_json::to_value(response).unwrap()["outcome"]["optionId"],
            "reject"
        );
    }
    #[test]
    fn session_settings_reject_path_traversal() {
        assert!(MuseAgent::settings_path("../../outside").is_err());
    }
}
