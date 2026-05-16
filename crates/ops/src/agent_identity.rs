#![allow(dead_code)]
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::sync::Mutex;

fn state_file() -> std::path::PathBuf {
    std::env::var("OPS_STATE_FILE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("LOCALAPPDATA")
                .map(|p| std::path::PathBuf::from(p).join("Ops").join("state.json"))
                .unwrap_or_else(|_| std::path::PathBuf::from("state.json"))
        })
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct WriterIdentity {
    pub actor: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor_source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_name_source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_version_source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id_source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id_source: Option<String>,
    pub observed_at: String,
}

impl WriterIdentity {
    /// Return actor name qualified with an optional specialist role, e.g. "claude_code:reviewer".
    pub fn actor_with_role(&self, role: Option<&str>) -> String {
        match role {
            Some(r) if !r.is_empty() => format!("{}:{}", self.actor, r),
            _ => self.actor.clone(),
        }
    }
}

#[derive(Default)]
struct IdentitySeed {
    actor: Option<String>,
    actor_source: Option<String>,
    client_name: Option<String>,
    client_name_source: Option<String>,
    client_version: Option<String>,
    client_version_source: Option<String>,
    thread_id: Option<String>,
    thread_id_source: Option<String>,
    session_id: Option<String>,
    session_id_source: Option<String>,
}

static CURRENT_IDENTITY: Lazy<Mutex<WriterIdentity>> = Lazy::new(|| Mutex::new(default_identity()));
static LAST_INITIALIZE: Lazy<Mutex<Value>> = Lazy::new(|| Mutex::new(Value::Null));

fn now_rfc3339() -> String {
    chrono::Local::now().to_rfc3339()
}

fn nonempty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn normalize_actor(input: &str) -> String {
    let lower = input.trim().to_lowercase();
    if lower.is_empty() {
        return "unknown".to_string();
    }
    if lower.contains("codex") {
        return "codex".to_string();
    }
    if lower.contains("claude code") || lower.contains("claude_code") {
        return "claude_code".to_string();
    }
    if lower.contains("cowork") {
        return "cowork".to_string();
    }
    if lower.contains("claude") {
        return "claude".to_string();
    }
    if lower.contains("gemini") {
        return "gemini".to_string();
    }
    if lower.contains("chatgpt") || lower.contains("openai") || lower == "gpt" {
        return "gpt".to_string();
    }

    lower
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect::<String>()
        .trim_matches('_')
        .to_string()
}

impl IdentitySeed {
    fn set_actor_if_missing(&mut self, value: Option<String>, source: impl Into<String>) {
        if self.actor.is_none() {
            if let Some(value) = value {
                let normalized = normalize_actor(&value);
                if !normalized.is_empty() {
                    self.actor = Some(normalized);
                    self.actor_source = Some(source.into());
                }
            }
        }
    }

    fn set_if_missing(
        slot: &mut Option<String>,
        source_slot: &mut Option<String>,
        value: Option<String>,
        source: impl Into<String>,
    ) {
        if slot.is_none() {
            if let Some(value) = value {
                *slot = Some(value);
                *source_slot = Some(source.into());
            }
        }
    }

    fn adopt_existing(&mut self, identity: &WriterIdentity) {
        if identity.actor != "unknown" {
            self.actor = Some(identity.actor.clone());
            self.actor_source = identity.actor_source.clone();
        }
        if identity.client_name.is_some() {
            self.client_name = identity.client_name.clone();
            self.client_name_source = identity.client_name_source.clone();
        }
        if identity.client_version.is_some() {
            self.client_version = identity.client_version.clone();
            self.client_version_source = identity.client_version_source.clone();
        }
        if identity.thread_id.is_some() {
            self.thread_id = identity.thread_id.clone();
            self.thread_id_source = identity.thread_id_source.clone();
        }
        if identity.session_id.is_some() {
            self.session_id = identity.session_id.clone();
            self.session_id_source = identity.session_id_source.clone();
        }
    }

    fn finalize(mut self) -> WriterIdentity {
        if self.actor.is_none() {
            if let Some(client_name) = self.client_name.clone() {
                self.actor = Some(normalize_actor(&client_name));
                self.actor_source = self
                    .client_name_source
                    .clone()
                    .or_else(|| Some("derived_from_client_name".to_string()));
            }
        }

        WriterIdentity {
            actor: self.actor.unwrap_or_else(|| "unknown".to_string()),
            actor_source: self.actor_source.or_else(|| Some("default".to_string())),
            client_name: self.client_name,
            client_name_source: self.client_name_source,
            client_version: self.client_version,
            client_version_source: self.client_version_source,
            thread_id: self.thread_id,
            thread_id_source: self.thread_id_source,
            session_id: self.session_id,
            session_id_source: self.session_id_source,
            observed_at: now_rfc3339(),
        }
    }
}

fn env_first_with_key(keys: &[&str]) -> Option<(String, String)> {
    for key in keys {
        if let Ok(value) = std::env::var(key) {
            if let Some(value) = nonempty(&value) {
                return Some((value, format!("env.{}", key)));
            }
        }
    }
    None
}

fn obj_string(value: &Value, keys: &[&str]) -> Option<(String, String)> {
    let obj = value.as_object()?;
    for key in keys {
        if let Some(candidate) = obj.get(*key).and_then(|v| v.as_str()) {
            if let Some(candidate) = nonempty(candidate) {
                return Some((candidate, (*key).to_string()));
            }
        }
    }
    None
}

fn apply_writer_value(seed: &mut IdentitySeed, value: Option<&Value>, prefix: &str) {
    let Some(value) = value else {
        return;
    };

    if let Some((actor, field)) = obj_string(value, &["actor"]) {
        seed.set_actor_if_missing(Some(actor), format!("{}.{}", prefix, field));
    }
    if let Some((client_name, field)) = obj_string(value, &["client_name", "clientName"]) {
        IdentitySeed::set_if_missing(
            &mut seed.client_name,
            &mut seed.client_name_source,
            Some(client_name),
            format!("{}.{}", prefix, field),
        );
    }
    if let Some((client_version, field)) = obj_string(value, &["client_version", "clientVersion"]) {
        IdentitySeed::set_if_missing(
            &mut seed.client_version,
            &mut seed.client_version_source,
            Some(client_version),
            format!("{}.{}", prefix, field),
        );
    }
    if let Some((thread_id, field)) = obj_string(value, &["thread_id", "threadId"]) {
        IdentitySeed::set_if_missing(
            &mut seed.thread_id,
            &mut seed.thread_id_source,
            Some(thread_id),
            format!("{}.{}", prefix, field),
        );
    }
    if let Some((session_id, field)) = obj_string(value, &["session_id", "sessionId"]) {
        IdentitySeed::set_if_missing(
            &mut seed.session_id,
            &mut seed.session_id_source,
            Some(session_id),
            format!("{}.{}", prefix, field),
        );
    }
}

fn load_state_json() -> Option<Value> {
    let content = fs::read_to_string(state_file()).ok()?;
    serde_json::from_str::<Value>(&content).ok()
}

fn extract_state_identity(state: &Value) -> IdentitySeed {
    let mut seed = IdentitySeed::default();

    if let Some(session) = state.get("session") {
        if let Some((actor, field)) = obj_string(session, &["current_agent"]) {
            seed.set_actor_if_missing(Some(actor), format!("state.session.{}", field));
        }
        if let Some((thread_id, field)) = obj_string(session, &["thread_id", "threadId"]) {
            IdentitySeed::set_if_missing(
                &mut seed.thread_id,
                &mut seed.thread_id_source,
                Some(thread_id),
                format!("state.session.{}", field),
            );
        }
        if let Some((session_id, field)) =
            obj_string(session, &["client_session_id", "session_id", "sessionId"])
        {
            IdentitySeed::set_if_missing(
                &mut seed.session_id,
                &mut seed.session_id_source,
                Some(session_id),
                format!("state.session.{}", field),
            );
        }
    }

    apply_writer_value(
        &mut seed,
        state.pointer("/session/last_writer"),
        "state.session.last_writer",
    );
    apply_writer_value(
        &mut seed,
        state.pointer("/operation/last_writer"),
        "state.operation.last_writer",
    );
    apply_writer_value(
        &mut seed,
        state.pointer("/_meta/last_writer"),
        "state._meta.last_writer",
    );
    seed
}

fn apply_state_fallback(seed: &mut IdentitySeed) {
    if let Some(state) = load_state_json() {
        let state_seed = extract_state_identity(&state);
        if let Some(actor) = state_seed.actor {
            seed.set_actor_if_missing(
                Some(actor),
                state_seed
                    .actor_source
                    .unwrap_or_else(|| "state".to_string()),
            );
        }
        IdentitySeed::set_if_missing(
            &mut seed.client_name,
            &mut seed.client_name_source,
            state_seed.client_name,
            state_seed
                .client_name_source
                .unwrap_or_else(|| "state.client_name".to_string()),
        );
        IdentitySeed::set_if_missing(
            &mut seed.client_version,
            &mut seed.client_version_source,
            state_seed.client_version,
            state_seed
                .client_version_source
                .unwrap_or_else(|| "state.client_version".to_string()),
        );
        IdentitySeed::set_if_missing(
            &mut seed.thread_id,
            &mut seed.thread_id_source,
            state_seed.thread_id,
            state_seed
                .thread_id_source
                .unwrap_or_else(|| "state.thread_id".to_string()),
        );
        IdentitySeed::set_if_missing(
            &mut seed.session_id,
            &mut seed.session_id_source,
            state_seed.session_id,
            state_seed
                .session_id_source
                .unwrap_or_else(|| "state.session_id".to_string()),
        );
    }
}

fn apply_env_fallback(seed: &mut IdentitySeed) {
    if let Some((actor, source)) = env_first_with_key(&["OPS_AGENT", "CODEX_AGENT", "AI_AGENT"]) {
        seed.set_actor_if_missing(Some(actor), source);
    }
    if let Some((client_name, source)) =
        env_first_with_key(&["CODEX_CLIENT_NAME", "AI_CLIENT_NAME", "OPS_AGENT"])
    {
        IdentitySeed::set_if_missing(
            &mut seed.client_name,
            &mut seed.client_name_source,
            Some(client_name),
            source,
        );
    }
    if let Some((client_version, source)) =
        env_first_with_key(&["CODEX_CLIENT_VERSION", "AI_CLIENT_VERSION"])
    {
        IdentitySeed::set_if_missing(
            &mut seed.client_version,
            &mut seed.client_version_source,
            Some(client_version),
            source,
        );
    }
    if let Some((thread_id, source)) =
        env_first_with_key(&["CODEX_THREAD_ID", "OPS_THREAD_ID", "AI_THREAD_ID"])
    {
        IdentitySeed::set_if_missing(
            &mut seed.thread_id,
            &mut seed.thread_id_source,
            Some(thread_id),
            source,
        );
    }
    if let Some((session_id, source)) =
        env_first_with_key(&["CODEX_SESSION_ID", "OPS_SESSION_ID", "AI_SESSION_ID"])
    {
        IdentitySeed::set_if_missing(
            &mut seed.session_id,
            &mut seed.session_id_source,
            Some(session_id),
            source,
        );
    }
}

fn apply_initialize(seed: &mut IdentitySeed, params: Option<&Value>) {
    let Some(params) = params else {
        return;
    };

    if let Some((actor, field)) = obj_string(params, &["actor"]) {
        seed.actor = Some(normalize_actor(&actor));
        seed.actor_source = Some(format!("initialize.{}", field));
    }

    if let Some(client_info) = params.get("clientInfo") {
        if let Some((client_name, field)) = obj_string(client_info, &["name"]) {
            seed.client_name = Some(client_name);
            seed.client_name_source = Some(format!("initialize.clientInfo.{}", field));
        }
        if let Some((client_version, field)) = obj_string(client_info, &["version"]) {
            seed.client_version = Some(client_version);
            seed.client_version_source = Some(format!("initialize.clientInfo.{}", field));
        }
    }

    if let Some((thread_id, field)) = obj_string(params, &["thread_id", "threadId"]) {
        seed.thread_id = Some(thread_id);
        seed.thread_id_source = Some(format!("initialize.{}", field));
    }
    if let Some((session_id, field)) = obj_string(params, &["session_id", "sessionId"]) {
        seed.session_id = Some(session_id);
        seed.session_id_source = Some(format!("initialize.{}", field));
    }
}

fn apply_args(seed: &mut IdentitySeed, args: Option<&Value>) {
    let Some(args) = args else {
        return;
    };

    if let Some((actor, field)) = obj_string(args, &["actor"]) {
        seed.actor = Some(normalize_actor(&actor));
        seed.actor_source = Some(format!("args.{}", field));
    }
    if let Some((client_name, field)) = obj_string(args, &["client_name", "clientName"]) {
        seed.client_name = Some(client_name);
        seed.client_name_source = Some(format!("args.{}", field));
    }
    if let Some((client_version, field)) = obj_string(args, &["client_version", "clientVersion"]) {
        seed.client_version = Some(client_version);
        seed.client_version_source = Some(format!("args.{}", field));
    }
    if let Some((thread_id, field)) = obj_string(args, &["thread_id", "threadId"]) {
        seed.thread_id = Some(thread_id);
        seed.thread_id_source = Some(format!("args.{}", field));
    }
    if let Some((session_id, field)) = obj_string(args, &["session_id", "sessionId"]) {
        seed.session_id = Some(session_id);
        seed.session_id_source = Some(format!("args.{}", field));
    }
}

fn summarize_initialize(value: &Value) -> Value {
    if value.is_null() {
        return Value::Null;
    }

    json!({
        "actor": value.get("actor").and_then(|v| v.as_str()),
        "thread_id": value.get("thread_id").and_then(|v| v.as_str())
            .or_else(|| value.get("threadId").and_then(|v| v.as_str())),
        "session_id": value.get("session_id").and_then(|v| v.as_str())
            .or_else(|| value.get("sessionId").and_then(|v| v.as_str())),
        "client_name": value.pointer("/clientInfo/name").and_then(|v| v.as_str()),
        "client_version": value.pointer("/clientInfo/version").and_then(|v| v.as_str())
    })
}

fn environment_snapshot() -> Value {
    json!({
        "actor": std::env::var("OPS_AGENT").ok()
            .or_else(|| std::env::var("CODEX_AGENT").ok())
            .or_else(|| std::env::var("AI_AGENT").ok()),
        "client_name": std::env::var("CODEX_CLIENT_NAME").ok()
            .or_else(|| std::env::var("AI_CLIENT_NAME").ok()),
        "client_version": std::env::var("CODEX_CLIENT_VERSION").ok()
            .or_else(|| std::env::var("AI_CLIENT_VERSION").ok()),
        "thread_id": std::env::var("CODEX_THREAD_ID").ok()
            .or_else(|| std::env::var("OPS_THREAD_ID").ok())
            .or_else(|| std::env::var("AI_THREAD_ID").ok()),
        "session_id": std::env::var("CODEX_SESSION_ID").ok()
            .or_else(|| std::env::var("OPS_SESSION_ID").ok())
            .or_else(|| std::env::var("AI_SESSION_ID").ok())
    })
}

fn state_snapshot() -> Value {
    let Some(state) = load_state_json() else {
        return json!({ "status": "missing" });
    };

    json!({
        "status": "ok",
        "session": {
            "current_agent": state.pointer("/session/current_agent").cloned().unwrap_or(Value::Null),
            "thread_id": state.pointer("/session/thread_id").cloned().unwrap_or(Value::Null),
            "client_session_id": state.pointer("/session/client_session_id").cloned().unwrap_or(Value::Null),
            "contributors": state.pointer("/session/contributors").cloned().unwrap_or_else(|| json!([])),
            "last_writer": state.pointer("/session/last_writer").cloned().unwrap_or(Value::Null)
        },
        "meta_last_writer": state.pointer("/_meta/last_writer").cloned().unwrap_or(Value::Null)
    })
}

fn default_identity() -> WriterIdentity {
    let mut seed = IdentitySeed::default();
    apply_state_fallback(&mut seed);
    apply_env_fallback(&mut seed);
    seed.finalize()
}

fn remember_improved_identity(resolved: &WriterIdentity) {
    let mut current = CURRENT_IDENTITY.lock().unwrap();

    if current.actor == "unknown" && resolved.actor != "unknown" {
        current.actor = resolved.actor.clone();
        current.actor_source = resolved.actor_source.clone();
    }
    if current.client_name.is_none() && resolved.client_name.is_some() {
        current.client_name = resolved.client_name.clone();
        current.client_name_source = resolved.client_name_source.clone();
    }
    if current.client_version.is_none() && resolved.client_version.is_some() {
        current.client_version = resolved.client_version.clone();
        current.client_version_source = resolved.client_version_source.clone();
    }
    if current.thread_id.is_none() && resolved.thread_id.is_some() {
        current.thread_id = resolved.thread_id.clone();
        current.thread_id_source = resolved.thread_id_source.clone();
    }
    if current.session_id.is_none() && resolved.session_id.is_some() {
        current.session_id = resolved.session_id.clone();
        current.session_id_source = resolved.session_id_source.clone();
    }
    current.observed_at = resolved.observed_at.clone();
}

pub fn set_from_initialize(params: Option<&Value>) {
    let mut seed = IdentitySeed::default();
    apply_state_fallback(&mut seed);
    apply_env_fallback(&mut seed);
    apply_initialize(&mut seed, params);

    let identity = seed.finalize();
    *CURRENT_IDENTITY.lock().unwrap() = identity;
    *LAST_INITIALIZE.lock().unwrap() = params.cloned().unwrap_or(Value::Null);
}

pub fn identity_from_args(args: Option<&Value>) -> WriterIdentity {
    let current = CURRENT_IDENTITY.lock().unwrap().clone();
    let mut seed = IdentitySeed::default();
    apply_state_fallback(&mut seed);
    apply_env_fallback(&mut seed);
    seed.adopt_existing(&current);
    apply_args(&mut seed, args);

    let identity = seed.finalize();
    remember_improved_identity(&identity);
    identity
}

pub fn identity_json(args: Option<&Value>) -> Value {
    serde_json::to_value(identity_from_args(args)).unwrap_or_else(|_| {
        json!({
            "actor": "unknown",
            "actor_source": "default",
            "observed_at": now_rfc3339()
        })
    })
}

pub fn identity_status_json(args: Option<&Value>) -> Value {
    let effective = identity_from_args(args);
    let current = CURRENT_IDENTITY.lock().unwrap().clone();
    let initialize = LAST_INITIALIZE.lock().unwrap().clone();

    let mut notes = Vec::new();
    if effective.thread_id.is_none() {
        notes.push("thread_id unresolved");
    }
    if effective.session_id.is_none() {
        notes.push("session_id unresolved");
    }

    json!({
        "effective": effective,
        "current": current,
        "initialize": summarize_initialize(&initialize),
        "environment": environment_snapshot(),
        "state_fallback": state_snapshot(),
        "notes": notes
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_actor_distinguishes_known_clients() {
        assert_eq!(normalize_actor("Codex Desktop"), "codex");
        assert_eq!(normalize_actor("Claude Code"), "claude_code");
        assert_eq!(normalize_actor("Claude"), "claude");
        assert_eq!(normalize_actor("Cowork"), "cowork");
    }

    #[test]
    fn state_identity_prefers_session_fields() {
        let state = json!({
            "session": {
                "current_agent": "codex",
                "thread_id": "thread-123",
                "client_session_id": "session-456",
                "last_writer": {
                    "actor": "claude",
                    "thread_id": "thread-old",
                    "session_id": "session-old"
                }
            },
            "_meta": {
                "last_writer": {
                    "actor": "gemini"
                }
            }
        });

        let extracted = extract_state_identity(&state).finalize();
        assert_eq!(extracted.actor, "codex");
        assert_eq!(extracted.thread_id.as_deref(), Some("thread-123"));
        assert_eq!(extracted.session_id.as_deref(), Some("session-456"));
    }
}
