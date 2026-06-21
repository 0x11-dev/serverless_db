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
    Ilike,
}

#[derive(Debug, Clone)]
pub enum FilterExpr {
    Condition(FilterCondition),
    And(Vec<FilterExpr>),
    Or(Vec<FilterExpr>),
    Not(Box<FilterExpr>),
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
                return Err(ParseError(
                    "order clause must have a column name".to_string(),
                ));
            }
            let column = parts[0].to_string();
            let direction = match parts.get(1).map(|s| *s) {
                Some("asc") => OrderDirection::Asc,
                Some("desc") => OrderDirection::Desc,
                None => OrderDirection::Asc,
                Some(other) => {
                    return Err(ParseError(format!(
                        "order direction must be asc or desc, got: {other}"
                    )));
                }
            };
            let nulls = match parts.get(2).map(|s| *s) {
                Some("nullsfirst") => OrderNulls::First,
                Some("nullslast") => OrderNulls::Last,
                None => OrderNulls::Default,
                Some(other) => {
                    return Err(ParseError(format!(
                        "order nulls must be nullsfirst or nullslast, got: {other}"
                    )));
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
        "ilike" => Ok(FilterOp::Ilike),
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
    for op_str in ["neq", "gt", "gte", "lt", "lte", "in", "is", "like", "ilike"] {
        if let Some(rest) = value.strip_prefix(&format!("{op_str}.")) {
            let op = parse_op(op_str)?;
            return Ok(FilterCondition {
                column: key.to_string(),
                op,
                value: rest.to_string(),
            });
        }
    }
    Err(ParseError(format!(
        "filter for {key} must use format column.op=value or key=op.value"
    )))
}

fn parse_filter_entry(key: &str, value: &str) -> Result<FilterExpr, ParseError> {
    if let Some(rest) = value.strip_prefix("not.") {
        let cond = parse_condition(key, rest)?;
        return Ok(FilterExpr::Not(Box::new(FilterExpr::Condition(cond))));
    }
    Ok(FilterExpr::Condition(parse_condition(key, value)?))
}

fn parse_or_segment(part: &str) -> Result<FilterExpr, ParseError> {
    let part = part.trim();
    if let Some(eq_pos) = part.find('=') {
        let key = &part[..eq_pos];
        let val = &part[eq_pos + 1..];
        return Ok(FilterExpr::Condition(parse_condition(key, val)?));
    }
    let parts: Vec<&str> = part.splitn(3, '.').collect();
    if parts.len() >= 3 {
        let column = parts[0];
        let op = parts[1];
        let value = parts[2..].join(".");
        let cond = parse_condition(&format!("{column}.{op}"), &value)?;
        return Ok(FilterExpr::Condition(cond));
    }
    if parts.len() == 2 {
        let column = parts[0];
        let value = parts[1];
        return Ok(FilterExpr::Condition(FilterCondition {
            column: column.to_string(),
            op: FilterOp::Eq,
            value: value.to_string(),
        }));
    }
    Err(ParseError(format!("invalid or filter segment: {part}")))
}

fn parse_or_expr(value: &str) -> Result<FilterExpr, ParseError> {
    let parts: Vec<&str> = value.split(',').collect();
    if parts.is_empty() {
        return Err(ParseError("or filter must not be empty".to_string()));
    }
    let conditions: Vec<FilterExpr> = parts
        .iter()
        .map(|part| parse_or_segment(part))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(FilterExpr::Or(conditions))
}

pub fn parse_query_params(query: &HashMap<String, String>) -> Result<SelectQuery, ParseError> {
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
                filter_conditions.push(parse_filter_entry(key, value)?);
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

pub fn compile_filters(expr: &FilterExpr) -> Result<(String, Vec<SqlValue>), ParseError> {
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
        FilterExpr::Not(inner) => {
            let (sql, params) = compile_filters(inner)?;
            Ok((format!("NOT ({sql})"), params))
        }
    }
}

fn compile_condition(cond: &FilterCondition) -> Result<(String, Vec<SqlValue>), ParseError> {
    let column = quote_ident(&cond.column).map_err(|e: PolicyError| ParseError(e.0))?;
    match cond.op {
        FilterOp::Eq => Ok((
            format!("{column} = ?"),
            vec![SqlValue::Text(cond.value.clone())],
        )),
        FilterOp::Neq => Ok((
            format!("{column} != ?"),
            vec![SqlValue::Text(cond.value.clone())],
        )),
        FilterOp::Gt => Ok((
            format!("{column} > ?"),
            vec![SqlValue::Text(cond.value.clone())],
        )),
        FilterOp::Gte => Ok((
            format!("{column} >= ?"),
            vec![SqlValue::Text(cond.value.clone())],
        )),
        FilterOp::Lt => Ok((
            format!("{column} < ?"),
            vec![SqlValue::Text(cond.value.clone())],
        )),
        FilterOp::Lte => Ok((
            format!("{column} <= ?"),
            vec![SqlValue::Text(cond.value.clone())],
        )),
        FilterOp::Like => Ok((
            format!("{column} LIKE ?"),
            vec![SqlValue::Text(cond.value.clone())],
        )),
        FilterOp::Ilike => Ok((
            format!("{column} LIKE ? COLLATE NOCASE"),
            vec![SqlValue::Text(cond.value.clone())],
        )),
        FilterOp::In => {
            let cleaned = cond.value.trim_start_matches('(').trim_end_matches(')');
            let values: Vec<&str> = cleaned.split(',').map(|s| s.trim()).collect();
            if values.is_empty() {
                return Ok(("0=1".to_string(), Vec::new()));
            }
            let placeholders: Vec<&str> = values.iter().map(|_| "?").collect();
            let params: Vec<SqlValue> = values
                .iter()
                .map(|v| SqlValue::Text(v.to_string()))
                .collect();
            Ok((format!("{column} IN ({})", placeholders.join(",")), params))
        }
        FilterOp::Is => {
            let val = cond.value.to_lowercase();
            match val.as_str() {
                "null" => Ok((format!("{column} IS NULL"), Vec::new())),
                "not.null" | "notnull" => Ok((format!("{column} IS NOT NULL"), Vec::new())),
                "true" => Ok((format!("{column} IS 1"), Vec::new())),
                "false" => Ok((format!("{column} IS 0"), Vec::new())),
                _ => Err(ParseError(format!(
                    "is filter expects null, not.null, true, or false, got: {}",
                    cond.value
                ))),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_query(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn eq_filter_compiles_to_sql_with_param() {
        let cond = FilterCondition {
            column: "name".to_string(),
            op: FilterOp::Eq,
            value: "alice".to_string(),
        };
        let (sql, params) = compile_condition(&cond).unwrap();
        assert_eq!(sql, "\"name\" = ?");
        assert_eq!(params.len(), 1);
        assert_eq!(params[0], SqlValue::Text("alice".to_string()));
    }

    #[test]
    fn neq_filter_compiles_correctly() {
        let cond = FilterCondition {
            column: "status".to_string(),
            op: FilterOp::Neq,
            value: "deleted".to_string(),
        };
        let (sql, _) = compile_condition(&cond).unwrap();
        assert_eq!(sql, "\"status\" != ?");
    }

    #[test]
    fn gt_gte_lt_lte_filters_compile() {
        for (op, expected) in [
            (FilterOp::Gt, ">"),
            (FilterOp::Gte, ">="),
            (FilterOp::Lt, "<"),
            (FilterOp::Lte, "<="),
        ] {
            let cond = FilterCondition {
                column: "age".to_string(),
                op,
                value: "18".to_string(),
            };
            let (sql, _) = compile_condition(&cond).unwrap();
            assert_eq!(sql, format!("\"age\" {expected} ?"));
        }
    }

    #[test]
    fn like_filter_compiles() {
        let cond = FilterCondition {
            column: "title".to_string(),
            op: FilterOp::Like,
            value: "%hello%".to_string(),
        };
        let (sql, _) = compile_condition(&cond).unwrap();
        assert_eq!(sql, "\"title\" LIKE ?");
    }

    #[test]
    fn ilike_filter_uses_collate_nocase() {
        let cond = FilterCondition {
            column: "title".to_string(),
            op: FilterOp::Ilike,
            value: "%hello%".to_string(),
        };
        let (sql, _) = compile_condition(&cond).unwrap();
        assert_eq!(sql, "\"title\" LIKE ? COLLATE NOCASE");
    }

    #[test]
    fn in_filter_compiles_with_multiple_params() {
        let cond = FilterCondition {
            column: "id".to_string(),
            op: FilterOp::In,
            value: "(1,2,3)".to_string(),
        };
        let (sql, params) = compile_condition(&cond).unwrap();
        assert_eq!(sql, "\"id\" IN (?,?,?)");
        assert_eq!(params.len(), 3);
    }

    #[test]
    fn is_null_filter_compiles() {
        let cond = FilterCondition {
            column: "deleted_at".to_string(),
            op: FilterOp::Is,
            value: "null".to_string(),
        };
        let (sql, params) = compile_condition(&cond).unwrap();
        assert_eq!(sql, "\"deleted_at\" IS NULL");
        assert!(params.is_empty());
    }

    #[test]
    fn is_not_null_filter_compiles() {
        let cond = FilterCondition {
            column: "deleted_at".to_string(),
            op: FilterOp::Is,
            value: "not.null".to_string(),
        };
        let (sql, _) = compile_condition(&cond).unwrap();
        assert_eq!(sql, "\"deleted_at\" IS NOT NULL");
    }

    #[test]
    fn is_true_and_is_false_compile() {
        let cond = FilterCondition {
            column: "active".to_string(),
            op: FilterOp::Is,
            value: "true".to_string(),
        };
        let (sql, _) = compile_condition(&cond).unwrap();
        assert_eq!(sql, "\"active\" IS 1");

        let cond = FilterCondition {
            column: "active".to_string(),
            op: FilterOp::Is,
            value: "false".to_string(),
        };
        let (sql, _) = compile_condition(&cond).unwrap();
        assert_eq!(sql, "\"active\" IS 0");
    }

    #[test]
    fn and_filter_combines_with_and() {
        let expr = FilterExpr::And(vec![
            FilterExpr::Condition(FilterCondition {
                column: "a".to_string(),
                op: FilterOp::Eq,
                value: "1".to_string(),
            }),
            FilterExpr::Condition(FilterCondition {
                column: "b".to_string(),
                op: FilterOp::Eq,
                value: "2".to_string(),
            }),
        ]);
        let (sql, params) = compile_filters(&expr).unwrap();
        assert_eq!(sql, "(\"a\" = ?) AND (\"b\" = ?)");
        assert_eq!(params.len(), 2);
    }

    #[test]
    fn or_filter_combines_with_or() {
        let expr = FilterExpr::Or(vec![
            FilterExpr::Condition(FilterCondition {
                column: "a".to_string(),
                op: FilterOp::Eq,
                value: "1".to_string(),
            }),
            FilterExpr::Condition(FilterCondition {
                column: "b".to_string(),
                op: FilterOp::Eq,
                value: "2".to_string(),
            }),
        ]);
        let (sql, _) = compile_filters(&expr).unwrap();
        assert_eq!(sql, "(\"a\" = ?) OR (\"b\" = ?)");
    }

    #[test]
    fn not_filter_wraps_with_not() {
        let expr = FilterExpr::Not(Box::new(FilterExpr::Condition(FilterCondition {
            column: "a".to_string(),
            op: FilterOp::Eq,
            value: "1".to_string(),
        })));
        let (sql, _) = compile_filters(&expr).unwrap();
        assert_eq!(sql, "NOT (\"a\" = ?)");
    }

    #[test]
    fn empty_and_filter_returns_1_eq_1() {
        let expr = FilterExpr::And(vec![]);
        let (sql, params) = compile_filters(&expr).unwrap();
        assert_eq!(sql, "1=1");
        assert!(params.is_empty());
    }

    #[test]
    fn parse_query_params_extracts_eq_filter() {
        let q = make_query(&[("name", "eq.alice"), ("limit", "10")]);
        let result = parse_query_params(&q).unwrap();
        assert_eq!(result.limit, 10);
        match &result.filters {
            FilterExpr::Condition(cond) => {
                assert_eq!(cond.column, "name");
                assert_eq!(cond.op, FilterOp::Eq);
                assert_eq!(cond.value, "alice");
            }
            other => panic!("expected Condition, got {other:?}"),
        }
    }

    #[test]
    fn parse_query_params_extracts_dot_syntax_filter() {
        let q = make_query(&[("age.gte", "18")]);
        let result = parse_query_params(&q).unwrap();
        match &result.filters {
            FilterExpr::Condition(cond) => {
                assert_eq!(cond.column, "age");
                assert_eq!(cond.op, FilterOp::Gte);
                assert_eq!(cond.value, "18");
            }
            other => panic!("expected Condition, got {other:?}"),
        }
    }

    #[test]
    fn parse_query_params_extracts_or_filter() {
        let q = make_query(&[("or", "name.eq.alice,age.gte.18")]);
        let result = parse_query_params(&q).unwrap();
        match &result.filters {
            FilterExpr::Or(children) => {
                assert_eq!(children.len(), 2);
            }
            other => panic!("expected Or, got {other:?}"),
        }
    }

    #[test]
    fn parse_query_params_extracts_not_filter() {
        let q = make_query(&[("name", "not.eq.bob")]);
        let result = parse_query_params(&q).unwrap();
        match &result.filters {
            FilterExpr::Not(inner) => match inner.as_ref() {
                FilterExpr::Condition(cond) => {
                    assert_eq!(cond.op, FilterOp::Eq);
                    assert_eq!(cond.value, "bob");
                }
                other => panic!("expected Condition inside Not, got {other:?}"),
            },
            other => panic!("expected Not, got {other:?}"),
        }
    }

    #[test]
    fn parse_order_parses_column_direction_and_nulls() {
        let clauses = parse_order("created_at.desc.nullslast").unwrap();
        assert_eq!(clauses.len(), 1);
        assert_eq!(clauses[0].column, "created_at");
        assert_eq!(clauses[0].direction, OrderDirection::Desc);
        assert_eq!(clauses[0].nulls, OrderNulls::Last);
    }

    #[test]
    fn parse_order_defaults_to_asc_no_nulls() {
        let clauses = parse_order("name").unwrap();
        assert_eq!(clauses[0].direction, OrderDirection::Asc);
        assert_eq!(clauses[0].nulls, OrderNulls::Default);
    }

    #[test]
    fn parse_order_rejects_invalid_direction() {
        assert!(parse_order("name.sideways").is_err());
    }

    #[test]
    fn parse_select_param_handles_star_and_columns() {
        assert!(parse_select_param("*").unwrap().is_empty());
        let cols = parse_select_param("id,name,email").unwrap();
        assert_eq!(cols, vec!["id", "name", "email"]);
    }

    #[test]
    fn parse_query_params_extracts_select_and_count() {
        let q = make_query(&[("select", "id,name"), ("count", "exact")]);
        let result = parse_query_params(&q).unwrap();
        assert_eq!(result.select, vec!["id", "name"]);
        assert_eq!(result.count, CountMode::Exact);
    }

    #[test]
    fn parse_query_params_multiple_filters_become_and() {
        let q = make_query(&[("a", "eq.1"), ("b", "eq.2")]);
        let result = parse_query_params(&q).unwrap();
        match &result.filters {
            FilterExpr::And(children) => assert_eq!(children.len(), 2),
            other => panic!("expected And, got {other:?}"),
        }
    }
}
