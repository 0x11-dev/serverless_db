use crate::auth::Actor;
use crate::object_store::{LocalObjectStore, ObjectStoreRef};
use crate::policy::{compile_policies, evaluate_policies, quote_ident, valid_ident};
use crate::postgrest::{self, SelectQuery};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OptionalExtension, params_from_iter};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
    mpsc,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Notify;

#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
}

impl ApiError {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ApiError {}

impl From<rusqlite::Error> for ApiError {
    fn from(value: rusqlite::Error) -> Self {
        Self::new(500, format!("sqlite error: {value}"))
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(value: anyhow::Error) -> Self {
        Self::new(500, value.to_string())
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(value: serde_json::Error) -> Self {
        Self::new(500, format!("json error: {value}"))
    }
}

impl From<crate::policy::PolicyError> for ApiError {
    fn from(value: crate::policy::PolicyError) -> Self {
        Self::new(400, value.to_string())
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeOptions {
    pub snapshot_every_ops: u64,
    pub snapshot_every_ms: u64,
    pub metadata_every_ops: u64,
    pub group_commit_max_ops: usize,
    pub group_commit_delay_ms: u64,
    pub writer_queue_capacity: usize,
    pub max_durable_wal_bytes: u64,
    pub writer_lease_ttl_ms: u64,
    pub read_replica: bool,
    pub replica_refresh_interval_ms: u64,
    pub replica_bookmark_wait_timeout_ms: u64,
    pub primary_url: Option<String>,
    pub routing_region: Option<String>,
    pub routing_endpoints: Vec<RoutingEndpoint>,
    pub forward_connect_timeout_ms: u64,
    pub forward_request_timeout_ms: u64,
    pub forward_max_attempts: usize,
    pub forward_retry_backoff_ms: u64,
    pub routing_endpoint_failure_threshold: u32,
    pub routing_endpoint_cooldown_ms: u64,
    pub sqlite_synchronous: String,
    pub supabase_project_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingEndpoint {
    pub role: String,
    pub region: Option<String>,
    pub url: String,
}

impl Default for RuntimeOptions {
    fn default() -> Self {
        Self {
            snapshot_every_ops: 1000,
            snapshot_every_ms: 60_000,
            metadata_every_ops: 100,
            group_commit_max_ops: 64,
            group_commit_delay_ms: 2,
            writer_queue_capacity: 1024,
            max_durable_wal_bytes: 64 * 1024 * 1024,
            writer_lease_ttl_ms: 30_000,
            read_replica: false,
            replica_refresh_interval_ms: 1_000,
            replica_bookmark_wait_timeout_ms: 5_000,
            primary_url: None,
            routing_region: None,
            routing_endpoints: Vec::new(),
            forward_connect_timeout_ms: 1_000,
            forward_request_timeout_ms: 5_000,
            forward_max_attempts: 3,
            forward_retry_backoff_ms: 25,
            routing_endpoint_failure_threshold: 2,
            routing_endpoint_cooldown_ms: 1_000,
            sqlite_synchronous: "NORMAL".to_string(),
            supabase_project_id: "demo".to_string(),
        }
    }
}

#[derive(Clone)]
pub struct ProjectRuntime {
    runtime_dir: PathBuf,
    cache_dir: PathBuf,
    object_store: ObjectStoreRef,
    options: RuntimeOptions,
    forward_client: reqwest::Client,
    forward_trace_seq: Arc<AtomicU64>,
    endpoint_health: Arc<Mutex<HashMap<String, EndpointHealth>>>,
    runtime_id: String,
    states: Arc<Mutex<HashMap<String, Arc<ProjectHandle>>>>,
    notifiers: Arc<Mutex<HashMap<String, Arc<Notify>>>>,
}

#[derive(Debug, Clone)]
struct EndpointHealth {
    consecutive_failures: u32,
    open_until: Option<Instant>,
}

struct ProjectHandle {
    state: Arc<Mutex<ProjectState>>,
    writer: Mutex<Option<mpsc::SyncSender<WriteJob>>>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
    replica_stop: Mutex<Option<mpsc::Sender<()>>>,
    replica_worker: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl ProjectHandle {
    fn writer(&self) -> Result<mpsc::SyncSender<WriteJob>, ApiError> {
        self.writer
            .lock()
            .unwrap()
            .as_ref()
            .cloned()
            .ok_or_else(|| ApiError::new(503, "project writer is unavailable"))
    }

    fn stop_writer(&self) -> Result<(), ApiError> {
        self.writer.lock().unwrap().take();
        if let Some(worker) = self.worker.lock().unwrap().take() {
            worker
                .join()
                .map_err(|_| ApiError::new(500, "project writer panicked"))?;
        }
        self.replica_stop.lock().unwrap().take();
        if let Some(worker) = self.replica_worker.lock().unwrap().take() {
            worker
                .join()
                .map_err(|_| ApiError::new(500, "project replica worker panicked"))?;
        }
        Ok(())
    }
}

struct ProjectState {
    project_id: String,
    conn: Connection,
    cache_dir: PathBuf,
    db_path: PathBuf,
    generation: u64,
    commit_seq: u64,
    current_snapshot: Option<SnapshotManifest>,
    wal_segments: Vec<WalSegmentManifest>,
    last_durable_wal_bytes: u64,
    next_wal_segment_id: u64,
    ops_since_snapshot: u64,
    last_snapshot_at_ms: u128,
    writer_lease_fencing_token: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WriterLease {
    schema_version: u32,
    project_id: String,
    owner_id: String,
    fencing_token: u64,
    acquired_at_ms: u128,
    expires_at_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DurableManifest {
    schema_version: u32,
    project_id: String,
    generation: u64,
    #[serde(default)]
    commit_seq: u64,
    #[serde(default)]
    bookmark: String,
    updated_at: String,
    reason: Option<String>,
    snapshot: SnapshotManifest,
    wal: WalChainManifest,
    ops_since_snapshot: u64,
    gc_watermark_generation: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SnapshotManifest {
    key: String,
    bytes: u64,
    sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WalChainManifest {
    prefix: String,
    durable_bytes: u64,
    segments: Vec<WalSegmentManifest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WalSegmentManifest {
    id: u64,
    key: String,
    offset: u64,
    len: u64,
    sha256: String,
}

struct WriteJob {
    kind: WriteJobKind,
    idempotency: Option<WriteIdempotency>,
    response: mpsc::SyncSender<Result<Value, ApiError>>,
}

enum WriteJobKind {
    CreateTable(TableSpec),
    SetPolicy(PolicySpec),
    Insert {
        table: String,
        data: Value,
        actor: Actor,
    },
    Update {
        table: String,
        filters: HashMap<String, String>,
        data: Value,
        actor: Actor,
    },
    Delete {
        table: String,
        filters: HashMap<String, String>,
        actor: Actor,
    },
    UpdatePostgrest {
        table: String,
        filters: crate::postgrest::FilterExpr,
        data: Value,
        actor: Actor,
    },
    DeletePostgrest {
        table: String,
        filters: crate::postgrest::FilterExpr,
        actor: Actor,
    },
    CreateBucket {
        name: String,
    },
    PutObject {
        bucket: String,
        key: String,
        data: Vec<u8>,
        content_type: String,
        actor: Actor,
    },
    DeleteObject {
        bucket: String,
        key: String,
        actor: Actor,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ColumnSpec {
    pub name: String,
    #[serde(default = "default_text_type")]
    pub r#type: String,
    #[serde(default)]
    pub primary_key: bool,
    #[serde(default = "default_auto_increment")]
    pub auto_increment: bool,
    #[serde(default)]
    pub not_null: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TableSpec {
    pub name: String,
    pub columns: Vec<ColumnSpec>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PolicySpec {
    pub table: String,
    #[serde(default = "default_all_operation")]
    pub operation: String,
    #[serde(default)]
    pub name: Option<String>,
    pub rule: Value,
}

#[derive(Debug, Serialize)]
pub struct ObjectRead {
    pub meta: Value,
    #[serde(skip)]
    pub data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct WriteIdempotency {
    pub key: String,
    pub request_hash: String,
}

const TYPE_MAP: &[(&str, &str)] = &[
    ("text", "TEXT"),
    ("integer", "INTEGER"),
    ("real", "REAL"),
    ("numeric", "NUMERIC"),
    ("blob", "BLOB"),
    ("boolean", "INTEGER"),
    ("json", "TEXT"),
    ("timestamp", "TEXT"),
];

impl ProjectRuntime {
    pub fn new(runtime_dir: impl AsRef<Path>, options: RuntimeOptions) -> anyhow::Result<Self> {
        let runtime_dir = runtime_dir.as_ref().to_path_buf();
        let object_store = Arc::new(LocalObjectStore::new(runtime_dir.join("object_store"))?);
        Self::with_object_store(runtime_dir, options, object_store)
    }

    pub fn with_object_store(
        runtime_dir: impl AsRef<Path>,
        options: RuntimeOptions,
        object_store: ObjectStoreRef,
    ) -> anyhow::Result<Self> {
        let runtime_dir = runtime_dir.as_ref().to_path_buf();
        let cache_dir = runtime_dir.join("cache");
        fs::create_dir_all(&cache_dir)?;
        let forward_client = build_forward_client(&options)?;
        Ok(Self {
            runtime_dir,
            cache_dir,
            object_store,
            options,
            forward_client,
            forward_trace_seq: Arc::new(AtomicU64::new(1)),
            endpoint_health: Arc::new(Mutex::new(HashMap::new())),
            runtime_id: new_runtime_id(),
            states: Arc::new(Mutex::new(HashMap::new())),
            notifiers: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub fn runtime_dir(&self) -> &Path {
        &self.runtime_dir
    }

    pub fn is_read_replica(&self) -> bool {
        self.options.read_replica
    }

    pub fn runtime_id(&self) -> &str {
        &self.runtime_id
    }

    pub fn forward_client(&self) -> &reqwest::Client {
        &self.forward_client
    }

    pub fn forward_max_attempts(&self) -> usize {
        self.options.forward_max_attempts.max(1)
    }

    pub fn forward_retry_backoff(&self) -> Duration {
        Duration::from_millis(self.options.forward_retry_backoff_ms)
    }

    pub fn next_forward_trace_id(&self) -> String {
        let seq = self.forward_trace_seq.fetch_add(1, Ordering::SeqCst);
        format!("{}-{seq}", self.runtime_id)
    }

    pub fn record_forward_success(&self, endpoint_url: &str) {
        self.endpoint_health.lock().unwrap().remove(endpoint_url);
    }

    pub fn record_forward_failure(&self, endpoint_url: &str) {
        let threshold = self.options.routing_endpoint_failure_threshold.max(1);
        let cooldown = Duration::from_millis(self.options.routing_endpoint_cooldown_ms);
        let mut health = self.endpoint_health.lock().unwrap();
        let entry = health
            .entry(endpoint_url.to_string())
            .or_insert(EndpointHealth {
                consecutive_failures: 0,
                open_until: None,
            });
        entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
        if entry.consecutive_failures >= threshold {
            entry.open_until = Some(Instant::now() + cooldown);
        }
    }

    pub fn forward_endpoint_available(&self, endpoint_url: &str) -> bool {
        let mut health = self.endpoint_health.lock().unwrap();
        let Some(entry) = health.get(endpoint_url).cloned() else {
            return true;
        };
        if let Some(open_until) = entry.open_until
            && open_until > Instant::now()
        {
            return false;
        }
        health.remove(endpoint_url);
        true
    }

    pub fn forward_endpoint_health_info(&self) -> Value {
        let now = Instant::now();
        let mut items = self
            .endpoint_health
            .lock()
            .unwrap()
            .iter()
            .map(|(url, health)| {
                json!({
                    "url": url,
                    "consecutive_failures": health.consecutive_failures,
                    "open": health.open_until.is_some_and(|open_until| open_until > now)
                })
            })
            .collect::<Vec<_>>();
        items.sort_by(|left, right| {
            left["url"]
                .as_str()
                .unwrap_or_default()
                .cmp(right["url"].as_str().unwrap_or_default())
        });
        Value::Array(items)
    }

    pub fn primary_url(&self) -> Option<&str> {
        self.options
            .routing_endpoints
            .iter()
            .find(|endpoint| endpoint.role == "primary")
            .map(|endpoint| endpoint.url.as_str())
            .or(self.options.primary_url.as_deref())
    }

    pub fn replica_url_for_region(&self, preferred_region: Option<&str>) -> Option<&str> {
        let replicas = self
            .options
            .routing_endpoints
            .iter()
            .filter(|endpoint| endpoint.role == "replica")
            .filter(|endpoint| self.forward_endpoint_available(&endpoint.url))
            .collect::<Vec<_>>();
        if replicas.is_empty() {
            return None;
        }
        if let Some(preferred_region) = preferred_region.filter(|region| !region.is_empty())
            && let Some(endpoint) = replicas
                .iter()
                .find(|endpoint| endpoint.region.as_deref() == Some(preferred_region))
        {
            return Some(endpoint.url.as_str());
        }
        if let Some(local_region) = self.options.routing_region.as_deref()
            && let Some(endpoint) = replicas
                .iter()
                .find(|endpoint| endpoint.region.as_deref() == Some(local_region))
        {
            return Some(endpoint.url.as_str());
        }
        replicas.first().map(|endpoint| endpoint.url.as_str())
    }

    pub fn served_by_meta(&self) -> Value {
        json!({
            "served_by_region": self.options.routing_region.clone(),
            "served_by_primary": !self.options.read_replica,
            "runtime_mode": if self.options.read_replica { "read_replica" } else { "primary" }
        })
    }

    pub fn supabase_project_id(&self) -> &str {
        &self.options.supabase_project_id
    }

    pub fn routing_info(&self) -> Value {
        json!({
            "region": self.options.routing_region.clone(),
            "primary_url": self.primary_url(),
            "selected_replica_url": self.replica_url_for_region(None),
            "legacy_primary_url": self.options.primary_url.clone(),
            "endpoints": self.options.routing_endpoints.clone(),
            "endpoint_health": self.forward_endpoint_health_info()
        })
    }

    pub fn create_project(&self, project_id: &str) -> Result<Value, ApiError> {
        self.ensure_primary_write_allowed("create_project")?;
        self.ensure_project(project_id)?;
        self.project_info(project_id)
    }

    pub fn create_table_with_idempotency(
        &self,
        project_id: &str,
        spec: TableSpec,
        idempotency: Option<WriteIdempotency>,
    ) -> Result<Value, ApiError> {
        self.submit_write_with_idempotency(project_id, WriteJobKind::CreateTable(spec), idempotency)
    }

    pub fn set_policy_with_idempotency(
        &self,
        project_id: &str,
        spec: PolicySpec,
        idempotency: Option<WriteIdempotency>,
    ) -> Result<Value, ApiError> {
        self.submit_write_with_idempotency(project_id, WriteJobKind::SetPolicy(spec), idempotency)
    }

    pub fn project_info(&self, project_id: &str) -> Result<Value, ApiError> {
        let pid = safe_project_id(project_id)?;
        let manifest_key = manifest_key(pid);
        let wal_prefix = wal_prefix(pid);
        let wal_segments = self.object_store.list_prefix(&wal_prefix)?;
        let wal_bytes = wal_segments.iter().map(|item| item.len).sum::<u64>();
        let manifest = self.load_manifest(pid)?;
        let remote_commit_seq = manifest_commit_seq(manifest.as_ref());
        let local_commit_seq = self.local_commit_seq(pid);
        let (snapshot_key, snapshot_value) = if let Some(manifest) = &manifest {
            (
                manifest.snapshot.key.clone(),
                json!({
                    "key": manifest.snapshot.key,
                    "bytes": manifest.snapshot.bytes,
                    "sha256": manifest.snapshot.sha256
                }),
            )
        } else {
            let key = snapshot_key(pid);
            (key.clone(), json!({ "key": key }))
        };
        let bookmark = manifest
            .as_ref()
            .map(manifest_bookmark)
            .unwrap_or_else(|| bookmark_for_seq(0));
        let manifest_value = manifest
            .as_ref()
            .map(serde_json::to_value)
            .transpose()?
            .unwrap_or(Value::Null);
        let lease_value = self
            .load_writer_lease(pid)?
            .map(serde_json::to_value)
            .transpose()?
            .unwrap_or(Value::Null);
        Ok(json!({
            "project_id": pid,
            "cache_path": self.cache_dir.join(pid).join("main.sqlite"),
            "runtime_mode": if self.options.read_replica { "read_replica" } else { "primary" },
            "routing": self.routing_info(),
            "bookmark": bookmark,
            "local_bookmark": bookmark_for_seq(local_commit_seq),
            "replica": {
                "enabled": self.options.read_replica,
                "local_commit_seq": local_commit_seq,
                "remote_commit_seq": remote_commit_seq,
                "local_bookmark": bookmark_for_seq(local_commit_seq),
                "remote_bookmark": bookmark_for_seq(remote_commit_seq),
                "lag_commits": remote_commit_seq.saturating_sub(local_commit_seq),
                "refresh_interval_ms": self.options.replica_refresh_interval_ms,
                "bookmark_wait_timeout_ms": self.options.replica_bookmark_wait_timeout_ms
            },
            "snapshot_uri": self.object_store.describe(&snapshot_key),
            "snapshot_exists": self.object_store.exists(&snapshot_key)?,
            "snapshot_bytes": self.object_store.len(&snapshot_key)?.unwrap_or(0),
            "snapshot": snapshot_value,
            "wal_segment_prefix": self.object_store.describe(&wal_prefix),
            "wal_segment_count": wal_segments.len(),
            "durable_wal_bytes": wal_bytes,
            "manifest_uri": self.object_store.describe(&manifest_key),
            "manifest": manifest_value,
            "writer_lease": lease_value
        }))
    }

    fn local_commit_seq(&self, project_id: &str) -> u64 {
        self.states
            .lock()
            .unwrap()
            .get(project_id)
            .map(|handle| handle.state.lock().unwrap().commit_seq)
            .unwrap_or(0)
    }

    fn ensure_project_for_read(
        &self,
        project_id: &str,
        min_bookmark: Option<&str>,
    ) -> Result<Arc<ProjectHandle>, ApiError> {
        let pid = safe_project_id(project_id)?.to_string();
        let Some(requested_bookmark) = min_bookmark else {
            let handle = self.ensure_project(&pid)?;
            if self.options.read_replica {
                self.refresh_replica_handle(&handle, 0)?;
            }
            return Ok(handle);
        };
        let requested_seq = bookmark_to_seq(requested_bookmark)?;
        let handle = self.ensure_project(&pid)?;
        if self.options.read_replica {
            self.wait_for_replica_bookmark(&handle, requested_seq)?;
        }
        let current_seq = handle.state.lock().unwrap().commit_seq;
        if current_seq >= requested_seq {
            return Ok(handle);
        }

        self.evict_project_cache(&pid)?;
        let refreshed = self.ensure_project(&pid)?;
        let current_seq = refreshed.state.lock().unwrap().commit_seq;
        if current_seq >= requested_seq {
            return Ok(refreshed);
        }
        Err(ApiError::new(
            425,
            format!(
                "requested bookmark is not available yet: requested={}, current={}",
                requested_bookmark,
                bookmark_for_seq(current_seq)
            ),
        ))
    }

    fn wait_for_replica_bookmark(
        &self,
        handle: &Arc<ProjectHandle>,
        requested_seq: u64,
    ) -> Result<(), ApiError> {
        let started = Instant::now();
        let timeout = Duration::from_millis(self.options.replica_bookmark_wait_timeout_ms);
        let sleep = Duration::from_millis(self.options.replica_refresh_interval_ms.clamp(10, 250));
        loop {
            self.refresh_replica_handle(handle, requested_seq)?;
            let current_seq = handle.state.lock().unwrap().commit_seq;
            if current_seq >= requested_seq {
                return Ok(());
            }
            if started.elapsed() >= timeout {
                return Ok(());
            }
            std::thread::sleep(sleep);
        }
    }

    fn refresh_replica_handle(
        &self,
        handle: &Arc<ProjectHandle>,
        requested_seq: u64,
    ) -> Result<bool, ApiError> {
        let mut state = handle.state.lock().unwrap();
        self.refresh_replica_state(&mut state, requested_seq)
    }

    fn refresh_replica_state(
        &self,
        state: &mut ProjectState,
        requested_seq: u64,
    ) -> Result<bool, ApiError> {
        if !self.options.read_replica {
            return Ok(false);
        }
        let Some(manifest) = self.load_manifest(&state.project_id)? else {
            return Ok(false);
        };
        let remote_seq = manifest_commit_seq(Some(&manifest));
        if state.commit_seq >= remote_seq && state.commit_seq >= requested_seq {
            return Ok(false);
        }
        if remote_seq <= state.commit_seq && remote_seq < requested_seq {
            return Ok(false);
        }
        self.rehydrate_state_from_manifest(state, &manifest)?;
        Ok(true)
    }

    fn rehydrate_state_from_manifest(
        &self,
        state: &mut ProjectState,
        manifest: &DurableManifest,
    ) -> Result<(), ApiError> {
        let wal_path = state.db_path.with_extension("sqlite-wal");
        let placeholder = Connection::open_in_memory()?;
        let old_conn = std::mem::replace(&mut state.conn, placeholder);
        drop(old_conn);
        fs::remove_file(&state.db_path).ok();
        fs::remove_file(&wal_path).ok();
        fs::remove_file(state.db_path.with_extension("sqlite-shm")).ok();
        self.restore_snapshot(&state.db_path, &manifest.snapshot)?;
        let recovered_wal_segments = self.restore_manifest_wal(&wal_path, manifest)?;
        state.conn = open_project_connection(&state.db_path, &self.options.sqlite_synchronous)?;
        self.ensure_meta(&state.conn)?;
        state.generation = manifest.generation;
        state.commit_seq = manifest_commit_seq(Some(manifest));
        state.current_snapshot = Some(manifest.snapshot.clone());
        state.last_durable_wal_bytes = recovered_wal_segments
            .iter()
            .map(|item| item.len)
            .sum::<u64>();
        state.next_wal_segment_id = recovered_wal_segments
            .iter()
            .map(|item| item.id)
            .max()
            .map(|id| id + 1)
            .unwrap_or(0);
        state.wal_segments = recovered_wal_segments;
        state.ops_since_snapshot = manifest.ops_since_snapshot;
        state.last_snapshot_at_ms = now_ms();
        Ok(())
    }

    fn evict_project_cache(&self, project_id: &str) -> Result<(), ApiError> {
        if let Some(handle) = self.states.lock().unwrap().remove(project_id) {
            handle.stop_writer()?;
            drop(handle);
        }
        fs::remove_dir_all(self.cache_dir.join(project_id)).ok();
        Ok(())
    }

    fn spawn_replica_worker(
        &self,
        project_id: &str,
        state: Arc<Mutex<ProjectState>>,
    ) -> Result<
        (
            Option<mpsc::Sender<()>>,
            Option<std::thread::JoinHandle<()>>,
        ),
        ApiError,
    > {
        if !self.options.read_replica || self.options.replica_refresh_interval_ms == 0 {
            return Ok((None, None));
        }
        let (stop, receiver) = mpsc::channel();
        let runtime = self.clone();
        let pid = project_id.to_string();
        let worker = std::thread::Builder::new()
            .name(format!("sdb-replica-{pid}"))
            .spawn(move || runtime.replica_loop(&pid, state, receiver))
            .map_err(|err| ApiError::new(500, format!("failed to start replica worker: {err}")))?;
        Ok((Some(stop), Some(worker)))
    }

    fn replica_loop(
        &self,
        project_id: &str,
        state: Arc<Mutex<ProjectState>>,
        stop: mpsc::Receiver<()>,
    ) {
        let interval = Duration::from_millis(self.options.replica_refresh_interval_ms.max(1));
        loop {
            match stop.recv_timeout(interval) {
                Ok(_) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    let refreshed = {
                        let mut state = state.lock().unwrap();
                        self.refresh_replica_state(&mut state, 0)
                    };
                    if refreshed.is_ok_and(|changed| changed) {
                        self.notify(project_id);
                    }
                }
            }
        }
    }

    fn load_writer_lease(&self, project_id: &str) -> Result<Option<WriterLease>, ApiError> {
        let key = writer_lease_key(project_id);
        if !self.object_store.exists(&key)? {
            return Ok(None);
        }
        let lease: WriterLease = serde_json::from_slice(&self.object_store.read_bytes(&key)?)
            .map_err(|err| {
                ApiError::new(503, format!("invalid writer lease for {project_id}: {err}"))
            })?;
        Ok(Some(lease))
    }

    fn load_writer_lease_claim(&self, key: &str) -> Result<WriterLease, ApiError> {
        serde_json::from_slice(&self.object_store.read_bytes(key)?)
            .map_err(|err| ApiError::new(503, format!("invalid writer lease claim {key}: {err}")))
    }

    fn claim_expired_writer_lease(
        &self,
        project_id: &str,
        mut base_lease: WriterLease,
        now: u128,
        ttl_ms: u64,
    ) -> Result<WriterLease, ApiError> {
        for _ in 0..4 {
            let lease = WriterLease {
                schema_version: 1,
                project_id: project_id.to_string(),
                owner_id: self.runtime_id.clone(),
                fencing_token: base_lease.fencing_token.saturating_add(1),
                acquired_at_ms: now,
                expires_at_ms: now + ttl_ms as u128,
            };
            let claim_key = writer_lease_claim_key(project_id, lease.fencing_token);
            if self
                .object_store
                .put_bytes_if_absent(&claim_key, serde_json::to_vec_pretty(&lease)?.as_slice())?
            {
                self.object_store.put_bytes(
                    &writer_lease_key(project_id),
                    serde_json::to_vec_pretty(&lease)?.as_slice(),
                )?;
                return Ok(lease);
            }

            let claimed = self.load_writer_lease_claim(&claim_key)?;
            if claimed.project_id != project_id {
                return Err(ApiError::new(
                    503,
                    format!(
                        "writer lease claim project mismatch: expected {project_id}, got {}",
                        claimed.project_id
                    ),
                ));
            }
            if claimed.owner_id == self.runtime_id {
                self.object_store.put_bytes(
                    &writer_lease_key(project_id),
                    serde_json::to_vec_pretty(&claimed)?.as_slice(),
                )?;
                return Ok(claimed);
            }
            if claimed.expires_at_ms > now {
                return Err(ApiError::new(
                    423,
                    format!(
                        "project writer lease takeover is held by another runtime: project_id={project_id}, owner_id={}, fencing_token={}, expires_at_ms={}",
                        claimed.owner_id, claimed.fencing_token, claimed.expires_at_ms
                    ),
                ));
            }
            base_lease = claimed;
        }
        Err(ApiError::new(
            423,
            format!("project writer lease takeover kept changing: project_id={project_id}"),
        ))
    }

    fn ensure_writer_lease(
        &self,
        project_id: &str,
        known_fencing_token: Option<u64>,
    ) -> Result<Option<WriterLease>, ApiError> {
        let ttl_ms = self.options.writer_lease_ttl_ms;
        if ttl_ms == 0 {
            return Ok(None);
        }
        let now = now_ms();
        let key = writer_lease_key(project_id);
        let new_lease = |fencing_token| WriterLease {
            schema_version: 1,
            project_id: project_id.to_string(),
            owner_id: self.runtime_id.clone(),
            fencing_token,
            acquired_at_ms: now,
            expires_at_ms: now + ttl_ms as u128,
        };
        let first_lease = new_lease(now as u64);
        if self
            .object_store
            .put_bytes_if_absent(&key, serde_json::to_vec_pretty(&first_lease)?.as_slice())?
        {
            return Ok(Some(first_lease));
        }

        let Some(current) = self.load_writer_lease(project_id)? else {
            let retry_lease = new_lease(now as u64 + 1);
            if self
                .object_store
                .put_bytes_if_absent(&key, serde_json::to_vec_pretty(&retry_lease)?.as_slice())?
            {
                return Ok(Some(retry_lease));
            }
            return Err(ApiError::new(
                423,
                format!("project writer lease changed while acquiring {project_id}"),
            ));
        };
        if current.project_id != project_id {
            return Err(ApiError::new(
                503,
                format!(
                    "writer lease project mismatch: expected {project_id}, got {}",
                    current.project_id
                ),
            ));
        }
        if current.owner_id == self.runtime_id && current.expires_at_ms > now {
            let renewed = new_lease(current.fencing_token.saturating_add(1));
            self.object_store
                .put_bytes(&key, serde_json::to_vec_pretty(&renewed)?.as_slice())?;
            return Ok(Some(renewed));
        }
        if current.expires_at_ms <= now {
            if let Some(known_fencing_token) = known_fencing_token
                && current.owner_id != self.runtime_id
            {
                return Err(ApiError::new(
                    423,
                    format!(
                        "project writer lease was lost; rehydrate before reacquiring: project_id={project_id}, current_owner_id={}, current_fencing_token={}, known_fencing_token={known_fencing_token}",
                        current.owner_id, current.fencing_token
                    ),
                ));
            }
            return self
                .claim_expired_writer_lease(project_id, current, now, ttl_ms)
                .map(Some);
        }
        Err(ApiError::new(
            423,
            format!(
                "project writer lease is held by another runtime: project_id={}, owner_id={}, expires_at_ms={}",
                project_id, current.owner_id, current.expires_at_ms
            ),
        ))
    }

    fn ensure_writer_lease_for_state(&self, state: &mut ProjectState) -> Result<(), ApiError> {
        if let Some(lease) =
            self.ensure_writer_lease(&state.project_id, state.writer_lease_fencing_token)?
        {
            state.writer_lease_fencing_token = Some(lease.fencing_token);
        }
        Ok(())
    }

    fn release_writer_lease(&self, project_id: &str) -> Result<(), ApiError> {
        if self.options.writer_lease_ttl_ms == 0 {
            return Ok(());
        }
        let key = writer_lease_key(project_id);
        if let Some(current) = self.load_writer_lease(project_id)?
            && current.owner_id == self.runtime_id
        {
            self.object_store.delete(&key)?;
        }
        Ok(())
    }

    fn load_manifest(&self, project_id: &str) -> Result<Option<DurableManifest>, ApiError> {
        let key = manifest_key(project_id);
        if !self.object_store.exists(&key)? {
            return Ok(None);
        }
        let manifest: DurableManifest =
            serde_json::from_slice(&self.object_store.read_bytes(&key)?)
                .map_err(|err| ApiError::new(503, format!("invalid durable manifest: {err}")))?;
        if manifest.schema_version != 1 {
            return Err(ApiError::new(
                503,
                format!(
                    "unsupported durable manifest schema version: {}",
                    manifest.schema_version
                ),
            ));
        }
        if manifest.project_id != project_id {
            return Err(ApiError::new(
                503,
                format!(
                    "durable manifest project mismatch: expected {project_id}, got {}",
                    manifest.project_id
                ),
            ));
        }
        Ok(Some(manifest))
    }

    fn restore_snapshot(
        &self,
        db_path: &Path,
        snapshot: &SnapshotManifest,
    ) -> Result<(), ApiError> {
        let bytes =
            self.read_verified_object(&snapshot.key, snapshot.bytes, &snapshot.sha256, "snapshot")?;
        fs::write(db_path, bytes).map_err(anyhow::Error::from)?;
        Ok(())
    }

    fn restore_manifest_wal(
        &self,
        wal_path: &Path,
        manifest: &DurableManifest,
    ) -> Result<Vec<WalSegmentManifest>, ApiError> {
        let mut expected_offset = 0;
        let mut expected_id = None;
        let mut wal_file = None;
        for segment in &manifest.wal.segments {
            if let Some(id) = expected_id {
                if segment.id != id {
                    return Err(ApiError::new(
                        503,
                        format!(
                            "non-contiguous WAL segment id: expected {id}, got {}",
                            segment.id
                        ),
                    ));
                }
            }
            if segment.offset != expected_offset {
                return Err(ApiError::new(
                    503,
                    format!(
                        "non-contiguous WAL segment offset: expected {expected_offset}, got {}",
                        segment.offset
                    ),
                ));
            }
            let bytes = self.read_verified_object(
                &segment.key,
                segment.len,
                &segment.sha256,
                "WAL segment",
            )?;
            if wal_file.is_none() {
                wal_file = Some(fs::File::create(wal_path).map_err(anyhow::Error::from)?);
            }
            wal_file
                .as_mut()
                .unwrap()
                .write_all(&bytes)
                .map_err(anyhow::Error::from)?;
            expected_offset += segment.len;
            expected_id = Some(segment.id + 1);
        }
        if expected_offset != manifest.wal.durable_bytes {
            return Err(ApiError::new(
                503,
                format!(
                    "WAL manifest byte mismatch: segments sum to {expected_offset}, manifest says {}",
                    manifest.wal.durable_bytes
                ),
            ));
        }
        if let Some(file) = wal_file {
            file.sync_all().map_err(anyhow::Error::from)?;
        }
        Ok(manifest.wal.segments.clone())
    }

    fn restore_legacy_wal_segments(
        &self,
        project_id: &str,
        wal_path: &Path,
    ) -> Result<Vec<WalSegmentManifest>, ApiError> {
        let segments = self.object_store.list_prefix(&wal_prefix(project_id))?;
        let mut recovered = Vec::with_capacity(segments.len());
        let mut offset = 0;
        let mut wal_file = None;
        for object in segments {
            let id = wal_segment_id(&object.key).ok_or_else(|| {
                ApiError::new(
                    503,
                    format!("invalid legacy WAL segment key: {}", object.key),
                )
            })?;
            let bytes = self.object_store.read_bytes(&object.key)?;
            if bytes.len() as u64 != object.len {
                return Err(ApiError::new(
                    503,
                    format!(
                        "legacy WAL segment size mismatch for {}: listed {}, read {}",
                        object.key,
                        object.len,
                        bytes.len()
                    ),
                ));
            }
            if wal_file.is_none() {
                wal_file = Some(fs::File::create(wal_path).map_err(anyhow::Error::from)?);
            }
            wal_file
                .as_mut()
                .unwrap()
                .write_all(&bytes)
                .map_err(anyhow::Error::from)?;
            recovered.push(WalSegmentManifest {
                id,
                key: object.key,
                offset,
                len: bytes.len() as u64,
                sha256: hex_sha256(&bytes),
            });
            offset += bytes.len() as u64;
        }
        recovered.sort_by_key(|segment| segment.id);
        let mut expected_offset = 0;
        for segment in &mut recovered {
            segment.offset = expected_offset;
            expected_offset += segment.len;
        }
        if let Some(file) = wal_file {
            file.sync_all().map_err(anyhow::Error::from)?;
        }
        Ok(recovered)
    }

    fn read_verified_object(
        &self,
        key: &str,
        expected_len: u64,
        expected_sha256: &str,
        label: &str,
    ) -> Result<Vec<u8>, ApiError> {
        let bytes = self
            .object_store
            .read_bytes(key)
            .map_err(|err| ApiError::new(503, format!("failed to read {label} {key}: {err}")))?;
        if bytes.len() as u64 != expected_len {
            return Err(ApiError::new(
                503,
                format!(
                    "{label} length mismatch for {key}: expected {expected_len}, got {}",
                    bytes.len()
                ),
            ));
        }
        let actual_sha256 = hex_sha256(&bytes);
        if actual_sha256 != expected_sha256 {
            return Err(ApiError::new(
                503,
                format!(
                    "{label} checksum mismatch for {key}: expected {expected_sha256}, got {actual_sha256}"
                ),
            ));
        }
        Ok(bytes)
    }

    fn ensure_project(&self, project_id: &str) -> Result<Arc<ProjectHandle>, ApiError> {
        let pid = safe_project_id(project_id)?.to_string();
        if let Some(existing) = self.states.lock().unwrap().get(&pid).cloned() {
            return Ok(existing);
        }

        let project_cache = self.cache_dir.join(&pid);
        let db_path = project_cache.join("main.sqlite");
        fs::create_dir_all(&project_cache).map_err(anyhow::Error::from)?;
        let manifest = self.load_manifest(&pid)?;
        let legacy_snapshot = snapshot_key(&pid);
        let mut generation = manifest.as_ref().map(|item| item.generation).unwrap_or(0);
        let mut commit_seq = manifest_commit_seq(manifest.as_ref());
        let mut current_snapshot = manifest.as_ref().map(|item| item.snapshot.clone());
        let mut recovered_wal_segments = Vec::new();
        let snapshot_exists =
            current_snapshot.is_some() || self.object_store.exists(&legacy_snapshot)?;
        if self.options.read_replica && !snapshot_exists {
            return Err(ApiError::new(
                404,
                format!("project is not available on read replica: {pid}"),
            ));
        }
        if snapshot_exists {
            fs::remove_file(&db_path).ok();
            fs::remove_file(db_path.with_extension("sqlite-wal")).ok();
            fs::remove_file(db_path.with_extension("sqlite-shm")).ok();
            let wal_path = db_path.with_extension("sqlite-wal");
            if let Some(manifest) = &manifest {
                self.restore_snapshot(&db_path, &manifest.snapshot)?;
                recovered_wal_segments = self.restore_manifest_wal(&wal_path, manifest)?;
            } else {
                let snapshot_bytes = self.object_store.read_bytes(&legacy_snapshot)?;
                let snapshot = SnapshotManifest {
                    key: legacy_snapshot.clone(),
                    bytes: snapshot_bytes.len() as u64,
                    sha256: hex_sha256(&snapshot_bytes),
                };
                fs::write(&db_path, snapshot_bytes).map_err(anyhow::Error::from)?;
                recovered_wal_segments = self.restore_legacy_wal_segments(&pid, &wal_path)?;
                current_snapshot = Some(snapshot);
                generation = 0;
                commit_seq = 0;
            }
        }

        let conn = open_project_connection(&db_path, &self.options.sqlite_synchronous)?;

        let last_durable_wal_bytes = recovered_wal_segments
            .iter()
            .map(|item| item.len)
            .sum::<u64>();
        let next_wal_segment_id = recovered_wal_segments
            .iter()
            .map(|item| item.id)
            .max()
            .map(|id| id + 1)
            .unwrap_or(0);
        let ops_since_snapshot = manifest
            .as_ref()
            .map(|item| item.ops_since_snapshot)
            .unwrap_or(0);
        let state = Arc::new(Mutex::new(ProjectState {
            project_id: pid.clone(),
            conn,
            cache_dir: project_cache,
            db_path: db_path.clone(),
            generation,
            commit_seq,
            current_snapshot,
            wal_segments: recovered_wal_segments.clone(),
            last_durable_wal_bytes,
            next_wal_segment_id,
            ops_since_snapshot,
            last_snapshot_at_ms: now_ms(),
            writer_lease_fencing_token: None,
        }));

        {
            let mut locked = state.lock().unwrap();
            self.ensure_meta(&locked.conn)?;
            if !snapshot_exists {
                self.ensure_writer_lease_for_state(&mut locked)?;
                self.persist_snapshot(&mut locked, "init")?;
            } else if last_durable_wal_bytes > 0 && !self.options.read_replica {
                self.append_change_log(
                    &locked,
                    "rehydrate_from_snapshot_and_wal",
                    json!({
                        "durable_wal_bytes": last_durable_wal_bytes,
                        "wal_segment_count": recovered_wal_segments.len()
                    }),
                )?;
            }
        }

        let (writer, worker) = if self.options.read_replica {
            (None, None)
        } else {
            let (writer, receiver) = mpsc::sync_channel(self.options.writer_queue_capacity.max(1));
            let runtime = self.clone();
            let state_for_worker = state.clone();
            let worker = std::thread::Builder::new()
                .name(format!("sdb-writer-{pid}"))
                .spawn(move || runtime.writer_loop(state_for_worker, receiver))
                .map_err(|err| ApiError::new(500, format!("failed to start writer: {err}")))?;
            (Some(writer), Some(worker))
        };
        let (replica_stop, replica_worker) = self.spawn_replica_worker(&pid, state.clone())?;
        let handle = Arc::new(ProjectHandle {
            state: state.clone(),
            writer: Mutex::new(writer),
            worker: Mutex::new(worker),
            replica_stop: Mutex::new(replica_stop),
            replica_worker: Mutex::new(replica_worker),
        });
        self.states
            .lock()
            .unwrap()
            .insert(pid.clone(), handle.clone());
        Ok(handle)
    }

    pub fn hibernate(&self, project_id: &str) -> Result<Value, ApiError> {
        let pid = safe_project_id(project_id)?.to_string();
        if self.options.read_replica {
            self.evict_project_cache(&pid)?;
            return Ok(json!({ "project_id": pid, "cache_removed": true, "read_replica": true }));
        }
        let handle = self.states.lock().unwrap().remove(&pid);
        if let Some(handle) = handle {
            handle.stop_writer()?;
            {
                let mut state = handle.state.lock().unwrap();
                self.ensure_writer_lease_for_state(&mut state)?;
                self.persist_snapshot(&mut state, "hibernate")?;
            }
            drop(handle);
        }
        fs::remove_dir_all(self.cache_dir.join(&pid)).ok();
        self.release_writer_lease(&pid).ok();
        Ok(json!({ "project_id": pid, "cache_removed": true }))
    }

    pub fn crash_project(&self, project_id: &str) -> Result<Value, ApiError> {
        let pid = safe_project_id(project_id)?.to_string();
        if let Some(handle) = self.states.lock().unwrap().remove(&pid) {
            handle.stop_writer()?;
            drop(handle);
        }
        fs::remove_dir_all(self.cache_dir.join(&pid)).ok();
        maybe_crash_after_stage("after_project_cache_delete");
        Ok(json!({ "project_id": pid, "cache_removed": true, "snapshot_forced": false }))
    }

    pub fn create_table(&self, project_id: &str, spec: TableSpec) -> Result<Value, ApiError> {
        self.submit_write(project_id, WriteJobKind::CreateTable(spec))
    }

    pub fn schema(&self, project_id: &str) -> Result<Value, ApiError> {
        let handle = self.ensure_project_for_read(project_id, None)?;
        let state = handle.state.lock().unwrap();
        let mut stmt = state
            .conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE '_sdb_%' AND name NOT LIKE 'sqlite_%' ORDER BY name")?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        let mut tables = Vec::new();
        for table in rows {
            tables.push(json!({ "name": table, "columns": table_columns(&state.conn, &table)? }));
        }
        Ok(with_bookmark(
            json!({ "project_id": state.project_id, "tables": tables }),
            &bookmark_for_seq(state.commit_seq),
        ))
    }

    pub fn set_policy(&self, project_id: &str, spec: PolicySpec) -> Result<Value, ApiError> {
        self.submit_write(project_id, WriteJobKind::SetPolicy(spec))
    }

    pub fn list_policies(&self, project_id: &str) -> Result<Value, ApiError> {
        let handle = self.ensure_project_for_read(project_id, None)?;
        let state = handle.state.lock().unwrap();
        let mut stmt = state
            .conn
            .prepare("SELECT table_name, operation, name, rule_json, updated_at FROM _sdb_policies ORDER BY table_name, operation, name")?;
        let policies = stmt
            .query_map([], |row| {
                Ok(json!({
                    "table": row.get::<_, String>(0)?,
                    "operation": row.get::<_, String>(1)?,
                    "name": row.get::<_, String>(2)?,
                    "rule": serde_json::from_str::<Value>(&row.get::<_, String>(3)?).unwrap_or(Value::Null),
                    "updated_at": row.get::<_, String>(4)?
                }))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(with_bookmark(
            json!({ "policies": policies }),
            &bookmark_for_seq(state.commit_seq),
        ))
    }

    pub fn select_rows(
        &self,
        project_id: &str,
        table: &str,
        filters: &HashMap<String, String>,
        actor: &Actor,
        limit: u64,
    ) -> Result<Value, ApiError> {
        self.select_rows_at_bookmark(project_id, table, filters, actor, limit, None)
    }

    pub fn select_rows_at_bookmark(
        &self,
        project_id: &str,
        table: &str,
        filters: &HashMap<String, String>,
        actor: &Actor,
        limit: u64,
        min_bookmark: Option<&str>,
    ) -> Result<Value, ApiError> {
        let handle = self.ensure_project_for_read(project_id, min_bookmark)?;
        let state = handle.state.lock().unwrap();
        let table = assert_user_ident(table)?;
        require_table(&state.conn, table)?;
        let (where_sql, mut params) =
            self.where_for_operation(&state.conn, table, "select", filters, actor)?;
        params.push(SqlValue::Integer(limit.clamp(1, 1000) as i64));
        let sql = format!(
            "SELECT * FROM {} WHERE {where_sql} LIMIT ?",
            quote_ident(table)?
        );
        let mut stmt = state.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params_from_iter(params), row_to_json_map)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(with_bookmark(
            json!({ "rows": rows }),
            &bookmark_for_seq(state.commit_seq),
        ))
    }

    pub fn select_rows_postgrest(
        &self,
        project_id: &str,
        table: &str,
        query: &SelectQuery,
        actor: &Actor,
        min_bookmark: Option<&str>,
    ) -> Result<Value, ApiError> {
        let handle = self.ensure_project_for_read(project_id, min_bookmark)?;
        let state = handle.state.lock().unwrap();
        let table = assert_user_ident(table)?;
        require_table(&state.conn, table)?;
        let rules = policy_rules(&state.conn, table, "select")?;
        let (sql, params) = postgrest::build_select_sql(table, query, &rules, actor)
            .map_err(|err| ApiError::new(400, err.to_string()))?;
        let mut stmt = state.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params_from_iter(params), row_to_json_map)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(with_bookmark(
            json!({ "rows": rows }),
            &bookmark_for_seq(state.commit_seq),
        ))
    }

    pub fn insert_row(
        &self,
        project_id: &str,
        table: &str,
        data: Value,
        actor: &Actor,
    ) -> Result<Value, ApiError> {
        self.insert_row_with_idempotency(project_id, table, data, actor, None)
    }

    pub fn insert_row_with_idempotency(
        &self,
        project_id: &str,
        table: &str,
        data: Value,
        actor: &Actor,
        idempotency: Option<WriteIdempotency>,
    ) -> Result<Value, ApiError> {
        self.submit_write_with_idempotency(
            project_id,
            WriteJobKind::Insert {
                table: table.to_string(),
                data,
                actor: actor.clone(),
            },
            idempotency,
        )
    }

    pub fn update_rows(
        &self,
        project_id: &str,
        table: &str,
        filters: &HashMap<String, String>,
        data: Value,
        actor: &Actor,
    ) -> Result<Value, ApiError> {
        self.update_rows_with_idempotency(project_id, table, filters, data, actor, None)
    }

    pub fn update_rows_with_idempotency(
        &self,
        project_id: &str,
        table: &str,
        filters: &HashMap<String, String>,
        data: Value,
        actor: &Actor,
        idempotency: Option<WriteIdempotency>,
    ) -> Result<Value, ApiError> {
        self.submit_write_with_idempotency(
            project_id,
            WriteJobKind::Update {
                table: table.to_string(),
                filters: filters.clone(),
                data,
                actor: actor.clone(),
            },
            idempotency,
        )
    }

    pub fn update_rows_postgrest(
        &self,
        project_id: &str,
        table: &str,
        filters: &crate::postgrest::FilterExpr,
        data: Value,
        actor: &Actor,
        idempotency: Option<WriteIdempotency>,
    ) -> Result<Value, ApiError> {
        self.submit_write_with_idempotency(
            project_id,
            WriteJobKind::UpdatePostgrest {
                table: table.to_string(),
                filters: filters.clone(),
                data,
                actor: actor.clone(),
            },
            idempotency,
        )
    }

    pub fn delete_rows_postgrest(
        &self,
        project_id: &str,
        table: &str,
        filters: &crate::postgrest::FilterExpr,
        actor: &Actor,
        idempotency: Option<WriteIdempotency>,
    ) -> Result<Value, ApiError> {
        self.submit_write_with_idempotency(
            project_id,
            WriteJobKind::DeletePostgrest {
                table: table.to_string(),
                filters: filters.clone(),
                actor: actor.clone(),
            },
            idempotency,
        )
    }

    pub fn delete_rows(
        &self,
        project_id: &str,
        table: &str,
        filters: &HashMap<String, String>,
        actor: &Actor,
    ) -> Result<Value, ApiError> {
        self.delete_rows_with_idempotency(project_id, table, filters, actor, None)
    }

    pub fn delete_rows_with_idempotency(
        &self,
        project_id: &str,
        table: &str,
        filters: &HashMap<String, String>,
        actor: &Actor,
        idempotency: Option<WriteIdempotency>,
    ) -> Result<Value, ApiError> {
        self.submit_write_with_idempotency(
            project_id,
            WriteJobKind::Delete {
                table: table.to_string(),
                filters: filters.clone(),
                actor: actor.clone(),
            },
            idempotency,
        )
    }

    pub fn create_bucket(&self, project_id: &str, name: &str) -> Result<Value, ApiError> {
        self.create_bucket_with_idempotency(project_id, name, None)
    }

    pub fn create_bucket_with_idempotency(
        &self,
        project_id: &str,
        name: &str,
        idempotency: Option<WriteIdempotency>,
    ) -> Result<Value, ApiError> {
        self.submit_write_with_idempotency(
            project_id,
            WriteJobKind::CreateBucket {
                name: name.to_string(),
            },
            idempotency,
        )
    }

    pub fn put_object(
        &self,
        project_id: &str,
        bucket: &str,
        key: &str,
        data: &[u8],
        content_type: &str,
        actor: &Actor,
    ) -> Result<Value, ApiError> {
        self.put_object_with_idempotency(project_id, bucket, key, data, content_type, actor, None)
    }

    pub fn put_object_with_idempotency(
        &self,
        project_id: &str,
        bucket: &str,
        key: &str,
        data: &[u8],
        content_type: &str,
        actor: &Actor,
        idempotency: Option<WriteIdempotency>,
    ) -> Result<Value, ApiError> {
        self.submit_write_with_idempotency(
            project_id,
            WriteJobKind::PutObject {
                bucket: bucket.to_string(),
                key: key.to_string(),
                data: data.to_vec(),
                content_type: content_type.to_string(),
                actor: actor.clone(),
            },
            idempotency,
        )
    }

    pub fn get_object(
        &self,
        project_id: &str,
        bucket: &str,
        key: &str,
        actor: &Actor,
    ) -> Result<ObjectRead, ApiError> {
        let handle = self.ensure_project_for_read(project_id, None)?;
        let state = handle.state.lock().unwrap();
        let bucket = assert_user_ident(bucket)?;
        let key = safe_object_key(key)?;
        let meta = state
            .conn
            .query_row(
                "SELECT bucket, object_key AS key, size, content_type, etag, owner_id, created_at, updated_at FROM _sdb_objects WHERE bucket=? AND object_key=?",
                (bucket, key),
                row_to_json_map,
            )
            .optional()?
            .ok_or_else(|| ApiError::new(404, "object not found"))?;
        if !actor.is_admin() {
            let owner_id = meta.get("owner_id").and_then(Value::as_str).unwrap_or("");
            if !owner_id.is_empty() {
                if let Some(sub) = &actor.sub {
                    if sub != owner_id {
                        return Err(ApiError::new(403, "access denied: object not owned by actor"));
                    }
                } else {
                    return Err(ApiError::new(403, "access denied: anonymous access to owned object"));
                }
            }
        }
        let data = self
            .object_store
            .read_bytes(&storage_key(&state.project_id, bucket, key))?;
        Ok(ObjectRead {
            meta: Value::Object(meta),
            data,
        })
    }

    pub fn delete_object(
        &self,
        project_id: &str,
        bucket: &str,
        key: &str,
        actor: &Actor,
    ) -> Result<Value, ApiError> {
        self.delete_object_with_idempotency(project_id, bucket, key, actor, None)
    }

    pub fn delete_object_with_idempotency(
        &self,
        project_id: &str,
        bucket: &str,
        key: &str,
        actor: &Actor,
        idempotency: Option<WriteIdempotency>,
    ) -> Result<Value, ApiError> {
        self.submit_write_with_idempotency(
            project_id,
            WriteJobKind::DeleteObject {
                bucket: bucket.to_string(),
                key: key.to_string(),
                actor: actor.clone(),
            },
            idempotency,
        )
    }

    pub fn events(&self, project_id: &str, since: i64, limit: i64) -> Result<Value, ApiError> {
        let handle = self.ensure_project_for_read(project_id, None)?;
        let state = handle.state.lock().unwrap();
        let mut stmt = state.conn.prepare(
            "
            SELECT id, created_at, table_name, operation, row_json, actor_sub, actor_role
            FROM _sdb_outbox
            WHERE id > ?
            ORDER BY id
            LIMIT ?
            ",
        )?;
        let events = stmt
            .query_map((since, limit.clamp(1, 1000)), |row| {
                let row_json: String = row.get(4)?;
                Ok(json!({
                    "id": row.get::<_, i64>(0)?,
                    "created_at": row.get::<_, String>(1)?,
                    "table": row.get::<_, String>(2)?,
                    "operation": row.get::<_, String>(3)?,
                    "row": serde_json::from_str::<Value>(&row_json).unwrap_or(Value::Null),
                    "actor_sub": row.get::<_, Option<String>>(5)?,
                    "actor_role": row.get::<_, Option<String>>(6)?
                }))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(with_bookmark(
            json!({ "events": events }),
            &bookmark_for_seq(state.commit_seq),
        ))
    }

    pub async fn wait_for_events(&self, project_id: &str, since: i64) -> Result<Value, ApiError> {
        let immediate = self.events(project_id, since, 100)?;
        if immediate
            .get("events")
            .and_then(Value::as_array)
            .map(|items| !items.is_empty())
            .unwrap_or(false)
        {
            return Ok(immediate);
        }
        let notify = self.notifier(project_id)?;
        tokio::select! {
            _ = notify.notified() => self.events(project_id, since, 100),
            _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => self.events(project_id, since, 100),
        }
    }

    fn submit_write(&self, project_id: &str, kind: WriteJobKind) -> Result<Value, ApiError> {
        self.submit_write_with_idempotency(project_id, kind, None)
    }

    fn submit_write_with_idempotency(
        &self,
        project_id: &str,
        kind: WriteJobKind,
        idempotency: Option<WriteIdempotency>,
    ) -> Result<Value, ApiError> {
        self.ensure_primary_write_allowed("write")?;
        let handle = self.ensure_project(project_id)?;
        let (response, receiver) = mpsc::sync_channel(1);
        match handle.writer()?.try_send(WriteJob {
            kind,
            idempotency,
            response,
        }) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(_)) => {
                return Err(ApiError::new(429, "project writer queue is full"));
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                return Err(ApiError::new(503, "project writer is unavailable"));
            }
        }
        receiver
            .recv()
            .map_err(|_| ApiError::new(503, "project writer stopped"))?
    }

    fn ensure_primary_write_allowed(&self, operation: &str) -> Result<(), ApiError> {
        if self.options.read_replica {
            return Err(ApiError::new(
                405,
                format!(
                    "read replica cannot accept {operation}; forward the request to the primary"
                ),
            ));
        }
        Ok(())
    }

    fn writer_loop(&self, state: Arc<Mutex<ProjectState>>, receiver: mpsc::Receiver<WriteJob>) {
        while let Ok(first) = receiver.recv() {
            let mut batch = vec![first];
            let max_ops = self.options.group_commit_max_ops.max(1);
            let delay = Duration::from_millis(self.options.group_commit_delay_ms);
            let started = Instant::now();

            while batch.len() < max_ops {
                let Some(remaining) = delay.checked_sub(started.elapsed()) else {
                    break;
                };
                if remaining.is_zero() {
                    break;
                }
                match receiver.recv_timeout(remaining) {
                    Ok(job) => batch.push(job),
                    Err(mpsc::RecvTimeoutError::Timeout) => break,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }

            while batch.len() < max_ops {
                match receiver.try_recv() {
                    Ok(job) => batch.push(job),
                    Err(_) => break,
                }
            }

            self.execute_write_batch(&state, batch);
        }
    }

    fn execute_write_batch(&self, state: &Arc<Mutex<ProjectState>>, batch: Vec<WriteJob>) {
        struct ResponseSlot {
            response: mpsc::SyncSender<Result<Value, ApiError>>,
            result: Result<Value, ApiError>,
            committed: bool,
        }

        let mut committed_any = false;
        let mut committed_bookmark = None;
        let mut notify_project = None;
        let mut slots = Vec::with_capacity(batch.len());
        let flush_result = {
            let mut locked = state.lock().unwrap();
            if let Err(err) = self.ensure_writer_lease_for_state(&mut locked) {
                for job in batch {
                    slots.push(ResponseSlot {
                        response: job.response,
                        result: Err(err.clone()),
                        committed: false,
                    });
                }
                Ok(())
            } else if let Err(err) = self.ensure_wal_budget(&mut locked) {
                for job in batch {
                    slots.push(ResponseSlot {
                        response: job.response,
                        result: Err(err.clone()),
                        committed: false,
                    });
                }
                Ok(())
            } else {
                for job in batch {
                    let pending_bookmark = bookmark_for_seq(locked.commit_seq.saturating_add(1));
                    let (response, result, committed) =
                        self.execute_write_job(&mut locked, job, &pending_bookmark);
                    committed_any |= committed;
                    slots.push(ResponseSlot {
                        response,
                        result,
                        committed,
                    });
                }
                if committed_any {
                    maybe_crash_after_stage("after_sqlite_commit_before_durable_wal");
                    let committed_ops = slots.iter().filter(|slot| slot.committed).count() as u64;
                    let reason = format!("group_commit:{committed_ops}_ops");
                    let result = self
                        .ensure_writer_lease_for_state(&mut locked)
                        .and_then(|_| self.durabilize_wal(&mut locked, &reason, committed_ops));
                    if result.is_ok() {
                        committed_bookmark = Some(bookmark_for_seq(locked.commit_seq));
                    }
                    notify_project = Some(locked.project_id.clone());
                    result
                } else {
                    Ok(())
                }
            }
        };

        if let Some(project_id) = notify_project {
            if flush_result.is_ok() {
                self.notify(&project_id);
            }
        }

        for slot in slots {
            let response = match (&slot.result, &flush_result, slot.committed) {
                (Ok(_), Err(flush_error), true) => Err(flush_error.clone()),
                _ => slot
                    .result
                    .map(|value| match (&committed_bookmark, slot.committed) {
                        (Some(bookmark), true) => with_bookmark(value, bookmark),
                        _ => value,
                    }),
            };
            let _ = slot.response.send(response);
        }
    }

    fn execute_write_job(
        &self,
        state: &mut ProjectState,
        job: WriteJob,
        pending_bookmark: &str,
    ) -> (
        mpsc::SyncSender<Result<Value, ApiError>>,
        Result<Value, ApiError>,
        bool,
    ) {
        let response = job.response;
        let (result, committed) = match job.idempotency {
            Some(idempotency) => {
                self.execute_idempotent_write_kind(state, job.kind, &idempotency, pending_bookmark)
            }
            None => self.execute_write_kind(state, job.kind),
        };
        (response, result, committed)
    }

    fn execute_idempotent_write_kind(
        &self,
        state: &mut ProjectState,
        kind: WriteJobKind,
        idempotency: &WriteIdempotency,
        pending_bookmark: &str,
    ) -> (Result<Value, ApiError>, bool) {
        if let Err(err) = validate_idempotency(idempotency) {
            return (Err(err), false);
        }
        match self.load_idempotency_record(state, idempotency) {
            Ok(Some(value)) => return (Ok(value), false),
            Ok(None) => {}
            Err(err) => return (Err(err), false),
        }

        let (result, _committed) = self.execute_write_kind(state, kind);
        match result {
            Ok(value) => {
                let response = with_bookmark(value, pending_bookmark);
                match self.store_idempotency_record(state, idempotency, pending_bookmark, &response)
                {
                    Ok(()) => (Ok(response), true),
                    Err(err) => (Err(err), true),
                }
            }
            Err(err) => (Err(err), false),
        }
    }

    fn load_idempotency_record(
        &self,
        state: &ProjectState,
        idempotency: &WriteIdempotency,
    ) -> Result<Option<Value>, ApiError> {
        let existing = state
            .conn
            .query_row(
                "SELECT request_hash, response_json, bookmark FROM _sdb_idempotency WHERE idempotency_key=?",
                [&idempotency.key],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((request_hash, response_json, bookmark)) = existing else {
            return Ok(None);
        };
        if request_hash != idempotency.request_hash {
            return Err(ApiError::new(
                409,
                "idempotency key was already used with a different request",
            ));
        }
        let bookmark_seq = bookmark_to_seq(&bookmark)?;
        if state.commit_seq < bookmark_seq {
            return Err(ApiError::new(
                425,
                format!(
                    "idempotent write result is pending durable commit: requested={}, current={}",
                    bookmark,
                    bookmark_for_seq(state.commit_seq)
                ),
            ));
        }
        Ok(Some(serde_json::from_str(&response_json)?))
    }

    fn store_idempotency_record(
        &self,
        state: &ProjectState,
        idempotency: &WriteIdempotency,
        bookmark: &str,
        response: &Value,
    ) -> Result<(), ApiError> {
        state.conn.execute(
            "
            INSERT INTO _sdb_idempotency(idempotency_key, request_hash, response_json, bookmark)
            VALUES(?, ?, ?, ?)
            ",
            (
                &idempotency.key,
                &idempotency.request_hash,
                &response.to_string(),
                bookmark,
            ),
        )?;
        Ok(())
    }

    fn execute_write_kind(
        &self,
        state: &mut ProjectState,
        kind: WriteJobKind,
    ) -> (Result<Value, ApiError>, bool) {
        let result = match kind {
            WriteJobKind::CreateTable(spec) => self
                .execute_create_table(state, spec)
                .map(|value| (value, true)),
            WriteJobKind::SetPolicy(spec) => self
                .execute_set_policy(state, spec)
                .map(|value| (value, true)),
            WriteJobKind::Insert { table, data, actor } => self
                .execute_insert(state, &table, data, &actor)
                .map(|value| (value, true)),
            WriteJobKind::Update {
                table,
                filters,
                data,
                actor,
            } => self.execute_update(state, &table, &filters, data, &actor),
            WriteJobKind::Delete {
                table,
                filters,
                actor,
            } => self.execute_delete(state, &table, &filters, &actor),
            WriteJobKind::UpdatePostgrest {
                table,
                filters,
                data,
                actor,
            } => self.execute_update_postgrest(state, &table, &filters, data, &actor),
            WriteJobKind::DeletePostgrest {
                table,
                filters,
                actor,
            } => self.execute_delete_postgrest(state, &table, &filters, &actor),
            WriteJobKind::CreateBucket { name } => self
                .execute_create_bucket(state, &name)
                .map(|value| (value, true)),
            WriteJobKind::PutObject {
                bucket,
                key,
                data,
                content_type,
                actor,
            } => self
                .execute_put_object(state, &bucket, &key, &data, &content_type, &actor)
                .map(|value| (value, true)),
            WriteJobKind::DeleteObject { bucket, key, actor } => {
                self.execute_delete_object(state, &bucket, &key, &actor)
            }
        };
        match result {
            Ok((value, committed)) => (Ok(value), committed),
            Err(err) => (Err(err), false),
        }
    }

    fn execute_create_table(
        &self,
        state: &mut ProjectState,
        spec: TableSpec,
    ) -> Result<Value, ApiError> {
        let table = assert_user_ident(&spec.name)?;
        let mut names = std::collections::HashSet::new();
        let mut column_defs = Vec::new();
        let mut has_pk = false;
        for column in &spec.columns {
            let name = assert_user_ident(&column.name)?;
            if !names.insert(name.to_string()) {
                return Err(ApiError::new(400, format!("duplicate column: {name}")));
            }
            let sql_type = sql_type(&column.r#type)?;
            let mut parts = vec![quote_ident(name)?, sql_type.to_string()];
            if column.primary_key {
                parts.push("PRIMARY KEY".to_string());
                if sql_type == "INTEGER" && column.auto_increment {
                    parts.push("AUTOINCREMENT".to_string());
                }
                has_pk = true;
            }
            if column.not_null {
                parts.push("NOT NULL".to_string());
            }
            column_defs.push(parts.join(" "));
        }
        if !has_pk && !names.contains("id") {
            column_defs.insert(0, "\"id\" INTEGER PRIMARY KEY AUTOINCREMENT".to_string());
        }
        state.conn.execute(
            &format!(
                "CREATE TABLE IF NOT EXISTS {} ({})",
                quote_ident(table)?,
                column_defs.join(", ")
            ),
            [],
        )?;
        Ok(json!({ "table": table, "columns": table_columns(&state.conn, table)? }))
    }

    fn execute_set_policy(
        &self,
        state: &mut ProjectState,
        spec: PolicySpec,
    ) -> Result<Value, ApiError> {
        let table = assert_user_ident(&spec.table)?;
        if !["select", "insert", "update", "delete", "all"].contains(&spec.operation.as_str()) {
            return Err(ApiError::new(
                400,
                "operation must be one of select, insert, update, delete, all",
            ));
        }
        require_table(&state.conn, table)?;
        let name = spec
            .name
            .unwrap_or_else(|| format!("{}_policy", spec.operation));
        state.conn.execute(
            "
            INSERT INTO _sdb_policies(table_name, operation, name, rule_json)
            VALUES(?, ?, ?, ?)
            ON CONFLICT(table_name, operation, name)
            DO UPDATE SET rule_json=excluded.rule_json, updated_at=datetime('now')
            ",
            (&table, &spec.operation, &name, spec.rule.to_string()),
        )?;
        Ok(json!({ "table": table, "operation": spec.operation, "name": name, "rule": spec.rule }))
    }

    fn execute_insert(
        &self,
        state: &mut ProjectState,
        table: &str,
        data: Value,
        actor: &Actor,
    ) -> Result<Value, ApiError> {
        let table = assert_user_ident(table)?;
        require_table(&state.conn, table)?;
        let row = normalize_row_payload(data)?;
        let rules = policy_rules(&state.conn, table, "insert")?;
        if !evaluate_policies(&rules, &row, actor)
            .map_err(|err| ApiError::new(400, err.to_string()))?
        {
            return Err(ApiError::new(403, "insert rejected by policy"));
        }
        let columns = row
            .keys()
            .map(|key| assert_user_ident(key).map(str::to_string))
            .collect::<Result<Vec<_>, _>>()?;
        let values = columns
            .iter()
            .map(|column| json_to_sql_value(row.get(column).unwrap_or(&Value::Null)))
            .collect::<Vec<_>>();
        let tx = state.conn.transaction()?;
        tx.execute(
            &format!(
                "INSERT INTO {} ({}) VALUES ({})",
                quote_ident(table)?,
                columns
                    .iter()
                    .map(|column| quote_ident(column))
                    .collect::<Result<Vec<_>, _>>()?
                    .join(","),
                columns.iter().map(|_| "?").collect::<Vec<_>>().join(",")
            ),
            params_from_iter(values),
        )?;
        let rowid = tx.last_insert_rowid();
        let inserted = tx.query_row(
            &format!("SELECT * FROM {} WHERE rowid=?", quote_ident(table)?),
            [rowid],
            row_to_json_map,
        )?;
        record_event(&tx, table, "insert", &inserted, actor)?;
        tx.commit()?;
        Ok(json!({ "row": inserted }))
    }

    fn execute_update(
        &self,
        state: &mut ProjectState,
        table: &str,
        filters: &HashMap<String, String>,
        data: Value,
        actor: &Actor,
    ) -> Result<(Value, bool), ApiError> {
        let table = assert_user_ident(table)?;
        require_table(&state.conn, table)?;
        let patch = normalize_row_payload(data)?;
        let updates = patch
            .keys()
            .map(|key| assert_user_ident(key).map(str::to_string))
            .collect::<Result<Vec<_>, _>>()?;
        let (where_sql, params) =
            self.where_for_operation(&state.conn, table, "update", filters, actor)?;
        let rules = policy_rules(&state.conn, table, "update")?;
        let tx = state.conn.transaction()?;
        let before = {
            let mut stmt = tx.prepare(&format!(
                "SELECT rowid AS _rowid, * FROM {} WHERE {where_sql}",
                quote_ident(table)?
            ))?;
            stmt.query_map(params_from_iter(params), row_to_json_map)?
                .collect::<Result<Vec<_>, _>>()?
        };
        let mut rowids = Vec::new();
        for item in &before {
            let mut merged = item.clone();
            merged.remove("_rowid");
            for (key, value) in &patch {
                merged.insert(key.clone(), value.clone());
            }
            if evaluate_policies(&rules, &merged, actor)
                .map_err(|err| ApiError::new(400, err.to_string()))?
            {
                rowids.push(
                    item.get("_rowid")
                        .and_then(Value::as_i64)
                        .unwrap_or_default(),
                );
            }
        }
        if !before.is_empty() && rowids.is_empty() {
            return Err(ApiError::new(403, "update rejected by policy"));
        }
        let updated = if rowids.is_empty() {
            Vec::new()
        } else {
            let rowid_sql = rowids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let mut update_params = updates
                .iter()
                .map(|column| json_to_sql_value(patch.get(column).unwrap_or(&Value::Null)))
                .collect::<Vec<_>>();
            update_params.extend(rowids.iter().map(|id| SqlValue::Integer(*id)));
            tx.execute(
                &format!(
                    "UPDATE {} SET {} WHERE rowid IN ({rowid_sql})",
                    quote_ident(table)?,
                    updates
                        .iter()
                        .map(|column| Ok(format!("{}=?", quote_ident(column)?)))
                        .collect::<Result<Vec<_>, ApiError>>()?
                        .join(",")
                ),
                params_from_iter(update_params),
            )?;
            let mut stmt = tx.prepare(&format!(
                "SELECT * FROM {} WHERE rowid IN ({rowid_sql})",
                quote_ident(table)?
            ))?;
            let updated = stmt
                .query_map(
                    params_from_iter(rowids.iter().map(|id| SqlValue::Integer(*id))),
                    row_to_json_map,
                )?
                .collect::<Result<Vec<_>, _>>()?;
            for row in &updated {
                record_event(&tx, table, "update", row, actor)?;
            }
            updated
        };
        tx.commit()?;
        Ok((
            json!({ "affected": updated.len(), "rows": updated }),
            !updated.is_empty(),
        ))
    }

    fn execute_delete(
        &self,
        state: &mut ProjectState,
        table: &str,
        filters: &HashMap<String, String>,
        actor: &Actor,
    ) -> Result<(Value, bool), ApiError> {
        let table = assert_user_ident(table)?;
        require_table(&state.conn, table)?;
        let (where_sql, params) =
            self.where_for_operation(&state.conn, table, "delete", filters, actor)?;
        let tx = state.conn.transaction()?;
        let rows = {
            let mut stmt = tx.prepare(&format!(
                "SELECT rowid AS _rowid, * FROM {} WHERE {where_sql}",
                quote_ident(table)?
            ))?;
            stmt.query_map(params_from_iter(params), row_to_json_map)?
                .collect::<Result<Vec<_>, _>>()?
        };
        let rowids = rows
            .iter()
            .map(|row| {
                row.get("_rowid")
                    .and_then(Value::as_i64)
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>();
        if !rowids.is_empty() {
            let rowid_sql = rowids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            tx.execute(
                &format!(
                    "DELETE FROM {} WHERE rowid IN ({rowid_sql})",
                    quote_ident(table)?
                ),
                params_from_iter(rowids.iter().map(|id| SqlValue::Integer(*id))),
            )?;
            for row in &rows {
                let mut payload = row.clone();
                payload.remove("_rowid");
                record_event(&tx, table, "delete", &payload, actor)?;
            }
        }
        let affected = rows.len();
        let deleted = rows
            .into_iter()
            .map(|mut row| {
                row.remove("_rowid");
                Value::Object(row)
            })
            .collect::<Vec<_>>();
        tx.commit()?;
        Ok((
            json!({ "affected": affected, "rows": deleted }),
            affected > 0,
        ))
    }

    fn execute_update_postgrest(
        &self,
        state: &mut ProjectState,
        table: &str,
        filters: &crate::postgrest::FilterExpr,
        data: Value,
        actor: &Actor,
    ) -> Result<(Value, bool), ApiError> {
        let table = assert_user_ident(table)?;
        require_table(&state.conn, table)?;
        let patch = normalize_row_payload(data)?;
        let updates = patch
            .keys()
            .map(|key| assert_user_ident(key).map(str::to_string))
            .collect::<Result<Vec<_>, _>>()?;
        let (where_sql, params) =
            self.where_for_operation_postgrest(&state.conn, table, "update", filters, actor)?;
        let rules = policy_rules(&state.conn, table, "update")?;
        let tx = state.conn.transaction()?;
        let before = {
            let mut stmt = tx.prepare(&format!(
                "SELECT rowid AS _rowid, * FROM {} WHERE {where_sql}",
                quote_ident(table)?
            ))?;
            stmt.query_map(params_from_iter(params), row_to_json_map)?
                .collect::<Result<Vec<_>, _>>()?
        };
        let mut rowids = Vec::new();
        for item in &before {
            let mut merged = item.clone();
            merged.remove("_rowid");
            for (key, value) in &patch {
                merged.insert(key.clone(), value.clone());
            }
            if evaluate_policies(&rules, &merged, actor)
                .map_err(|err| ApiError::new(400, err.to_string()))?
            {
                rowids.push(
                    item.get("_rowid")
                        .and_then(Value::as_i64)
                        .unwrap_or_default(),
                );
            }
        }
        if !before.is_empty() && rowids.is_empty() {
            return Err(ApiError::new(403, "update rejected by policy"));
        }
        let updated = if rowids.is_empty() {
            Vec::new()
        } else {
            let rowid_sql = rowids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let mut update_params = updates
                .iter()
                .map(|column| json_to_sql_value(patch.get(column).unwrap_or(&Value::Null)))
                .collect::<Vec<_>>();
            update_params.extend(rowids.iter().map(|id| SqlValue::Integer(*id)));
            tx.execute(
                &format!(
                    "UPDATE {} SET {} WHERE rowid IN ({rowid_sql})",
                    quote_ident(table)?,
                    updates
                        .iter()
                        .map(|column| Ok(format!("{}=?", quote_ident(column)?)))
                        .collect::<Result<Vec<_>, ApiError>>()?
                        .join(",")
                ),
                params_from_iter(update_params),
            )?;
            let mut stmt = tx.prepare(&format!(
                "SELECT * FROM {} WHERE rowid IN ({rowid_sql})",
                quote_ident(table)?
            ))?;
            let updated = stmt
                .query_map(
                    params_from_iter(rowids.iter().map(|id| SqlValue::Integer(*id))),
                    row_to_json_map,
                )?
                .collect::<Result<Vec<_>, _>>()?;
            for row in &updated {
                record_event(&tx, table, "update", row, actor)?;
            }
            updated
        };
        tx.commit()?;
        Ok((
            json!({ "affected": updated.len(), "rows": updated }),
            !updated.is_empty(),
        ))
    }

    fn execute_delete_postgrest(
        &self,
        state: &mut ProjectState,
        table: &str,
        filters: &crate::postgrest::FilterExpr,
        actor: &Actor,
    ) -> Result<(Value, bool), ApiError> {
        let table = assert_user_ident(table)?;
        require_table(&state.conn, table)?;
        let (where_sql, params) =
            self.where_for_operation_postgrest(&state.conn, table, "delete", filters, actor)?;
        let tx = state.conn.transaction()?;
        let rows = {
            let mut stmt = tx.prepare(&format!(
                "SELECT rowid AS _rowid, * FROM {} WHERE {where_sql}",
                quote_ident(table)?
            ))?;
            stmt.query_map(params_from_iter(params), row_to_json_map)?
                .collect::<Result<Vec<_>, _>>()?
        };
        let rowids = rows
            .iter()
            .map(|row| {
                row.get("_rowid")
                    .and_then(Value::as_i64)
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>();
        if !rowids.is_empty() {
            let rowid_sql = rowids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            tx.execute(
                &format!(
                    "DELETE FROM {} WHERE rowid IN ({rowid_sql})",
                    quote_ident(table)?
                ),
                params_from_iter(rowids.iter().map(|id| SqlValue::Integer(*id))),
            )?;
            for row in &rows {
                let mut payload = row.clone();
                payload.remove("_rowid");
                record_event(&tx, table, "delete", &payload, actor)?;
            }
        }
        let affected = rows.len();
        let deleted = rows
            .into_iter()
            .map(|mut row| {
                row.remove("_rowid");
                Value::Object(row)
            })
            .collect::<Vec<_>>();
        tx.commit()?;
        Ok((
            json!({ "affected": affected, "rows": deleted }),
            affected > 0,
        ))
    }

    fn execute_create_bucket(
        &self,
        state: &mut ProjectState,
        name: &str,
    ) -> Result<Value, ApiError> {
        let bucket = assert_user_ident(name)?;
        state.conn.execute(
            "INSERT INTO _sdb_buckets(name) VALUES(?) ON CONFLICT(name) DO NOTHING",
            [bucket],
        )?;
        Ok(json!({ "bucket": bucket }))
    }

    fn execute_put_object(
        &self,
        state: &mut ProjectState,
        bucket: &str,
        key: &str,
        data: &[u8],
        content_type: &str,
        actor: &Actor,
    ) -> Result<Value, ApiError> {
        let bucket = assert_user_ident(bucket)?;
        let key = safe_object_key(key)?;
        require_bucket(&state.conn, bucket)?;
        let etag = hex_sha256(data);
        let now = utc_now();
        self.object_store
            .put_bytes(&storage_key(&state.project_id, bucket, key), data)?;
        let tx = state.conn.transaction()?;
        tx.execute(
            "
            INSERT INTO _sdb_objects(bucket, object_key, size, content_type, etag, owner_id, created_at, updated_at)
            VALUES(?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(bucket, object_key)
            DO UPDATE SET size=excluded.size, content_type=excluded.content_type, etag=excluded.etag,
                          owner_id=excluded.owner_id, updated_at=excluded.updated_at
            ",
            (
                bucket,
                key,
                data.len() as i64,
                content_type,
                &etag,
                actor.sub.as_deref(),
                &now,
                &now,
            ),
        )?;
        let object = tx.query_row(
            "SELECT bucket, object_key AS key, size, content_type, etag, owner_id, created_at, updated_at FROM _sdb_objects WHERE bucket=? AND object_key=?",
            (bucket, key),
            row_to_json_map,
        )?;
        record_event(&tx, "_sdb_objects", "storage_put", &object, actor)?;
        tx.commit()?;
        Ok(json!({ "object": object }))
    }

    fn execute_delete_object(
        &self,
        state: &mut ProjectState,
        bucket: &str,
        key: &str,
        actor: &Actor,
    ) -> Result<(Value, bool), ApiError> {
        let bucket = assert_user_ident(bucket)?;
        let key = safe_object_key(key)?;
        let meta = state
            .conn
            .query_row(
                "SELECT bucket, object_key AS key, size, content_type, etag, owner_id, created_at, updated_at FROM _sdb_objects WHERE bucket=? AND object_key=?",
                (bucket, key),
                row_to_json_map,
            )
            .optional()?
            .ok_or_else(|| ApiError::new(404, "object not found"))?;
        let tx = state.conn.transaction()?;
        tx.execute(
            "DELETE FROM _sdb_objects WHERE bucket=? AND object_key=?",
            (bucket, key),
        )?;
        record_event(&tx, "_sdb_objects", "storage_delete", &meta, actor)?;
        tx.commit()?;
        self.object_store
            .delete(&storage_key(&state.project_id, bucket, key))?;
        Ok((json!({ "deleted": true }), true))
    }

    fn ensure_meta(&self, conn: &Connection) -> Result<(), ApiError> {
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS _sdb_policies(
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                table_name TEXT NOT NULL,
                operation TEXT NOT NULL,
                name TEXT NOT NULL,
                rule_json TEXT NOT NULL,
                updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                UNIQUE(table_name, operation, name)
            );
            CREATE TABLE IF NOT EXISTS _sdb_buckets(
                name TEXT PRIMARY KEY,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE TABLE IF NOT EXISTS _sdb_objects(
                bucket TEXT NOT NULL,
                object_key TEXT NOT NULL,
                size INTEGER NOT NULL,
                content_type TEXT NOT NULL,
                etag TEXT NOT NULL,
                owner_id TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                PRIMARY KEY(bucket, object_key),
                FOREIGN KEY(bucket) REFERENCES _sdb_buckets(name) ON DELETE CASCADE
            );
            CREATE TABLE IF NOT EXISTS _sdb_outbox(
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                table_name TEXT NOT NULL,
                operation TEXT NOT NULL,
                row_json TEXT NOT NULL,
                actor_sub TEXT,
                actor_role TEXT
            );
            CREATE TABLE IF NOT EXISTS _sdb_idempotency(
                idempotency_key TEXT PRIMARY KEY,
                request_hash TEXT NOT NULL,
                response_json TEXT NOT NULL,
                bookmark TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            ",
        )?;
        Ok(())
    }

    fn persist_snapshot(&self, state: &mut ProjectState, reason: &str) -> Result<(), ApiError> {
        let tmp = state.cache_dir.join(format!(
            "snapshot-{}-{}.sqlite",
            std::process::id(),
            now_ms()
        ));
        if tmp.exists() {
            fs::remove_file(&tmp).map_err(anyhow::Error::from)?;
        }
        state.conn.execute_batch(&format!(
            "VACUUM INTO '{}'",
            tmp.to_string_lossy().replace('\'', "''")
        ))?;
        let checksum = sha256_file(&tmp)?;
        let snapshot_bytes = fs::metadata(&tmp).map_err(anyhow::Error::from)?.len();
        let next_generation = state.generation + 1;
        let snapshot_object_key = snapshot_generation_key(&state.project_id, next_generation);
        self.object_store.put_file(&snapshot_object_key, &tmp)?;
        let new_snapshot = SnapshotManifest {
            key: snapshot_object_key,
            bytes: snapshot_bytes,
            sha256: checksum.clone(),
        };
        self.put_manifest(&DurableManifest {
            schema_version: 1,
            project_id: state.project_id.clone(),
            generation: next_generation,
            commit_seq: state.commit_seq,
            bookmark: bookmark_for_seq(state.commit_seq),
            updated_at: utc_now(),
            reason: Some(reason.to_string()),
            snapshot: new_snapshot.clone(),
            wal: WalChainManifest {
                prefix: wal_prefix(&state.project_id),
                durable_bytes: 0,
                segments: Vec::new(),
            },
            ops_since_snapshot: 0,
            gc_watermark_generation: next_generation.saturating_sub(1),
        })?;
        maybe_crash_after_stage("after_snapshot_manifest_before_cache_replace");
        let placeholder = Connection::open_in_memory()?;
        let old_conn = std::mem::replace(&mut state.conn, placeholder);
        drop(old_conn);
        fs::copy(&tmp, &state.db_path).map_err(anyhow::Error::from)?;
        fs::remove_file(state.db_path.with_extension("sqlite-wal")).ok();
        fs::remove_file(state.db_path.with_extension("sqlite-shm")).ok();
        maybe_crash_after_stage("after_snapshot_cache_replace_before_reopen");
        state.conn = open_project_connection(&state.db_path, &self.options.sqlite_synchronous)?;
        self.ensure_meta(&state.conn)?;
        state.generation = next_generation;
        state.current_snapshot = Some(new_snapshot);
        state.wal_segments.clear();
        state.last_durable_wal_bytes = 0;
        state.next_wal_segment_id = 0;
        state.ops_since_snapshot = 0;
        state.last_snapshot_at_ms = now_ms();
        self.object_store
            .delete_prefix(&wal_prefix(&state.project_id))
            .ok();
        fs::remove_file(&tmp).ok();
        self.append_change_log(
            state,
            reason,
            json!({
                "snapshot_sha256": checksum,
                "snapshot_bytes": snapshot_bytes,
                "durable_wal_bytes": 0,
                "wal_segment_count": 0,
                "ops_since_snapshot": 0
            }),
        )?;
        Ok(())
    }

    fn durabilize_wal(
        &self,
        state: &mut ProjectState,
        reason: &str,
        committed_ops: u64,
    ) -> Result<(), ApiError> {
        let wal_path = state.db_path.with_extension("sqlite-wal");
        let mut wal_bytes = 0;
        if wal_path.exists() {
            wal_bytes = fs::metadata(&wal_path).map_err(anyhow::Error::from)?.len();
            if wal_bytes < state.last_durable_wal_bytes {
                self.object_store
                    .delete_prefix(&wal_prefix(&state.project_id))?;
                state.last_durable_wal_bytes = 0;
                state.next_wal_segment_id = 0;
                state.wal_segments.clear();
            }
            if wal_bytes > state.last_durable_wal_bytes {
                let offset = state.last_durable_wal_bytes;
                let segment = read_file_range(&wal_path, state.last_durable_wal_bytes)?;
                if !segment.is_empty() {
                    let key = wal_segment_key(&state.project_id, state.next_wal_segment_id);
                    let len = segment.len() as u64;
                    let sha256 = hex_sha256(&segment);
                    self.object_store.put_bytes(&key, &segment)?;
                    state.wal_segments.push(WalSegmentManifest {
                        id: state.next_wal_segment_id,
                        key,
                        offset,
                        len,
                        sha256,
                    });
                    state.next_wal_segment_id += 1;
                }
            }
            state.last_durable_wal_bytes = wal_bytes;
        }
        state.ops_since_snapshot += committed_ops.max(1);
        state.commit_seq = state.commit_seq.saturating_add(1);
        self.write_manifest(state, Some(reason))?;
        if self.should_write_metadata(state) {
            self.append_change_log(
                state,
                reason,
                json!({
                    "durable_wal_bytes": wal_bytes,
                    "wal_segment_count": state.wal_segments.len(),
                    "ops_since_snapshot": state.ops_since_snapshot
                }),
            )?;
        }
        if self.should_snapshot(state) {
            if let Err(err) = self.persist_snapshot(state, &format!("compact_after:{reason}")) {
                let _ = self.append_change_log(
                    state,
                    "snapshot_failed",
                    json!({
                        "error": err.message,
                        "durable_wal_bytes": state.last_durable_wal_bytes,
                        "wal_segment_count": state.wal_segments.len(),
                        "ops_since_snapshot": state.ops_since_snapshot
                    }),
                );
            }
        }
        Ok(())
    }

    fn should_write_metadata(&self, state: &ProjectState) -> bool {
        self.options.metadata_every_ops > 0
            && state.ops_since_snapshot % self.options.metadata_every_ops == 0
    }

    fn should_snapshot(&self, state: &ProjectState) -> bool {
        (self.options.snapshot_every_ops > 0
            && state.ops_since_snapshot >= self.options.snapshot_every_ops)
            || (self.options.snapshot_every_ms > 0
                && now_ms() - state.last_snapshot_at_ms >= self.options.snapshot_every_ms as u128)
    }

    fn ensure_wal_budget(&self, state: &mut ProjectState) -> Result<(), ApiError> {
        let max_bytes = self.options.max_durable_wal_bytes;
        if max_bytes == 0 || state.last_durable_wal_bytes < max_bytes {
            return Ok(());
        }
        match self.persist_snapshot(state, "wal_budget") {
            Ok(()) => Ok(()),
            Err(err) => Err(ApiError::new(
                507,
                format!(
                    "project durable WAL budget exceeded: durable_wal_bytes={}, max_durable_wal_bytes={}, compact_error={}",
                    state.last_durable_wal_bytes, max_bytes, err.message
                ),
            )),
        }
    }

    fn write_manifest(&self, state: &ProjectState, reason: Option<&str>) -> Result<(), ApiError> {
        let snapshot = state.current_snapshot.clone().ok_or_else(|| {
            ApiError::new(
                500,
                "cannot write durable manifest before snapshot is initialized",
            )
        })?;
        let manifest = DurableManifest {
            schema_version: 1,
            project_id: state.project_id.clone(),
            generation: state.generation,
            commit_seq: state.commit_seq,
            bookmark: bookmark_for_seq(state.commit_seq),
            updated_at: utc_now(),
            reason: reason.map(ToOwned::to_owned),
            snapshot,
            wal: WalChainManifest {
                prefix: wal_prefix(&state.project_id),
                durable_bytes: state.last_durable_wal_bytes,
                segments: state.wal_segments.clone(),
            },
            ops_since_snapshot: state.ops_since_snapshot,
            gc_watermark_generation: state.generation.saturating_sub(1),
        };
        self.put_manifest(&manifest)
    }

    fn put_manifest(&self, manifest: &DurableManifest) -> Result<(), ApiError> {
        self.object_store.put_bytes(
            &manifest_key(&manifest.project_id),
            serde_json::to_vec_pretty(&manifest)?.as_slice(),
        )?;
        Ok(())
    }

    fn append_change_log(
        &self,
        state: &ProjectState,
        reason: &str,
        fields: Value,
    ) -> Result<(), ApiError> {
        let mut obj = Map::new();
        obj.insert("at".to_string(), Value::String(utc_now()));
        obj.insert("reason".to_string(), Value::String(reason.to_string()));
        if let Value::Object(fields) = fields {
            for (key, value) in fields {
                obj.insert(key, value);
            }
        }
        let key = change_log_key(&state.project_id, state.ops_since_snapshot, reason);
        self.object_store
            .put_bytes(&key, serde_json::to_vec(&Value::Object(obj))?.as_slice())?;
        Ok(())
    }

    fn where_for_operation(
        &self,
        conn: &Connection,
        table: &str,
        operation: &str,
        filters: &HashMap<String, String>,
        actor: &Actor,
    ) -> Result<(String, Vec<SqlValue>), ApiError> {
        let mut clauses = Vec::new();
        let mut params = Vec::new();
        for (key, value) in filters {
            let column = assert_user_ident(key)?;
            clauses.push(format!("{} = ?", quote_ident(column)?));
            params.push(SqlValue::Text(value.clone()));
        }
        let rules = policy_rules(conn, table, operation)?;
        let (policy_sql, mut policy_params) =
            compile_policies(&rules, actor).map_err(|err| ApiError::new(400, err.to_string()))?;
        clauses.push(format!("({policy_sql})"));
        params.append(&mut policy_params);
        Ok((clauses.join(" AND "), params))
    }

    fn where_for_operation_postgrest(
        &self,
        conn: &Connection,
        table: &str,
        operation: &str,
        filters: &crate::postgrest::FilterExpr,
        actor: &Actor,
    ) -> Result<(String, Vec<SqlValue>), ApiError> {
        let (filter_sql, mut filter_params) = crate::postgrest::compile_filters(filters)
            .map_err(|err| ApiError::new(400, err.to_string()))?;
        let rules = policy_rules(conn, table, operation)?;
        let (policy_sql, mut policy_params) =
            compile_policies(&rules, actor).map_err(|err| ApiError::new(400, err.to_string()))?;
        let where_sql = if filter_sql == "1=1" {
            format!("({policy_sql})")
        } else {
            format!("{filter_sql} AND ({policy_sql})")
        };
        filter_params.append(&mut policy_params);
        Ok((where_sql, filter_params))
    }

    fn notifier(&self, project_id: &str) -> Result<Arc<Notify>, ApiError> {
        let pid = safe_project_id(project_id)?.to_string();
        let mut notifiers = self.notifiers.lock().unwrap();
        Ok(notifiers
            .entry(pid)
            .or_insert_with(|| Arc::new(Notify::new()))
            .clone())
    }

    fn notify(&self, project_id: &str) {
        if let Ok(notifier) = self.notifier(project_id) {
            notifier.notify_waiters();
        }
    }
}

fn policy_rules(conn: &Connection, table: &str, operation: &str) -> Result<Vec<Value>, ApiError> {
    let mut stmt = conn.prepare("SELECT rule_json FROM _sdb_policies WHERE table_name=? AND operation IN (?, 'all') ORDER BY id")?;
    let rows = stmt
        .query_map((table, operation), |row| {
            let raw: String = row.get(0)?;
            Ok(serde_json::from_str::<Value>(&raw).unwrap_or(Value::Null))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn record_event(
    tx: &rusqlite::Transaction<'_>,
    table: &str,
    operation: &str,
    row: &Map<String, Value>,
    actor: &Actor,
) -> Result<(), ApiError> {
    tx.execute(
        "INSERT INTO _sdb_outbox(table_name, operation, row_json, actor_sub, actor_role) VALUES(?, ?, ?, ?, ?)",
        (table, operation, Value::Object(row.clone()).to_string(), actor.sub.as_deref(), actor.role.as_str()),
    )?;
    Ok(())
}

fn table_columns(conn: &Connection, table: &str) -> Result<Vec<Value>, ApiError> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({})", quote_ident(table)?))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(json!({
                "cid": row.get::<_, i64>(0)?,
                "name": row.get::<_, String>(1)?,
                "type": row.get::<_, String>(2)?,
                "not_null": row.get::<_, i64>(3)? != 0,
                "default": row.get::<_, Option<String>>(4)?,
                "primary_key": row.get::<_, i64>(5)? != 0
            }))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn row_to_json_map(row: &rusqlite::Row<'_>) -> rusqlite::Result<Map<String, Value>> {
    let mut out = Map::new();
    let row_ref = row.as_ref();
    for idx in 0..row_ref.column_count() {
        let name = row_ref.column_name(idx)?.to_string();
        let value: SqlValue = row.get(idx)?;
        out.insert(name, sql_to_json(value));
    }
    Ok(out)
}

fn normalize_row_payload(data: Value) -> Result<Map<String, Value>, ApiError> {
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

fn json_to_sql_value(value: &Value) -> SqlValue {
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

fn sql_to_json(value: SqlValue) -> Value {
    match value {
        SqlValue::Null => Value::Null,
        SqlValue::Integer(value) => json!(value),
        SqlValue::Real(value) => json!(value),
        SqlValue::Text(value) => Value::String(value),
        SqlValue::Blob(value) => Value::String(STANDARD.encode(value)),
    }
}

fn require_table(conn: &Connection, table: &str) -> Result<(), ApiError> {
    let found = conn
        .query_row(
            "SELECT name FROM sqlite_master WHERE type='table' AND name=? AND name NOT LIKE '_sdb_%' AND name NOT LIKE 'sqlite_%'",
            [table],
            |_| Ok(()),
        )
        .optional()?;
    found.ok_or_else(|| ApiError::new(404, format!("table not found: {table}")))
}

fn require_bucket(conn: &Connection, bucket: &str) -> Result<(), ApiError> {
    let found = conn
        .query_row(
            "SELECT name FROM _sdb_buckets WHERE name=?",
            [bucket],
            |_| Ok(()),
        )
        .optional()?;
    found.ok_or_else(|| ApiError::new(404, format!("bucket not found: {bucket}")))
}

fn open_project_connection(
    db_path: &Path,
    sqlite_synchronous: &str,
) -> Result<Connection, ApiError> {
    if let Some(parent) = db_path.parent() {
        fs::create_dir_all(parent).map_err(anyhow::Error::from)?;
    }
    let sync_mode = sqlite_synchronous.to_ascii_uppercase();
    match sync_mode.as_str() {
        "OFF" | "NORMAL" | "FULL" | "EXTRA" => {}
        other => {
            return Err(ApiError::new(
                400,
                format!("unsupported sqlite synchronous mode: {other}"),
            ));
        }
    }
    let conn = Connection::open(db_path)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.execute_batch(&format!(
        "
        PRAGMA journal_mode=WAL;
        PRAGMA synchronous={sync_mode};
        PRAGMA wal_autocheckpoint=0;
        PRAGMA foreign_keys=ON;
        "
    ))?;
    Ok(conn)
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

fn assert_user_ident(name: &str) -> Result<&str, ApiError> {
    if !valid_ident(name) || name.starts_with("_sdb_") || name.starts_with("sqlite_") {
        return Err(ApiError::new(
            400,
            format!("invalid user identifier: {name}"),
        ));
    }
    Ok(name)
}

fn safe_object_key(key: &str) -> Result<&str, ApiError> {
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

fn sql_type(name: &str) -> Result<&'static str, ApiError> {
    TYPE_MAP
        .iter()
        .find_map(|(key, value)| (*key == name.to_lowercase()).then_some(*value))
        .ok_or_else(|| ApiError::new(400, format!("unsupported column type: {name}")))
}

fn snapshot_key(project_id: &str) -> String {
    format!("projects/{project_id}/database.sqlite")
}

fn snapshot_generation_key(project_id: &str, generation: u64) -> String {
    format!("projects/{project_id}/snapshots/{generation:020}.sqlite")
}

fn manifest_key(project_id: &str) -> String {
    format!("projects/{project_id}/manifest.json")
}

fn writer_lease_key(project_id: &str) -> String {
    format!("projects/{project_id}/writer-lease.json")
}

fn writer_lease_claim_key(project_id: &str, fencing_token: u64) -> String {
    format!("projects/{project_id}/writer-lease-claims/{fencing_token:020}.json")
}

fn bookmark_for_seq(seq: u64) -> String {
    format!("sdb1-{seq:020}")
}

fn bookmark_to_seq(bookmark: &str) -> Result<u64, ApiError> {
    let raw = bookmark.trim();
    let seq = raw
        .strip_prefix("sdb1-")
        .ok_or_else(|| ApiError::new(400, format!("invalid bookmark: {bookmark}")))?;
    seq.parse::<u64>()
        .map_err(|_| ApiError::new(400, format!("invalid bookmark: {bookmark}")))
}

fn manifest_commit_seq(manifest: Option<&DurableManifest>) -> u64 {
    manifest
        .map(|manifest| bookmark_to_seq(&manifest.bookmark).unwrap_or(manifest.commit_seq))
        .unwrap_or(0)
}

fn manifest_bookmark(manifest: &DurableManifest) -> String {
    if manifest.bookmark.is_empty() {
        bookmark_for_seq(manifest.commit_seq)
    } else {
        manifest.bookmark.clone()
    }
}

fn with_bookmark(mut value: Value, bookmark: &str) -> Value {
    match &mut value {
        Value::Object(map) => {
            map.insert("bookmark".to_string(), Value::String(bookmark.to_string()));
            value
        }
        _ => json!({ "value": value, "bookmark": bookmark }),
    }
}

fn validate_idempotency(idempotency: &WriteIdempotency) -> Result<(), ApiError> {
    let key = idempotency.key.trim();
    if key.is_empty() || key.len() > 256 {
        return Err(ApiError::new(400, "idempotency key must be 1..256 bytes"));
    }
    if !key
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        return Err(ApiError::new(
            400,
            "idempotency key may contain only ASCII letters, digits, '-', '_', '.', ':'",
        ));
    }
    if idempotency.request_hash.len() != 64
        || !idempotency
            .request_hash
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(ApiError::new(
            400,
            "idempotency request hash must be a SHA-256 hex string",
        ));
    }
    Ok(())
}

fn wal_prefix(project_id: &str) -> String {
    format!("projects/{project_id}/wal")
}

fn wal_segment_key(project_id: &str, segment_id: u64) -> String {
    format!("{}/{segment_id:020}.wal", wal_prefix(project_id))
}

fn wal_segment_id(key: &str) -> Option<u64> {
    key.rsplit('/')
        .next()
        .and_then(|name| name.strip_suffix(".wal"))
        .and_then(|id| id.parse::<u64>().ok())
}

fn storage_key(project_id: &str, bucket: &str, key: &str) -> String {
    format!("projects/{project_id}/storage/{bucket}/{key}")
}

fn change_log_key(project_id: &str, ops_since_snapshot: u64, reason: &str) -> String {
    let reason = reason
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect::<String>();
    format!(
        "projects/{project_id}/change-log/{}-{ops_since_snapshot:020}-{reason}.json",
        now_ms()
    )
}

fn read_file_range(path: &Path, offset: u64) -> Result<Vec<u8>, ApiError> {
    let mut file = fs::File::open(path).map_err(anyhow::Error::from)?;
    file.seek(SeekFrom::Start(offset))
        .map_err(anyhow::Error::from)?;
    let mut out = Vec::new();
    file.read_to_end(&mut out).map_err(anyhow::Error::from)?;
    Ok(out)
}

fn hex_sha256(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

fn sha256_file(path: &Path) -> Result<String, ApiError> {
    let data = fs::read(path).map_err(anyhow::Error::from)?;
    Ok(hex_sha256(&data))
}

fn utc_now() -> String {
    format!("{}", now_ms())
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn new_runtime_id() -> String {
    static NEXT_RUNTIME_ID: AtomicU64 = AtomicU64::new(1);
    format!(
        "runtime-{}-{}",
        std::process::id(),
        NEXT_RUNTIME_ID.fetch_add(1, Ordering::SeqCst)
    )
}

fn build_forward_client(options: &RuntimeOptions) -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_millis(
            options.forward_connect_timeout_ms.max(1),
        ))
        .timeout(Duration::from_millis(
            options.forward_request_timeout_ms.max(1),
        ))
        .pool_max_idle_per_host(32)
        .pool_idle_timeout(Duration::from_secs(90))
        .tcp_nodelay(true)
        .user_agent("serverless-db-core/0.1")
        .build()
        .map_err(anyhow::Error::from)
}

fn maybe_crash_after_stage(stage: &str) {
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

fn default_text_type() -> String {
    "text".to_string()
}

fn default_auto_increment() -> bool {
    true
}

fn default_all_operation() -> String {
    "all".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_store::{ObjectMeta, ObjectStore, ObjectStoreRef};
    use serde_json::json;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Barrier, Mutex};
    use std::thread;
    use std::time::Duration;

    fn runtime() -> (tempfile::TempDir, ProjectRuntime) {
        let dir = tempfile::tempdir().unwrap();
        let runtime = ProjectRuntime::new(
            dir.path().join("runtime"),
            RuntimeOptions {
                snapshot_every_ops: 1000,
                snapshot_every_ms: 300_000,
                metadata_every_ops: 100,
                group_commit_max_ops: 64,
                group_commit_delay_ms: 2,
                writer_queue_capacity: 1024,
                max_durable_wal_bytes: 64 * 1024 * 1024,
                writer_lease_ttl_ms: 30_000,
                read_replica: false,
                replica_refresh_interval_ms: 1_000,
                replica_bookmark_wait_timeout_ms: 5_000,
                primary_url: None,
                routing_region: None,
                routing_endpoints: Vec::new(),
                forward_connect_timeout_ms: 1_000,
                forward_request_timeout_ms: 5_000,
                forward_max_attempts: 3,
                forward_retry_backoff_ms: 25,
                routing_endpoint_failure_threshold: 2,
                routing_endpoint_cooldown_ms: 1_000,
                sqlite_synchronous: "NORMAL".to_string(),
                supabase_project_id: "demo".to_string(),
            },
        )
        .unwrap();
        (dir, runtime)
    }

    fn actor(sub: &str) -> Actor {
        Actor {
            sub: Some(sub.to_string()),
            role: "authenticated".to_string(),
            claims: Map::new(),
        }
    }

    fn setup_notes(runtime: &ProjectRuntime) {
        runtime.create_project("demo").unwrap();
        runtime
            .create_table(
                "demo",
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
                },
            )
            .unwrap();
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

    fn object_store_path(dir: &tempfile::TempDir, key: &str) -> PathBuf {
        let mut path = dir.path().join("runtime").join("object_store");
        for part in key.split('/') {
            path.push(part);
        }
        path
    }

    fn durable_manifest(dir: &tempfile::TempDir) -> DurableManifest {
        let path = object_store_path(dir, &manifest_key("demo"));
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    fn shared_runtime_pair(
        writer_lease_ttl_ms: u64,
    ) -> (tempfile::TempDir, ProjectRuntime, ProjectRuntime) {
        let dir = tempfile::tempdir().unwrap();
        let store: ObjectStoreRef =
            Arc::new(LocalObjectStore::new(dir.path().join("object_store")).unwrap());
        let options = RuntimeOptions {
            snapshot_every_ops: 1000,
            snapshot_every_ms: 300_000,
            metadata_every_ops: 100,
            group_commit_max_ops: 1,
            group_commit_delay_ms: 0,
            writer_queue_capacity: 1024,
            max_durable_wal_bytes: 64 * 1024 * 1024,
            writer_lease_ttl_ms,
            read_replica: false,
            replica_refresh_interval_ms: 1_000,
            replica_bookmark_wait_timeout_ms: 5_000,
            primary_url: None,
            routing_region: None,
            routing_endpoints: Vec::new(),
            forward_connect_timeout_ms: 1_000,
            forward_request_timeout_ms: 5_000,
            forward_max_attempts: 3,
            forward_retry_backoff_ms: 25,
            routing_endpoint_failure_threshold: 2,
            routing_endpoint_cooldown_ms: 1_000,
            sqlite_synchronous: "NORMAL".to_string(),
            supabase_project_id: "demo".to_string(),
        };
        let first = ProjectRuntime::with_object_store(
            dir.path().join("first"),
            options.clone(),
            store.clone(),
        )
        .unwrap();
        let second =
            ProjectRuntime::with_object_store(dir.path().join("second"), options, store).unwrap();
        (dir, first, second)
    }

    fn shared_primary_replica() -> (tempfile::TempDir, ProjectRuntime, ProjectRuntime) {
        let dir = tempfile::tempdir().unwrap();
        let store: ObjectStoreRef =
            Arc::new(LocalObjectStore::new(dir.path().join("object_store")).unwrap());
        let primary_options = RuntimeOptions {
            snapshot_every_ops: 1000,
            snapshot_every_ms: 300_000,
            metadata_every_ops: 100,
            group_commit_max_ops: 1,
            group_commit_delay_ms: 0,
            writer_queue_capacity: 1024,
            max_durable_wal_bytes: 64 * 1024 * 1024,
            writer_lease_ttl_ms: 30_000,
            read_replica: false,
            replica_refresh_interval_ms: 20,
            replica_bookmark_wait_timeout_ms: 500,
            primary_url: None,
            routing_region: Some("local".to_string()),
            routing_endpoints: Vec::new(),
            forward_connect_timeout_ms: 1_000,
            forward_request_timeout_ms: 5_000,
            forward_max_attempts: 3,
            forward_retry_backoff_ms: 25,
            routing_endpoint_failure_threshold: 2,
            routing_endpoint_cooldown_ms: 1_000,
            sqlite_synchronous: "NORMAL".to_string(),
            supabase_project_id: "demo".to_string(),
        };
        let replica_options = RuntimeOptions {
            read_replica: true,
            ..primary_options.clone()
        };
        let primary = ProjectRuntime::with_object_store(
            dir.path().join("primary"),
            primary_options,
            store.clone(),
        )
        .unwrap();
        let replica =
            ProjectRuntime::with_object_store(dir.path().join("replica"), replica_options, store)
                .unwrap();
        (dir, primary, replica)
    }

    #[derive(Clone)]
    struct FaultingObjectStore {
        inner: LocalObjectStore,
        fail_next_put_contains: Arc<Mutex<Option<String>>>,
        delay_put_ms: Arc<Mutex<u64>>,
    }

    impl FaultingObjectStore {
        fn new(base_dir: impl AsRef<Path>) -> anyhow::Result<Self> {
            Ok(Self {
                inner: LocalObjectStore::new(base_dir)?,
                fail_next_put_contains: Arc::new(Mutex::new(None)),
                delay_put_ms: Arc::new(Mutex::new(0)),
            })
        }

        fn fail_next_put_containing(&self, pattern: impl Into<String>) {
            *self.fail_next_put_contains.lock().unwrap() = Some(pattern.into());
        }

        fn delay_puts_by_ms(&self, delay_ms: u64) {
            *self.delay_put_ms.lock().unwrap() = delay_ms;
        }

        fn should_fail_put(&self, key: &str) -> bool {
            let mut guard = self.fail_next_put_contains.lock().unwrap();
            if guard.as_ref().is_some_and(|pattern| key.contains(pattern)) {
                guard.take();
                return true;
            }
            false
        }

        fn maybe_delay_put(&self) {
            let delay_ms = *self.delay_put_ms.lock().unwrap();
            if delay_ms > 0 {
                thread::sleep(Duration::from_millis(delay_ms));
            }
        }
    }

    impl ObjectStore for FaultingObjectStore {
        fn describe(&self, key: &str) -> String {
            self.inner.describe(key)
        }

        fn exists(&self, key: &str) -> anyhow::Result<bool> {
            self.inner.exists(key)
        }

        fn read_bytes(&self, key: &str) -> anyhow::Result<Vec<u8>> {
            self.inner.read_bytes(key)
        }

        fn len(&self, key: &str) -> anyhow::Result<Option<u64>> {
            self.inner.len(key)
        }

        fn put_bytes(&self, key: &str, data: &[u8]) -> anyhow::Result<()> {
            if self.should_fail_put(key) {
                anyhow::bail!("injected object-store put failure for {key}");
            }
            self.maybe_delay_put();
            self.inner.put_bytes(key, data)
        }

        fn put_file(&self, key: &str, source: &Path) -> anyhow::Result<()> {
            if self.should_fail_put(key) {
                anyhow::bail!("injected object-store put failure for {key}");
            }
            self.maybe_delay_put();
            self.inner.put_file(key, source)
        }

        fn delete(&self, key: &str) -> anyhow::Result<()> {
            self.inner.delete(key)
        }

        fn delete_prefix(&self, prefix: &str) -> anyhow::Result<()> {
            self.inner.delete_prefix(prefix)
        }

        fn list_prefix(&self, prefix: &str) -> anyhow::Result<Vec<ObjectMeta>> {
            self.inner.list_prefix(prefix)
        }
    }

    fn fault_runtime(
        snapshot_every_ops: u64,
    ) -> (tempfile::TempDir, ProjectRuntime, Arc<FaultingObjectStore>) {
        fault_runtime_with_queue_capacity(snapshot_every_ops, 16)
    }

    fn fault_runtime_with_queue_capacity(
        snapshot_every_ops: u64,
        writer_queue_capacity: usize,
    ) -> (tempfile::TempDir, ProjectRuntime, Arc<FaultingObjectStore>) {
        fault_runtime_with_limits(snapshot_every_ops, writer_queue_capacity, 64 * 1024 * 1024)
    }

    fn fault_runtime_with_limits(
        snapshot_every_ops: u64,
        writer_queue_capacity: usize,
        max_durable_wal_bytes: u64,
    ) -> (tempfile::TempDir, ProjectRuntime, Arc<FaultingObjectStore>) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            FaultingObjectStore::new(dir.path().join("runtime").join("object_store")).unwrap(),
        );
        let object_store: ObjectStoreRef = store.clone();
        let runtime = ProjectRuntime::with_object_store(
            dir.path().join("runtime"),
            RuntimeOptions {
                snapshot_every_ops,
                snapshot_every_ms: 300_000,
                metadata_every_ops: 100,
                group_commit_max_ops: 1,
                group_commit_delay_ms: 0,
                writer_queue_capacity,
                max_durable_wal_bytes,
                writer_lease_ttl_ms: 30_000,
                read_replica: false,
                replica_refresh_interval_ms: 1_000,
                replica_bookmark_wait_timeout_ms: 5_000,
                primary_url: None,
                routing_region: None,
                routing_endpoints: Vec::new(),
                forward_connect_timeout_ms: 1_000,
                forward_request_timeout_ms: 5_000,
                forward_max_attempts: 3,
                forward_retry_backoff_ms: 25,
                routing_endpoint_failure_threshold: 2,
                routing_endpoint_cooldown_ms: 1_000,
                sqlite_synchronous: "NORMAL".to_string(),
                supabase_project_id: "demo".to_string(),
            },
            object_store,
        )
        .unwrap();
        (dir, runtime, store)
    }

    fn select_note_titles(runtime: &ProjectRuntime, title: &str) -> usize {
        let mut filters = HashMap::new();
        filters.insert("title".to_string(), title.to_string());
        runtime
            .select_rows("demo", "notes", &filters, &actor("alice"), 100)
            .unwrap()["rows"]
            .as_array()
            .unwrap()
            .len()
    }

    fn response_bookmark(value: &Value) -> String {
        value["bookmark"].as_str().unwrap().to_string()
    }

    fn test_idempotency(key: &str, hash_byte: char) -> WriteIdempotency {
        WriteIdempotency {
            key: key.to_string(),
            request_hash: std::iter::repeat_n(hash_byte, 64).collect(),
        }
    }

    #[test]
    fn owner_policy_and_crash_recovery() {
        let (_dir, runtime) = runtime();
        setup_notes(&runtime);
        runtime
            .insert_row(
                "demo",
                "notes",
                json!({"owner_id":"alice","title":"durable"}),
                &actor("alice"),
            )
            .unwrap();
        assert!(
            runtime
                .insert_row(
                    "demo",
                    "notes",
                    json!({"owner_id":"bob","title":"bad"}),
                    &actor("alice")
                )
                .is_err()
        );
        runtime.crash_project("demo").unwrap();
        let rows = runtime
            .select_rows("demo", "notes", &HashMap::new(), &actor("alice"), 100)
            .unwrap();
        assert_eq!(rows["rows"].as_array().unwrap()[0]["title"], "durable");
    }

    #[test]
    fn storage_roundtrip() {
        let (_dir, runtime) = runtime();
        runtime.create_project("demo").unwrap();
        runtime.create_bucket("demo", "files").unwrap();
        runtime
            .put_object(
                "demo",
                "files",
                "hello.txt",
                b"hello",
                "text/plain",
                &actor("alice"),
            )
            .unwrap();
        let object = runtime.get_object("demo", "files", "hello.txt", &actor("alice")).unwrap();
        assert_eq!(object.data, b"hello");
    }

    #[test]
    fn writer_lease_blocks_second_runtime_writer() {
        let (_dir, first, second) = shared_runtime_pair(30_000);
        setup_notes(&first);
        first
            .insert_row(
                "demo",
                "notes",
                json!({"owner_id":"alice","title":"first"}),
                &actor("alice"),
            )
            .unwrap();

        let err = second
            .insert_row(
                "demo",
                "notes",
                json!({"owner_id":"alice","title":"second"}),
                &actor("alice"),
            )
            .unwrap_err();
        assert_eq!(err.status, 423);
        assert!(err.message.contains("writer lease"));
        assert_eq!(select_note_titles(&first, "second"), 0);
    }

    #[test]
    fn expired_writer_lease_allows_takeover_and_fences_old_runtime() {
        let (_dir, first, second) = shared_runtime_pair(25);
        setup_notes(&first);
        first
            .insert_row(
                "demo",
                "notes",
                json!({"owner_id":"alice","title":"first"}),
                &actor("alice"),
            )
            .unwrap();
        thread::sleep(Duration::from_millis(40));

        second
            .insert_row(
                "demo",
                "notes",
                json!({"owner_id":"alice","title":"second"}),
                &actor("alice"),
            )
            .unwrap();
        let err = first
            .insert_row(
                "demo",
                "notes",
                json!({"owner_id":"alice","title":"old-runtime"}),
                &actor("alice"),
            )
            .unwrap_err();
        assert_eq!(err.status, 423);
        assert_eq!(select_note_titles(&second, "second"), 1);
        assert_eq!(select_note_titles(&second, "old-runtime"), 0);
    }

    #[test]
    fn expired_writer_lease_takeover_has_single_winner() {
        let dir = tempfile::tempdir().unwrap();
        let store: ObjectStoreRef =
            Arc::new(LocalObjectStore::new(dir.path().join("object_store")).unwrap());
        let options = RuntimeOptions {
            snapshot_every_ops: 1000,
            snapshot_every_ms: 300_000,
            metadata_every_ops: 100,
            group_commit_max_ops: 1,
            group_commit_delay_ms: 0,
            writer_queue_capacity: 1024,
            max_durable_wal_bytes: 64 * 1024 * 1024,
            writer_lease_ttl_ms: 250,
            read_replica: false,
            replica_refresh_interval_ms: 1_000,
            replica_bookmark_wait_timeout_ms: 5_000,
            primary_url: None,
            routing_region: None,
            routing_endpoints: Vec::new(),
            forward_connect_timeout_ms: 1_000,
            forward_request_timeout_ms: 5_000,
            forward_max_attempts: 3,
            forward_retry_backoff_ms: 25,
            routing_endpoint_failure_threshold: 2,
            routing_endpoint_cooldown_ms: 1_000,
            sqlite_synchronous: "NORMAL".to_string(),
            supabase_project_id: "demo".to_string(),
        };
        let first = ProjectRuntime::with_object_store(
            dir.path().join("first"),
            options.clone(),
            store.clone(),
        )
        .unwrap();
        let second = ProjectRuntime::with_object_store(
            dir.path().join("second"),
            options.clone(),
            store.clone(),
        )
        .unwrap();
        let third = ProjectRuntime::with_object_store(
            dir.path().join("third"),
            options.clone(),
            store.clone(),
        )
        .unwrap();
        setup_notes(&first);
        first
            .insert_row(
                "demo",
                "notes",
                json!({"owner_id":"alice","title":"first"}),
                &actor("alice"),
            )
            .unwrap();
        thread::sleep(Duration::from_millis(300));

        let barrier = Arc::new(Barrier::new(3));
        let results = Arc::new(Mutex::new(Vec::<Result<String, (u16, String)>>::new()));
        let mut threads = Vec::new();
        for (runtime, title) in [
            (second.clone(), "second".to_string()),
            (third.clone(), "third".to_string()),
        ] {
            let barrier = barrier.clone();
            let results = results.clone();
            threads.push(thread::spawn(move || {
                barrier.wait();
                let result = runtime
                    .insert_row(
                        "demo",
                        "notes",
                        json!({"owner_id":"alice","title": title.clone()}),
                        &actor("alice"),
                    )
                    .map(|_| title)
                    .map_err(|err| (err.status, err.message));
                results.lock().unwrap().push(result);
            }));
        }
        barrier.wait();
        for handle in threads {
            handle.join().unwrap();
        }

        let results = results.lock().unwrap().clone();
        assert_eq!(
            results.iter().filter(|result| result.is_ok()).count(),
            1,
            "{results:?}"
        );
        assert_eq!(
            results
                .iter()
                .filter(|result| result.as_ref().is_err_and(|(status, _)| *status == 423))
                .count(),
            1,
            "{results:?}"
        );

        let reader =
            ProjectRuntime::with_object_store(dir.path().join("reader"), options, store).unwrap();
        assert_eq!(
            select_note_titles(&reader, "second") + select_note_titles(&reader, "third"),
            1
        );
    }

    #[test]
    fn write_responses_include_monotonic_bookmarks() {
        let (_dir, runtime) = runtime();
        setup_notes(&runtime);
        let first = runtime
            .insert_row(
                "demo",
                "notes",
                json!({"owner_id":"alice","title":"first"}),
                &actor("alice"),
            )
            .unwrap();
        let second = runtime
            .insert_row(
                "demo",
                "notes",
                json!({"owner_id":"alice","title":"second"}),
                &actor("alice"),
            )
            .unwrap();
        let first_bookmark = response_bookmark(&first);
        let second_bookmark = response_bookmark(&second);

        assert!(second_bookmark > first_bookmark);
        assert!(
            bookmark_to_seq(&second_bookmark).unwrap() > bookmark_to_seq(&first_bookmark).unwrap()
        );
    }

    #[test]
    fn idempotent_insert_replays_response_without_duplicate_row() {
        let (_dir, runtime) = runtime();
        setup_notes(&runtime);
        let idempotency = test_idempotency("insert:1", 'a');
        let first = runtime
            .insert_row_with_idempotency(
                "demo",
                "notes",
                json!({"owner_id":"alice","title":"once"}),
                &actor("alice"),
                Some(idempotency.clone()),
            )
            .unwrap();
        let replay = runtime
            .insert_row_with_idempotency(
                "demo",
                "notes",
                json!({"owner_id":"alice","title":"once"}),
                &actor("alice"),
                Some(idempotency),
            )
            .unwrap();

        assert_eq!(replay["bookmark"], first["bookmark"]);
        assert_eq!(replay["row"], first["row"]);
        assert_eq!(select_note_titles(&runtime, "once"), 1);
    }

    #[test]
    fn idempotent_insert_replays_after_crash_recovery() {
        let (_dir, runtime) = runtime();
        setup_notes(&runtime);
        let idempotency = test_idempotency("insert:recover", 'c');
        let first = runtime
            .insert_row_with_idempotency(
                "demo",
                "notes",
                json!({"owner_id":"alice","title":"recover-once"}),
                &actor("alice"),
                Some(idempotency.clone()),
            )
            .unwrap();
        runtime.crash_project("demo").unwrap();

        let replay = runtime
            .insert_row_with_idempotency(
                "demo",
                "notes",
                json!({"owner_id":"alice","title":"recover-once"}),
                &actor("alice"),
                Some(idempotency),
            )
            .unwrap();
        assert_eq!(replay["bookmark"], first["bookmark"]);
        assert_eq!(select_note_titles(&runtime, "recover-once"), 1);
    }

    #[test]
    fn idempotent_key_rejects_different_request_hash() {
        let (_dir, runtime) = runtime();
        setup_notes(&runtime);
        runtime
            .insert_row_with_idempotency(
                "demo",
                "notes",
                json!({"owner_id":"alice","title":"first"}),
                &actor("alice"),
                Some(test_idempotency("insert:conflict", 'a')),
            )
            .unwrap();

        let err = runtime
            .insert_row_with_idempotency(
                "demo",
                "notes",
                json!({"owner_id":"alice","title":"second"}),
                &actor("alice"),
                Some(test_idempotency("insert:conflict", 'b')),
            )
            .unwrap_err();
        assert_eq!(err.status, 409);
        assert_eq!(select_note_titles(&runtime, "first"), 1);
        assert_eq!(select_note_titles(&runtime, "second"), 0);
    }

    #[test]
    fn bookmark_read_rehydrates_stale_runtime() {
        let (_dir, first, second) = shared_runtime_pair(30_000);
        setup_notes(&first);
        assert_eq!(select_note_titles(&second, "after"), 0);

        let inserted = first
            .insert_row(
                "demo",
                "notes",
                json!({"owner_id":"alice","title":"after"}),
                &actor("alice"),
            )
            .unwrap();
        let bookmark = response_bookmark(&inserted);
        let mut filters = HashMap::new();
        filters.insert("title".to_string(), "after".to_string());

        let stale = second
            .select_rows("demo", "notes", &filters, &actor("alice"), 100)
            .unwrap();
        assert_eq!(stale["rows"].as_array().unwrap().len(), 0);

        let consistent = second
            .select_rows_at_bookmark(
                "demo",
                "notes",
                &filters,
                &actor("alice"),
                100,
                Some(&bookmark),
            )
            .unwrap();
        assert_eq!(consistent["rows"].as_array().unwrap().len(), 1);
        assert!(consistent["bookmark"].as_str().unwrap() >= bookmark.as_str());
    }

    #[test]
    fn unavailable_bookmark_returns_425() {
        let (_dir, runtime) = runtime();
        setup_notes(&runtime);
        let err = runtime
            .select_rows_at_bookmark(
                "demo",
                "notes",
                &HashMap::new(),
                &actor("alice"),
                100,
                Some(&bookmark_for_seq(999)),
            )
            .unwrap_err();
        assert_eq!(err.status, 425);
        assert!(err.message.contains("requested bookmark"));
    }

    #[test]
    fn read_replica_rejects_writes() {
        let (_dir, primary, replica) = shared_primary_replica();
        setup_notes(&primary);

        let err = replica
            .insert_row(
                "demo",
                "notes",
                json!({"owner_id":"alice","title":"replica-write"}),
                &actor("alice"),
            )
            .unwrap_err();
        assert_eq!(err.status, 405);
        assert!(err.message.contains("read replica"));
    }

    #[test]
    fn read_replica_refreshes_from_manifest_without_bookmark() {
        let (_dir, primary, replica) = shared_primary_replica();
        setup_notes(&primary);
        primary
            .insert_row(
                "demo",
                "notes",
                json!({"owner_id":"alice","title":"first"}),
                &actor("alice"),
            )
            .unwrap();
        assert_eq!(select_note_titles(&replica, "first"), 1);

        let second = primary
            .insert_row(
                "demo",
                "notes",
                json!({"owner_id":"alice","title":"second"}),
                &actor("alice"),
            )
            .unwrap();
        let second_bookmark = response_bookmark(&second);
        let mut filters = HashMap::new();
        filters.insert("title".to_string(), "second".to_string());

        let rows = replica
            .select_rows("demo", "notes", &filters, &actor("alice"), 100)
            .unwrap();
        assert_eq!(rows["rows"].as_array().unwrap().len(), 1);
        assert!(rows["bookmark"].as_str().unwrap() >= second_bookmark.as_str());
    }

    #[test]
    fn read_replica_background_refresh_reduces_lag() {
        let (_dir, primary, replica) = shared_primary_replica();
        setup_notes(&primary);
        let first = primary
            .insert_row(
                "demo",
                "notes",
                json!({"owner_id":"alice","title":"first"}),
                &actor("alice"),
            )
            .unwrap();
        assert_eq!(select_note_titles(&replica, "first"), 1);

        let second = primary
            .insert_row(
                "demo",
                "notes",
                json!({"owner_id":"alice","title":"second"}),
                &actor("alice"),
            )
            .unwrap();
        let first_seq = bookmark_to_seq(first["bookmark"].as_str().unwrap()).unwrap();
        let second_seq = bookmark_to_seq(second["bookmark"].as_str().unwrap()).unwrap();
        assert!(second_seq > first_seq);
        thread::sleep(Duration::from_millis(120));

        let info = replica.project_info("demo").unwrap();
        assert_eq!(
            info["replica"]["remote_commit_seq"].as_u64().unwrap(),
            second_seq
        );
        assert_eq!(
            info["replica"]["local_commit_seq"].as_u64().unwrap(),
            second_seq
        );
        assert_eq!(info["replica"]["lag_commits"], 0);
    }

    #[test]
    fn read_replica_waits_for_requested_bookmark() {
        let (_dir, primary, replica) = shared_primary_replica();
        setup_notes(&primary);
        let first = primary
            .insert_row(
                "demo",
                "notes",
                json!({"owner_id":"alice","title":"first"}),
                &actor("alice"),
            )
            .unwrap();
        let requested =
            bookmark_for_seq(bookmark_to_seq(first["bookmark"].as_str().unwrap()).unwrap() + 1);
        let primary_for_thread = primary.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(60));
            primary_for_thread
                .insert_row(
                    "demo",
                    "notes",
                    json!({"owner_id":"alice","title":"delayed"}),
                    &actor("alice"),
                )
                .unwrap();
        });

        let mut filters = HashMap::new();
        filters.insert("title".to_string(), "delayed".to_string());
        let started = Instant::now();
        let rows = replica
            .select_rows_at_bookmark(
                "demo",
                "notes",
                &filters,
                &actor("alice"),
                100,
                Some(&requested),
            )
            .unwrap();
        assert!(started.elapsed() >= Duration::from_millis(40));
        assert_eq!(rows["rows"].as_array().unwrap().len(), 1);
        assert!(rows["bookmark"].as_str().unwrap() >= requested.as_str());
    }

    #[test]
    fn group_commit_preserves_concurrent_writes_after_crash() {
        let (_dir, runtime) = runtime();
        setup_notes(&runtime);
        let runtime = Arc::new(runtime);
        let mut workers = Vec::new();
        for worker_id in 0..8 {
            let runtime = runtime.clone();
            workers.push(thread::spawn(move || {
                for idx in 0..25 {
                    let title = format!("note-{worker_id}-{idx}");
                    runtime
                        .insert_row(
                            "demo",
                            "notes",
                            json!({"owner_id":"alice","title":title}),
                            &actor("alice"),
                        )
                        .unwrap();
                }
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        runtime.crash_project("demo").unwrap();
        let rows = runtime
            .select_rows("demo", "notes", &HashMap::new(), &actor("alice"), 1000)
            .unwrap();
        assert_eq!(rows["rows"].as_array().unwrap().len(), 200);
    }

    #[test]
    fn wal_segments_survive_past_sqlite_auto_checkpoint_threshold() {
        let (_dir, runtime) = runtime();
        runtime.create_project("demo").unwrap();
        runtime
            .create_table(
                "demo",
                TableSpec {
                    name: "events".to_string(),
                    columns: vec![
                        ColumnSpec {
                            name: "owner_id".to_string(),
                            r#type: "text".to_string(),
                            primary_key: false,
                            auto_increment: true,
                            not_null: true,
                        },
                        ColumnSpec {
                            name: "seq".to_string(),
                            r#type: "integer".to_string(),
                            primary_key: false,
                            auto_increment: true,
                            not_null: true,
                        },
                        ColumnSpec {
                            name: "payload".to_string(),
                            r#type: "text".to_string(),
                            primary_key: false,
                            auto_increment: true,
                            not_null: true,
                        },
                    ],
                },
            )
            .unwrap();
        runtime
            .set_policy(
                "demo",
                PolicySpec {
                    table: "events".to_string(),
                    operation: "all".to_string(),
                    name: Some("owner_only".to_string()),
                    rule: json!({"column": "owner_id", "equals_claim": "sub"}),
                },
            )
            .unwrap();

        let runtime = Arc::new(runtime);
        let mut workers = Vec::new();
        for worker_id in 0..16 {
            let runtime = runtime.clone();
            workers.push(thread::spawn(move || {
                let mut seq = worker_id;
                while seq < 2000 {
                    runtime
                        .insert_row(
                            "demo",
                            "events",
                            json!({
                                "owner_id": "alice",
                                "seq": seq as i64,
                                "payload": format!("payload-{seq}-{}", "x".repeat(512)),
                            }),
                            &actor("alice"),
                        )
                        .unwrap();
                    seq += 16;
                }
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }

        runtime.crash_project("demo").unwrap();
        let mut filters = HashMap::new();
        filters.insert("seq".to_string(), "1999".to_string());
        let rows = runtime
            .select_rows("demo", "events", &filters, &actor("alice"), 100)
            .unwrap();
        assert_eq!(rows["rows"].as_array().unwrap().len(), 1);
        let first_page = runtime
            .select_rows("demo", "events", &HashMap::new(), &actor("alice"), 1000)
            .unwrap();
        assert_eq!(first_page["rows"].as_array().unwrap().len(), 1000);
    }

    #[test]
    fn corrupt_manifest_fails_closed_on_recovery() {
        let (dir, runtime) = runtime();
        setup_notes(&runtime);
        runtime
            .insert_row(
                "demo",
                "notes",
                json!({"owner_id":"alice","title":"durable"}),
                &actor("alice"),
            )
            .unwrap();
        runtime.crash_project("demo").unwrap();
        fs::write(object_store_path(&dir, &manifest_key("demo")), b"not-json").unwrap();

        let err = runtime
            .select_rows("demo", "notes", &HashMap::new(), &actor("alice"), 100)
            .unwrap_err();
        assert_eq!(err.status, 503);
        assert!(err.message.contains("invalid durable manifest"));
    }

    #[test]
    fn missing_manifest_wal_segment_fails_closed_on_recovery() {
        let (dir, runtime) = runtime();
        setup_notes(&runtime);
        runtime
            .insert_row(
                "demo",
                "notes",
                json!({"owner_id":"alice","title":"durable"}),
                &actor("alice"),
            )
            .unwrap();
        let manifest = durable_manifest(&dir);
        let segment = manifest.wal.segments.first().unwrap().clone();
        runtime.crash_project("demo").unwrap();
        fs::remove_file(object_store_path(&dir, &segment.key)).unwrap();

        let err = runtime
            .select_rows("demo", "notes", &HashMap::new(), &actor("alice"), 100)
            .unwrap_err();
        assert_eq!(err.status, 503);
        assert!(err.message.contains("failed to read WAL segment"));
    }

    #[test]
    fn wal_segment_checksum_mismatch_fails_closed_on_recovery() {
        let (dir, runtime) = runtime();
        setup_notes(&runtime);
        runtime
            .insert_row(
                "demo",
                "notes",
                json!({"owner_id":"alice","title":"durable"}),
                &actor("alice"),
            )
            .unwrap();
        let manifest = durable_manifest(&dir);
        let segment = manifest.wal.segments.first().unwrap().clone();
        runtime.crash_project("demo").unwrap();
        let path = object_store_path(&dir, &segment.key);
        let mut bytes = fs::read(&path).unwrap();
        bytes[0] ^= 0xff;
        fs::write(path, bytes).unwrap();

        let err = runtime
            .select_rows("demo", "notes", &HashMap::new(), &actor("alice"), 100)
            .unwrap_err();
        assert_eq!(err.status, 503);
        assert!(err.message.contains("checksum mismatch"));
    }

    #[test]
    fn manifest_put_failure_is_not_acknowledged_and_recovers_previous_manifest() {
        let (_dir, runtime, store) = fault_runtime(1000);
        setup_notes(&runtime);
        runtime
            .insert_row(
                "demo",
                "notes",
                json!({"owner_id":"alice","title":"durable"}),
                &actor("alice"),
            )
            .unwrap();

        store.fail_next_put_containing("manifest.json");
        let err = runtime
            .insert_row(
                "demo",
                "notes",
                json!({"owner_id":"alice","title":"not-durable"}),
                &actor("alice"),
            )
            .unwrap_err();
        assert_eq!(err.status, 500);
        assert!(err.message.contains("injected object-store put failure"));

        runtime.crash_project("demo").unwrap();
        assert_eq!(select_note_titles(&runtime, "durable"), 1);
        assert_eq!(select_note_titles(&runtime, "not-durable"), 0);
    }

    #[test]
    fn wal_segment_put_failure_is_not_acknowledged_or_recovered() {
        let (_dir, runtime, store) = fault_runtime(1000);
        setup_notes(&runtime);
        runtime
            .insert_row(
                "demo",
                "notes",
                json!({"owner_id":"alice","title":"durable"}),
                &actor("alice"),
            )
            .unwrap();

        store.fail_next_put_containing("wal/");
        let err = runtime
            .insert_row(
                "demo",
                "notes",
                json!({"owner_id":"alice","title":"not-durable"}),
                &actor("alice"),
            )
            .unwrap_err();
        assert_eq!(err.status, 500);
        assert!(err.message.contains("injected object-store put failure"));

        runtime.crash_project("demo").unwrap();
        assert_eq!(select_note_titles(&runtime, "durable"), 1);
        assert_eq!(select_note_titles(&runtime, "not-durable"), 0);
    }

    #[test]
    fn snapshot_put_failure_keeps_wal_chain_and_future_writes_recoverable() {
        let (_dir, runtime, store) = fault_runtime(3);
        setup_notes(&runtime);

        store.fail_next_put_containing("snapshots/");
        runtime
            .insert_row(
                "demo",
                "notes",
                json!({"owner_id":"alice","title":"survives-snapshot-failure"}),
                &actor("alice"),
            )
            .unwrap();
        runtime
            .insert_row(
                "demo",
                "notes",
                json!({"owner_id":"alice","title":"after-snapshot-failure"}),
                &actor("alice"),
            )
            .unwrap();

        runtime.crash_project("demo").unwrap();
        assert_eq!(select_note_titles(&runtime, "survives-snapshot-failure"), 1);
        assert_eq!(select_note_titles(&runtime, "after-snapshot-failure"), 1);
    }

    #[test]
    fn writer_queue_applies_backpressure_when_full() {
        let (_dir, runtime, store) = fault_runtime_with_queue_capacity(1000, 1);
        setup_notes(&runtime);
        store.delay_puts_by_ms(150);

        let runtime = Arc::new(runtime);
        let start = Arc::new(Barrier::new(33));
        let mut workers = Vec::new();
        for idx in 0..32 {
            let runtime = runtime.clone();
            let start = start.clone();
            workers.push(thread::spawn(move || {
                start.wait();
                runtime.insert_row(
                    "demo",
                    "notes",
                    json!({"owner_id":"alice","title":format!("queued-{idx}")}),
                    &actor("alice"),
                )
            }));
        }
        start.wait();

        let mut accepted = 0;
        let mut rejected = 0;
        for worker in workers {
            match worker.join().unwrap() {
                Ok(_) => accepted += 1,
                Err(err) if err.status == 429 => rejected += 1,
                Err(err) => panic!("unexpected write error: {err:?}"),
            }
        }
        assert!(accepted > 0, "at least one write should enter the queue");
        assert!(
            rejected > 0,
            "bounded queue should reject some concurrent writes"
        );
    }

    #[test]
    fn wal_budget_triggers_compaction_before_accepting_more_writes() {
        let (_dir, runtime, _store) = fault_runtime_with_limits(1000, 16, 1);
        runtime.create_project("demo").unwrap();
        runtime
            .create_table(
                "demo",
                TableSpec {
                    name: "notes".to_string(),
                    columns: vec![ColumnSpec {
                        name: "title".to_string(),
                        r#type: "text".to_string(),
                        primary_key: false,
                        auto_increment: true,
                        not_null: true,
                    }],
                },
            )
            .unwrap();
        runtime
            .create_table(
                "demo",
                TableSpec {
                    name: "more_notes".to_string(),
                    columns: vec![ColumnSpec {
                        name: "title".to_string(),
                        r#type: "text".to_string(),
                        primary_key: false,
                        auto_increment: true,
                        not_null: true,
                    }],
                },
            )
            .unwrap();
        let info = runtime.project_info("demo").unwrap();
        assert_eq!(info["manifest"]["generation"], 2);
        assert!(info["manifest"]["wal"]["durable_bytes"].as_u64().unwrap() > 0);
        let schema = runtime.schema("demo").unwrap();
        let tables = schema["tables"]
            .as_array()
            .unwrap()
            .iter()
            .map(|table| table["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert!(tables.contains(&"notes"));
        assert!(tables.contains(&"more_notes"));
    }

    #[test]
    fn wal_budget_rejects_writes_when_compaction_fails() {
        let (_dir, runtime, store) = fault_runtime_with_limits(1000, 16, 1);
        runtime.create_project("demo").unwrap();
        runtime
            .create_table(
                "demo",
                TableSpec {
                    name: "notes".to_string(),
                    columns: vec![ColumnSpec {
                        name: "title".to_string(),
                        r#type: "text".to_string(),
                        primary_key: false,
                        auto_increment: true,
                        not_null: true,
                    }],
                },
            )
            .unwrap();

        store.fail_next_put_containing("snapshots/");
        let err = runtime
            .create_table(
                "demo",
                TableSpec {
                    name: "rejected_notes".to_string(),
                    columns: vec![ColumnSpec {
                        name: "title".to_string(),
                        r#type: "text".to_string(),
                        primary_key: false,
                        auto_increment: true,
                        not_null: true,
                    }],
                },
            )
            .unwrap_err();
        assert_eq!(err.status, 507);
        assert!(err.message.contains("durable WAL budget exceeded"));

        runtime.crash_project("demo").unwrap();
        let schema = runtime.schema("demo").unwrap();
        let tables = schema["tables"]
            .as_array()
            .unwrap()
            .iter()
            .map(|table| table["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert!(tables.contains(&"notes"));
        assert!(!tables.contains(&"rejected_notes"));
    }
}
