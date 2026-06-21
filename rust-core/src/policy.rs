use crate::auth::{Actor, actor_claim};
use rusqlite::types::Value as SqlValue;
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct PolicyError(pub String);

impl std::fmt::Display for PolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for PolicyError {}

pub fn quote_ident(name: &str) -> Result<String, PolicyError> {
    if valid_ident(name) {
        Ok(format!("\"{}\"", name))
    } else {
        Err(PolicyError(format!("invalid identifier: {name}")))
    }
}

pub fn compile_policies(
    rules: &[Value],
    actor: &Actor,
) -> Result<(String, Vec<SqlValue>), PolicyError> {
    if actor.role == "service_role" || rules.is_empty() {
        return Ok(("1=1".to_string(), vec![]));
    }
    let mut clauses = Vec::new();
    let mut params = Vec::new();
    for rule in rules {
        let (sql, mut sql_params) = compile_rule(rule, actor)?;
        clauses.push(format!("({sql})"));
        params.append(&mut sql_params);
    }
    Ok((clauses.join(" OR "), params))
}

pub fn evaluate_policies(
    rules: &[Value],
    row: &serde_json::Map<String, Value>,
    actor: &Actor,
) -> Result<bool, PolicyError> {
    if actor.role == "service_role" || rules.is_empty() {
        return Ok(true);
    }
    for rule in rules {
        if evaluate_rule(rule, row, actor)? {
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn valid_ident(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(ch) if ch == '_' || ch.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn compile_rule(rule: &Value, actor: &Actor) -> Result<(String, Vec<SqlValue>), PolicyError> {
    let Some(obj) = rule.as_object() else {
        return Err(PolicyError("policy rule must be an object".to_string()));
    };
    if let Some(allow) = obj.get("allow").and_then(Value::as_bool) {
        return Ok((if allow { "1=1" } else { "0=1" }.to_string(), vec![]));
    }
    if let Some(roles) = obj.get("role_in").and_then(Value::as_array) {
        let allowed = roles
            .iter()
            .filter_map(Value::as_str)
            .any(|role| role == actor.role);
        return Ok((if allowed { "1=1" } else { "0=1" }.to_string(), vec![]));
    }
    if let Some(children) = obj.get("and").and_then(Value::as_array) {
        return join(
            "AND",
            children
                .iter()
                .map(|child| compile_rule(child, actor))
                .collect::<Result<Vec<_>, _>>()?,
        );
    }
    if let Some(children) = obj.get("or").and_then(Value::as_array) {
        return join(
            "OR",
            children
                .iter()
                .map(|child| compile_rule(child, actor))
                .collect::<Result<Vec<_>, _>>()?,
        );
    }
    let Some(column) = obj.get("column").and_then(Value::as_str) else {
        return Err(PolicyError("unsupported policy rule".to_string()));
    };
    let column_sql = quote_ident(column)?;
    if let Some(claim) = obj.get("equals_claim").and_then(Value::as_str) {
        let value = actor_claim(actor, claim);
        let Some(param) = json_to_sql_value(&value) else {
            return Ok(("0=1".to_string(), vec![]));
        };
        return Ok((format!("{column_sql} = ?"), vec![param]));
    }
    if let Some(claim) = obj.get("in_claim").and_then(Value::as_str) {
        let value = actor_claim(actor, claim);
        let values = value
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(json_to_sql_value)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if values.is_empty() {
            return Ok(("0=1".to_string(), vec![]));
        }
        return Ok((
            format!(
                "{column_sql} IN ({})",
                values.iter().map(|_| "?").collect::<Vec<_>>().join(",")
            ),
            values,
        ));
    }
    if let Some(value) = obj.get("equals") {
        let Some(param) = json_to_sql_value(value) else {
            return Ok(("0=1".to_string(), vec![]));
        };
        return Ok((format!("{column_sql} = ?"), vec![param]));
    }
    Err(PolicyError("unsupported policy rule".to_string()))
}

fn evaluate_rule(
    rule: &Value,
    row: &serde_json::Map<String, Value>,
    actor: &Actor,
) -> Result<bool, PolicyError> {
    let Some(obj) = rule.as_object() else {
        return Err(PolicyError("policy rule must be an object".to_string()));
    };
    if let Some(allow) = obj.get("allow").and_then(Value::as_bool) {
        return Ok(allow);
    }
    if let Some(roles) = obj.get("role_in").and_then(Value::as_array) {
        return Ok(roles
            .iter()
            .filter_map(Value::as_str)
            .any(|role| role == actor.role));
    }
    if let Some(children) = obj.get("and").and_then(Value::as_array) {
        for child in children {
            if !evaluate_rule(child, row, actor)? {
                return Ok(false);
            }
        }
        return Ok(true);
    }
    if let Some(children) = obj.get("or").and_then(Value::as_array) {
        for child in children {
            if evaluate_rule(child, row, actor)? {
                return Ok(true);
            }
        }
        return Ok(false);
    }
    let Some(column) = obj.get("column").and_then(Value::as_str) else {
        return Err(PolicyError("unsupported policy rule".to_string()));
    };
    let row_value = row.get(column).unwrap_or(&Value::Null);
    if let Some(claim) = obj.get("equals_claim").and_then(Value::as_str) {
        return Ok(row_value == &actor_claim(actor, claim));
    }
    if let Some(claim) = obj.get("in_claim").and_then(Value::as_str) {
        let values = actor_claim(actor, claim);
        return Ok(values
            .as_array()
            .map(|items| items.contains(row_value))
            .unwrap_or(false));
    }
    if let Some(value) = obj.get("equals") {
        return Ok(row_value == value);
    }
    Err(PolicyError("unsupported policy rule".to_string()))
}

fn join(
    op: &str,
    compiled: Vec<(String, Vec<SqlValue>)>,
) -> Result<(String, Vec<SqlValue>), PolicyError> {
    if compiled.is_empty() {
        return Err(PolicyError("compound policy must not be empty".to_string()));
    }
    let mut clauses = Vec::new();
    let mut params = Vec::new();
    for (sql, mut sql_params) in compiled {
        clauses.push(format!("({sql})"));
        params.append(&mut sql_params);
    }
    Ok((clauses.join(&format!(" {op} ")), params))
}

fn json_to_sql_value(value: &Value) -> Option<SqlValue> {
    match value {
        Value::Null => Some(SqlValue::Null),
        Value::Bool(value) => Some(SqlValue::Integer(i64::from(*value))),
        Value::Number(value) => value
            .as_i64()
            .map(SqlValue::Integer)
            .or_else(|| value.as_f64().map(SqlValue::Real)),
        Value::String(value) => Some(SqlValue::Text(value.clone())),
        Value::Array(_) | Value::Object(_) => None,
    }
}
