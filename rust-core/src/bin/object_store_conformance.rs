use clap::Parser;
use serde_json::{Map, json};
use serverless_db_core::auth::Actor;
use serverless_db_core::object_store::{LocalObjectStore, ObjectStoreRef};
use serverless_db_core::runtime::{
    ColumnSpec, PolicySpec, ProjectRuntime, RuntimeOptions, TableSpec,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Parser)]
struct Args {
    #[arg(long, default_value = "local")]
    object_store: String,
    #[arg(long, default_value = ".runtime-conformance")]
    runtime_dir: String,
    #[arg(long)]
    namespace: Option<String>,
    #[arg(long)]
    s3_bucket: Option<String>,
    #[arg(long, default_value = "serverless-db-conformance")]
    s3_prefix: String,
    #[arg(long)]
    s3_endpoint: Option<String>,
    #[arg(long)]
    s3_region: Option<String>,
    #[arg(long, default_value_t = false)]
    s3_force_path_style: bool,
    #[arg(long, default_value_t = 256)]
    rows: usize,
    #[arg(long, default_value_t = false)]
    keep_objects: bool,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let namespace = args
        .namespace
        .clone()
        .unwrap_or_else(|| format!("run-{}-{}", std::process::id(), now_ms()));
    let store = build_store(&args, &namespace)?;
    let object_store_report = run_object_store_checks(&store)?;
    let runtime_report = run_runtime_smoke(&args, store.clone(), &namespace)?;

    if !args.keep_objects {
        store.delete_prefix("raw").ok();
        if let Some(project_id) = runtime_report
            .get("project_id")
            .and_then(|value| value.as_str())
        {
            store.delete_prefix(&format!("projects/{project_id}")).ok();
        }
    }

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "object_store": args.object_store,
            "namespace": namespace,
            "checks": object_store_report,
            "runtime": runtime_report
        }))?
    );
    Ok(())
}

fn build_store(args: &Args, namespace: &str) -> anyhow::Result<ObjectStoreRef> {
    if args.object_store == "local" {
        let base = PathBuf::from(&args.runtime_dir)
            .join("object_store")
            .join(namespace);
        return Ok(Arc::new(LocalObjectStore::new(base)?));
    }
    if args.object_store == "s3" {
        return build_s3_store(args, namespace);
    }
    anyhow::bail!("--object-store must be local or s3")
}

#[cfg(feature = "s3")]
fn build_s3_store(args: &Args, namespace: &str) -> anyhow::Result<ObjectStoreRef> {
    use serverless_db_core::object_store::s3::{S3ObjectStore, S3ObjectStoreConfig};
    let bucket = args
        .s3_bucket
        .clone()
        .ok_or_else(|| anyhow::anyhow!("--s3-bucket is required when --object-store=s3"))?;
    Ok(Arc::new(S3ObjectStore::new(S3ObjectStoreConfig {
        bucket,
        prefix: join_prefix(&args.s3_prefix, namespace),
        endpoint: args.s3_endpoint.clone(),
        region: args.s3_region.clone(),
        force_path_style: args.s3_force_path_style,
    })?))
}

#[cfg(not(feature = "s3"))]
fn build_s3_store(_args: &Args, _namespace: &str) -> anyhow::Result<ObjectStoreRef> {
    anyhow::bail!("S3 object store requires building with --features s3")
}

fn run_object_store_checks(store: &ObjectStoreRef) -> anyhow::Result<serde_json::Value> {
    store.delete_prefix("raw").ok();
    store.put_bytes("raw/list/0001.txt", b"one")?;
    store.put_bytes("raw/list/0002.txt", b"two")?;
    store.put_bytes("raw/list/0002.txt", b"two-overwritten")?;
    let read_back = store.read_bytes("raw/list/0002.txt")?;
    anyhow::ensure!(read_back == b"two-overwritten", "overwrite/read mismatch");
    anyhow::ensure!(store.exists("raw/list/0001.txt")?, "exists returned false");
    anyhow::ensure!(
        store.len("raw/list/0002.txt")? == Some("two-overwritten".len() as u64),
        "len mismatch"
    );

    let temp = tempfile::tempdir()?;
    let file_path = temp.path().join("put-file.txt");
    std::fs::write(&file_path, b"from-file")?;
    store.put_file("raw/files/put-file.txt", &file_path)?;
    anyhow::ensure!(
        store.read_bytes("raw/files/put-file.txt")? == b"from-file",
        "put_file/read mismatch"
    );

    let listed = store.list_prefix("raw/list")?;
    let listed_keys = listed
        .iter()
        .map(|item| item.key.clone())
        .collect::<Vec<_>>();
    anyhow::ensure!(
        listed_keys == vec!["raw/list/0001.txt", "raw/list/0002.txt"],
        "list_prefix ordering mismatch: {listed_keys:?}"
    );

    store.delete("raw/list/0001.txt")?;
    anyhow::ensure!(
        !store.exists("raw/list/0001.txt")?,
        "delete did not remove object"
    );
    store.delete_prefix("raw")?;
    anyhow::ensure!(
        store.list_prefix("raw")?.is_empty(),
        "delete_prefix left objects behind"
    );

    Ok(json!({
        "put_bytes": true,
        "overwrite": true,
        "put_file": true,
        "list_prefix_sorted": true,
        "delete": true,
        "delete_prefix": true
    }))
}

fn run_runtime_smoke(
    args: &Args,
    store: ObjectStoreRef,
    namespace: &str,
) -> anyhow::Result<serde_json::Value> {
    let runtime_dir = Path::new(&args.runtime_dir).join("runtime").join(namespace);
    let runtime = ProjectRuntime::with_object_store(
        runtime_dir,
        RuntimeOptions {
            snapshot_every_ops: 128,
            snapshot_every_ms: 300_000,
            metadata_every_ops: 32,
            group_commit_max_ops: 32,
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
        store,
    )?;
    let project_id = format!("conf-{}", std::process::id());
    let actor = Actor {
        sub: Some("alice".to_string()),
        role: "authenticated".to_string(),
        claims: Map::new(),
    };
    runtime.create_project(&project_id)?;
    runtime.create_table(
        &project_id,
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
    )?;
    runtime.set_policy(
        &project_id,
        PolicySpec {
            table: "events".to_string(),
            operation: "all".to_string(),
            name: Some("owner_only".to_string()),
            rule: json!({"column": "owner_id", "equals_claim": "sub"}),
        },
    )?;
    for seq in 0..args.rows {
        runtime.insert_row(
            &project_id,
            "events",
            json!({
                "owner_id": "alice",
                "seq": seq as i64,
                "payload": format!("payload-{seq}-{}", "x".repeat(128)),
            }),
            &actor,
        )?;
    }
    runtime.crash_project(&project_id)?;
    let mut filters = HashMap::new();
    filters.insert("seq".to_string(), (args.rows - 1).to_string());
    let recovered = runtime.select_rows(&project_id, "events", &filters, &actor, 100)?;
    anyhow::ensure!(
        recovered["rows"].as_array().map(Vec::len) == Some(1),
        "runtime crash recovery did not recover last row"
    );
    let info = runtime.project_info(&project_id)?;
    runtime.crash_project(&project_id).ok();
    Ok(json!({
        "project_id": project_id,
        "rows": args.rows,
        "crash_recovery_last_row": true,
        "object_store": info
    }))
}

#[cfg(feature = "s3")]
fn join_prefix(prefix: &str, namespace: &str) -> String {
    let prefix = prefix.trim_matches('/');
    if prefix.is_empty() {
        namespace.to_string()
    } else {
        format!("{prefix}/{namespace}")
    }
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
