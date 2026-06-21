use clap::Parser;
use serde_json::{Map, Value, json};
use serverless_db_core::auth::Actor;
use serverless_db_core::object_store::{LocalObjectStore, ObjectMeta, ObjectStore, ObjectStoreRef};
use serverless_db_core::runtime::{
    ColumnSpec, PolicySpec, ProjectRuntime, RuntimeOptions, TableSpec,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

const CRASH_EXIT_CODE: i32 = 199;
const PROJECT_ID: &str = "crashproj";
const TABLE: &str = "events";
const CRASH_SEQ: i64 = 1;

#[derive(Debug, Parser)]
struct Args {
    #[arg(long, default_value = ".runtime-crash-matrix")]
    runtime_dir: PathBuf,
    #[arg(long, hide = true, default_value_t = false)]
    child: bool,
    #[arg(long, hide = true)]
    scenario: Option<String>,
}

#[derive(Debug, Clone)]
struct Scenario {
    name: &'static str,
    trigger: CrashTrigger,
    crash_on_match: usize,
    force_snapshot: bool,
    child_action: ChildAction,
    expected_row_present: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CrashTrigger {
    WalPut,
    ManifestPut,
    SnapshotPut,
    WalDeletePrefix,
    RuntimeStage(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildAction {
    Insert,
    InsertThenCrashProject,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("Error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> anyhow::Result<()> {
    let args = Args::parse();
    if args.child {
        let scenario_name = args
            .scenario
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("--scenario is required in child mode"))?;
        let scenario = scenarios()
            .into_iter()
            .find(|item| item.name == scenario_name)
            .ok_or_else(|| anyhow::anyhow!("unknown scenario: {scenario_name}"))?;
        run_child(&args.runtime_dir, &scenario)?;
        anyhow::bail!(
            "child scenario returned without crashing: {}",
            scenario.name
        );
    }

    run_parent(args.runtime_dir)
}

fn run_parent(runtime_dir: PathBuf) -> anyhow::Result<()> {
    let mut reports = Vec::new();
    let exe = std::env::current_exe()?;
    for scenario in scenarios() {
        let scenario_dir = runtime_dir.join(format!("{}-{}", scenario.name, now_ms()));
        std::fs::remove_dir_all(&scenario_dir).ok();
        prepare_base(&scenario_dir)?;

        let mut command = Command::new(&exe);
        command
            .arg("--child")
            .arg("--runtime-dir")
            .arg(&scenario_dir)
            .arg("--scenario")
            .arg(scenario.name);
        if let CrashTrigger::RuntimeStage(stage) = scenario.trigger {
            command
                .env("SDB_INTERNAL_CRASH_AFTER_STAGE", stage)
                .env("SDB_INTERNAL_CRASH_EXIT_CODE", CRASH_EXIT_CODE.to_string());
        }
        let status = command.status()?;
        anyhow::ensure!(
            status.code() == Some(CRASH_EXIT_CODE),
            "scenario {} exited with {status}; expected code {CRASH_EXIT_CODE}",
            scenario.name
        );

        let verification = verify_recovery(&scenario_dir, scenario.expected_row_present)?;
        reports.push(json!({
            "scenario": scenario.name,
            "trigger": format!("{:?}", scenario.trigger),
            "crash_on_match": scenario.crash_on_match,
            "force_snapshot": scenario.force_snapshot,
            "child_action": format!("{:?}", scenario.child_action),
            "child_exit_code": CRASH_EXIT_CODE,
            "expected_row_present": scenario.expected_row_present,
            "recovered_row_present": verification.recovered_row_present,
            "manifest_generation": verification.manifest_generation,
            "manifest_wal_segments": verification.manifest_wal_segments,
            "listed_wal_segments": verification.listed_wal_segments,
        }));
    }

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "checks": {
                "all_scenarios_passed": true,
                "process_exit_code": CRASH_EXIT_CODE
            },
            "scenarios": reports
        }))?
    );
    Ok(())
}

fn scenarios() -> Vec<Scenario> {
    vec![
        Scenario {
            name: "sqlite_commit_before_durable_wal",
            trigger: CrashTrigger::RuntimeStage("after_sqlite_commit_before_durable_wal"),
            crash_on_match: 1,
            force_snapshot: false,
            child_action: ChildAction::Insert,
            expected_row_present: false,
        },
        Scenario {
            name: "wal_segment_after_put",
            trigger: CrashTrigger::WalPut,
            crash_on_match: 1,
            force_snapshot: false,
            child_action: ChildAction::Insert,
            expected_row_present: false,
        },
        Scenario {
            name: "wal_manifest_after_put",
            trigger: CrashTrigger::ManifestPut,
            crash_on_match: 1,
            force_snapshot: false,
            child_action: ChildAction::Insert,
            expected_row_present: true,
        },
        Scenario {
            name: "snapshot_after_upload",
            trigger: CrashTrigger::SnapshotPut,
            crash_on_match: 1,
            force_snapshot: true,
            child_action: ChildAction::Insert,
            expected_row_present: true,
        },
        Scenario {
            name: "snapshot_manifest_after_put",
            trigger: CrashTrigger::ManifestPut,
            crash_on_match: 2,
            force_snapshot: true,
            child_action: ChildAction::Insert,
            expected_row_present: true,
        },
        Scenario {
            name: "snapshot_manifest_before_cache_replace",
            trigger: CrashTrigger::RuntimeStage("after_snapshot_manifest_before_cache_replace"),
            crash_on_match: 1,
            force_snapshot: true,
            child_action: ChildAction::Insert,
            expected_row_present: true,
        },
        Scenario {
            name: "snapshot_cache_replace_before_reopen",
            trigger: CrashTrigger::RuntimeStage("after_snapshot_cache_replace_before_reopen"),
            crash_on_match: 1,
            force_snapshot: true,
            child_action: ChildAction::Insert,
            expected_row_present: true,
        },
        Scenario {
            name: "wal_delete_after_snapshot",
            trigger: CrashTrigger::WalDeletePrefix,
            crash_on_match: 1,
            force_snapshot: true,
            child_action: ChildAction::Insert,
            expected_row_present: true,
        },
        Scenario {
            name: "project_cache_delete_after_durable_write",
            trigger: CrashTrigger::RuntimeStage("after_project_cache_delete"),
            crash_on_match: 1,
            force_snapshot: false,
            child_action: ChildAction::InsertThenCrashProject,
            expected_row_present: true,
        },
    ]
}

fn prepare_base(runtime_dir: &Path) -> anyhow::Result<()> {
    let runtime = ProjectRuntime::new(runtime_dir, stable_options(false))?;
    let actor = actor();
    runtime.create_project(PROJECT_ID)?;
    runtime.create_table(
        PROJECT_ID,
        TableSpec {
            name: TABLE.to_string(),
            columns: event_columns(),
        },
    )?;
    runtime.set_policy(
        PROJECT_ID,
        PolicySpec {
            table: TABLE.to_string(),
            operation: "all".to_string(),
            name: Some("owner_only".to_string()),
            rule: json!({"column": "owner_id", "equals_claim": "sub"}),
        },
    )?;
    runtime.insert_row(PROJECT_ID, TABLE, event_row(0), &actor)?;
    runtime.crash_project(PROJECT_ID)?;
    Ok(())
}

fn run_child(runtime_dir: &Path, scenario: &Scenario) -> anyhow::Result<()> {
    let local = LocalObjectStore::new(runtime_dir.join("object_store"))?;
    let store: ObjectStoreRef = Arc::new(CrashingObjectStore {
        inner: local,
        scenario: scenario.clone(),
        matched: Mutex::new(0),
    });
    let runtime = ProjectRuntime::with_object_store(
        runtime_dir,
        stable_options(scenario.force_snapshot),
        store,
    )?;
    runtime.insert_row(PROJECT_ID, TABLE, event_row(CRASH_SEQ), &actor())?;
    if scenario.child_action == ChildAction::InsertThenCrashProject {
        runtime.crash_project(PROJECT_ID)?;
    }
    Ok(())
}

#[derive(Debug)]
struct RecoveryVerification {
    recovered_row_present: bool,
    manifest_generation: Option<u64>,
    manifest_wal_segments: Option<usize>,
    listed_wal_segments: usize,
}

fn verify_recovery(
    runtime_dir: &Path,
    expected_row_present: bool,
) -> anyhow::Result<RecoveryVerification> {
    let runtime = ProjectRuntime::new(runtime_dir, stable_options(false))?;
    runtime.crash_project(PROJECT_ID)?;
    let mut filters = HashMap::new();
    filters.insert("seq".to_string(), CRASH_SEQ.to_string());
    let rows = runtime.select_rows(PROJECT_ID, TABLE, &filters, &actor(), 10)?;
    let recovered_row_present = rows["rows"].as_array().is_some_and(|rows| !rows.is_empty());
    anyhow::ensure!(
        recovered_row_present == expected_row_present,
        "recovered row expectation mismatch: expected {expected_row_present}, got {recovered_row_present}"
    );
    let info = runtime.project_info(PROJECT_ID)?;
    let manifest = &info["manifest"];
    Ok(RecoveryVerification {
        recovered_row_present,
        manifest_generation: manifest["generation"].as_u64(),
        manifest_wal_segments: manifest["wal"]["segments"].as_array().map(Vec::len),
        listed_wal_segments: info["wal_segment_count"].as_u64().unwrap_or_default() as usize,
    })
}

fn stable_options(force_snapshot: bool) -> RuntimeOptions {
    RuntimeOptions {
        snapshot_every_ops: if force_snapshot { 1 } else { 10_000 },
        snapshot_every_ms: 0,
        metadata_every_ops: 0,
        group_commit_max_ops: 1,
        group_commit_delay_ms: 0,
        writer_queue_capacity: 16,
        max_durable_wal_bytes: 64 * 1024 * 1024,
        writer_lease_ttl_ms: 0,
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

fn actor() -> Actor {
    Actor {
        sub: Some("alice".to_string()),
        role: "authenticated".to_string(),
        claims: Map::new(),
    }
}

fn event_columns() -> Vec<ColumnSpec> {
    vec![
        ColumnSpec {
            name: "owner_id".to_string(),
            r#type: "text".to_string(),
            primary_key: false,
            auto_increment: false,
            not_null: true,
        },
        ColumnSpec {
            name: "seq".to_string(),
            r#type: "integer".to_string(),
            primary_key: false,
            auto_increment: false,
            not_null: true,
        },
        ColumnSpec {
            name: "payload".to_string(),
            r#type: "text".to_string(),
            primary_key: false,
            auto_increment: false,
            not_null: true,
        },
    ]
}

fn event_row(seq: i64) -> Value {
    json!({
        "owner_id": "alice",
        "seq": seq,
        "payload": format!("payload-{seq}")
    })
}

struct CrashingObjectStore {
    inner: LocalObjectStore,
    scenario: Scenario,
    matched: Mutex<usize>,
}

impl CrashingObjectStore {
    fn maybe_crash_after_success(&self, trigger: CrashTrigger, key: &str) {
        if trigger != self.scenario.trigger || !self.key_matches(trigger, key) {
            return;
        }
        let mut matched = self.matched.lock().unwrap();
        *matched += 1;
        if *matched == self.scenario.crash_on_match {
            eprintln!(
                "crash_matrix: exiting after {:?} match {} on key {}",
                trigger, *matched, key
            );
            std::process::exit(CRASH_EXIT_CODE);
        }
    }

    fn key_matches(&self, trigger: CrashTrigger, key: &str) -> bool {
        match trigger {
            CrashTrigger::WalPut => key.starts_with(&format!("projects/{PROJECT_ID}/wal/")),
            CrashTrigger::ManifestPut => key == format!("projects/{PROJECT_ID}/manifest.json"),
            CrashTrigger::SnapshotPut => {
                key.starts_with(&format!("projects/{PROJECT_ID}/snapshots/"))
            }
            CrashTrigger::WalDeletePrefix => key == format!("projects/{PROJECT_ID}/wal"),
            CrashTrigger::RuntimeStage(_) => false,
        }
    }
}

impl ObjectStore for CrashingObjectStore {
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
        self.inner.put_bytes(key, data)?;
        self.maybe_crash_after_success(
            if key == format!("projects/{PROJECT_ID}/manifest.json") {
                CrashTrigger::ManifestPut
            } else {
                CrashTrigger::WalPut
            },
            key,
        );
        Ok(())
    }

    fn put_file(&self, key: &str, source: &Path) -> anyhow::Result<()> {
        self.inner.put_file(key, source)?;
        self.maybe_crash_after_success(CrashTrigger::SnapshotPut, key);
        Ok(())
    }

    fn delete(&self, key: &str) -> anyhow::Result<()> {
        self.inner.delete(key)
    }

    fn delete_prefix(&self, prefix: &str) -> anyhow::Result<()> {
        self.inner.delete_prefix(prefix)?;
        self.maybe_crash_after_success(CrashTrigger::WalDeletePrefix, prefix.trim_end_matches('/'));
        Ok(())
    }

    fn list_prefix(&self, prefix: &str) -> anyhow::Result<Vec<ObjectMeta>> {
        self.inner.list_prefix(prefix)
    }
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
