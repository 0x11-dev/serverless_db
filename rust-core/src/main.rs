use clap::Parser;
use serverless_db_core::http::app;
use serverless_db_core::object_store::LocalObjectStore;
use serverless_db_core::runtime::{ProjectRuntime, RoutingEndpoint, RuntimeOptions};
use std::net::SocketAddr;
use std::sync::Arc;

#[derive(Debug, Parser)]
struct Args {
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    #[arg(long, default_value_t = 8765)]
    port: u16,
    #[arg(long, default_value = ".runtime-rust")]
    runtime_dir: String,
    #[arg(long, default_value = "local")]
    object_store: String,
    #[arg(long)]
    s3_bucket: Option<String>,
    #[arg(long, default_value = "serverless-db")]
    s3_prefix: String,
    #[arg(long)]
    s3_endpoint: Option<String>,
    #[arg(long)]
    s3_region: Option<String>,
    #[arg(long, default_value_t = false)]
    s3_force_path_style: bool,
    #[arg(long, default_value_t = 1000)]
    snapshot_every_ops: u64,
    #[arg(long, default_value_t = 60_000)]
    snapshot_every_ms: u64,
    #[arg(long, default_value_t = 100)]
    metadata_every_ops: u64,
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
    #[arg(long, default_value_t = false)]
    read_replica: bool,
    #[arg(long, default_value_t = 1_000)]
    replica_refresh_interval_ms: u64,
    #[arg(long, default_value_t = 5_000)]
    replica_bookmark_wait_timeout_ms: u64,
    #[arg(long)]
    primary_url: Option<String>,
    #[arg(long)]
    routing_region: Option<String>,
    #[arg(long = "routing-endpoint")]
    routing_endpoints: Vec<String>,
    #[arg(long, default_value_t = 1_000)]
    forward_connect_timeout_ms: u64,
    #[arg(long, default_value_t = 5_000)]
    forward_request_timeout_ms: u64,
    #[arg(long, default_value_t = 3)]
    forward_max_attempts: usize,
    #[arg(long, default_value_t = 25)]
    forward_retry_backoff_ms: u64,
    #[arg(long, default_value_t = 2)]
    routing_endpoint_failure_threshold: u32,
    #[arg(long, default_value_t = 1_000)]
    routing_endpoint_cooldown_ms: u64,
    #[arg(long, default_value = "NORMAL")]
    sqlite_synchronous: String,
    #[arg(long, default_value = "demo")]
    supabase_project_id: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let options = RuntimeOptions {
        snapshot_every_ops: args.snapshot_every_ops,
        snapshot_every_ms: args.snapshot_every_ms,
        metadata_every_ops: args.metadata_every_ops,
        group_commit_max_ops: args.group_commit_max_ops,
        group_commit_delay_ms: args.group_commit_delay_ms,
        writer_queue_capacity: args.writer_queue_capacity,
        max_durable_wal_bytes: args.max_durable_wal_bytes,
        writer_lease_ttl_ms: args.writer_lease_ttl_ms,
        read_replica: args.read_replica,
        replica_refresh_interval_ms: args.replica_refresh_interval_ms,
        replica_bookmark_wait_timeout_ms: args.replica_bookmark_wait_timeout_ms,
        primary_url: args.primary_url.clone(),
        routing_region: args.routing_region.clone(),
        routing_endpoints: parse_routing_endpoints(&args.routing_endpoints)?,
        forward_connect_timeout_ms: args.forward_connect_timeout_ms,
        forward_request_timeout_ms: args.forward_request_timeout_ms,
        forward_max_attempts: args.forward_max_attempts,
        forward_retry_backoff_ms: args.forward_retry_backoff_ms,
        routing_endpoint_failure_threshold: args.routing_endpoint_failure_threshold,
        routing_endpoint_cooldown_ms: args.routing_endpoint_cooldown_ms,
        sqlite_synchronous: args.sqlite_synchronous.clone(),
        supabase_project_id: args.supabase_project_id.clone(),
    };
    let runtime = if args.object_store == "local" {
        ProjectRuntime::with_object_store(
            &args.runtime_dir,
            options,
            Arc::new(LocalObjectStore::new(format!(
                "{}/object_store",
                args.runtime_dir
            ))?),
        )?
    } else if args.object_store == "s3" {
        build_s3_runtime(&args, options)?
    } else {
        anyhow::bail!("--object-store must be local or s3");
    };
    let app = app(runtime);
    let addr: SocketAddr = format!("{}:{}", args.host, args.port).parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!(
        "Rust Serverless DB core listening on http://{}",
        listener.local_addr()?
    );
    axum::serve(listener, app).await?;
    Ok(())
}

fn parse_routing_endpoints(values: &[String]) -> anyhow::Result<Vec<RoutingEndpoint>> {
    values
        .iter()
        .map(|value| {
            let parts = value.splitn(3, ',').collect::<Vec<_>>();
            if parts.len() != 3 {
                anyhow::bail!(
                    "--routing-endpoint must use role,region,url, for example primary,wnam,http://127.0.0.1:8765"
                );
            }
            let role = parts[0].trim();
            if !["primary", "replica"].contains(&role) {
                anyhow::bail!("routing endpoint role must be primary or replica");
            }
            let region = parts[1].trim();
            let url = parts[2].trim();
            if url.is_empty() {
                anyhow::bail!("routing endpoint url is required");
            }
            Ok(RoutingEndpoint {
                role: role.to_string(),
                region: if region.is_empty() || region == "-" {
                    None
                } else {
                    Some(region.to_string())
                },
                url: url.to_string(),
            })
        })
        .collect()
}

#[cfg(feature = "s3")]
fn build_s3_runtime(args: &Args, options: RuntimeOptions) -> anyhow::Result<ProjectRuntime> {
    use serverless_db_core::object_store::s3::{S3ObjectStore, S3ObjectStoreConfig};
    let bucket = args
        .s3_bucket
        .clone()
        .ok_or_else(|| anyhow::anyhow!("--s3-bucket is required when --object-store=s3"))?;
    ProjectRuntime::with_object_store(
        &args.runtime_dir,
        options,
        Arc::new(S3ObjectStore::new(S3ObjectStoreConfig {
            bucket,
            prefix: args.s3_prefix.clone(),
            endpoint: args.s3_endpoint.clone(),
            region: args.s3_region.clone(),
            force_path_style: args.s3_force_path_style,
        })?),
    )
}

#[cfg(not(feature = "s3"))]
fn build_s3_runtime(_args: &Args, _options: RuntimeOptions) -> anyhow::Result<ProjectRuntime> {
    anyhow::bail!("S3 object store requires building with --features s3")
}
