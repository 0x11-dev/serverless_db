use crate::runtime::ApiError;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn snapshot_key(project_id: &str) -> String {
    format!("projects/{project_id}/database.sqlite")
}

pub fn snapshot_generation_key(project_id: &str, generation: u64) -> String {
    format!("projects/{project_id}/snapshots/{generation:020}.sqlite")
}

pub fn manifest_key(project_id: &str) -> String {
    format!("projects/{project_id}/manifest.json")
}

pub fn writer_lease_key(project_id: &str) -> String {
    format!("projects/{project_id}/writer-lease.json")
}

pub fn writer_lease_claim_key(project_id: &str, fencing_token: u64) -> String {
    format!("projects/{project_id}/writer-lease-claims/{fencing_token:020}.json")
}

pub fn bookmark_for_seq(seq: u64) -> String {
    format!("sdb1-{seq:020}")
}

pub fn bookmark_to_seq(bookmark: &str) -> Result<u64, ApiError> {
    let raw = bookmark.trim();
    let seq = raw
        .strip_prefix("sdb1-")
        .ok_or_else(|| ApiError::new(400, format!("invalid bookmark: {bookmark}")))?;
    seq.parse::<u64>()
        .map_err(|_| ApiError::new(400, format!("invalid bookmark: {bookmark}")))
}

pub fn with_bookmark(mut value: Value, bookmark: &str) -> Value {
    match &mut value {
        Value::Object(map) => {
            map.insert("bookmark".to_string(), Value::String(bookmark.to_string()));
            value
        }
        _ => json!({ "value": value, "bookmark": bookmark }),
    }
}

pub fn wal_prefix(project_id: &str) -> String {
    format!("projects/{project_id}/wal")
}

pub fn wal_segment_key(project_id: &str, segment_id: u64) -> String {
    format!("{}/{segment_id:020}.wal", wal_prefix(project_id))
}

pub fn wal_segment_id(key: &str) -> Option<u64> {
    key.rsplit('/')
        .next()
        .and_then(|name| name.strip_suffix(".wal"))
        .and_then(|id| id.parse::<u64>().ok())
}

pub fn storage_key(project_id: &str, bucket: &str, key: &str) -> String {
    format!("projects/{project_id}/storage/{bucket}/{key}")
}

pub fn change_log_key(project_id: &str, ops_since_snapshot: u64, reason: &str) -> String {
    let reason = reason
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect::<String>();
    format!(
        "projects/{project_id}/change-log/{}-{ops_since_snapshot:020}-{reason}.json",
        now_ms()
    )
}

pub fn read_file_range(path: &Path, offset: u64) -> Result<Vec<u8>, ApiError> {
    let mut file = fs::File::open(path).map_err(anyhow::Error::from)?;
    file.seek(SeekFrom::Start(offset))
        .map_err(anyhow::Error::from)?;
    let mut out = Vec::new();
    file.read_to_end(&mut out).map_err(anyhow::Error::from)?;
    Ok(out)
}

pub fn hex_sha256(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

pub fn sha256_file(path: &Path) -> Result<String, ApiError> {
    let data = fs::read(path).map_err(anyhow::Error::from)?;
    Ok(hex_sha256(&data))
}

pub fn utc_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    iso_from_unix(secs)
}

pub fn iso_from_unix(secs: i64) -> String {
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let h = rem / 3600;
    let mi = (rem % 3600) / 60;
    let s = rem % 60;
    let mut y = 1970i64;
    let mut remaining = days;
    loop {
        let dy = if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
            366
        } else {
            365
        };
        if remaining < dy as i64 {
            break;
        }
        remaining -= dy as i64;
        y += 1;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let months = if leap {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut mo = 0u32;
    for (i, &dm) in months.iter().enumerate() {
        if remaining < dm as i64 {
            mo = (i + 1) as u32;
            break;
        }
        remaining -= dm as i64;
    }
    format!("{y:04}-{mo:02}-{:02}T{h:02}:{mi:02}:{s:02}Z", remaining + 1)
}

pub fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

pub fn new_runtime_id() -> String {
    static NEXT_RUNTIME_ID: AtomicU64 = AtomicU64::new(1);
    format!(
        "runtime-{}-{}",
        std::process::id(),
        NEXT_RUNTIME_ID.fetch_add(1, Ordering::SeqCst)
    )
}

pub fn maybe_crash_after_stage(stage: &str) {
    if std::env::var("SDB_INTERNAL_CRASH_AFTER_STAGE")
        .ok()
        .as_deref()
        != Some(stage)
    {
        return;
    }
    let code = std::env::var("SDB_INTERNAL_CRASH_EXIT_CODE")
        .ok()
        .and_then(|value| value.parse::<i32>().ok())
        .unwrap_or(199);
    eprintln!("runtime crash injection: exiting after stage {stage}");
    std::process::exit(code);
}

pub fn default_text_type() -> String {
    "text".to_string()
}

pub fn default_auto_increment() -> bool {
    true
}

pub fn default_all_operation() -> String {
    "all".to_string()
}

pub fn safe_project_id(project_id: &str) -> Result<&str, ApiError> {
    let mut chars = project_id.chars();
    let Some(first) = chars.next() else {
        return Err(ApiError::new(
            400,
            "project id must match [A-Za-z0-9][A-Za-z0-9_-]{0,63}",
        ));
    };
    if !first.is_ascii_alphanumeric()
        || project_id.len() > 64
        || !chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return Err(ApiError::new(
            400,
            "project id must match [A-Za-z0-9][A-Za-z0-9_-]{0,63}",
        ));
    }
    Ok(project_id)
}

pub fn safe_object_key(key: &str) -> Result<&str, ApiError> {
    if key.is_empty() || key.starts_with('/') || key.contains('\0') {
        return Err(ApiError::new(400, "invalid object key"));
    }
    if key.split('/').any(|part| part == "." || part == "..") {
        return Err(ApiError::new(
            400,
            "object key may not contain . or .. path segments",
        ));
    }
    Ok(key)
}

pub fn row_to_json_map(row: &rusqlite::Row<'_>) -> rusqlite::Result<Map<String, Value>> {
    let mut out = Map::new();
    let row_ref = row.as_ref();
    for idx in 0..row_ref.column_count() {
        let name = row_ref.column_name(idx)?.to_string();
        let value: rusqlite::types::Value = row.get(idx)?;
        out.insert(name, sql_to_json(value));
    }
    Ok(out)
}

pub fn sql_to_json(value: rusqlite::types::Value) -> Value {
    use rusqlite::types::Value as SqlValue;
    match value {
        SqlValue::Null => Value::Null,
        SqlValue::Integer(value) => json!(value),
        SqlValue::Real(value) => json!(value),
        SqlValue::Text(value) => Value::String(value),
        SqlValue::Blob(value) => Value::String(STANDARD.encode(value)),
    }
}

pub fn json_to_sql_value(value: &Value) -> rusqlite::types::Value {
    use rusqlite::types::Value as SqlValue;
    match value {
        Value::Null => SqlValue::Null,
        Value::Bool(value) => SqlValue::Integer(i64::from(*value)),
        Value::Number(number) => number
            .as_i64()
            .map(SqlValue::Integer)
            .or_else(|| number.as_f64().map(SqlValue::Real))
            .unwrap_or(SqlValue::Null),
        Value::String(value) => SqlValue::Text(value.clone()),
        Value::Array(_) | Value::Object(_) => SqlValue::Text(value.to_string()),
    }
}

pub fn normalize_row_payload(data: Value) -> Result<Map<String, Value>, ApiError> {
    let Some(obj) = data.as_object() else {
        return Err(ApiError::new(400, "row body must be an object"));
    };
    if obj.is_empty() {
        return Err(ApiError::new(400, "row body must be a non-empty object"));
    }
    let mut out = Map::new();
    for (key, value) in obj {
        let column = assert_user_ident(key)?;
        let normalized = match value {
            Value::Bool(value) => json!(i64::from(*value)),
            Value::Number(_) | Value::String(_) | Value::Null => value.clone(),
            Value::Array(_) | Value::Object(_) => Value::String(value.to_string()),
        };
        out.insert(column.to_string(), normalized);
    }
    Ok(out)
}

pub fn assert_user_ident(name: &str) -> Result<&str, ApiError> {
    if !crate::policy::valid_ident(name) || name.starts_with("_sdb_") || name.starts_with("sqlite_")
    {
        return Err(ApiError::new(
            400,
            format!("invalid user identifier: {name}"),
        ));
    }
    Ok(name)
}
