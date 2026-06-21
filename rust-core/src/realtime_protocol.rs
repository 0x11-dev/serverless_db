use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::fmt;

pub const EVENT_PHX_JOIN: &str = "phx_join";
pub const EVENT_PHX_REPLY: &str = "phx_reply";
pub const EVENT_PHX_ERROR: &str = "phx_error";
pub const EVENT_HEARTBEAT: &str = "heartbeat";
pub const EVENT_BROADCAST: &str = "broadcast";
pub const EVENT_POSTGRES_CHANGES: &str = "postgres_changes";
pub const TOPIC_PHOENIX: &str = "phoenix";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageEnvelope<T> {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub join_ref: Option<String>,
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub ref_id: Option<String>,
    pub topic: String,
    pub event: String,
    pub payload: T,
}

impl<T> MessageEnvelope<T> {
    pub fn with_payload<U>(self, payload: U) -> MessageEnvelope<U> {
        MessageEnvelope {
            join_ref: self.join_ref,
            ref_id: self.ref_id,
            topic: self.topic,
            event: self.event,
            payload,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum RealtimeMessage {
    Join(MessageEnvelope<JoinPayload>),
    Heartbeat(MessageEnvelope<Value>),
    Broadcast(MessageEnvelope<BroadcastPayload>),
    PostgresChanges(MessageEnvelope<PostgresChangesPayload>),
    Reply(MessageEnvelope<ReplyPayload<Value>>),
    Error(MessageEnvelope<ErrorPayload>),
    Other(MessageEnvelope<Value>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct JoinPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    #[serde(default)]
    pub config: JoinConfig,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct JoinConfig {
    #[serde(default)]
    pub broadcast: BroadcastConfig,
    #[serde(default)]
    pub presence: PresenceConfig,
    #[serde(default)]
    pub postgres_changes: Vec<PostgresChangesFilter>,
    #[serde(default)]
    pub private: bool,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct BroadcastConfig {
    #[serde(default)]
    pub ack: bool,
    #[serde(default, rename = "self")]
    pub self_: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay: Option<ReplayConfig>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ReplayConfig {
    #[serde(default)]
    pub since: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<i64>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PresenceConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default)]
    pub enabled: bool,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PostgresChangesFilter {
    pub event: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BroadcastPayload {
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    pub event: String,
    #[serde(default)]
    pub payload: Value,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplyPayload<T> {
    pub status: ReplyStatus,
    pub response: T,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReplyStatus {
    Ok,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ErrorPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JoinResponse {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub postgres_changes: Vec<PostgresChangesFilter>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PostgresChangesPayload {
    pub ids: Vec<String>,
    pub data: PostgresChangeData,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PostgresChangeData {
    pub schema: String,
    pub table: String,
    #[serde(rename = "type")]
    pub event_type: PostgresChangeEvent,
    pub commit_timestamp: String,
    pub columns: Vec<PostgresColumn>,
    pub record: Value,
    pub old_record: Value,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PostgresChangeEvent {
    #[serde(rename = "INSERT")]
    Insert,
    #[serde(rename = "UPDATE")]
    Update,
    #[serde(rename = "DELETE")]
    Delete,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PostgresColumn {
    pub name: String,
    #[serde(rename = "type")]
    pub type_name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutboxEvent {
    pub id: i64,
    pub created_at: String,
    pub table: String,
    pub operation: String,
    pub row: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_sub: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_role: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolError {
    pub message: String,
}

impl ProtocolError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ProtocolError {}

impl From<serde_json::Error> for ProtocolError {
    fn from(value: serde_json::Error) -> Self {
        Self::new(format!("invalid realtime JSON: {value}"))
    }
}

pub fn parse_frame(raw: &str) -> Result<RealtimeMessage, ProtocolError> {
    let value: Value = serde_json::from_str(raw)?;
    classify_envelope(envelope_from_value(value)?)
}

pub fn parse_envelope(raw: &str) -> Result<MessageEnvelope<Value>, ProtocolError> {
    let value: Value = serde_json::from_str(raw)?;
    envelope_from_value(value)
}

pub fn encode_frame<T: Serialize>(message: &MessageEnvelope<T>) -> Result<String, ProtocolError> {
    let payload = serde_json::to_value(&message.payload)?;
    serde_json::to_string(&json!([
        message.join_ref,
        message.ref_id,
        message.topic,
        message.event,
        payload
    ]))
    .map_err(ProtocolError::from)
}

pub fn heartbeat_reply(message: &MessageEnvelope<Value>) -> MessageEnvelope<ReplyPayload<Value>> {
    reply(
        message.topic.clone(),
        message.ref_id.clone(),
        message.join_ref.clone(),
        ReplyStatus::Ok,
        json!({}),
    )
}

pub fn join_reply(
    message: &MessageEnvelope<JoinPayload>,
) -> MessageEnvelope<ReplyPayload<JoinResponse>> {
    let postgres_changes = message
        .payload
        .config
        .postgres_changes
        .iter()
        .enumerate()
        .map(|(index, filter)| filter_with_id(filter, index))
        .collect();
    reply(
        message.topic.clone(),
        message.ref_id.clone(),
        message.join_ref.clone(),
        ReplyStatus::Ok,
        JoinResponse { postgres_changes },
    )
}

pub fn error_reply(
    topic: impl Into<String>,
    ref_id: Option<String>,
    join_ref: Option<String>,
    reason: impl Into<String>,
) -> MessageEnvelope<ReplyPayload<Value>> {
    reply(
        topic.into(),
        ref_id,
        join_ref,
        ReplyStatus::Error,
        json!({ "reason": reason.into() }),
    )
}

pub fn channel_error(
    topic: impl Into<String>,
    join_ref: Option<String>,
    reason: impl Into<String>,
) -> MessageEnvelope<ErrorPayload> {
    MessageEnvelope {
        join_ref,
        ref_id: None,
        topic: topic.into(),
        event: EVENT_PHX_ERROR.to_string(),
        payload: ErrorPayload {
            reason: Some(reason.into()),
            extra: Map::new(),
        },
    }
}

pub fn outbox_event_from_value(value: Value) -> Result<OutboxEvent, ProtocolError> {
    serde_json::from_value(value).map_err(ProtocolError::from)
}

pub fn outbox_event_to_postgres_changes(
    topic: impl Into<String>,
    ids: Vec<String>,
    event: &OutboxEvent,
) -> Result<MessageEnvelope<PostgresChangesPayload>, ProtocolError> {
    let event_type = PostgresChangeEvent::from_outbox_operation(&event.operation)?;
    let (record, old_record) = match event_type {
        PostgresChangeEvent::Insert | PostgresChangeEvent::Update => {
            (event.row.clone(), Value::Null)
        }
        PostgresChangeEvent::Delete => (Value::Null, event.row.clone()),
    };
    let columns_source = if record.is_object() {
        &record
    } else {
        &old_record
    };

    Ok(MessageEnvelope {
        join_ref: None,
        ref_id: None,
        topic: topic.into(),
        event: EVENT_POSTGRES_CHANGES.to_string(),
        payload: PostgresChangesPayload {
            ids,
            data: PostgresChangeData {
                schema: "public".to_string(),
                table: event.table.clone(),
                event_type,
                commit_timestamp: event.created_at.clone(),
                columns: infer_columns(columns_source),
                record,
                old_record,
                errors: Vec::new(),
            },
        },
    })
}

fn reply<T>(
    topic: String,
    ref_id: Option<String>,
    join_ref: Option<String>,
    status: ReplyStatus,
    response: T,
) -> MessageEnvelope<ReplyPayload<T>> {
    MessageEnvelope {
        join_ref,
        ref_id,
        topic,
        event: EVENT_PHX_REPLY.to_string(),
        payload: ReplyPayload { status, response },
    }
}

fn classify_envelope(envelope: MessageEnvelope<Value>) -> Result<RealtimeMessage, ProtocolError> {
    match envelope.event.as_str() {
        EVENT_PHX_JOIN => Ok(RealtimeMessage::Join(typed_envelope(envelope)?)),
        EVENT_HEARTBEAT => Ok(RealtimeMessage::Heartbeat(envelope)),
        EVENT_BROADCAST => Ok(RealtimeMessage::Broadcast(typed_envelope(envelope)?)),
        EVENT_POSTGRES_CHANGES => Ok(RealtimeMessage::PostgresChanges(typed_envelope(envelope)?)),
        EVENT_PHX_REPLY => Ok(RealtimeMessage::Reply(typed_envelope(envelope)?)),
        EVENT_PHX_ERROR => Ok(RealtimeMessage::Error(typed_envelope(envelope)?)),
        _ => Ok(RealtimeMessage::Other(envelope)),
    }
}

fn typed_envelope<T: DeserializeOwned>(
    envelope: MessageEnvelope<Value>,
) -> Result<MessageEnvelope<T>, ProtocolError> {
    let MessageEnvelope {
        join_ref,
        ref_id,
        topic,
        event,
        payload,
    } = envelope;
    Ok(MessageEnvelope {
        join_ref,
        ref_id,
        topic,
        event,
        payload: serde_json::from_value(payload)?,
    })
}

fn envelope_from_value(value: Value) -> Result<MessageEnvelope<Value>, ProtocolError> {
    match value {
        Value::Array(items) => envelope_from_array(items),
        Value::Object(_) => serde_json::from_value(value).map_err(ProtocolError::from),
        _ => Err(ProtocolError::new(
            "realtime frame must be a Phoenix array or JSON object",
        )),
    }
}

fn envelope_from_array(items: Vec<Value>) -> Result<MessageEnvelope<Value>, ProtocolError> {
    if items.len() != 5 {
        return Err(ProtocolError::new(
            "Phoenix realtime array frame must have 5 entries",
        ));
    }
    Ok(MessageEnvelope {
        join_ref: optional_string(&items[0], "join_ref")?,
        ref_id: optional_string(&items[1], "ref")?,
        topic: required_string(&items[2], "topic")?,
        event: required_string(&items[3], "event")?,
        payload: items[4].clone(),
    })
}

fn optional_string(value: &Value, name: &str) -> Result<Option<String>, ProtocolError> {
    match value {
        Value::Null => Ok(None),
        Value::String(value) => Ok(Some(value.clone())),
        _ => Err(ProtocolError::new(format!(
            "Phoenix realtime frame {name} must be a string or null"
        ))),
    }
}

fn required_string(value: &Value, name: &str) -> Result<String, ProtocolError> {
    match value {
        Value::String(value) => Ok(value.clone()),
        _ => Err(ProtocolError::new(format!(
            "Phoenix realtime frame {name} must be a string"
        ))),
    }
}

fn filter_with_id(filter: &PostgresChangesFilter, index: usize) -> PostgresChangesFilter {
    let mut next = filter.clone();
    if next.id.is_none() {
        next.id = Some(subscription_id(filter, index));
    }
    next
}

fn subscription_id(filter: &PostgresChangesFilter, index: usize) -> String {
    let schema = filter.schema.as_deref().unwrap_or("*");
    let table = filter.table.as_deref().unwrap_or("*");
    let normalized_filter = filter.filter.as_deref().unwrap_or("*");
    format!(
        "pg:{schema}:{table}:{}:{normalized_filter}:{index}",
        filter.event
    )
}

fn infer_columns(record: &Value) -> Vec<PostgresColumn> {
    let Some(object) = record.as_object() else {
        return Vec::new();
    };
    let mut names = object.keys().cloned().collect::<Vec<_>>();
    names.sort();
    names
        .into_iter()
        .map(|name| PostgresColumn {
            name,
            type_name: "jsonb".to_string(),
            flags: Vec::new(),
        })
        .collect()
}

impl PostgresChangeEvent {
    fn from_outbox_operation(operation: &str) -> Result<Self, ProtocolError> {
        match operation.to_ascii_lowercase().as_str() {
            "insert" => Ok(Self::Insert),
            "update" => Ok(Self::Update),
            "delete" => Ok(Self::Delete),
            _ => Err(ProtocolError::new(format!(
                "unsupported postgres_changes outbox operation: {operation}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_join_payload_with_postgres_changes_filters() {
        let raw = r#"[null,"1","realtime:public:posts","phx_join",{"access_token":"jwt","config":{"broadcast":{"ack":true,"self":false},"presence":{"key":"user-1","enabled":false},"postgres_changes":[{"event":"INSERT","schema":"public","table":"posts","filter":"id=eq.1"}],"private":false}}]"#;

        let RealtimeMessage::Join(message) = parse_frame(raw).unwrap() else {
            panic!("expected join frame");
        };

        assert_eq!(message.ref_id.as_deref(), Some("1"));
        assert_eq!(message.topic, "realtime:public:posts");
        assert_eq!(message.payload.access_token.as_deref(), Some("jwt"));
        assert!(message.payload.config.broadcast.ack);
        assert_eq!(
            message.payload.config.presence.key.as_deref(),
            Some("user-1")
        );

        let filters = &message.payload.config.postgres_changes;
        assert_eq!(filters.len(), 1);
        assert_eq!(filters[0].event, "INSERT");
        assert_eq!(filters[0].schema.as_deref(), Some("public"));
        assert_eq!(filters[0].table.as_deref(), Some("posts"));
        assert_eq!(filters[0].filter.as_deref(), Some("id=eq.1"));

        let reply = join_reply(&message);
        assert_eq!(reply.payload.response.postgres_changes.len(), 1);
        assert!(reply.payload.response.postgres_changes[0].id.is_some());
    }

    #[test]
    fn encodes_heartbeat_reply_as_phoenix_reply() {
        let raw = r#"[null,"2","phoenix","heartbeat",{}]"#;
        let RealtimeMessage::Heartbeat(message) = parse_frame(raw).unwrap() else {
            panic!("expected heartbeat frame");
        };

        let reply = heartbeat_reply(&message);
        let encoded = encode_frame(&reply).unwrap();
        let value: Value = serde_json::from_str(&encoded).unwrap();

        assert_eq!(
            value,
            json!([
                null,
                "2",
                TOPIC_PHOENIX,
                EVENT_PHX_REPLY,
                { "status": "ok", "response": {} }
            ])
        );
    }

    #[test]
    fn converts_outbox_event_to_postgres_changes_payload() {
        let outbox = outbox_event_from_value(json!({
            "id": 42,
            "created_at": "2026-06-21 12:00:00",
            "table": "posts",
            "operation": "insert",
            "row": { "id": 7, "title": "hello" },
            "actor_sub": "alice",
            "actor_role": "authenticated"
        }))
        .unwrap();

        let message = outbox_event_to_postgres_changes(
            "realtime:public:posts",
            vec!["pg:public:posts:INSERT:*:0".to_string()],
            &outbox,
        )
        .unwrap();
        let value = serde_json::to_value(&message).unwrap();

        assert_eq!(message.event, EVENT_POSTGRES_CHANGES);
        assert_eq!(message.payload.ids, vec!["pg:public:posts:INSERT:*:0"]);
        assert_eq!(message.payload.data.schema, "public");
        assert_eq!(message.payload.data.table, "posts");
        assert_eq!(message.payload.data.event_type, PostgresChangeEvent::Insert);
        assert_eq!(message.payload.data.record["title"], "hello");
        assert_eq!(message.payload.data.old_record, Value::Null);
        assert_eq!(message.payload.data.commit_timestamp, "2026-06-21 12:00:00");
        assert_eq!(message.payload.data.columns[0].name, "id");
        assert_eq!(message.payload.data.columns[1].name, "title");
        assert_eq!(value["payload"]["data"]["type"], "INSERT");
    }
}
