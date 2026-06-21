use crate::auth::Actor;
use crate::policy::{PolicyError, compile_policies, quote_ident};
use rusqlite::types::Value as SqlValue;
use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct FilterCondition {
    pub column: String,
    pub op: FilterOp,
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterOp {
    Eq,
    Neq,
    Gt,
    Gte,
    Lt,
    Lte,
    In,
    Is,
    Like,
}

#[derive(Debug, Clone)]
pub enum FilterExpr {
    Condition(FilterCondition),
    And(Vec<FilterExpr>),
    Or(Vec<FilterExpr>),
}

#[derive(Debug, Clone)]
pub struct OrderClause {
    pub column: String,
    pub direction: OrderDirection,
    pub nulls: OrderNulls,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderDirection {
    Asc,
    Desc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderNulls {
    First,
    Last,
    Default,
}

#[derive(Debug, Clone)]
pub struct SelectQuery {
    pub filters: FilterExpr,
    pub limit: u64,
    pub offset: u64,
    pub order: Vec<OrderClause>,
    pub select: Vec<String>,
    pub count: CountMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CountMode {
    None,
    Exact,
    Planned,
    Estimated,
}

impl Default for SelectQuery {
    fn default() -> Self {
        SelectQuery {
            filters: FilterExpr::And(Vec::new()),
            limit: 100,
            offset: 0,
            order: Vec::new(),
            select: Vec::new(),
            count: CountMode::None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ParseError(pub String);

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ParseError {}

pub fn parse_select_param(value: &str) -> Result<Vec<String>, ParseError> {
    if value.is_empty() || value == "*" {
        return Ok(Vec::new());
    }
    Ok(value
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect())
}

pub fn parse_order(value: &str) -> Result<Vec<OrderClause>, ParseError> {
    if value.is_empty() {
        return Ok(Vec::new());
    }
    value
        .split(',')
        .map(|clause| {
            let parts: Vec<&str> = clause.trim().split('.').collect();
            if parts.is_empty() || parts[0].is_empty() {
                return Err(ParseError("order clause must have a column name".to_string()));
            }
            let column = parts[0].to_string();
            let direction = match parts.get(1).map(|s| *s) {
                Some("asc") => OrderDirection::Asc,
                Some("desc") => OrderDirection::Desc,
                None => OrderDirection::Asc,
                Some(other) => {
                    return Err(ParseError(format!(
                        "order direction must be asc or desc, got: {other}"
                    )))
                }
            };
            let nulls = match parts.get(2).map(|s| *s) {
                Some("nullsfirst") => OrderNulls::First,
                Some("nullslast") => OrderNulls::Last,
                None => OrderNulls::Default,
                Some(other) => {
                    return Err(ParseError(format!(
                        "order nulls must be nullsfirst or nullslast, got: {other}"
                    )))
                }
            };
            Ok(OrderClause {
                column,
                direction,
                nulls,
            })
        })
        .collect()
}

fn parse_op(op_str: &str) -> Result<FilterOp, ParseError> {
    match op_str {
        "eq" => Ok(FilterOp::Eq),
        "neq" => Ok(FilterOp::Neq),
        "gt" => Ok(FilterOp::Gt),
        "gte" => Ok(FilterOp::Gte),
        "lt" => Ok(FilterOp::Lt),
        "lte" => Ok(FilterOp::Lte),
        "in" => Ok(FilterOp::In),
        "is" => Ok(FilterOp::Is),
        "like" => Ok(FilterOp::Like),
        other => Err(ParseError(format!("unsupported filter operator: {other}"))),
    }
}

fn parse_condition(key: &str, value: &str) -> Result<FilterCondition, ParseError> {
    if let Some(dot_pos) = key.find('.') {
        let column = &key[..dot_pos];
        let op_str = &key[dot_pos + 1..];
        let op = parse_op(op_str)?;
        return Ok(FilterCondition {
            column: column.to_string(),
            op,
            value: value.to_string(),
        });
    }
    if let Some(filter_value) = value.strip_prefix("eq.") {
        return Ok(FilterCondition {
            column: key.to_string(),
            op: FilterOp::Eq,
            value: filter_value.to_string(),
        });
    }
    Err(ParseError(format!(
        "filter for {key} must use format column.op=value or key=eq.value"
    )))
}

fn parse_or_expr(value: &str) -> Result<FilterExpr, ParseError> {
    let parts: Vec<&str> = value.split(',').collect();
    if parts.is_empty() {
        return Err(ParseError("or filter must not be empty".to_string()));
    }
    let conditions: Vec<FilterExpr> = parts
        .iter()
        .map(|part| {
            let part = part.trim();
            if let Some(eq_pos) = part.find('=') {
                let key = &part[..eq_pos];
                let val = &part[eq_pos + 1..];
                Ok(FilterExpr::Condition(parse_condition(key, val)?))
            } else {
                Err(ParseError(format!("invalid or filter segment: {part}")))
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(FilterExpr::Or(conditions))
}

pub fn parse_query_params(
    query: &HashMap<String, String>,
) -> Result<SelectQuery, ParseError> {
    let mut result = SelectQuery::default();
    let mut filter_conditions: Vec<FilterExpr> = Vec::new();
    let mut or_expr: Option<FilterExpr> = None;

    for (key, value) in query {
        match key.as_str() {
            "select" => {
                result.select = parse_select_param(value)?;
            }
            "limit" => {
                result.limit = value.parse::<u64>().unwrap_or(100);
            }
            "offset" => {
                result.offset = value.parse::<u64>().unwrap_or(0);
            }
            "order" => {
                result.order = parse_order(value)?;
            }
            "bookmark" | "session" | "route_region" => {}
            "count" => {
                result.count = match value.as_str() {
                    "exact" => CountMode::Exact,
                    "planned" => CountMode::Planned,
                    "estimated" => CountMode::Estimated,
                    _ => CountMode::None,
                };
            }
            "or" => {
                or_expr = Some(parse_or_expr(value)?);
            }
            "and" => {
                let parts: Vec<&str> = value.split(',').collect();
                let conditions: Vec<FilterExpr> = parts
                    .iter()
                    .map(|part| {
                        let part = part.trim();
                        if let Some(eq_pos) = part.find('=') {
                            let k = &part[..eq_pos];
                            let v = &part[eq_pos + 1..];
                            Ok(FilterExpr::Condition(parse_condition(k, v)?))
                        } else {
                            Err(ParseError(format!("invalid and filter segment: {part}")))
                        }
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                filter_conditions.push(FilterExpr::And(conditions));
            }
            _ => {
                filter_conditions.push(FilterExpr::Condition(parse_condition(key, value)?));
            }
        }
    }

    if let Some(or) = or_expr {
        filter_conditions.push(or);
    }

    result.filters = if filter_conditions.len() == 1 {
        filter_conditions.into_iter().next().unwrap()
    } else {
        FilterExpr::And(filter_conditions)
    };

    Ok(result)
}

pub fn compile_filters(
    expr: &FilterExpr,
) -> Result<(String, Vec<SqlValue>), ParseError> {
    match expr {
        FilterExpr::Condition(cond) => compile_condition(cond),
        FilterExpr::And(children) => {
            if children.is_empty() {
                return Ok(("1=1".to_string(), Vec::new()));
            }
            let mut clauses = Vec::new();
            let mut params = Vec::new();
            for child in children {
                let (sql, mut child_params) = compile_filters(child)?;
                clauses.push(format!("({sql})"));
                params.append(&mut child_params);
            }
            Ok((clauses.join(" AND "), params))
        }
        FilterExpr::Or(children) => {
            if children.is_empty() {
                return Ok(("1=1".to_string(), Vec::new()));
            }
            let mut clauses = Vec::new();
            let mut params = Vec::new();
            for child in children {
                let (sql, mut child_params) = compile_filters(child)?;
                clauses.push(format!("({sql})"));
                params.append(&mut child_params);
            }
            Ok((clauses.join(" OR "), params))
        }
    }
}

fn compile_condition(cond: &FilterCondition) -> Result<(String, Vec<SqlValue>), ParseError> {
    let column = quote_ident(&cond.column).map_err(|e: PolicyError| ParseError(e.0))?;
    match cond.op {
        FilterOp::Eq => Ok((format!("{column} = ?"), vec![SqlValue::Text(cond.value.clone())])),
        FilterOp::Neq => Ok((format!("{column} != ?"), vec![SqlValue::Text(cond.value.clone())])),
        FilterOp::Gt => Ok((format!("{column} > ?"), vec![SqlValue::Text(cond.value.clone())])),
        FilterOp::Gte => Ok((format!("{column} >= ?"), vec![SqlValue::Text(cond.value.clone())])),
        FilterOp::Lt => Ok((format!("{column} < ?"), vec![SqlValue::Text(cond.value.clone())])),
        FilterOp::Lte => Ok((format!("{column} <= ?"), vec![SqlValue::Text(cond.value.clone())])),
        FilterOp::Like => Ok((format!("{column} LIKE ?"), vec![SqlValue::Text(cond.value.clone())])),
        FilterOp::In => {
            let values: Vec<&str> = cond.value.split(',').map(|s| s.trim()).collect();
            if values.is_empty() {
                return Ok(("0=1".to_string(), Vec::new()));
            }
            let placeholders: Vec<&str> = values.iter().map(|_| "?").collect();
            let params: Vec<SqlValue> = values.iter().map(|v| SqlValue::Text(v.to_string())).collect();
            Ok((format!("{column} IN ({})", placeholders.join(",")), params))
        }
        FilterOp::Is => {
            let val = cond.value.to_lowercase();
            match val.as_str() {
                "null" => Ok((format!("{column} IS NULL"), Vec::new())),
                "not.null" | "notnull" => Ok((format!("{column} IS NOT NULL"), Vec::new())),
                "true" => Ok((format!("{column} IS 1"), Vec::new())),
                "false" => Ok((format!("{column} IS 0"), Vec::new())),
                _ => Err(ParseError(format!("is filter expects null, not.null, true, or false, got: {}", cond.value))),
            }
        }
    }
}

pub fn compile_order(order: &[OrderClause]) -> Result<String, ParseError> {
    if order.is_empty() {
        return Ok(String::new());
    }
    let mut parts = Vec::new();
    for clause in order {
        let column = quote_ident(&clause.column).map_err(|e: PolicyError| ParseError(e.0))?;
        let dir = match clause.direction {
            OrderDirection::Asc => "ASC",
            OrderDirection::Desc => "DESC",
        };
        let nulls = match clause.nulls {
            OrderNulls::First => " NULLS FIRST",
            OrderNulls::Last => " NULLS LAST",
            OrderNulls::Default => "",
        };
        parts.push(format!("{column} {dir}{nulls}"));
    }
    Ok(format!("ORDER BY {}", parts.join(", ")))
}

pub fn compile_select_columns(columns: &[String]) -> Result<String, ParseError> {
    if columns.is_empty() {
        return Ok("*".to_string());
    }
    let mut parts = Vec::new();
    for col in columns {
        let quoted = quote_ident(col).map_err(|e: PolicyError| ParseError(e.0))?;
        parts.push(quoted);
    }
    Ok(parts.join(", "))
}

pub fn build_select_sql(
    table: &str,
    query: &SelectQuery,
    rules: &[Value],
    actor: &Actor,
) -> Result<(String, Vec<SqlValue>), ParseError> {
    let table_quoted = quote_ident(table).map_err(|e: PolicyError| ParseError(e.0))?;
    let columns = compile_select_columns(&query.select)?;
    let (filter_sql, mut filter_params) = compile_filters(&query.filters)?;
    let (policy_sql, mut policy_params) =
        compile_policies(rules, actor).map_err(|e: PolicyError| ParseError(e.0))?;
    let order_sql = compile_order(&query.order)?;
    let limit = query.limit.clamp(1, 1000) as i64;
    let offset = query.offset as i64;

    let where_sql = if filter_sql == "1=1" {
        format!("({policy_sql})")
    } else {
        format!("{filter_sql} AND ({policy_sql})")
    };

    let mut sql = format!("SELECT {columns} FROM {table_quoted} WHERE {where_sql}");
    let mut params = Vec::new();
    params.append(&mut filter_params);
    params.append(&mut policy_params);

    if !order_sql.is_empty() {
        sql.push_str(&format!(" {order_sql}"));
    }
    sql.push_str(&format!(" LIMIT ? OFFSET ?"));
    params.push(SqlValue::Integer(limit));
    params.push(SqlValue::Integer(offset));

    Ok((sql, params))
}
