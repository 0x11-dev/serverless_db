use clap::Parser;
use serde_json::{Map, Value, json};
use serverless_db_core::auth::Actor;
use serverless_db_core::runtime::{
    ColumnSpec, PolicySpec, ProjectRuntime, RuntimeOptions, TableSpec,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::thread;
use std::time::Instant;

#[derive(Debug, Parser)]
struct Args {
    #[arg(long, default_value_t = 2000)]
    rows: usize,
    #[arg(long, default_value_t = 1000)]
    snapshot_every_ops: u64,
    #[arg(long, default_value_t = 100)]
    metadata_every_ops: u64,
    #[arg(long, default_value_t = 16)]
    concurrency: usize,
    #[arg(long, default_value_t = 64)]
    group_commit_max_ops: usize,
    #[arg(long, default_value_t = 2)]
    group_commit_delay_ms: u64,
    #[arg(long, default_value_t = 1024)]
    writer_queue_capacity: usize,
    #[arg(long, default_value_t = 64 * 1024 * 1024)]
    max_durable_wal_bytes: u64,
    #[arg(long, default_value_t = 30_000)]
    writer_lease_ttl_ms: u64,
    #[arg(long, default_value_t = 96)]
    payload_bytes: usize,
    #[arg(long)]
    runtime_dir: Option<String>,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let temp_dir;
    let runtime_dir = match &args.runtime_dir {
        Some(path) => path.clone(),
        None => {
            temp_dir = tempfile::tempdir()?;
            temp_dir
                .path()
                .join("runtime")
                .to_string_lossy()
                .to_string()
        }
    };
    let runtime = ProjectRuntime::new(
        runtime_dir,
        RuntimeOptions {
            snapshot_every_ops: args.snapshot_every_ops,
            snapshot_every_ms: 300_000,
            metadata_every_ops: args.metadata_every_ops,
            group_commit_max_ops: args.group_commit_max_ops,
            group_commit_delay_ms: args.group_commit_delay_ms,
            writer_queue_capacity: args.writer_queue_capacity,
            max_durable_wal_bytes: args.max_durable_wal_bytes,
            writer_lease_ttl_ms: args.writer_lease_ttl_ms,
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
    )?;
    let runtime = Arc::new(runtime);
    let actor = Actor {
        sub: Some("bench-user".to_string()),
        role: "authenticated".to_string(),
        claims: Map::new(),
    };
    runtime.create_project("bench")?;
    runtime.create_table(
        "bench",
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
        "bench",
        PolicySpec {
            table: "events".to_string(),
            operation: "all".to_string(),
            name: Some("owner_only".to_string()),
            rule: json!({"column": "owner_id", "equals_claim": "sub"}),
        },
    )?;

    let start = Instant::now();
    let mut workers = Vec::new();
    for worker_idx in 0..args.concurrency.max(1) {
        let runtime = runtime.clone();
        let actor = actor.clone();
        let rows = args.rows;
        let concurrency = args.concurrency.max(1);
        let payload_bytes = args.payload_bytes;
        workers.push(thread::spawn(move || -> Result<Vec<f64>, String> {
            let mut latencies = Vec::new();
            let mut idx = worker_idx;
            while idx < rows {
                let before = Instant::now();
                runtime
                    .insert_row(
                        "bench",
                        "events",
                        json!({"owner_id":"bench-user","seq":idx as i64,"payload": format!("payload-{idx}-{}", "x".repeat(payload_bytes))}),
                        &actor,
                    )
                    .map_err(|err| err.to_string())?;
                latencies.push(before.elapsed().as_secs_f64() * 1000.0);
                idx += concurrency;
            }
            Ok(latencies)
        }));
    }
    let mut latencies = Vec::with_capacity(args.rows);
    for worker in workers {
        latencies.extend(worker.join().unwrap().map_err(anyhow::Error::msg)?);
    }
    let insert_elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;

    let read_start = Instant::now();
    let mut read_latencies = Vec::with_capacity(args.rows.min(1000));
    for idx in 0..args.rows.min(1000) {
        let mut filters = HashMap::new();
        filters.insert("seq".to_string(), idx.to_string());
        let before = Instant::now();
        runtime.select_rows("bench", "events", &filters, &actor, 100)?;
        read_latencies.push(before.elapsed().as_secs_f64() * 1000.0);
    }
    let read_elapsed_ms = read_start.elapsed().as_secs_f64() * 1000.0;

    let recovery_start = Instant::now();
    runtime.crash_project("bench")?;
    let mut filters = HashMap::new();
    filters.insert("seq".to_string(), (args.rows - 1).to_string());
    let recovered = runtime.select_rows("bench", "events", &filters, &actor, 100)?;
    assert_eq!(recovered["rows"].as_array().unwrap().len(), 1);
    let recovery_ms = recovery_start.elapsed().as_secs_f64() * 1000.0;
    let object_store = summarize_object_store(runtime.project_info("bench")?);

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "language": "rust",
            "rows": args.rows,
            "concurrency": args.concurrency,
            "snapshot_every_ops": args.snapshot_every_ops,
            "metadata_every_ops": args.metadata_every_ops,
            "group_commit_max_ops": args.group_commit_max_ops,
            "group_commit_delay_ms": args.group_commit_delay_ms,
            "writer_queue_capacity": args.writer_queue_capacity,
            "max_durable_wal_bytes": args.max_durable_wal_bytes,
            "writer_lease_ttl_ms": args.writer_lease_ttl_ms,
            "payload_bytes": args.payload_bytes,
            "insert": summarize(&latencies, insert_elapsed_ms),
            "point_read": summarize(&read_latencies, read_elapsed_ms),
            "crash_recovery_ms": round(recovery_ms),
            "object_store": object_store
        }))?
    );
    Ok(())
}

fn summarize_object_store(mut value: Value) -> Value {
    let Some(manifest) = value.get_mut("manifest").and_then(Value::as_object_mut) else {
        return value;
    };
    let Some(wal) = manifest.get_mut("wal").and_then(Value::as_object_mut) else {
        return value;
    };
    let Some(segments) = wal.remove("segments").and_then(|value| match value {
        Value::Array(items) => Some(items),
        _ => None,
    }) else {
        return value;
    };
    let first = segments.first().cloned().unwrap_or(Value::Null);
    let last = segments.last().cloned().unwrap_or(Value::Null);
    wal.insert("segment_count".to_string(), json!(segments.len()));
    wal.insert("first_segment".to_string(), first);
    wal.insert("last_segment".to_string(), last);
    value
}

fn summarize(values: &[f64], elapsed_ms: f64) -> serde_json::Value {
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    json!({
        "count": sorted.len(),
        "throughput_per_sec": round(sorted.len() as f64 / (elapsed_ms / 1000.0)),
        "elapsed_ms": round(elapsed_ms),
        "p50_ms": percentile(&sorted, 0.50),
        "p95_ms": percentile(&sorted, 0.95),
        "p99_ms": percentile(&sorted, 0.99),
        "max_ms": round(*sorted.last().unwrap_or(&0.0))
    })
}

fn percentile(sorted: &[f64], pct: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() as f64 * pct).ceil() as usize)
        .saturating_sub(1)
        .min(sorted.len() - 1);
    round(sorted[idx])
}

fn round(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}
