use crate::auth::{actor_from_authorization, is_production, mint_token, verify_token};
use crate::auth_store::{AuthLogoutScope, AuthUserPatch};
use crate::postgrest::CountMode;
use crate::runtime::{ApiError, PolicySpec, ProjectRuntime, TableSpec, WriteIdempotency};
use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, options, post, put};
use axum::{Json, Router};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use rand_core::OsRng;
use reqwest::Method;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

pub type AppState = Arc<ProjectRuntime>;
const ACCESS_TOKEN_TTL_SECS: i64 = 3600;
const REFRESH_TOKEN_TTL_SECS: i64 = 30 * 24 * 60 * 60;

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
        .route(
            "/storage/v1/buckets",
            post(supabase_create_bucket).get(supabase_list_buckets),
        )
        .route(
            "/storage/v1/buckets/{bucket_id}",
            get(supabase_get_bucket).delete(supabase_delete_bucket),
        )
        .route(
            "/storage/v1/object/{bucket_id}/{*key}",
            post(supabase_upload_object)
                .get(supabase_download_object)
                .put(supabase_upload_object)
                .delete(supabase_delete_object),
        )
        .route(
            "/storage/v1/object/list/{bucket_id}",
            post(supabase_list_objects),
        )
        .route("/realtime/v1/stream", get(supabase_realtime_stream))
        .route("/auth/v1/signup", post(auth_signup))
        .route("/auth/v1/token", post(auth_token))
        .route("/auth/v1/logout", post(auth_logout))
        .route("/auth/v1/user", get(auth_get_user).put(auth_update_user))
        .route("/auth/v1/settings", get(auth_settings))
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
    let upsert = prefer_upsert(&headers);
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
        let value = if upsert {
            runtime.upsert_row_with_idempotency(&project_id, &table, row, &actor, idempotency)?
        } else {
            runtime.insert_row_with_idempotency(&project_id, &table, row, &actor, idempotency)?
        };
        if let Some(row) = value.get("row").cloned() {
            inserted.push(row);
        }
    }
    Ok(supabase_json_with_prefer(
        Value::Array(inserted),
        None,
        &headers,
        PreferReturn::Default,
        0,
    ))
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
    let value =
        runtime.update_rows_postgrest(&project_id, &table, &filters, body, &actor, idempotency)?;
    let affected = value.get("affected").and_then(Value::as_u64).unwrap_or(0) as usize;
    let rows = value
        .get("rows")
        .cloned()
        .unwrap_or(Value::Array(Vec::new()));
    let prefer = prefer_return(&headers);
    Ok(supabase_json_with_prefer(
        rows, None, &headers, prefer, affected,
    ))
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
    let affected = value.get("affected").and_then(Value::as_u64).unwrap_or(0) as usize;
    let rows = value
        .get("rows")
        .cloned()
        .unwrap_or(Value::Array(Vec::new()));
    let prefer = prefer_return(&headers);
    Ok(supabase_json_with_prefer(
        rows, None, &headers, prefer, affected,
    ))
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

async fn supabase_create_bucket(
    State(runtime): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, HttpError> {
    require_admin(&headers)?;
    let project_id = runtime.supabase_project_id().to_string();
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| HttpError(ApiError::new(400, "missing 'name' field")))?;
    let idempotency = write_idempotency(
        "POST",
        &format!("/storage/v1/buckets/{name}"),
        &headers,
        "application/json",
        &[],
    )?;
    Ok(local_json(
        &runtime,
        runtime.create_bucket_with_idempotency(&project_id, name, idempotency)?,
    ))
}

async fn supabase_list_buckets(
    State(runtime): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, HttpError> {
    require_admin(&headers)?;
    let project_id = runtime.supabase_project_id().to_string();
    Ok(Json(runtime.list_buckets_async(&project_id).await?))
}

async fn supabase_get_bucket(
    State(runtime): State<AppState>,
    Path(bucket_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, HttpError> {
    require_admin(&headers)?;
    let project_id = runtime.supabase_project_id().to_string();
    Ok(local_json(
        &runtime,
        runtime.get_bucket_async(&project_id, &bucket_id).await?,
    ))
}

async fn supabase_delete_bucket(
    State(runtime): State<AppState>,
    Path(bucket_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, HttpError> {
    let project_id = runtime.supabase_project_id().to_string();
    require_admin(&headers)?;
    let uri = format!("/storage/v1/buckets/{bucket_id}");
    if should_forward_write(&runtime) {
        return forward_json(&runtime, "DELETE", &uri, &headers, Vec::new()).await;
    }
    let idempotency = write_idempotency("DELETE", &uri, &headers, "", &[])?;
    let _ = idempotency;
    Ok(local_json(
        &runtime,
        runtime.delete_bucket(&project_id, &bucket_id)?,
    ))
}

async fn supabase_upload_object(
    State(runtime): State<AppState>,
    Path((bucket_id, key)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, HttpError> {
    let project_id = runtime.supabase_project_id().to_string();
    let uri = format!("/storage/v1/object/{bucket_id}/{key}");
    if should_forward_write(&runtime) {
        return forward_json(&runtime, "POST", &uri, &headers, body.to_vec()).await;
    }
    let actor = require_storage_actor(&headers)?;
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    let idempotency = write_idempotency("POST", &uri, &headers, &content_type, &body)?;
    Ok(local_json(
        &runtime,
        runtime.put_object_with_idempotency(
            &project_id,
            &bucket_id,
            &key,
            &body,
            &content_type,
            &actor,
            idempotency,
        )?,
    ))
}

async fn supabase_download_object(
    State(runtime): State<AppState>,
    Path((bucket_id, key)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    let project_id = runtime.supabase_project_id().to_string();
    let actor = require_storage_actor(&headers)?;
    let object = runtime
        .get_object_async(&project_id, &bucket_id, &key, &actor)
        .await?;
    let content_type = object
        .meta
        .get("content_type")
        .and_then(Value::as_str)
        .unwrap_or("application/octet-stream")
        .to_string();
    Ok(([(header::CONTENT_TYPE, content_type)], object.data).into_response())
}

async fn supabase_delete_object(
    State(runtime): State<AppState>,
    Path((bucket_id, key)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<Value>, HttpError> {
    let project_id = runtime.supabase_project_id().to_string();
    let uri = format!("/storage/v1/object/{bucket_id}/{key}");
    if should_forward_write(&runtime) {
        return forward_json(&runtime, "DELETE", &uri, &headers, Vec::new()).await;
    }
    let actor = require_storage_actor(&headers)?;
    let idempotency = write_idempotency("DELETE", &uri, &headers, "", &[])?;
    Ok(local_json(
        &runtime,
        runtime.delete_object_with_idempotency(
            &project_id,
            &bucket_id,
            &key,
            &actor,
            idempotency,
        )?,
    ))
}

#[derive(Debug, Deserialize)]
struct SupabaseListObjectsRequest {
    #[serde(default)]
    prefix: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
    #[serde(default)]
    offset: Option<i64>,
}

async fn supabase_list_objects(
    State(runtime): State<AppState>,
    Path(bucket_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<SupabaseListObjectsRequest>,
) -> Result<Json<Value>, HttpError> {
    let project_id = runtime.supabase_project_id().to_string();
    let actor = require_storage_actor(&headers)?;
    Ok(Json(
        runtime
            .list_objects_async(
                &project_id,
                &bucket_id,
                body.prefix.as_deref(),
                body.limit.unwrap_or(100),
                body.offset.unwrap_or(0),
                &actor,
            )
            .await?,
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

#[derive(Debug, Deserialize)]
struct SupabaseRealtimeQuery {
    #[serde(default)]
    since: i64,
    #[serde(default)]
    table: Option<String>,
}

async fn supabase_realtime_stream(
    State(runtime): State<AppState>,
    Query(query): Query<SupabaseRealtimeQuery>,
    headers: HeaderMap,
) -> Result<Sse<impl futures_core::Stream<Item = Result<Event, Infallible>>>, HttpError> {
    let actor = actor(&headers)?;
    if actor.is_anon() {
        return Err(HttpError(ApiError::new(
            403,
            "realtime stream requires authenticated or service_role",
        )));
    }
    let project_id = runtime.supabase_project_id().to_string();
    let events = runtime.wait_for_events(&project_id, query.since).await?;
    let table_filter = query.table.clone();
    let items = events
        .get("events")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(move |event| {
            if let Some(ref table) = table_filter {
                event.get("table").and_then(Value::as_str) == Some(table.as_str())
            } else {
                true
            }
        })
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
            let table = event
                .get("table")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let record = event.get("row").cloned().unwrap_or(Value::Null);
            let payload = json!({
                "type": operation,
                "table": table,
                "schema": "public",
                "record": record,
                "old": null
            });
            Ok(Event::default()
                .id(id)
                .event(operation)
                .data(payload.to_string()))
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

fn require_storage_actor(headers: &HeaderMap) -> Result<crate::auth::Actor, HttpError> {
    let actor = actor(headers)?;
    if actor.is_anon() {
        return Err(HttpError(ApiError::new(
            403,
            "storage access requires authenticated or service_role",
        )));
    }
    Ok(actor)
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

fn supabase_filters(
    query: HashMap<String, String>,
) -> Result<crate::postgrest::FilterExpr, HttpError> {
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

fn prefer_return(headers: &HeaderMap) -> PreferReturn {
    headers
        .get("prefer")
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            if v.contains("return=representation") {
                PreferReturn::Representation
            } else if v.contains("return=minimal") {
                PreferReturn::Minimal
            } else {
                PreferReturn::Default
            }
        })
        .unwrap_or(PreferReturn::Default)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreferReturn {
    Default,
    Minimal,
    Representation,
}

fn prefer_upsert(headers: &HeaderMap) -> bool {
    headers
        .get("prefer")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("resolution=merge-duplicates"))
        .unwrap_or(false)
}

fn prefer_count(headers: &HeaderMap) -> CountMode {
    headers
        .get("prefer")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            if v.contains("count=exact") {
                Some(CountMode::Exact)
            } else if v.contains("count=planned") {
                Some(CountMode::Planned)
            } else if v.contains("count=estimated") {
                Some(CountMode::Estimated)
            } else {
                None
            }
        })
        .unwrap_or(CountMode::None)
}

fn content_range_header(start: usize, end: Option<usize>, total: Option<usize>) -> String {
    let range = match end {
        Some(e) => format!("{}-{}", start, e),
        None => format!("{}-*", start),
    };
    match total {
        Some(t) => format!("{range}/{t}"),
        None => format!("{range}/*"),
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

    let accept = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if accept.contains("vnd.pgrst.object+json") {
        let single = if let Some(arr) = rows.as_array() {
            if arr.is_empty() {
                Value::Null
            } else {
                arr[0].clone()
            }
        } else {
            rows.clone()
        };
        return supabase_json_with_bookmark(single, bookmark.as_deref(), headers);
    }

    supabase_json_with_bookmark(rows, bookmark.as_deref(), headers)
}

fn supabase_json(value: Value, headers: &HeaderMap) -> Response {
    supabase_json_with_bookmark(value, None, headers)
}

fn supabase_json_with_prefer(
    value: Value,
    bookmark: Option<&str>,
    headers: &HeaderMap,
    prefer: PreferReturn,
    affected: usize,
) -> Response {
    if prefer == PreferReturn::Minimal {
        let origin = cors_origin(headers);
        let mut response = (
            StatusCode::NO_CONTENT,
            [
                (header::ACCESS_CONTROL_ALLOW_ORIGIN, origin.as_str()),
                (
                    header::ACCESS_CONTROL_EXPOSE_HEADERS,
                    "content-range, x-sdb-bookmark, x-d1-bookmark",
                ),
            ],
        )
            .into_response();
        if affected > 0 {
            let range = content_range_header(0, Some(affected - 1), Some(affected));
            if let Ok(val) = range.parse() {
                response.headers_mut().insert("content-range", val);
            }
        }
        if let Some(bookmark) = bookmark {
            if let Ok(val) = bookmark.parse() {
                response.headers_mut().insert("x-sdb-bookmark", val);
            }
        }
        return response;
    }
    supabase_json_with_bookmark(unwrap_single(value, headers), bookmark, headers)
}

fn unwrap_single(value: Value, headers: &HeaderMap) -> Value {
    let accept = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if accept.contains("vnd.pgrst.object+json") {
        if let Some(arr) = value.as_array() {
            if arr.is_empty() {
                return Value::Null;
            }
            return arr[0].clone();
        }
    }
    value
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

fn supabase_json_with_bookmark(
    value: Value,
    bookmark: Option<&str>,
    headers: &HeaderMap,
) -> Response {
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

// ---------------------------------------------------------------------------
// GoTrue-compatible auth endpoints (/auth/v1/*)
// ---------------------------------------------------------------------------

struct RateLimiter {
    window_secs: u64,
    max_requests: usize,
    buckets: Mutex<HashMap<String, Vec<Instant>>>,
}

impl RateLimiter {
    fn new(window_secs: u64, max_requests: usize) -> Self {
        Self {
            window_secs,
            max_requests,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    fn check(&self, key: &str) -> bool {
        let now = Instant::now();
        let window = std::time::Duration::from_secs(self.window_secs);
        let mut buckets = self.buckets.lock().unwrap();
        let entries = buckets.entry(key.to_string()).or_default();
        entries.retain(|t| now.duration_since(*t) < window);
        if entries.len() >= self.max_requests {
            return false;
        }
        entries.push(now);
        true
    }
}

static AUTH_RATE_LIMITER: OnceLock<RateLimiter> = OnceLock::new();

fn auth_rate_limiter() -> &'static RateLimiter {
    AUTH_RATE_LIMITER.get_or_init(|| {
        let window_secs = std::env::var("SDB_AUTH_RATE_LIMIT_WINDOW_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(60);
        let max_requests = std::env::var("SDB_AUTH_RATE_LIMIT_MAX")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(10);
        RateLimiter::new(window_secs, max_requests)
    })
}

fn client_ip(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .map(|s| s.trim().to_string())
        .or_else(|| {
            headers
                .get("x-real-ip")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "unknown".to_string())
}

fn check_auth_rate_limit(headers: &HeaderMap) -> Result<(), HttpError> {
    let ip = client_ip(headers);
    if !auth_rate_limiter().check(&ip) {
        return Err(HttpError(ApiError::new(
            429,
            "Too many auth requests. Please try again later.",
        )));
    }
    Ok(())
}

fn legacy_sha256_password(password: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(password.as_bytes());
    URL_SAFE_NO_PAD.encode(hasher.finalize())
}

fn hash_password(password: &str) -> Result<String, HttpError> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|err| {
            HttpError(ApiError::new(
                500,
                format!("failed to hash password: {err}"),
            ))
        })
}

fn verify_password(password: &str, hash: &str) -> bool {
    if !hash.starts_with("$argon2") {
        return legacy_sha256_password(password) == hash;
    }
    let parsed = match PasswordHash::new(hash) {
        Ok(h) => h,
        Err(_) => return false,
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

fn gen_uuid() -> String {
    let mut buf = [0u8; 16];
    use std::io::Read;
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut buf);
    } else {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        for (i, b) in buf.iter_mut().enumerate() {
            *b = ((now >> (i * 8)) & 0xff) as u8;
        }
    }
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        buf[0],
        buf[1],
        buf[2],
        buf[3],
        buf[4],
        buf[5],
        buf[6],
        buf[7],
        buf[8],
        buf[9],
        buf[10],
        buf[11],
        buf[12],
        buf[13],
        buf[14],
        buf[15]
    )
}

fn gen_refresh_token() -> String {
    let mut buf = [0u8; 32];
    use std::io::Read;
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut buf);
    } else {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        for (i, b) in buf.iter_mut().enumerate() {
            *b = ((now >> (i % 16 * 8)) & 0xff) as u8;
        }
    }
    URL_SAFE_NO_PAD.encode(buf)
}

fn now_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (y, mo, d, h, mi, s) = unix_to_datetime(secs as i64);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

fn unix_to_datetime(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs / 86400;
    let rem = secs % 86400;
    let h = (rem / 3600) as u32;
    let mi = ((rem % 3600) / 60) as u32;
    let s = (rem % 60) as u32;
    let (y, mo, d) = days_to_ymd(days);
    (y, mo, d, h, mi, s)
}

fn days_to_ymd(days: i64) -> (i64, u32, u32) {
    let mut y = 1970i64;
    let mut remaining = days;
    loop {
        let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
        let dy = if leap { 366 } else { 365 };
        if remaining < dy {
            break;
        }
        remaining -= dy;
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
    (y, mo, (remaining + 1) as u32)
}

fn public_user(user: &Value) -> Value {
    let mut out = user.clone();
    if let Some(obj) = out.as_object_mut() {
        obj.remove("password_hash");
    }
    out
}

fn user_to_json(user: &Value) -> Value {
    user.clone()
}

fn auth_project_id(state: &AppState) -> String {
    state.supabase_project_id().to_string()
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn session_response(
    user: &Value,
    refresh_token: String,
    session_id: &str,
    now: i64,
) -> Result<Value, HttpError> {
    let user_id = user.get("id").and_then(Value::as_str).unwrap_or("unknown");
    let email = user.get("email").and_then(Value::as_str).unwrap_or("");
    let phone = user.get("phone").and_then(Value::as_str);

    let access_token = mint_token(
        user_id,
        "authenticated",
        {
            let mut m = Map::new();
            m.insert("email".to_string(), Value::String(email.to_string()));
            m.insert(
                "session_id".to_string(),
                Value::String(session_id.to_string()),
            );
            if let Some(p) = phone {
                m.insert("phone".to_string(), Value::String(p.to_string()));
            }
            m
        },
        Some(ACCESS_TOKEN_TTL_SECS),
    )
    .map_err(|err| {
        HttpError(ApiError::new(
            500,
            format!("failed to mint access token: {err}"),
        ))
    })?;

    Ok(json!({
        "access_token": access_token,
        "token_type": "bearer",
        "expires_in": ACCESS_TOKEN_TTL_SECS,
        "expires_at": now + ACCESS_TOKEN_TTL_SECS,
        "refresh_token": refresh_token,
        "user": user_to_json(user),
    }))
}

fn build_session_with_response(
    user: &Value,
    state: &AppState,
    response_fn: impl FnOnce(&Value, &str, &str, i64) -> Result<Value, HttpError>,
) -> Result<Value, HttpError> {
    let user_id = user
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| HttpError(ApiError::new(500, "auth user missing id")))?;
    let refresh_token = gen_refresh_token();
    let session_id = gen_uuid();
    let now = now_unix();
    let expires_at = now + REFRESH_TOKEN_TTL_SECS;
    let response = response_fn(user, &refresh_token, &session_id, now)?;
    let project_id = auth_project_id(state);
    state.auth_issue_refresh_token(
        &project_id,
        &refresh_token,
        user_id,
        &session_id,
        now,
        expires_at,
    )?;
    Ok(response)
}

fn build_session(user: &Value, state: &AppState) -> Result<Value, HttpError> {
    build_session_with_response(user, state, |user, refresh_token, session_id, now| {
        session_response(user, refresh_token.to_string(), session_id, now)
    })
}

async fn auth_signup(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, HttpError> {
    check_auth_rate_limit(&headers)?;
    let email = body
        .get("email")
        .and_then(Value::as_str)
        .ok_or_else(|| HttpError(ApiError::new(400, "missing email")))?;
    let password = body
        .get("password")
        .and_then(Value::as_str)
        .ok_or_else(|| HttpError(ApiError::new(400, "missing password")))?;

    let now = now_iso();
    let password_hash = hash_password(password)?;
    let user = json!({
        "id": gen_uuid(),
        "email": email,
        "phone": body.get("phone"),
        "password_hash": password_hash,
        "user_metadata": body.get("data").cloned().unwrap_or(json!({})),
        "app_metadata": {},
        "aud": "authenticated",
        "role": "authenticated",
        "created_at": now,
        "email_confirmed_at": now,
        "last_sign_in_at": now,
        "updated_at": now,
    });

    let project_id = auth_project_id(&state);
    let user = state.auth_create_user(&project_id, user)?;

    let session = build_session(&user, &state)?;
    Ok(Json(session))
}

async fn auth_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, HttpError> {
    check_auth_rate_limit(&headers)?;
    let grant_type = query
        .get("grant_type")
        .map(String::as_str)
        .unwrap_or("password");

    match grant_type {
        "password" => {
            let email = body
                .get("email")
                .or_else(|| body.get("phone"))
                .and_then(Value::as_str)
                .ok_or_else(|| HttpError(ApiError::new(400, "missing email or phone")))?;
            let password = body
                .get("password")
                .and_then(Value::as_str)
                .ok_or_else(|| HttpError(ApiError::new(400, "missing password")))?;

            let project_id = auth_project_id(&state);
            let user = state
                .auth_get_user_by_email_or_phone(&project_id, email)?
                .filter(|u| {
                    u.get("password_hash")
                        .and_then(Value::as_str)
                        .map(|h| verify_password(password, h))
                        .unwrap_or(false)
                });

            let user =
                user.ok_or_else(|| HttpError(ApiError::new(401, "Invalid login credentials")))?;

            let session = build_session(&user, &state)?;
            Ok(Json(session))
        }
        "refresh_token" => {
            let refresh_token = body
                .get("refresh_token")
                .and_then(Value::as_str)
                .ok_or_else(|| HttpError(ApiError::new(400, "missing refresh_token")))?;

            let new_refresh_token = gen_refresh_token();
            let now = now_unix();
            let expires_at = now + REFRESH_TOKEN_TTL_SECS;
            let project_id = auth_project_id(&state);
            let current = state.auth_get_active_refresh_token(&project_id, refresh_token, now)?;
            let session_id = current
                .get("session_id")
                .and_then(Value::as_str)
                .ok_or_else(|| HttpError(ApiError::new(500, "refresh token missing session id")))?;
            let user = current
                .get("user")
                .cloned()
                .ok_or_else(|| HttpError(ApiError::new(500, "refresh token missing user")))?;
            let session = session_response(&user, new_refresh_token.clone(), session_id, now)?;
            state.auth_rotate_refresh_token(
                &project_id,
                refresh_token,
                &new_refresh_token,
                now,
                expires_at,
            )?;
            Ok(Json(session))
        }
        _ => Err(HttpError(ApiError::new(400, "unsupported grant_type"))),
    }
}

#[derive(Debug, Deserialize)]
struct LogoutQuery {
    #[serde(default)]
    scope: Option<String>,
}

fn parse_logout_scope(scope: Option<&str>) -> Result<AuthLogoutScope, HttpError> {
    match scope.unwrap_or("global") {
        "global" => Ok(AuthLogoutScope::Global),
        "local" => Ok(AuthLogoutScope::Local),
        "others" => Ok(AuthLogoutScope::Others),
        _ => Err(HttpError(ApiError::new(
            400,
            "logout scope must be global, local, or others",
        ))),
    }
}

async fn auth_logout(
    State(state): State<AppState>,
    Query(query): Query<LogoutQuery>,
    headers: HeaderMap,
) -> Result<Json<Value>, HttpError> {
    let actor = actor(&headers)?;
    let user_id = actor
        .sub
        .as_deref()
        .ok_or_else(|| HttpError(ApiError::new(401, "no user associated with token")))?;
    let scope = parse_logout_scope(query.scope.as_deref())?;
    let session_id = actor.claims.get("session_id").and_then(Value::as_str);
    let project_id = auth_project_id(&state);
    let result = state.auth_revoke_sessions(&project_id, user_id, session_id, scope, now_unix())?;
    Ok(Json(result))
}

async fn auth_get_user(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, HttpError> {
    let actor = actor(&headers)?;
    let user_id = actor
        .sub
        .as_ref()
        .ok_or_else(|| HttpError(ApiError::new(401, "no user associated with token")))?;

    let project_id = auth_project_id(&state);
    let user = state.auth_get_user_by_id(&project_id, user_id)?;

    match user {
        Some(u) => Ok(Json(json!({ "user": public_user(&u) }))),
        None => Ok(Json(json!({ "user": null }))),
    }
}

async fn auth_update_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, HttpError> {
    let actor = actor(&headers)?;
    let user_id = actor
        .sub
        .as_ref()
        .ok_or_else(|| HttpError(ApiError::new(401, "no user associated with token")))?;

    let patch = AuthUserPatch {
        email: body
            .get("email")
            .and_then(Value::as_str)
            .map(str::to_string),
        phone: body
            .get("phone")
            .and_then(Value::as_str)
            .map(str::to_string),
        password_hash: body
            .get("password")
            .and_then(Value::as_str)
            .map(hash_password)
            .transpose()?,
        user_metadata: body.get("data").cloned(),
        updated_at: now_iso(),
    };
    let project_id = auth_project_id(&state);
    let user = state.auth_update_user(&project_id, user_id, patch)?;

    Ok(Json(json!({ "user": public_user(&user) })))
}

async fn auth_settings() -> Result<Json<Value>, HttpError> {
    Ok(Json(json!({
        "external": {},
        "disable_signup": false,
        "mailer_autoconfirm": true,
        "phone_autoconfirm": true,
        "sms_provider": "",
        "mfa": {},
        "saml_enabled": false,
    })))
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
        let code = match self.0.status {
            400 => "22023",
            401 => "42P17",
            403 => "42501",
            404 => "42P01",
            409 => "23P01",
            425 => "55P03",
            500 => "XX000",
            502 => "08006",
            503 => "57P03",
            504 => "57014",
            _ => "XX000",
        };
        let hint = if self.0.status == 401 {
            Some("Provide a valid Authorization header with a Bearer token or an apikey header.")
        } else if self.0.status == 403 {
            Some("Ensure the token has the required role (service_role or admin).")
        } else {
            None
        };
        (
            status,
            Json(json!({
                "code": code,
                "message": self.0.message,
                "details": null,
                "hint": hint,
            })),
        )
            .into_response()
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
        auth_headers_for_sub("alice")
    }

    fn auth_headers_for_sub(sub: &str) -> HeaderMap {
        let token = mint_token(sub, "authenticated", Map::new(), None).unwrap();
        bearer_headers(&token)
    }

    fn bearer_headers(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {}", token).parse().unwrap(),
        );
        headers
    }

    fn auth_rate_headers(ip: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", ip.parse().unwrap());
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
    async fn postgrest_order_and_offset() {
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
        for i in 0..5 {
            let mut q = HashMap::new();
            q.insert("select".to_string(), "*".to_string());
            supabase_insert_rows(
                State(runtime.clone()),
                Path("notes".to_string()),
                Query(q),
                auth_headers(),
                Json(json!({"owner_id":"alice","title":format!("row-{i}")})),
            )
            .await
            .unwrap();
        }

        let mut q = HashMap::new();
        q.insert("select".to_string(), "*".to_string());
        q.insert("order".to_string(), "title.desc".to_string());
        q.insert("offset".to_string(), "1".to_string());
        q.insert("limit".to_string(), "2".to_string());
        let selected = supabase_select_rows(
            State(runtime.clone()),
            Path("notes".to_string()),
            Query(q),
            auth_headers(),
        )
        .await
        .unwrap();
        let selected = response_json(selected).await;
        let arr = selected.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["title"], "row-3");
        assert_eq!(arr[1]["title"], "row-2");
    }

    #[tokio::test]
    async fn postgrest_neq_filter() {
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
        for title in ["alpha", "beta", "gamma"] {
            let mut q = HashMap::new();
            q.insert("select".to_string(), "*".to_string());
            supabase_insert_rows(
                State(runtime.clone()),
                Path("notes".to_string()),
                Query(q),
                auth_headers(),
                Json(json!({"owner_id":"alice","title":title})),
            )
            .await
            .unwrap();
        }

        let mut q = HashMap::new();
        q.insert("select".to_string(), "*".to_string());
        q.insert("title".to_string(), "neq.alpha".to_string());
        let selected = supabase_select_rows(
            State(runtime.clone()),
            Path("notes".to_string()),
            Query(q),
            auth_headers(),
        )
        .await
        .unwrap();
        let selected = response_json(selected).await;
        let arr = selected.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        for row in arr {
            assert_ne!(row["title"], "alpha");
        }
    }

    #[tokio::test]
    async fn postgrest_or_filter() {
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
        for title in ["alpha", "beta", "gamma"] {
            let mut q = HashMap::new();
            q.insert("select".to_string(), "*".to_string());
            supabase_insert_rows(
                State(runtime.clone()),
                Path("notes".to_string()),
                Query(q),
                auth_headers(),
                Json(json!({"owner_id":"alice","title":title})),
            )
            .await
            .unwrap();
        }

        let mut q = HashMap::new();
        q.insert("select".to_string(), "*".to_string());
        q.insert(
            "or".to_string(),
            "title.eq.alpha,title.eq.gamma".to_string(),
        );
        let selected = supabase_select_rows(
            State(runtime.clone()),
            Path("notes".to_string()),
            Query(q),
            auth_headers(),
        )
        .await
        .unwrap();
        let selected = response_json(selected).await;
        let arr = selected.as_array().unwrap();
        assert_eq!(arr.len(), 2);
    }

    #[tokio::test]
    async fn postgrest_select_projection() {
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
        let mut q = HashMap::new();
        q.insert("select".to_string(), "*".to_string());
        supabase_insert_rows(
            State(runtime.clone()),
            Path("notes".to_string()),
            Query(q.clone()),
            auth_headers(),
            Json(json!({"owner_id":"alice","title":"proj-test"})),
        )
        .await
        .unwrap();

        q.insert("select".to_string(), "title".to_string());
        q.insert("title".to_string(), "eq.proj-test".to_string());
        let selected = supabase_select_rows(
            State(runtime.clone()),
            Path("notes".to_string()),
            Query(q),
            auth_headers(),
        )
        .await
        .unwrap();
        let selected = response_json(selected).await;
        let arr = selected.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["title"], "proj-test");
        assert!(arr[0].get("owner_id").is_none());
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
            Json(ProjectRequest {
                id: None,
                project_id: Some("evil".to_string()),
            }),
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
            Query(EventsQuery {
                since: 0,
                limit: 100,
            }),
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
            Query(EventsQuery {
                since: 0,
                limit: 100,
            }),
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
            Query(EventsQuery {
                since: 0,
                limit: 100,
            }),
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

    #[tokio::test]
    async fn auth_logout_local_revokes_only_current_session() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = ProjectRuntime::new(dir.path().join("r"), RuntimeOptions::default()).unwrap();
        runtime.create_project("demo").unwrap();
        let runtime = Arc::new(runtime);
        let email = "local-logout@example.com";
        let password = "testpass123";

        let first = auth_signup(
            State(runtime.clone()),
            auth_rate_headers("10.0.0.10"),
            Json(json!({"email": email, "password": password})),
        )
        .await
        .unwrap()
        .0;
        let second = auth_token(
            State(runtime.clone()),
            auth_rate_headers("10.0.0.11"),
            Query(HashMap::from([(
                "grant_type".to_string(),
                "password".to_string(),
            )])),
            Json(json!({"email": email, "password": password})),
        )
        .await
        .unwrap()
        .0;

        let _ = auth_logout(
            State(runtime.clone()),
            Query(LogoutQuery {
                scope: Some("local".to_string()),
            }),
            bearer_headers(first["access_token"].as_str().unwrap()),
        )
        .await
        .unwrap();

        let err = auth_token(
            State(runtime.clone()),
            auth_rate_headers("10.0.0.12"),
            Query(HashMap::from([(
                "grant_type".to_string(),
                "refresh_token".to_string(),
            )])),
            Json(json!({"refresh_token": first["refresh_token"]})),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0.status, 401);

        let refreshed = auth_token(
            State(runtime.clone()),
            auth_rate_headers("10.0.0.13"),
            Query(HashMap::from([(
                "grant_type".to_string(),
                "refresh_token".to_string(),
            )])),
            Json(json!({"refresh_token": second["refresh_token"]})),
        )
        .await
        .unwrap()
        .0;
        assert!(refreshed["access_token"].as_str().unwrap().len() > 10);
    }

    #[tokio::test]
    async fn auth_logout_global_revokes_all_user_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = ProjectRuntime::new(dir.path().join("r"), RuntimeOptions::default()).unwrap();
        runtime.create_project("demo").unwrap();
        let runtime = Arc::new(runtime);
        let email = "global-logout@example.com";
        let password = "testpass123";

        let first = auth_signup(
            State(runtime.clone()),
            auth_rate_headers("10.0.0.20"),
            Json(json!({"email": email, "password": password})),
        )
        .await
        .unwrap()
        .0;
        let second = auth_token(
            State(runtime.clone()),
            auth_rate_headers("10.0.0.21"),
            Query(HashMap::from([(
                "grant_type".to_string(),
                "password".to_string(),
            )])),
            Json(json!({"email": email, "password": password})),
        )
        .await
        .unwrap()
        .0;

        let _ = auth_logout(
            State(runtime.clone()),
            Query(LogoutQuery { scope: None }),
            bearer_headers(first["access_token"].as_str().unwrap()),
        )
        .await
        .unwrap();

        for (idx, token) in [
            first["refresh_token"].as_str().unwrap(),
            second["refresh_token"].as_str().unwrap(),
        ]
        .iter()
        .enumerate()
        {
            let err = auth_token(
                State(runtime.clone()),
                auth_rate_headers(&format!("10.0.0.{}", 22 + idx)),
                Query(HashMap::from([(
                    "grant_type".to_string(),
                    "refresh_token".to_string(),
                )])),
                Json(json!({"refresh_token": token})),
            )
            .await
            .unwrap_err();
            assert_eq!(err.0.status, 401);
        }
    }

    #[test]
    fn failed_session_response_does_not_issue_refresh_token() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = ProjectRuntime::new(dir.path().join("r"), RuntimeOptions::default()).unwrap();
        runtime.create_project("demo").unwrap();
        let runtime = Arc::new(runtime);
        let user = runtime
            .auth_create_user(
                "demo",
                json!({
                    "id": "mint-fail-user",
                    "email": "mint-fail@example.com",
                    "phone": null,
                    "password_hash": "pw",
                    "user_metadata": {},
                    "app_metadata": {},
                    "aud": "authenticated",
                    "role": "authenticated",
                    "created_at": "2026-06-21T00:00:00Z",
                    "email_confirmed_at": "2026-06-21T00:00:00Z",
                    "last_sign_in_at": "2026-06-21T00:00:00Z",
                    "updated_at": "2026-06-21T00:00:00Z"
                }),
            )
            .unwrap();
        let err =
            build_session_with_response(&user, &runtime, |_user, _refresh, _session, _now| {
                Err(HttpError(ApiError::new(500, "mint failed")))
            })
            .unwrap_err();
        assert_eq!(err.0.status, 500);
        assert_eq!(
            runtime
                .auth_active_refresh_token_count("demo", "mint-fail-user", now_unix())
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn supabase_storage_rejects_anonymous_bucket_management() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = ProjectRuntime::new(dir.path().join("r"), RuntimeOptions::default()).unwrap();
        runtime.create_project("demo").unwrap();
        let runtime = Arc::new(runtime);

        let err = supabase_create_bucket(
            State(runtime.clone()),
            HeaderMap::new(),
            Json(json!({"name":"files"})),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0.status, 403);

        let _ = supabase_create_bucket(
            State(runtime.clone()),
            admin_headers(),
            Json(json!({"name":"files"})),
        )
        .await
        .unwrap();

        let err = supabase_list_buckets(State(runtime.clone()), HeaderMap::new())
            .await
            .unwrap_err();
        assert_eq!(err.0.status, 403);

        let err = supabase_get_bucket(
            State(runtime.clone()),
            Path("files".to_string()),
            HeaderMap::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0.status, 403);

        let err = supabase_delete_bucket(
            State(runtime.clone()),
            Path("files".to_string()),
            HeaderMap::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0.status, 403);
    }

    #[tokio::test]
    async fn supabase_storage_enforces_owner_for_object_access() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = ProjectRuntime::new(dir.path().join("r"), RuntimeOptions::default()).unwrap();
        runtime.create_project("demo").unwrap();
        let runtime = Arc::new(runtime);

        let _ = supabase_create_bucket(
            State(runtime.clone()),
            admin_headers(),
            Json(json!({"name":"files"})),
        )
        .await
        .unwrap();
        let mut alice_upload = auth_headers_for_sub("alice");
        alice_upload.insert(header::CONTENT_TYPE, "text/plain".parse().unwrap());
        let _ = supabase_upload_object(
            State(runtime.clone()),
            Path(("files".to_string(), "alice.txt".to_string())),
            alice_upload,
            Bytes::from("secret"),
        )
        .await
        .unwrap();

        let anon_err = supabase_download_object(
            State(runtime.clone()),
            Path(("files".to_string(), "alice.txt".to_string())),
            HeaderMap::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(anon_err.0.status, 403);

        let bob_err = supabase_download_object(
            State(runtime.clone()),
            Path(("files".to_string(), "alice.txt".to_string())),
            auth_headers_for_sub("bob"),
        )
        .await
        .unwrap_err();
        assert_eq!(bob_err.0.status, 403);

        let bob_objects = supabase_list_objects(
            State(runtime.clone()),
            Path("files".to_string()),
            auth_headers_for_sub("bob"),
            Json(SupabaseListObjectsRequest {
                prefix: None,
                limit: Some(10),
                offset: Some(0),
            }),
        )
        .await
        .unwrap()
        .into_response();
        let bob_objects = response_json(bob_objects).await;
        assert_eq!(bob_objects.as_array().unwrap().len(), 0);

        let alice_objects = supabase_list_objects(
            State(runtime.clone()),
            Path("files".to_string()),
            auth_headers_for_sub("alice"),
            Json(SupabaseListObjectsRequest {
                prefix: None,
                limit: Some(10),
                offset: Some(0),
            }),
        )
        .await
        .unwrap()
        .into_response();
        let alice_objects = response_json(alice_objects).await;
        assert_eq!(alice_objects.as_array().unwrap().len(), 1);

        let bob_delete = supabase_delete_object(
            State(runtime.clone()),
            Path(("files".to_string(), "alice.txt".to_string())),
            auth_headers_for_sub("bob"),
        )
        .await
        .unwrap_err();
        assert_eq!(bob_delete.0.status, 403);
    }

    #[tokio::test]
    async fn supabase_storage_bucket_crud() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = ProjectRuntime::new(dir.path().join("r"), RuntimeOptions::default()).unwrap();
        runtime.create_project("demo").unwrap();
        let runtime = Arc::new(runtime);

        let created = supabase_create_bucket(
            State(runtime.clone()),
            admin_headers(),
            Json(json!({"name":"test_bucket"})),
        )
        .await
        .unwrap()
        .into_response();
        let created = response_json(created).await;
        assert_eq!(created["bucket"], "test_bucket");

        let buckets = supabase_list_buckets(State(runtime.clone()), admin_headers())
            .await
            .unwrap()
            .into_response();
        let buckets = response_json(buckets).await;
        let arr = buckets.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["name"], "test_bucket");

        let bucket = supabase_get_bucket(
            State(runtime.clone()),
            Path("test_bucket".to_string()),
            admin_headers(),
        )
        .await
        .unwrap()
        .into_response();
        let bucket = response_json(bucket).await;
        assert_eq!(bucket["name"], "test_bucket");

        let deleted = supabase_delete_bucket(
            State(runtime.clone()),
            Path("test_bucket".to_string()),
            admin_headers(),
        )
        .await
        .unwrap()
        .into_response();
        let deleted = response_json(deleted).await;
        assert_eq!(deleted["deleted"], true);
    }

    #[tokio::test]
    async fn supabase_storage_object_upload_download_list_delete() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = ProjectRuntime::new(dir.path().join("r"), RuntimeOptions::default()).unwrap();
        runtime.create_project("demo").unwrap();
        let runtime = Arc::new(runtime);

        let _ = supabase_create_bucket(
            State(runtime.clone()),
            admin_headers(),
            Json(json!({"name":"files"})),
        )
        .await
        .unwrap();

        let mut upload_headers = admin_headers();
        upload_headers.insert(header::CONTENT_TYPE, "text/plain".parse().unwrap());
        let uploaded = supabase_upload_object(
            State(runtime.clone()),
            Path(("files".to_string(), "hello.txt".to_string())),
            upload_headers.clone(),
            Bytes::from("hello world"),
        )
        .await
        .unwrap()
        .into_response();
        let uploaded = response_json(uploaded).await;
        assert_eq!(uploaded["object"]["key"], "hello.txt");

        let result = supabase_download_object(
            State(runtime.clone()),
            Path(("files".to_string(), "hello.txt".to_string())),
            admin_headers(),
        )
        .await
        .unwrap();
        let body = axum::body::to_bytes(result.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(body.as_ref(), b"hello world");

        let objects = supabase_list_objects(
            State(runtime.clone()),
            Path("files".to_string()),
            admin_headers(),
            Json(SupabaseListObjectsRequest {
                prefix: None,
                limit: Some(10),
                offset: Some(0),
            }),
        )
        .await
        .unwrap()
        .into_response();
        let objects = response_json(objects).await;
        let arr = objects.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["key"], "hello.txt");

        let deleted = supabase_delete_object(
            State(runtime.clone()),
            Path(("files".to_string(), "hello.txt".to_string())),
            admin_headers(),
        )
        .await
        .unwrap()
        .into_response();
        let deleted = response_json(deleted).await;
        assert_eq!(deleted["deleted"], true);

        let err = supabase_download_object(
            State(runtime.clone()),
            Path(("files".to_string(), "hello.txt".to_string())),
            admin_headers(),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0.status, 404);
    }

    #[tokio::test]
    async fn supabase_realtime_stream_rejects_anon() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = ProjectRuntime::new(dir.path().join("r"), RuntimeOptions::default()).unwrap();
        runtime.create_project("demo").unwrap();
        let err = supabase_realtime_stream(
            State(Arc::new(runtime)),
            Query(SupabaseRealtimeQuery {
                since: 0,
                table: None,
            }),
            HeaderMap::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0.status, 403);
    }

    #[tokio::test]
    async fn supabase_realtime_stream_returns_events_for_authenticated() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = ProjectRuntime::new(dir.path().join("r"), RuntimeOptions::default()).unwrap();
        runtime.create_project("demo").unwrap();
        let runtime = Arc::new(runtime);

        let table = TableSpec {
            name: "posts".to_string(),
            columns: vec![
                ColumnSpec {
                    name: "id".to_string(),
                    r#type: "integer".to_string(),
                    primary_key: true,
                    auto_increment: true,
                    not_null: true,
                },
                ColumnSpec {
                    name: "title".to_string(),
                    r#type: "text".to_string(),
                    primary_key: false,
                    auto_increment: false,
                    not_null: true,
                },
            ],
        };
        let _ = create_table(
            State(runtime.clone()),
            Path("demo".to_string()),
            admin_headers(),
            Json(table),
        )
        .await
        .unwrap();

        let q = HashMap::new();
        supabase_insert_rows(
            State(runtime.clone()),
            Path("posts".to_string()),
            Query(q.clone()),
            auth_headers(),
            Json(json!({"title": "hello world"})),
        )
        .await
        .unwrap();

        let response = supabase_realtime_stream(
            State(runtime.clone()),
            Query(SupabaseRealtimeQuery {
                since: 0,
                table: None,
            }),
            auth_headers(),
        )
        .await
        .unwrap()
        .into_response();

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_str = String::from_utf8(body.to_vec()).unwrap();
        assert!(body_str.contains("event: insert"));
        assert!(body_str.contains("\"table\":\"posts\""));
        assert!(body_str.contains("\"record\""));
    }
}
