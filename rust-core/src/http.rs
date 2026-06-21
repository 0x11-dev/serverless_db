use crate::auth::{actor_from_authorization, is_production, mint_token, verify_token};
use crate::runtime::{ApiError, PolicySpec, ProjectRuntime, TableSpec, WriteIdempotency};
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, options, post, put};
use axum::{Json, Router};
use reqwest::Method;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;

pub type AppState = Arc<ProjectRuntime>;

pub fn app(runtime: ProjectRuntime) -> Router {
    let state = Arc::new(runtime);
    Router::new()
        .route("/health", get(health))
        .route("/v1/tokens", post(tokens))
        .route("/v1/projects", post(create_project))
        .route("/v1/projects/{project_id}/hibernate", post(hibernate))
        .route("/v1/projects/{project_id}/crash", post(crash))
        .route("/v1/projects/{project_id}/schema", get(schema))
        .route("/v1/projects/{project_id}/tables", post(create_table))
        .route(
            "/v1/projects/{project_id}/tables/{table}",
            get(select_rows)
                .post(insert_row)
                .patch(update_rows)
                .delete(delete_rows),
        )
        .route(
            "/rest/v1/{table}",
            options(supabase_preflight)
                .get(supabase_select_rows)
                .post(supabase_insert_rows)
                .patch(supabase_update_rows)
                .delete(supabase_delete_rows),
        )
        .route(
            "/v1/projects/{project_id}/policies",
            get(list_policies).put(set_policy),
        )
        .route("/v1/projects/{project_id}/buckets", post(create_bucket))
        .route("/v1/projects/{project_id}/events", get(events))
        .route("/v1/projects/{project_id}/realtime", get(realtime))
        .route(
            "/v1/projects/{project_id}/storage/{bucket}/{*key}",
            put(put_object).get(get_object).delete(delete_object),
        )
        .with_state(state)
}

async fn health() -> Json<Value> {
    Json(json!({ "ok": true }))
}

fn local_json(runtime: &ProjectRuntime, value: Value) -> Json<Value> {
    Json(with_served_by_meta(runtime, value))
}

fn with_served_by_meta(runtime: &ProjectRuntime, mut value: Value) -> Value {
    match &mut value {
        Value::Object(map) => {
            let served_by = runtime.served_by_meta();
            match map.get_mut("meta") {
                Some(Value::Object(meta)) => {
                    if let Value::Object(served_by) = served_by {
                        for (key, value) in served_by {
                            meta.insert(key, value);
                        }
                    }
                }
                _ => {
                    map.insert("meta".to_string(), served_by);
                }
            }
            value
        }
        _ => json!({ "value": value, "meta": runtime.served_by_meta() }),
    }
}

#[derive(Debug, Deserialize)]
struct TokenRequest {
    sub: String,
    #[serde(default = "default_role")]
    role: String,
    #[serde(default)]
    claims: Map<String, Value>,
    expires_in: Option<i64>,
}

async fn tokens(
    headers: HeaderMap,
    Json(body): Json<TokenRequest>,
) -> Result<Json<Value>, HttpError> {
    if is_production() {
        return Err(HttpError(ApiError::new(
            403,
            "token minting is disabled in production mode",
        )));
    }
    let admin = require_admin(&headers)?;
    let token = mint_token(&body.sub, &body.role, body.claims, body.expires_in)
        .map_err(|err| HttpError(ApiError::new(401, err.to_string())))?;
    Ok(Json(json!({ "token": token, "minted_by": admin.sub })))
}

#[derive(Debug, Deserialize)]
struct ProjectRequest {
    id: Option<String>,
    project_id: Option<String>,
}

impl ProjectRequest {
    fn to_value(&self) -> Value {
        json!({ "id": self.id.clone().or(self.project_id.clone()).unwrap_or_default() })
    }
}

async fn create_project(
    State(runtime): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ProjectRequest>,
) -> Result<Json<Value>, HttpError> {
    require_admin(&headers)?;
    if should_forward_write(&runtime) {
        return forward_json(
            &runtime,
            "POST",
            "/v1/projects",
            &headers,
            serde_json::to_vec(&body.to_value())?,
        )
        .await;
    }
    Ok(local_json(
        &runtime,
        runtime.create_project(body.id.or(body.project_id).unwrap_or_default().as_str())?,
    ))
}

async fn hibernate(
    State(runtime): State<AppState>,
    Path(project_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, HttpError> {
    require_admin(&headers)?;
    Ok(local_json(&runtime, runtime.hibernate(&project_id)?))
}

async fn crash(
    State(runtime): State<AppState>,
    Path(project_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, HttpError> {
    require_admin(&headers)?;
    Ok(local_json(&runtime, runtime.crash_project(&project_id)?))
}

async fn schema(
    State(runtime): State<AppState>,
    Path(project_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, HttpError> {
    require_admin(&headers)?;
    Ok(local_json(&runtime, runtime.schema(&project_id)?))
}

async fn create_table(
    State(runtime): State<AppState>,
    Path(project_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<TableSpec>,
) -> Result<Json<Value>, HttpError> {
    require_admin(&headers)?;
    let body_bytes = serde_json::to_vec(&body)?;
    if should_forward_write(&runtime) {
        return forward_json(
            &runtime,
            "POST",
            &format!("/v1/projects/{project_id}/tables"),
            &headers,
            body_bytes,
        )
        .await;
    }
    let idempotency = write_idempotency(
        "POST",
        &format!("/v1/projects/{project_id}/tables"),
        &headers,
        "application/json",
        &body_bytes,
    )?;
    Ok(local_json(
        &runtime,
        runtime.create_table_with_idempotency(&project_id, body, idempotency)?,
    ))
}

async fn set_policy(
    State(runtime): State<AppState>,
    Path(project_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<PolicySpec>,
) -> Result<Json<Value>, HttpError> {
    require_admin(&headers)?;
    let body_bytes = serde_json::to_vec(&body)?;
    if should_forward_write(&runtime) {
        return forward_json(
            &runtime,
            "PUT",
            &format!("/v1/projects/{project_id}/policies"),
            &headers,
            body_bytes,
        )
        .await;
    }
    let idempotency = write_idempotency(
        "PUT",
        &format!("/v1/projects/{project_id}/policies"),
        &headers,
        "application/json",
        &body_bytes,
    )?;
    Ok(local_json(
        &runtime,
        runtime.set_policy_with_idempotency(&project_id, body, idempotency)?,
    ))
}

async fn list_policies(
    State(runtime): State<AppState>,
    Path(project_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, HttpError> {
    require_admin(&headers)?;
    Ok(local_json(&runtime, runtime.list_policies(&project_id)?))
}

#[derive(Debug, Deserialize)]
struct BucketRequest {
    name: String,
}

async fn create_bucket(
    State(runtime): State<AppState>,
    Path(project_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<BucketRequest>,
) -> Result<Json<Value>, HttpError> {
    require_admin(&headers)?;
    let body_bytes = serde_json::to_vec(&json!({ "name": body.name }))?;
    if should_forward_write(&runtime) {
        return forward_json(
            &runtime,
            "POST",
            &format!("/v1/projects/{project_id}/buckets"),
            &headers,
            body_bytes,
        )
        .await;
    }
    let idempotency = write_idempotency(
        "POST",
        &format!("/v1/projects/{project_id}/buckets"),
        &headers,
        "application/json",
        &body_bytes,
    )?;
    Ok(local_json(
        &runtime,
        runtime.create_bucket_with_idempotency(&project_id, &body.name, idempotency)?,
    ))
}

async fn select_rows(
    State(runtime): State<AppState>,
    Path((project_id, table)): Path<(String, String)>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Json<Value>, HttpError> {
    let actor = actor(&headers)?;
    let query = read_query(query, &headers)?;
    if query.session == ReadSession::FirstPrimary
        && runtime.is_read_replica()
        && runtime.primary_url().is_some()
    {
        let uri = table_read_uri(&project_id, &table, &query);
        return forward_json(&runtime, "GET", &uri, &headers, Vec::new()).await;
    }
    if query.session != ReadSession::FirstPrimary
        && !runtime.is_read_replica()
        && let Some(replica_url) = runtime
            .replica_url_for_region(query.route_region.as_deref())
            .map(ToOwned::to_owned)
    {
        let uri = table_read_uri(&project_id, &table, &query);
        match forward_json_to_url(&runtime, &replica_url, "GET", &uri, &headers, Vec::new()).await {
            Ok(response) => return Ok(response),
            Err(err) if should_fallback_from_replica_route(err.0.status) => {}
            Err(err) => return Err(err),
        }
    }
    match runtime.select_rows_at_bookmark(
        &project_id,
        &table,
        &query.filters,
        &actor,
        query.limit,
        query.bookmark.as_deref(),
    ) {
        Err(err)
            if matches!(err.status, 404 | 425)
                && runtime.is_read_replica()
                && runtime.primary_url().is_some()
                && query.bookmark.is_some() =>
        {
            let uri = table_read_uri(&project_id, &table, &query);
            forward_json(&runtime, "GET", &uri, &headers, Vec::new()).await
        }
        Ok(value) => Ok(local_json(&runtime, value)),
        Err(err) => Err(err.into()),
    }
}

async fn insert_row(
    State(runtime): State<AppState>,
    Path((project_id, table)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, HttpError> {
    let uri = format!("/v1/projects/{project_id}/tables/{table}");
    let body_bytes = serde_json::to_vec(&body)?;
    if should_forward_write(&runtime) {
        return forward_json(&runtime, "POST", &uri, &headers, body_bytes).await;
    }
    let actor = actor(&headers)?;
    let idempotency = write_idempotency("POST", &uri, &headers, "application/json", &body_bytes)?;
    Ok(local_json(
        &runtime,
        runtime.insert_row_with_idempotency(&project_id, &table, body, &actor, idempotency)?,
    ))
}

async fn update_rows(
    State(runtime): State<AppState>,
    Path((project_id, table)): Path<(String, String)>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, HttpError> {
    let uri = table_write_uri(&project_id, &table, &query);
    let body_bytes = serde_json::to_vec(&body)?;
    if should_forward_write(&runtime) {
        return forward_json(&runtime, "PATCH", &uri, &headers, body_bytes).await;
    }
    let actor = actor(&headers)?;
    let idempotency = write_idempotency("PATCH", &uri, &headers, "application/json", &body_bytes)?;
    let (filters, _) = filters_and_limit(query);
    Ok(local_json(
        &runtime,
        runtime.update_rows_with_idempotency(
            &project_id,
            &table,
            &filters,
            body,
            &actor,
            idempotency,
        )?,
    ))
}

async fn delete_rows(
    State(runtime): State<AppState>,
    Path((project_id, table)): Path<(String, String)>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Json<Value>, HttpError> {
    let uri = table_write_uri(&project_id, &table, &query);
    if should_forward_write(&runtime) {
        return forward_json(&runtime, "DELETE", &uri, &headers, Vec::new()).await;
    }
    let actor = actor(&headers)?;
    let idempotency = write_idempotency("DELETE", &uri, &headers, "", &[])?;
    let (filters, _) = filters_and_limit(query);
    Ok(local_json(
        &runtime,
        runtime.delete_rows_with_idempotency(&project_id, &table, &filters, &actor, idempotency)?,
    ))
}

async fn supabase_preflight(headers: HeaderMap) -> Response {
    let origin = cors_origin(&headers);
    (
        StatusCode::NO_CONTENT,
        [
            (header::ACCESS_CONTROL_ALLOW_ORIGIN, origin.as_str()),
            (
                header::ACCESS_CONTROL_ALLOW_HEADERS,
                "authorization, apikey, content-type, prefer, x-client-info, x-sdb-bookmark, x-d1-bookmark, idempotency-key",
            ),
            (
                header::ACCESS_CONTROL_ALLOW_METHODS,
                "GET, POST, PATCH, DELETE, OPTIONS",
            ),
            (
                header::ACCESS_CONTROL_EXPOSE_HEADERS,
                "content-range, x-sdb-bookmark, x-d1-bookmark",
            ),
        ],
    )
        .into_response()
}

async fn supabase_select_rows(
    State(runtime): State<AppState>,
    Path(table): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    let project_id = runtime.supabase_project_id().to_string();
    let actor = actor(&headers)?;
    let read_query = supabase_read_query(query.clone(), &headers)?;
    if read_query.session == ReadSession::FirstPrimary
        && runtime.is_read_replica()
        && runtime.primary_url().is_some()
    {
        let uri = supabase_table_uri(&table, &query);
        let response = forward_json(&runtime, "GET", &uri, &headers, Vec::new()).await?;
        return Ok(supabase_json(response.0, &headers));
    }
    if read_query.session != ReadSession::FirstPrimary
        && !runtime.is_read_replica()
        && let Some(replica_url) = runtime
            .replica_url_for_region(read_query.route_region.as_deref())
            .map(ToOwned::to_owned)
    {
        let uri = supabase_table_uri(&table, &query);
        match forward_json_to_url(&runtime, &replica_url, "GET", &uri, &headers, Vec::new()).await {
            Ok(response) => return Ok(supabase_json(response.0, &headers)),
            Err(err) if should_fallback_from_replica_route(err.0.status) => {}
            Err(err) => return Err(err),
        }
    }
    match runtime.select_rows_postgrest(
        &project_id,
        &table,
        &read_query.pg,
        &actor,
        read_query.bookmark.as_deref(),
    ) {
        Err(err)
            if matches!(err.status, 404 | 425)
                && runtime.is_read_replica()
                && runtime.primary_url().is_some()
                && read_query.bookmark.is_some() =>
        {
            let uri = supabase_table_uri(&table, &query);
            let response = forward_json(&runtime, "GET", &uri, &headers, Vec::new()).await?;
            Ok(supabase_json(response.0, &headers))
        }
        Ok(value) => Ok(supabase_rows_json(value, &headers)),
        Err(err) => Err(err.into()),
    }
}

async fn supabase_insert_rows(
    State(runtime): State<AppState>,
    Path(table): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, HttpError> {
    let project_id = runtime.supabase_project_id().to_string();
    let uri = supabase_table_uri(&table, &query);
    let body_bytes = serde_json::to_vec(&body)?;
    if should_forward_write(&runtime) {
        let response = forward_json(&runtime, "POST", &uri, &headers, body_bytes).await?;
        return Ok(supabase_json(response.0, &headers));
    }
    let actor = actor(&headers)?;
    let rows = normalize_supabase_insert_body(body)?;
    let content_type = content_type(&headers);
    let mut inserted = Vec::with_capacity(rows.len());
    for (idx, row) in rows.into_iter().enumerate() {
        let idempotency = supabase_write_idempotency(
            "POST",
            &uri,
            &headers,
            &content_type,
            &serde_json::to_vec(&row)?,
            idx,
        )?;
        let value =
            runtime.insert_row_with_idempotency(&project_id, &table, row, &actor, idempotency)?;
        if let Some(row) = value.get("row").cloned() {
            inserted.push(row);
        }
    }
    Ok(supabase_json(Value::Array(inserted), &headers))
}

async fn supabase_update_rows(
    State(runtime): State<AppState>,
    Path(table): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, HttpError> {
    let project_id = runtime.supabase_project_id().to_string();
    let uri = supabase_table_uri(&table, &query);
    let body_bytes = serde_json::to_vec(&body)?;
    if should_forward_write(&runtime) {
        let response = forward_json(&runtime, "PATCH", &uri, &headers, body_bytes).await?;
        return Ok(supabase_json(response.0, &headers));
    }
    let actor = actor(&headers)?;
    let filters = supabase_filters(query)?;
    let idempotency = write_idempotency("PATCH", &uri, &headers, "application/json", &body_bytes)?;
    let value = runtime.update_rows_postgrest(
        &project_id,
        &table,
        &filters,
        body,
        &actor,
        idempotency,
    )?;
    Ok(supabase_rows_json(value, &headers))
}

async fn supabase_delete_rows(
    State(runtime): State<AppState>,
    Path(table): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    let project_id = runtime.supabase_project_id().to_string();
    let uri = supabase_table_uri(&table, &query);
    if should_forward_write(&runtime) {
        let response = forward_json(&runtime, "DELETE", &uri, &headers, Vec::new()).await?;
        return Ok(supabase_json(response.0, &headers));
    }
    let actor = actor(&headers)?;
    let filters = supabase_filters(query)?;
    let idempotency = write_idempotency("DELETE", &uri, &headers, "", &[])?;
    let value =
        runtime.delete_rows_postgrest(&project_id, &table, &filters, &actor, idempotency)?;
    Ok(supabase_rows_json(value, &headers))
}

async fn put_object(
    State(runtime): State<AppState>,
    Path((project_id, bucket, key)): Path<(String, String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, HttpError> {
    let uri = format!("/v1/projects/{project_id}/storage/{bucket}/{key}");
    if should_forward_write(&runtime) {
        return forward_json(&runtime, "PUT", &uri, &headers, body.to_vec()).await;
    }
    let actor = actor(&headers)?;
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream");
    let idempotency = write_idempotency("PUT", &uri, &headers, content_type, &body)?;
    Ok(local_json(
        &runtime,
        runtime.put_object_with_idempotency(
            &project_id,
            &bucket,
            &key,
            &body,
            content_type,
            &actor,
            idempotency,
        )?,
    ))
}

async fn get_object(
    State(runtime): State<AppState>,
    Path((project_id, bucket, key)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    let actor = actor(&headers)?;
    let object = runtime.get_object(&project_id, &bucket, &key, &actor)?;
    let content_type = object
        .meta
        .get("content_type")
        .and_then(Value::as_str)
        .unwrap_or("application/octet-stream")
        .to_string();
    Ok(([(header::CONTENT_TYPE, content_type)], object.data).into_response())
}

async fn delete_object(
    State(runtime): State<AppState>,
    Path((project_id, bucket, key)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Result<Json<Value>, HttpError> {
    let uri = format!("/v1/projects/{project_id}/storage/{bucket}/{key}");
    if should_forward_write(&runtime) {
        return forward_json(&runtime, "DELETE", &uri, &headers, Vec::new()).await;
    }
    let actor = actor(&headers)?;
    let idempotency = write_idempotency("DELETE", &uri, &headers, "", &[])?;
    Ok(local_json(
        &runtime,
        runtime.delete_object_with_idempotency(&project_id, &bucket, &key, &actor, idempotency)?,
    ))
}

#[derive(Debug, Deserialize)]
struct EventsQuery {
    #[serde(default)]
    since: i64,
    #[serde(default = "default_limit")]
    limit: i64,
}

async fn events(
    State(runtime): State<AppState>,
    Path(project_id): Path<String>,
    Query(query): Query<EventsQuery>,
    headers: HeaderMap,
) -> Result<Json<Value>, HttpError> {
    let actor = actor(&headers)?;
    if !actor.is_admin() {
        return Err(HttpError(ApiError::new(
            403,
            "events access requires service_role or admin",
        )));
    }
    Ok(local_json(
        &runtime,
        runtime.events(&project_id, query.since, query.limit)?,
    ))
}

async fn realtime(
    State(runtime): State<AppState>,
    Path(project_id): Path<String>,
    Query(query): Query<EventsQuery>,
    headers: HeaderMap,
) -> Result<Sse<impl futures_core::Stream<Item = Result<Event, Infallible>>>, HttpError> {
    let actor = actor(&headers)?;
    if !actor.is_admin() {
        return Err(HttpError(ApiError::new(
            403,
            "realtime access requires service_role or admin",
        )));
    }
    let events = runtime.wait_for_events(&project_id, query.since).await?;
    let items = events
        .get("events")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|event| {
            let id = event
                .get("id")
                .and_then(Value::as_i64)
                .unwrap_or_default()
                .to_string();
            let operation = event
                .get("operation")
                .and_then(Value::as_str)
                .unwrap_or("message")
                .to_string();
            Ok(Event::default()
                .id(id)
                .event(operation)
                .data(event.to_string()))
        });
    Ok(Sse::new(futures_util::stream::iter(items)))
}

fn actor(headers: &HeaderMap) -> Result<crate::auth::Actor, HttpError> {
    if let Some(value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        return actor_from_authorization(Some(value))
            .map_err(|err| HttpError(ApiError::new(401, err.to_string())));
    }
    if let Some(api_key) = header_value(headers, "apikey") {
        return verify_token(&api_key)
            .map_err(|err| HttpError(ApiError::new(401, err.to_string())));
    }
    actor_from_authorization(None).map_err(|err| HttpError(ApiError::new(401, err.to_string())))
}

fn require_admin(headers: &HeaderMap) -> Result<crate::auth::Actor, HttpError> {
    let actor = actor(headers)?;
    if actor.is_admin() {
        Ok(actor)
    } else {
        Err(HttpError(ApiError::new(
            403,
            "admin or service_role access required",
        )))
    }
}

fn filters_and_limit(query: HashMap<String, String>) -> (HashMap<String, String>, u64) {
    let mut filters = HashMap::new();
    let mut limit = 100;
    for (key, value) in query {
        if let Some(column) = key.strip_prefix("eq.") {
            filters.insert(column.to_string(), value);
        } else if key == "limit" {
            limit = value.parse::<u64>().unwrap_or(100);
        }
    }
    (filters, limit)
}

fn supabase_read_query(
    query: HashMap<String, String>,
    headers: &HeaderMap,
) -> Result<SupabaseReadQuery, HttpError> {
    let mut session = ReadSession::FirstUnconstrained;
    let mut route_region = headers
        .get("x-sdb-region")
        .or_else(|| headers.get("x-d1-region"))
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    let mut bookmark = headers
        .get("x-d1-bookmark")
        .or_else(|| headers.get("x-sdb-bookmark"))
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);

    let pg_query = crate::postgrest::parse_query_params(&query)
        .map_err(|err| HttpError(ApiError::new(400, err.to_string())))?;

    if let Some(session_val) = query.get("session") {
        session = match session_val.as_str() {
            "first-primary" => ReadSession::FirstPrimary,
            "first-unconstrained" => ReadSession::FirstUnconstrained,
            "bookmark" => ReadSession::Bookmark,
            _ => {
                return Err(HttpError(ApiError::new(
                    400,
                    "session must be first-primary, first-unconstrained, or bookmark",
                )));
            }
        };
    }
    if let Some(b) = query.get("bookmark") {
        if !b.is_empty() {
            bookmark = Some(b.clone());
        }
    }
    if let Some(region) = query.get("route_region") {
        if !region.is_empty() {
            route_region = Some(region.clone());
        }
    }

    Ok(SupabaseReadQuery {
        pg: pg_query,
        bookmark,
        session,
        route_region,
    })
}

struct SupabaseReadQuery {
    pg: crate::postgrest::SelectQuery,
    bookmark: Option<String>,
    session: ReadSession,
    route_region: Option<String>,
}

fn supabase_filters(query: HashMap<String, String>) -> Result<crate::postgrest::FilterExpr, HttpError> {
    let pg = crate::postgrest::parse_query_params(&query)
        .map_err(|err| HttpError(ApiError::new(400, err.to_string())))?;
    Ok(pg.filters)
}

fn normalize_supabase_insert_body(body: Value) -> Result<Vec<Value>, HttpError> {
    match body {
        Value::Object(_) => Ok(vec![body]),
        Value::Array(items) if !items.is_empty() && items.iter().all(Value::is_object) => Ok(items),
        Value::Array(_) => Err(HttpError(ApiError::new(
            400,
            "insert body array must contain one or more objects",
        ))),
        _ => Err(HttpError(ApiError::new(
            400,
            "insert body must be an object or an array of objects",
        ))),
    }
}

fn supabase_rows_json(value: Value, headers: &HeaderMap) -> Response {
    let bookmark = value
        .get("bookmark")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let rows = value
        .get("rows")
        .cloned()
        .unwrap_or_else(|| Value::Array(Vec::new()));
    supabase_json_with_bookmark(rows, bookmark.as_deref(), headers)
}

fn supabase_json(value: Value, headers: &HeaderMap) -> Response {
    supabase_json_with_bookmark(value, None, headers)
}

fn cors_origin(headers: &HeaderMap) -> String {
    let allowed: Vec<String> = std::env::var("SDB_CORS_ORIGINS")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if allowed.is_empty() && !is_production() {
        return "*".to_string();
    }
    let request_origin = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if allowed.iter().any(|o| o == request_origin) {
        request_origin.to_string()
    } else {
        String::new()
    }
}

fn supabase_json_with_bookmark(value: Value, bookmark: Option<&str>, headers: &HeaderMap) -> Response {
    let origin = cors_origin(headers);
    let mut response = (
        [
            (header::ACCESS_CONTROL_ALLOW_ORIGIN, origin.as_str()),
            (
                header::ACCESS_CONTROL_EXPOSE_HEADERS,
                "content-range, x-sdb-bookmark, x-d1-bookmark",
            ),
        ],
        Json(value),
    )
        .into_response();
    if let Some(bookmark) = bookmark {
        if let Ok(value) = bookmark.parse() {
            response.headers_mut().insert("x-sdb-bookmark", value);
        }
    }
    response
}

fn supabase_table_uri(table: &str, query: &HashMap<String, String>) -> String {
    let pairs = query
        .iter()
        .map(|(key, value)| format!("{}={}", percent_encode(key), percent_encode(value)))
        .collect::<Vec<_>>();
    with_query(&format!("/rest/v1/{table}"), pairs)
}

fn content_type(headers: &HeaderMap) -> String {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .unwrap_or("application/json")
        .to_string()
}

fn supabase_write_idempotency(
    method: &str,
    uri: &str,
    headers: &HeaderMap,
    content_type: &str,
    body: &[u8],
    row_index: usize,
) -> Result<Option<WriteIdempotency>, HttpError> {
    let Some(key) = idempotency_key(headers) else {
        return Ok(None);
    };
    let key = if row_index == 0 {
        key
    } else {
        format!("{key}:{row_index}")
    };
    write_idempotency_with_key(method, uri, key, content_type, body)
}

fn write_idempotency_with_key(
    method: &str,
    uri: &str,
    key: String,
    content_type: &str,
    body: &[u8],
) -> Result<Option<WriteIdempotency>, HttpError> {
    let mut hasher = Sha256::new();
    hasher.update(method.as_bytes());
    hasher.update(b"\n");
    hasher.update(uri.as_bytes());
    hasher.update(b"\n");
    hasher.update(content_type.as_bytes());
    hasher.update(b"\n");
    hasher.update(body);
    Ok(Some(WriteIdempotency {
        key,
        request_hash: hex_lower(&hasher.finalize()),
    }))
}

struct ReadQuery {
    filters: HashMap<String, String>,
    limit: u64,
    bookmark: Option<String>,
    session: ReadSession,
    route_region: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadSession {
    FirstUnconstrained,
    FirstPrimary,
    Bookmark,
}

fn read_query(query: HashMap<String, String>, headers: &HeaderMap) -> Result<ReadQuery, HttpError> {
    let mut filters = HashMap::new();
    let mut limit = 100;
    let mut session = ReadSession::FirstUnconstrained;
    let mut route_region = headers
        .get("x-sdb-region")
        .or_else(|| headers.get("x-d1-region"))
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    let mut bookmark = headers
        .get("x-d1-bookmark")
        .or_else(|| headers.get("x-sdb-bookmark"))
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    for (key, value) in query {
        if let Some(column) = key.strip_prefix("eq.") {
            filters.insert(column.to_string(), value);
        } else if key == "limit" {
            limit = value.parse::<u64>().unwrap_or(100);
        } else if key == "bookmark" {
            if !value.is_empty() {
                bookmark = Some(value);
            }
        } else if key == "session" {
            session = match value.as_str() {
                "first-primary" => ReadSession::FirstPrimary,
                "first-unconstrained" => ReadSession::FirstUnconstrained,
                "bookmark" => ReadSession::Bookmark,
                _ => {
                    return Err(HttpError(ApiError::new(
                        400,
                        "session must be first-primary, first-unconstrained, or bookmark",
                    )));
                }
            };
        } else if key == "route_region" {
            if !value.is_empty() {
                route_region = Some(value);
            }
        }
    }
    Ok(ReadQuery {
        filters,
        limit,
        bookmark,
        session,
        route_region,
    })
}

fn should_forward_write(runtime: &ProjectRuntime) -> bool {
    runtime.is_read_replica() && runtime.primary_url().is_some()
}

async fn forward_json(
    runtime: &ProjectRuntime,
    method: &str,
    uri: &str,
    headers: &HeaderMap,
    body: Vec<u8>,
) -> Result<Json<Value>, HttpError> {
    let primary_url = runtime
        .primary_url()
        .ok_or_else(|| {
            HttpError(ApiError::new(
                405,
                "read replica has no primary_url configured",
            ))
        })?
        .to_string();
    forward_json_to_url(runtime, &primary_url, method, uri, headers, body).await
}

async fn forward_json_to_url(
    runtime: &ProjectRuntime,
    endpoint_url: &str,
    method: &str,
    uri: &str,
    headers: &HeaderMap,
    body: Vec<u8>,
) -> Result<Json<Value>, HttpError> {
    let method = parse_forward_method(method)?;
    let url = build_forward_url(endpoint_url, uri)?;
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned);
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| "application/json".to_string());
    let trace_id = forward_trace_id(runtime, headers);
    let traceparent = header_value(headers, "traceparent");
    let request_id = header_value(headers, "x-request-id");
    let idempotency_key = idempotency_key(headers);
    let max_attempts = forward_attempts(runtime, &method, idempotency_key.is_some());
    let mut attempt = 1usize;
    let response = loop {
        match forward_http_json(
            runtime,
            method.clone(),
            url.clone(),
            auth.as_deref(),
            &content_type,
            &body,
            &trace_id,
            traceparent.as_deref(),
            request_id.as_deref(),
            idempotency_key.as_deref(),
            attempt,
        )
        .await
        {
            Ok(response)
                if should_retry_forward_response(
                    response.status,
                    attempt,
                    max_attempts,
                    &method,
                ) =>
            {
                sleep_before_forward_retry(runtime, attempt).await;
                attempt += 1;
            }
            Ok(response) => break response,
            Err(err) if should_retry_forward_error(&err, attempt, max_attempts, &method) => {
                sleep_before_forward_retry(runtime, attempt).await;
                attempt += 1;
            }
            Err(err) => {
                runtime.record_forward_failure(endpoint_url);
                return Err(err);
            }
        }
    };
    if is_forward_endpoint_failure_status(response.status) {
        runtime.record_forward_failure(endpoint_url);
    } else {
        runtime.record_forward_success(endpoint_url);
    }
    if (200..300).contains(&response.status) {
        Ok(Json(response.body))
    } else {
        let message = response
            .body
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("primary request failed")
            .to_string();
        Err(HttpError(ApiError::new(response.status, message)))
    }
}

struct ForwardResponse {
    status: u16,
    body: Value,
}

async fn forward_http_json(
    runtime: &ProjectRuntime,
    method: Method,
    url: reqwest::Url,
    authorization: Option<&str>,
    content_type: &str,
    body: &[u8],
    trace_id: &str,
    traceparent: Option<&str>,
    request_id: Option<&str>,
    idempotency_key: Option<&str>,
    attempt: usize,
) -> Result<ForwardResponse, HttpError> {
    let mut request = runtime
        .forward_client()
        .request(method, url.clone())
        .header(header::ACCEPT, "application/json")
        .header("x-sdb-forwarded-by", runtime.runtime_id())
        .header("x-sdb-forward-attempt", attempt.to_string())
        .header("x-sdb-forward-trace-id", trace_id);
    if !body.is_empty() {
        request = request
            .header(header::CONTENT_TYPE, content_type)
            .body(body.to_vec());
    }
    if let Some(authorization) = authorization {
        request = request.header(header::AUTHORIZATION, authorization);
    }
    if let Some(traceparent) = traceparent {
        request = request.header("traceparent", traceparent);
    }
    if let Some(request_id) = request_id {
        request = request.header("x-request-id", request_id);
    }
    if let Some(idempotency_key) = idempotency_key {
        request = request
            .header("idempotency-key", idempotency_key)
            .header("x-sdb-idempotency-key", idempotency_key);
    }
    let response = request.send().await.map_err(classify_forward_error)?;
    let status = response.status().as_u16();
    let body = response.bytes().await.map_err(classify_forward_error)?;
    let body = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).map_err(|err| {
            HttpError(ApiError::new(
                502,
                format!("forward response from {url} is not JSON: {err}"),
            ))
        })?
    };
    Ok(ForwardResponse { status, body })
}

fn parse_forward_method(method: &str) -> Result<Method, HttpError> {
    method
        .parse::<Method>()
        .map_err(|err| HttpError(ApiError::new(500, format!("invalid forward method: {err}"))))
}

fn build_forward_url(endpoint_url: &str, uri: &str) -> Result<reqwest::Url, HttpError> {
    let mut url = reqwest::Url::parse(endpoint_url).map_err(|err| {
        HttpError(ApiError::new(
            400,
            format!("forward endpoint URL is invalid: {endpoint_url}: {err}"),
        ))
    })?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(HttpError(ApiError::new(
            400,
            "forward endpoint URL must use http or https",
        )));
    }
    let base_path = if url.path() == "/" { "" } else { url.path() };
    let path_and_query = join_paths(base_path, uri);
    let (path, query) = path_and_query
        .split_once('?')
        .map(|(path, query)| (path, Some(query)))
        .unwrap_or((path_and_query.as_str(), None));
    url.set_path(path);
    url.set_query(query);
    Ok(url)
}

fn join_paths(base: &str, uri: &str) -> String {
    if base.is_empty() {
        uri.to_string()
    } else {
        format!(
            "{}/{}",
            base.trim_end_matches('/'),
            uri.trim_start_matches('/')
        )
    }
}

fn forward_trace_id(runtime: &ProjectRuntime, headers: &HeaderMap) -> String {
    header_value(headers, "x-sdb-forward-trace-id")
        .or_else(|| header_value(headers, "x-request-id"))
        .or_else(|| header_value(headers, "traceparent"))
        .unwrap_or_else(|| runtime.next_forward_trace_id())
}

fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn idempotency_key(headers: &HeaderMap) -> Option<String> {
    header_value(headers, "idempotency-key")
        .or_else(|| header_value(headers, "x-sdb-idempotency-key"))
}

fn write_idempotency(
    method: &str,
    uri: &str,
    headers: &HeaderMap,
    content_type: &str,
    body: &[u8],
) -> Result<Option<WriteIdempotency>, HttpError> {
    let Some(key) = idempotency_key(headers) else {
        return Ok(None);
    };
    let mut hasher = Sha256::new();
    hasher.update(method.as_bytes());
    hasher.update(b"\n");
    hasher.update(uri.as_bytes());
    hasher.update(b"\n");
    hasher.update(content_type.as_bytes());
    hasher.update(b"\n");
    hasher.update(body);
    Ok(Some(WriteIdempotency {
        key,
        request_hash: hex_lower(&hasher.finalize()),
    }))
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn forward_attempts(runtime: &ProjectRuntime, method: &Method, has_idempotency_key: bool) -> usize {
    if forward_retry_allowed(method, has_idempotency_key) {
        runtime.forward_max_attempts()
    } else {
        1
    }
}

fn should_retry_forward_response(
    status: u16,
    attempt: usize,
    max_attempts: usize,
    method: &Method,
) -> bool {
    attempt < max_attempts
        && forward_retry_allowed(method, max_attempts > 1)
        && is_forward_endpoint_failure_status(status)
}

fn should_fallback_from_replica_route(status: u16) -> bool {
    matches!(status, 404 | 425) || is_forward_endpoint_failure_status(status)
}

fn is_forward_endpoint_failure_status(status: u16) -> bool {
    matches!(status, 408 | 429 | 502 | 503 | 504)
}

fn should_retry_forward_error(
    err: &HttpError,
    attempt: usize,
    max_attempts: usize,
    method: &Method,
) -> bool {
    attempt < max_attempts
        && forward_retry_allowed(method, max_attempts > 1)
        && matches!(err.0.status, 502 | 504)
}

fn forward_retry_allowed(method: &Method, has_idempotency_key: bool) -> bool {
    matches!(*method, Method::GET | Method::HEAD) || has_idempotency_key
}

async fn sleep_before_forward_retry(runtime: &ProjectRuntime, attempt: usize) {
    let base = runtime.forward_retry_backoff();
    if base.is_zero() {
        return;
    }
    tokio::time::sleep(base.saturating_mul(attempt as u32)).await;
}

fn classify_forward_error(err: reqwest::Error) -> HttpError {
    let status = if err.is_timeout() { 504 } else { 502 };
    HttpError(ApiError::new(
        status,
        format!("forward request failed: {err}"),
    ))
}

fn table_read_uri(project_id: &str, table: &str, query: &ReadQuery) -> String {
    let mut pairs = query
        .filters
        .iter()
        .map(|(key, value)| format!("eq.{}={}", percent_encode(key), percent_encode(value)))
        .collect::<Vec<_>>();
    if query.limit != 100 {
        pairs.push(format!("limit={}", query.limit));
    }
    if query.session != ReadSession::FirstUnconstrained {
        pairs.push(format!(
            "session={}",
            percent_encode(match query.session {
                ReadSession::FirstUnconstrained => "first-unconstrained",
                ReadSession::FirstPrimary => "first-primary",
                ReadSession::Bookmark => "bookmark",
            })
        ));
    }
    if let Some(bookmark) = &query.bookmark {
        pairs.push(format!("bookmark={}", percent_encode(bookmark)));
    }
    if let Some(region) = &query.route_region {
        pairs.push(format!("route_region={}", percent_encode(region)));
    }
    with_query(&format!("/v1/projects/{project_id}/tables/{table}"), pairs)
}

fn table_write_uri(project_id: &str, table: &str, query: &HashMap<String, String>) -> String {
    let pairs = query
        .iter()
        .map(|(key, value)| format!("{}={}", percent_encode(key), percent_encode(value)))
        .collect::<Vec<_>>();
    with_query(&format!("/v1/projects/{project_id}/tables/{table}"), pairs)
}

fn with_query(path: &str, pairs: Vec<String>) -> String {
    if pairs.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{}", pairs.join("&"))
    }
}

fn percent_encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[derive(Debug)]
pub struct HttpError(pub ApiError);

impl From<ApiError> for HttpError {
    fn from(value: ApiError) -> Self {
        Self(value)
    }
}

impl From<serde_json::Error> for HttpError {
    fn from(value: serde_json::Error) -> Self {
        Self(ApiError::new(500, format!("json error: {value}")))
    }
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.0.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (status, Json(json!({ "error": self.0.message }))).into_response()
    }
}

fn default_role() -> String {
    "authenticated".to_string()
}

fn default_limit() -> i64 {
    100
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{Actor, mint_token};
    use crate::object_store::LocalObjectStore;
    use crate::runtime::{ColumnSpec, RoutingEndpoint, RuntimeOptions};
    use axum::Json;
    use axum::extract::{Path, State};
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering as AtomicOrdering},
    };

    fn actor(sub: &str) -> Actor {
        Actor {
            sub: Some(sub.to_string()),
            role: "authenticated".to_string(),
            claims: Map::new(),
        }
    }

    fn notes_table() -> TableSpec {
        TableSpec {
            name: "notes".to_string(),
            columns: vec![
                ColumnSpec {
                    name: "owner_id".to_string(),
                    r#type: "text".to_string(),
                    primary_key: false,
                    auto_increment: true,
                    not_null: true,
                },
                ColumnSpec {
                    name: "title".to_string(),
                    r#type: "text".to_string(),
                    primary_key: false,
                    auto_increment: true,
                    not_null: true,
                },
            ],
        }
    }

    fn setup_notes(runtime: &ProjectRuntime) {
        runtime.create_project("demo").unwrap();
        runtime.create_table("demo", notes_table()).unwrap();
        runtime
            .set_policy(
                "demo",
                PolicySpec {
                    table: "notes".to_string(),
                    operation: "all".to_string(),
                    name: Some("owner_only".to_string()),
                    rule: json!({"column": "owner_id", "equals_claim": "sub"}),
                },
            )
            .unwrap();
    }

    async fn spawn_runtime(runtime: ProjectRuntime) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app(runtime)).await.unwrap();
        });
        (format!("http://{addr}"), task)
    }

    async fn spawn_primary(runtime: ProjectRuntime) -> (String, tokio::task::JoinHandle<()>) {
        spawn_runtime(runtime).await
    }

    #[derive(Clone)]
    struct ProbeState {
        attempts: Arc<AtomicUsize>,
        fail_first: bool,
        seen_headers: Arc<Mutex<Vec<Value>>>,
    }

    async fn probe(State(state): State<ProbeState>, headers: HeaderMap) -> Response {
        let attempt = state.attempts.fetch_add(1, AtomicOrdering::SeqCst) + 1;
        let seen = json!({
            "attempt": attempt,
            "authorization": headers.get(header::AUTHORIZATION).and_then(|value| value.to_str().ok()),
            "content_type": headers.get(header::CONTENT_TYPE).and_then(|value| value.to_str().ok()),
            "forwarded_by": headers.get("x-sdb-forwarded-by").and_then(|value| value.to_str().ok()),
            "forward_attempt": headers.get("x-sdb-forward-attempt").and_then(|value| value.to_str().ok()),
            "forward_trace_id": headers.get("x-sdb-forward-trace-id").and_then(|value| value.to_str().ok()),
            "request_id": headers.get("x-request-id").and_then(|value| value.to_str().ok()),
            "traceparent": headers.get("traceparent").and_then(|value| value.to_str().ok())
        });
        state.seen_headers.lock().unwrap().push(seen.clone());
        if state.fail_first && attempt == 1 {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error":"retry me"})),
            )
                .into_response();
        }
        Json(seen).into_response()
    }

    async fn spawn_probe(
        fail_first: bool,
    ) -> (
        String,
        Arc<AtomicUsize>,
        Arc<Mutex<Vec<Value>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let attempts = Arc::new(AtomicUsize::new(0));
        let seen_headers = Arc::new(Mutex::new(Vec::new()));
        let state = ProbeState {
            attempts: attempts.clone(),
            fail_first,
            seen_headers: seen_headers.clone(),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/echo", get(probe).post(probe))
                    .with_state(state),
            )
            .await
            .unwrap();
        });
        (format!("http://{addr}"), attempts, seen_headers, task)
    }

    fn auth_headers() -> HeaderMap {
        let token = mint_token("alice", "authenticated", Map::new(), None).unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        headers
    }

    async fn response_json(response: Response) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn forward_url_supports_https_and_base_paths() {
        let url = build_forward_url(
            "https://db.example.test/base",
            "/v1/projects/demo/tables/notes?limit=1",
        )
        .unwrap();
        assert_eq!(url.scheme(), "https");
        assert_eq!(url.path(), "/base/v1/projects/demo/tables/notes");
        assert_eq!(url.query(), Some("limit=1"));
    }

    #[tokio::test]
    async fn forward_json_retries_idempotent_reads_and_sends_trace_headers() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = ProjectRuntime::new(
            dir.path().join("runtime"),
            RuntimeOptions {
                forward_max_attempts: 2,
                forward_retry_backoff_ms: 0,
                ..RuntimeOptions::default()
            },
        )
        .unwrap();
        let (url, attempts, seen_headers, server) = spawn_probe(true).await;
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer test-token".parse().unwrap());
        headers.insert("x-request-id", "req-123".parse().unwrap());
        headers.insert("traceparent", "00-abc-def-01".parse().unwrap());

        let response = forward_json_to_url(&runtime, &url, "GET", "/echo", &headers, Vec::new())
            .await
            .unwrap()
            .0;

        assert_eq!(attempts.load(AtomicOrdering::SeqCst), 2);
        assert_eq!(response["attempt"], 2);
        assert_eq!(response["authorization"], "Bearer test-token");
        assert_eq!(response["forwarded_by"], runtime.runtime_id());
        assert_eq!(response["forward_attempt"], "2");
        assert_eq!(response["forward_trace_id"], "req-123");
        assert_eq!(response["request_id"], "req-123");
        assert_eq!(response["traceparent"], "00-abc-def-01");
        assert_eq!(seen_headers.lock().unwrap().len(), 2);
        server.abort();
    }

    #[tokio::test]
    async fn forward_json_does_not_retry_non_idempotent_writes() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = ProjectRuntime::new(
            dir.path().join("runtime"),
            RuntimeOptions {
                forward_max_attempts: 3,
                forward_retry_backoff_ms: 0,
                ..RuntimeOptions::default()
            },
        )
        .unwrap();
        let (url, attempts, _seen_headers, server) = spawn_probe(true).await;

        let err = forward_json_to_url(
            &runtime,
            &url,
            "POST",
            "/echo",
            &HeaderMap::new(),
            b"{}".to_vec(),
        )
        .await
        .unwrap_err();

        assert_eq!(attempts.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(err.0.status, 503);
        assert_eq!(err.0.message, "retry me");
        server.abort();
    }

    #[tokio::test]
    async fn forward_json_retries_writes_with_idempotency_key() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = ProjectRuntime::new(
            dir.path().join("runtime"),
            RuntimeOptions {
                forward_max_attempts: 2,
                forward_retry_backoff_ms: 0,
                ..RuntimeOptions::default()
            },
        )
        .unwrap();
        let (url, attempts, _seen_headers, server) = spawn_probe(true).await;
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", "write-123".parse().unwrap());

        let response =
            forward_json_to_url(&runtime, &url, "POST", "/echo", &headers, b"{}".to_vec())
                .await
                .unwrap()
                .0;

        assert_eq!(attempts.load(AtomicOrdering::SeqCst), 2);
        assert_eq!(response["attempt"], 2);
        server.abort();
    }

    #[tokio::test]
    async fn read_replica_forwards_write_to_primary() {
        let dir = tempfile::tempdir().unwrap();
        let primary = ProjectRuntime::new(
            dir.path().join("primary"),
            RuntimeOptions {
                writer_lease_ttl_ms: 0,
                ..RuntimeOptions::default()
            },
        )
        .unwrap();
        setup_notes(&primary);
        let (primary_url, server) = spawn_primary(primary.clone()).await;
        let replica = ProjectRuntime::new(
            dir.path().join("replica"),
            RuntimeOptions {
                read_replica: true,
                primary_url: Some(primary_url),
                ..RuntimeOptions::default()
            },
        )
        .unwrap();

        let response = insert_row(
            State(Arc::new(replica)),
            Path(("demo".to_string(), "notes".to_string())),
            auth_headers(),
            Json(json!({"owner_id":"alice","title":"forwarded"})),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(response["row"]["title"], "forwarded");

        let mut filters = HashMap::new();
        filters.insert("title".to_string(), "forwarded".to_string());
        let rows = primary
            .select_rows("demo", "notes", &filters, &actor("alice"), 100)
            .unwrap();
        assert_eq!(rows["rows"].as_array().unwrap().len(), 1);
        server.abort();
    }

    #[tokio::test]
    async fn http_insert_idempotency_key_replays_without_duplicate_row() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = ProjectRuntime::new(
            dir.path().join("runtime"),
            RuntimeOptions {
                writer_lease_ttl_ms: 0,
                ..RuntimeOptions::default()
            },
        )
        .unwrap();
        setup_notes(&runtime);
        let runtime = Arc::new(runtime);
        let mut headers = auth_headers();
        headers.insert("idempotency-key", "http-insert-1".parse().unwrap());

        let first = insert_row(
            State(runtime.clone()),
            Path(("demo".to_string(), "notes".to_string())),
            headers.clone(),
            Json(json!({"owner_id":"alice","title":"http-once"})),
        )
        .await
        .unwrap()
        .0;
        let second = insert_row(
            State(runtime.clone()),
            Path(("demo".to_string(), "notes".to_string())),
            headers,
            Json(json!({"owner_id":"alice","title":"http-once"})),
        )
        .await
        .unwrap()
        .0;

        assert_eq!(second["bookmark"], first["bookmark"]);
        let mut filters = HashMap::new();
        filters.insert("title".to_string(), "http-once".to_string());
        let rows = runtime
            .select_rows("demo", "notes", &filters, &actor("alice"), 100)
            .unwrap();
        assert_eq!(rows["rows"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn supabase_rest_table_crud_uses_default_project() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = ProjectRuntime::new(
            dir.path().join("runtime"),
            RuntimeOptions {
                writer_lease_ttl_ms: 0,
                ..RuntimeOptions::default()
            },
        )
        .unwrap();
        setup_notes(&runtime);
        let runtime = Arc::new(runtime);
        let mut select_query = HashMap::new();
        select_query.insert("select".to_string(), "*".to_string());

        let inserted = supabase_insert_rows(
            State(runtime.clone()),
            Path("notes".to_string()),
            Query(select_query.clone()),
            auth_headers(),
            Json(json!({"owner_id":"alice","title":"from-sdk"})),
        )
        .await
        .unwrap();
        let inserted = response_json(inserted).await;
        assert_eq!(inserted[0]["title"], "from-sdk");

        let mut read_query = HashMap::new();
        read_query.insert("select".to_string(), "*".to_string());
        read_query.insert("title".to_string(), "eq.from-sdk".to_string());
        let selected = supabase_select_rows(
            State(runtime.clone()),
            Path("notes".to_string()),
            Query(read_query),
            auth_headers(),
        )
        .await
        .unwrap();
        let selected = response_json(selected).await;
        assert_eq!(selected.as_array().unwrap().len(), 1);

        let mut update_query = HashMap::new();
        update_query.insert("select".to_string(), "*".to_string());
        update_query.insert("title".to_string(), "eq.from-sdk".to_string());
        let updated = supabase_update_rows(
            State(runtime.clone()),
            Path("notes".to_string()),
            Query(update_query),
            auth_headers(),
            Json(json!({"title":"sdk-updated"})),
        )
        .await
        .unwrap();
        let updated = response_json(updated).await;
        assert_eq!(updated[0]["title"], "sdk-updated");

        let mut delete_query = HashMap::new();
        delete_query.insert("select".to_string(), "*".to_string());
        delete_query.insert("title".to_string(), "eq.sdk-updated".to_string());
        let deleted = supabase_delete_rows(
            State(runtime.clone()),
            Path("notes".to_string()),
            Query(delete_query),
            auth_headers(),
        )
        .await
        .unwrap();
        let deleted = response_json(deleted).await;
        assert_eq!(deleted[0]["title"], "sdk-updated");
    }

    #[tokio::test]
    async fn supabase_rest_accepts_jwt_from_apikey_header() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = ProjectRuntime::new(
            dir.path().join("runtime"),
            RuntimeOptions {
                writer_lease_ttl_ms: 0,
                ..RuntimeOptions::default()
            },
        )
        .unwrap();
        setup_notes(&runtime);
        runtime
            .insert_row(
                "demo",
                "notes",
                json!({"owner_id":"alice","title":"apikey-only"}),
                &actor("alice"),
            )
            .unwrap();
        let token = mint_token("alice", "authenticated", Map::new(), None).unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("apikey", token.parse().unwrap());
        let mut query = HashMap::new();
        query.insert("select".to_string(), "*".to_string());
        query.insert("title".to_string(), "eq.apikey-only".to_string());

        let selected = supabase_select_rows(
            State(Arc::new(runtime)),
            Path("notes".to_string()),
            Query(query),
            headers,
        )
        .await
        .unwrap();
        let selected = response_json(selected).await;
        assert_eq!(selected[0]["title"], "apikey-only");
    }

    #[tokio::test]
    async fn read_replica_uses_routing_registry_primary_for_forwarding() {
        let dir = tempfile::tempdir().unwrap();
        let primary = ProjectRuntime::new(
            dir.path().join("primary"),
            RuntimeOptions {
                writer_lease_ttl_ms: 0,
                ..RuntimeOptions::default()
            },
        )
        .unwrap();
        setup_notes(&primary);
        let (primary_url, server) = spawn_primary(primary.clone()).await;
        let routed_primary_url = primary_url.clone();
        let replica = ProjectRuntime::new(
            dir.path().join("replica"),
            RuntimeOptions {
                read_replica: true,
                routing_region: Some("iad".to_string()),
                routing_endpoints: vec![RoutingEndpoint {
                    role: "primary".to_string(),
                    region: Some("iad".to_string()),
                    url: routed_primary_url,
                }],
                ..RuntimeOptions::default()
            },
        )
        .unwrap();
        assert_eq!(replica.primary_url(), Some(primary_url.as_str()));

        let response = insert_row(
            State(Arc::new(replica)),
            Path(("demo".to_string(), "notes".to_string())),
            auth_headers(),
            Json(json!({"owner_id":"alice","title":"registry-forwarded"})),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(response["row"]["title"], "registry-forwarded");

        let mut filters = HashMap::new();
        filters.insert("title".to_string(), "registry-forwarded".to_string());
        let rows = primary
            .select_rows("demo", "notes", &filters, &actor("alice"), 100)
            .unwrap();
        assert_eq!(rows["rows"].as_array().unwrap().len(), 1);
        server.abort();
    }

    #[tokio::test]
    async fn primary_routes_unconstrained_read_to_matching_region_replica() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(LocalObjectStore::new(dir.path().join("object-store")).unwrap());
        let replica = ProjectRuntime::with_object_store(
            dir.path().join("replica"),
            RuntimeOptions {
                read_replica: true,
                routing_region: Some("iad".to_string()),
                replica_refresh_interval_ms: 0,
                ..RuntimeOptions::default()
            },
            store.clone(),
        )
        .unwrap();
        let (replica_url, server) = spawn_runtime(replica).await;
        let primary = ProjectRuntime::with_object_store(
            dir.path().join("primary"),
            RuntimeOptions {
                writer_lease_ttl_ms: 0,
                routing_region: Some("wnam".to_string()),
                routing_endpoints: vec![
                    RoutingEndpoint {
                        role: "replica".to_string(),
                        region: Some("sfo".to_string()),
                        url: "http://127.0.0.1:9".to_string(),
                    },
                    RoutingEndpoint {
                        role: "replica".to_string(),
                        region: Some("iad".to_string()),
                        url: replica_url,
                    },
                ],
                ..RuntimeOptions::default()
            },
            store,
        )
        .unwrap();
        setup_notes(&primary);
        primary
            .insert_row(
                "demo",
                "notes",
                json!({"owner_id":"alice","title":"replica-routed"}),
                &actor("alice"),
            )
            .unwrap();

        let mut query = HashMap::new();
        query.insert("eq.title".to_string(), "replica-routed".to_string());
        query.insert("route_region".to_string(), "iad".to_string());
        let response = select_rows(
            State(Arc::new(primary)),
            Path(("demo".to_string(), "notes".to_string())),
            Query(query),
            auth_headers(),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(response["rows"].as_array().unwrap().len(), 1);
        assert_eq!(response["meta"]["served_by_region"], "iad");
        assert_eq!(response["meta"]["served_by_primary"], false);
        server.abort();
    }

    #[tokio::test]
    async fn primary_falls_back_when_routed_replica_endpoint_is_unhealthy() {
        let dir = tempfile::tempdir().unwrap();
        let primary = Arc::new(
            ProjectRuntime::new(
                dir.path().join("primary"),
                RuntimeOptions {
                    writer_lease_ttl_ms: 0,
                    routing_region: Some("wnam".to_string()),
                    routing_endpoints: vec![RoutingEndpoint {
                        role: "replica".to_string(),
                        region: Some("iad".to_string()),
                        url: "http://127.0.0.1:9".to_string(),
                    }],
                    routing_endpoint_failure_threshold: 1,
                    routing_endpoint_cooldown_ms: 60_000,
                    ..RuntimeOptions::default()
                },
            )
            .unwrap(),
        );
        setup_notes(&primary);
        primary
            .insert_row(
                "demo",
                "notes",
                json!({"owner_id":"alice","title":"fallback-primary"}),
                &actor("alice"),
            )
            .unwrap();

        let mut query = HashMap::new();
        query.insert("eq.title".to_string(), "fallback-primary".to_string());
        query.insert("route_region".to_string(), "iad".to_string());
        let response = select_rows(
            State(primary.clone()),
            Path(("demo".to_string(), "notes".to_string())),
            Query(query),
            auth_headers(),
        )
        .await
        .unwrap()
        .0;

        assert_eq!(response["rows"].as_array().unwrap().len(), 1);
        assert_eq!(response["meta"]["served_by_primary"], true);
        assert!(primary.replica_url_for_region(Some("iad")).is_none());
        let info = primary.project_info("demo").unwrap();
        assert_eq!(
            info["routing"]["endpoint_health"][0]["url"],
            "http://127.0.0.1:9"
        );
        assert_eq!(info["routing"]["endpoint_health"][0]["open"], true);
    }

    #[tokio::test]
    async fn session_first_primary_bypasses_replica_route() {
        let dir = tempfile::tempdir().unwrap();
        let primary = ProjectRuntime::new(
            dir.path().join("primary"),
            RuntimeOptions {
                writer_lease_ttl_ms: 0,
                routing_region: Some("wnam".to_string()),
                routing_endpoints: vec![RoutingEndpoint {
                    role: "replica".to_string(),
                    region: Some("iad".to_string()),
                    url: "http://127.0.0.1:9".to_string(),
                }],
                ..RuntimeOptions::default()
            },
        )
        .unwrap();
        setup_notes(&primary);
        primary
            .insert_row(
                "demo",
                "notes",
                json!({"owner_id":"alice","title":"primary-session"}),
                &actor("alice"),
            )
            .unwrap();

        let mut query = HashMap::new();
        query.insert("eq.title".to_string(), "primary-session".to_string());
        query.insert("session".to_string(), "first-primary".to_string());
        let response = select_rows(
            State(Arc::new(primary)),
            Path(("demo".to_string(), "notes".to_string())),
            Query(query),
            auth_headers(),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(response["rows"].as_array().unwrap().len(), 1);
        assert_eq!(response["meta"]["served_by_region"], "wnam");
        assert_eq!(response["meta"]["served_by_primary"], true);
    }

    #[tokio::test]
    async fn read_replica_first_primary_forwards_read_to_primary() {
        let dir = tempfile::tempdir().unwrap();
        let primary = ProjectRuntime::new(
            dir.path().join("primary"),
            RuntimeOptions {
                writer_lease_ttl_ms: 0,
                routing_region: Some("wnam".to_string()),
                ..RuntimeOptions::default()
            },
        )
        .unwrap();
        setup_notes(&primary);
        primary
            .insert_row(
                "demo",
                "notes",
                json!({"owner_id":"alice","title":"forward-primary-read"}),
                &actor("alice"),
            )
            .unwrap();
        let (primary_url, server) = spawn_primary(primary).await;
        let replica = ProjectRuntime::new(
            dir.path().join("replica"),
            RuntimeOptions {
                read_replica: true,
                primary_url: Some(primary_url),
                routing_region: Some("iad".to_string()),
                ..RuntimeOptions::default()
            },
        )
        .unwrap();

        let mut query = HashMap::new();
        query.insert("eq.title".to_string(), "forward-primary-read".to_string());
        query.insert("session".to_string(), "first-primary".to_string());
        let response = select_rows(
            State(Arc::new(replica)),
            Path(("demo".to_string(), "notes".to_string())),
            Query(query),
            auth_headers(),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(response["rows"].as_array().unwrap().len(), 1);
        assert_eq!(response["meta"]["served_by_region"], "wnam");
        assert_eq!(response["meta"]["served_by_primary"], true);
        server.abort();
    }

    #[tokio::test]
    async fn read_replica_falls_back_to_primary_for_bookmarked_read() {
        let dir = tempfile::tempdir().unwrap();
        let primary = ProjectRuntime::new(
            dir.path().join("primary"),
            RuntimeOptions {
                writer_lease_ttl_ms: 0,
                ..RuntimeOptions::default()
            },
        )
        .unwrap();
        setup_notes(&primary);
        let inserted = primary
            .insert_row(
                "demo",
                "notes",
                json!({"owner_id":"alice","title":"primary-only"}),
                &actor("alice"),
            )
            .unwrap();
        let bookmark = inserted["bookmark"].as_str().unwrap().to_string();
        let (primary_url, server) = spawn_primary(primary).await;
        let replica = ProjectRuntime::with_object_store(
            dir.path().join("replica"),
            RuntimeOptions {
                read_replica: true,
                primary_url: Some(primary_url),
                ..RuntimeOptions::default()
            },
            Arc::new(LocalObjectStore::new(dir.path().join("replica-store")).unwrap()),
        )
        .unwrap();

        let mut query = HashMap::new();
        query.insert("eq.title".to_string(), "primary-only".to_string());
        query.insert("bookmark".to_string(), bookmark.clone());
        let response = select_rows(
            State(Arc::new(replica)),
            Path(("demo".to_string(), "notes".to_string())),
            Query(query),
            auth_headers(),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(response["rows"].as_array().unwrap().len(), 1);
        assert!(response["bookmark"].as_str().unwrap() >= bookmark.as_str());
        server.abort();
    }

    fn admin_headers() -> HeaderMap {
        let token = mint_token("admin", "service_role", Map::new(), None).unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        headers
    }

    #[tokio::test]
    async fn anonymous_cannot_mint_tokens() {
        let err = tokens(
            HeaderMap::new(),
            Json(TokenRequest {
                sub: "evil".to_string(),
                role: "service_role".to_string(),
                claims: Map::new(),
                expires_in: None,
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0.status, 403);
    }

    #[tokio::test]
    async fn anonymous_cannot_create_project() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = ProjectRuntime::new(dir.path().join("r"), RuntimeOptions::default()).unwrap();
        let err = create_project(
            State(Arc::new(runtime)),
            HeaderMap::new(),
            Json(ProjectRequest { id: None, project_id: Some("evil".to_string()) }),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0.status, 403);
    }

    #[tokio::test]
    async fn anonymous_cannot_hibernate() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = ProjectRuntime::new(dir.path().join("r"), RuntimeOptions::default()).unwrap();
        runtime.create_project("demo").unwrap();
        let err = hibernate(
            State(Arc::new(runtime)),
            Path("demo".to_string()),
            HeaderMap::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0.status, 403);
    }

    #[tokio::test]
    async fn anonymous_cannot_crash() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = ProjectRuntime::new(dir.path().join("r"), RuntimeOptions::default()).unwrap();
        runtime.create_project("demo").unwrap();
        let err = crash(
            State(Arc::new(runtime)),
            Path("demo".to_string()),
            HeaderMap::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0.status, 403);
    }

    #[tokio::test]
    async fn anonymous_cannot_create_table() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = ProjectRuntime::new(dir.path().join("r"), RuntimeOptions::default()).unwrap();
        runtime.create_project("demo").unwrap();
        let err = create_table(
            State(Arc::new(runtime)),
            Path("demo".to_string()),
            HeaderMap::new(),
            Json(notes_table()),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0.status, 403);
    }

    #[tokio::test]
    async fn anonymous_cannot_set_policy() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = ProjectRuntime::new(dir.path().join("r"), RuntimeOptions::default()).unwrap();
        runtime.create_project("demo").unwrap();
        runtime.create_table("demo", notes_table()).unwrap();
        let err = set_policy(
            State(Arc::new(runtime)),
            Path("demo".to_string()),
            HeaderMap::new(),
            Json(PolicySpec {
                table: "notes".to_string(),
                operation: "all".to_string(),
                name: Some("evil".to_string()),
                rule: json!({"allow": true}),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0.status, 403);
    }

    #[tokio::test]
    async fn anonymous_cannot_list_policies() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = ProjectRuntime::new(dir.path().join("r"), RuntimeOptions::default()).unwrap();
        runtime.create_project("demo").unwrap();
        let err = list_policies(
            State(Arc::new(runtime)),
            Path("demo".to_string()),
            HeaderMap::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0.status, 403);
    }

    #[tokio::test]
    async fn anonymous_cannot_access_events() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = ProjectRuntime::new(dir.path().join("r"), RuntimeOptions::default()).unwrap();
        runtime.create_project("demo").unwrap();
        let err = events(
            State(Arc::new(runtime)),
            Path("demo".to_string()),
            Query(EventsQuery { since: 0, limit: 100 }),
            HeaderMap::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0.status, 403);
    }

    #[tokio::test]
    async fn authenticated_non_admin_cannot_access_events() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = ProjectRuntime::new(dir.path().join("r"), RuntimeOptions::default()).unwrap();
        runtime.create_project("demo").unwrap();
        let err = events(
            State(Arc::new(runtime)),
            Path("demo".to_string()),
            Query(EventsQuery { since: 0, limit: 100 }),
            auth_headers(),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0.status, 403);
    }

    #[tokio::test]
    async fn service_role_can_access_events() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = ProjectRuntime::new(dir.path().join("r"), RuntimeOptions::default()).unwrap();
        runtime.create_project("demo").unwrap();
        let result = events(
            State(Arc::new(runtime)),
            Path("demo".to_string()),
            Query(EventsQuery { since: 0, limit: 100 }),
            admin_headers(),
        )
        .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn service_role_can_create_table() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = ProjectRuntime::new(dir.path().join("r"), RuntimeOptions::default()).unwrap();
        runtime.create_project("demo").unwrap();
        let result = create_table(
            State(Arc::new(runtime)),
            Path("demo".to_string()),
            admin_headers(),
            Json(notes_table()),
        )
        .await;
        assert!(result.is_ok());
    }
}
