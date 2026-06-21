import { createHash } from "node:crypto";
import { copyFileSync, existsSync, mkdirSync, readFileSync, rmSync, statSync, unlinkSync } from "node:fs";
import path from "node:path";
import Database from "better-sqlite3";
import { EventEmitter } from "node:events";
import { type Actor } from "./auth.js";
import { LocalObjectStore } from "./object-store.js";
import { type PolicyRule, compilePolicies, evaluatePolicies, quoteIdent, type SqlValue } from "./policy.js";

export class ApiError extends Error {
  readonly status: number;

  constructor(status: number, message: string) {
    super(message);
    this.status = status;
  }
}

type ProjectState = {
  projectId: string;
  db: Database.Database;
  cacheDir: string;
  dbPath: string;
  lastDurableWalBytes: number;
  walFlushesSinceSnapshot: number;
  lastSnapshotAtMs: number;
};

type ColumnSpec = {
  name: string;
  type?: string;
  primary_key?: boolean;
  auto_increment?: boolean;
  not_null?: boolean;
};

type TableSpec = {
  name: string;
  columns: ColumnSpec[];
};

type PolicySpec = {
  table: string;
  operation?: string;
  name?: string;
  rule: PolicyRule;
};

const PROJECT_RE = /^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$/;
const IDENT_RE = /^[A-Za-z_][A-Za-z0-9_]*$/;
const TYPE_MAP: Record<string, string> = {
  text: "TEXT",
  integer: "INTEGER",
  real: "REAL",
  numeric: "NUMERIC",
  blob: "BLOB",
  boolean: "INTEGER",
  json: "TEXT",
  timestamp: "TEXT"
};
const OPS = new Set(["select", "insert", "update", "delete", "all"]);

export type RuntimeOptions = {
  snapshotEveryOps?: number;
  snapshotEveryMs?: number;
  metadataEveryOps?: number;
  sqliteSynchronous?: "OFF" | "NORMAL" | "FULL";
};

const DEFAULT_OPTIONS: Required<RuntimeOptions> = {
  snapshotEveryOps: 1000,
  snapshotEveryMs: 60_000,
  metadataEveryOps: 100,
  sqliteSynchronous: "NORMAL"
};

export class ProjectRuntime {
  readonly runtimeDir: string;
  readonly cacheDir: string;
  readonly objectStore: LocalObjectStore;
  readonly options: Required<RuntimeOptions>;
  private readonly states = new Map<string, ProjectState>();
  private readonly emitter = new EventEmitter();

  constructor(runtimeDir = ".runtime", options: RuntimeOptions = {}) {
    this.runtimeDir = path.resolve(runtimeDir);
    this.cacheDir = path.join(this.runtimeDir, "cache");
    this.objectStore = new LocalObjectStore(path.join(this.runtimeDir, "object_store"));
    this.options = { ...DEFAULT_OPTIONS, ...options };
    mkdirSync(this.cacheDir, { recursive: true });
    this.emitter.setMaxListeners(1000);
  }

  createProject(projectId: string): Record<string, unknown> {
    this.ensureProject(projectId);
    return this.projectInfo(projectId);
  }

  projectInfo(projectId: string): Record<string, unknown> {
    const pid = safeProjectId(projectId);
    const snapshotPath = this.objectStore.path("projects", pid, "database.sqlite");
    const walPath = this.objectStore.path("projects", pid, "database.sqlite-wal");
    const manifestPath = this.objectStore.path("projects", pid, "manifest.json");
    return {
      project_id: pid,
      cache_path: path.join(this.cacheDir, pid, "main.sqlite"),
      snapshot_path: snapshotPath,
      snapshot_exists: existsSync(snapshotPath),
      snapshot_bytes: existsSync(snapshotPath) ? statSync(snapshotPath).size : 0,
      durable_wal_path: walPath,
      durable_wal_exists: existsSync(walPath),
      durable_wal_bytes: existsSync(walPath) ? statSync(walPath).size : 0,
      manifest_path: manifestPath,
      manifest: existsSync(manifestPath) ? JSON.parse(readFileSync(manifestPath, "utf8")) : null
    };
  }

  ensureProject(projectId: string): ProjectState {
    const pid = safeProjectId(projectId);
    const existing = this.states.get(pid);
    if (existing) {
      return existing;
    }

    const cacheDir = path.join(this.cacheDir, pid);
    const dbPath = path.join(cacheDir, "main.sqlite");
    mkdirSync(cacheDir, { recursive: true });
    const snapshot = this.objectStore.path("projects", pid, "database.sqlite");
    if (!existsSync(dbPath) && existsSync(snapshot)) {
      copyFileSync(snapshot, dbPath);
      const durableWal = this.objectStore.path("projects", pid, "database.sqlite-wal");
      if (existsSync(durableWal)) {
        copyFileSync(durableWal, `${dbPath}-wal`);
      }
    }

    const db = new Database(dbPath);
    db.pragma("foreign_keys = ON");
    db.pragma("journal_mode = WAL");
    db.pragma(`synchronous = ${this.options.sqliteSynchronous}`);
    db.pragma("wal_autocheckpoint = 0");

    const durableWalPath = this.objectStore.path("projects", pid, "database.sqlite-wal");
    const state: ProjectState = {
      projectId: pid,
      db,
      cacheDir,
      dbPath,
      lastDurableWalBytes: existsSync(durableWalPath) ? statSync(durableWalPath).size : 0,
      walFlushesSinceSnapshot: 0,
      lastSnapshotAtMs: Date.now()
    };
    this.states.set(pid, state);
    this.ensureMeta(db);
    if (!existsSync(snapshot)) {
      this.persistSnapshot(state, "init");
    } else if (existsSync(this.objectStore.path("projects", pid, "database.sqlite-wal"))) {
      this.appendChangeLog(state, "rehydrate_from_snapshot_and_wal", {
        durable_wal_bytes: statSync(this.objectStore.path("projects", pid, "database.sqlite-wal")).size
      });
    }
    return state;
  }

  hibernate(projectId: string): Record<string, unknown> {
    const pid = safeProjectId(projectId);
    const state = this.states.get(pid);
    if (state) {
      this.persistSnapshot(state, "hibernate");
      state.db.close();
      this.states.delete(pid);
    }
    const cacheDir = path.join(this.cacheDir, pid);
    rmSync(cacheDir, { recursive: true, force: true });
    return { project_id: pid, cache_removed: true };
  }

  crashProject(projectId: string): Record<string, unknown> {
    const pid = safeProjectId(projectId);
    const state = this.states.get(pid);
    if (state) {
      state.db.close();
      this.states.delete(pid);
    }
    const cacheDir = path.join(this.cacheDir, pid);
    rmSync(cacheDir, { recursive: true, force: true });
    return { project_id: pid, cache_removed: true, snapshot_forced: false };
  }

  createTable(projectId: string, spec: TableSpec): Record<string, unknown> {
    const state = this.ensureProject(projectId);
    const table = assertUserIdent(spec.name);
    if (!Array.isArray(spec.columns)) {
      throw new ApiError(400, "columns must be a list");
    }

    const names = new Set<string>();
    const columnDefs: string[] = [];
    let hasPrimaryKey = false;
    for (const column of spec.columns) {
      const name = assertUserIdent(column.name);
      if (names.has(name)) {
        throw new ApiError(400, `duplicate column: ${name}`);
      }
      names.add(name);
      const sqlType = TYPE_MAP[(column.type ?? "text").toLowerCase()];
      if (!sqlType) {
        throw new ApiError(400, `unsupported column type: ${column.type}`);
      }
      const parts = [quoteIdent(name), sqlType];
      if (column.primary_key) {
        parts.push("PRIMARY KEY");
        if (sqlType === "INTEGER" && column.auto_increment !== false) {
          parts.push("AUTOINCREMENT");
        }
        hasPrimaryKey = true;
      }
      if (column.not_null) {
        parts.push("NOT NULL");
      }
      columnDefs.push(parts.join(" "));
    }
    if (!hasPrimaryKey && !names.has("id")) {
      columnDefs.unshift('"id" INTEGER PRIMARY KEY AUTOINCREMENT');
    }

    state.db.prepare(`CREATE TABLE IF NOT EXISTS ${quoteIdent(table)} (${columnDefs.join(", ")})`).run();
    this.durabilizeWal(state, `create_table:${table}`);
    return { table, columns: this.tableColumns(state.db, table) };
  }

  schema(projectId: string): Record<string, unknown> {
    const state = this.ensureProject(projectId);
    const rows = state.db
      .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE '_sdb_%' ORDER BY name")
      .all() as Array<{ name: string }>;
    return {
      project_id: state.projectId,
      tables: rows.map((row) => ({ name: row.name, columns: this.tableColumns(state.db, row.name) }))
    };
  }

  setPolicy(projectId: string, spec: PolicySpec): Record<string, unknown> {
    const state = this.ensureProject(projectId);
    const table = assertUserIdent(spec.table);
    const operation = (spec.operation ?? "all").toLowerCase();
    if (!OPS.has(operation)) {
      throw new ApiError(400, "operation must be one of select, insert, update, delete, all");
    }
    this.requireTable(state.db, table);
    const name = spec.name ?? `${operation}_policy`;
    state.db
      .prepare(
        `
        INSERT INTO _sdb_policies(table_name, operation, name, rule_json)
        VALUES(?, ?, ?, ?)
        ON CONFLICT(table_name, operation, name)
        DO UPDATE SET rule_json=excluded.rule_json, updated_at=datetime('now')
        `
      )
      .run(table, operation, name, JSON.stringify(spec.rule));
    this.durabilizeWal(state, `set_policy:${table}:${operation}:${name}`);
    return { table, operation, name, rule: spec.rule };
  }

  listPolicies(projectId: string): Record<string, unknown>[] {
    const state = this.ensureProject(projectId);
    const rows = state.db
      .prepare("SELECT table_name, operation, name, rule_json, updated_at FROM _sdb_policies ORDER BY table_name, operation, name")
      .all() as Array<{ table_name: string; operation: string; name: string; rule_json: string; updated_at: string }>;
    return rows.map((row) => ({
      table: row.table_name,
      operation: row.operation,
      name: row.name,
      rule: JSON.parse(row.rule_json),
      updated_at: row.updated_at
    }));
  }

  selectRows(projectId: string, tableName: string, filters: Record<string, string>, actor: Actor, limit = 100): Record<string, unknown>[] {
    const state = this.ensureProject(projectId);
    const table = assertUserIdent(tableName);
    this.requireTable(state.db, table);
    const where = this.whereForOperation(state.db, table, "select", filters, actor);
    const cappedLimit = Math.max(1, Math.min(Math.trunc(limit), 1000));
    return state.db
      .prepare(`SELECT * FROM ${quoteIdent(table)} WHERE ${where.sql} LIMIT ?`)
      .all(...where.params, cappedLimit) as Record<string, unknown>[];
  }

  insertRow(projectId: string, tableName: string, data: Record<string, unknown>, actor: Actor): Record<string, unknown> {
    const state = this.ensureProject(projectId);
    const table = assertUserIdent(tableName);
    this.requireTable(state.db, table);
    const row = normalizeRowPayload(data);
    const rules = this.policyRules(state.db, table, "insert");
    if (!evaluatePolicies(rules, row, actor)) {
      throw new ApiError(403, "insert rejected by policy");
    }
    const columns = Object.keys(row).map(assertUserIdent);
    const insert = state.db.transaction(() => {
      const result = state.db
        .prepare(
          `INSERT INTO ${quoteIdent(table)} (${columns.map(quoteIdent).join(",")}) VALUES (${columns.map(() => "?").join(",")})`
        )
        .run(...columns.map((column) => row[column]));
      const inserted = state.db.prepare(`SELECT * FROM ${quoteIdent(table)} WHERE rowid=?`).get(result.lastInsertRowid) as Record<string, unknown>;
      this.recordEvent(state.db, table, "insert", inserted, actor);
      return inserted;
    });
    const inserted = insert();
    this.durabilizeWal(state, `insert:${table}`);
    this.notify(state.projectId);
    return inserted;
  }

  updateRows(projectId: string, tableName: string, filters: Record<string, string>, data: Record<string, unknown>, actor: Actor): Record<string, unknown> {
    const state = this.ensureProject(projectId);
    const table = assertUserIdent(tableName);
    this.requireTable(state.db, table);
    const patch = normalizeRowPayload(data);
    const updates = Object.keys(patch).map(assertUserIdent);
    const where = this.whereForOperation(state.db, table, "update", filters, actor);
    const rules = this.policyRules(state.db, table, "update");

    const tx = state.db.transaction(() => {
      const before = state.db
        .prepare(`SELECT rowid AS _rowid, * FROM ${quoteIdent(table)} WHERE ${where.sql}`)
        .all(...where.params) as Array<Record<string, unknown>>;
      const allowedRowIds: number[] = [];
      for (const candidate of before) {
        const merged = { ...candidate, ...patch };
        delete merged._rowid;
        if (evaluatePolicies(rules, merged, actor)) {
          allowedRowIds.push(Number(candidate._rowid));
        }
      }
      if (before.length > 0 && allowedRowIds.length === 0) {
        throw new ApiError(403, "update rejected by policy");
      }
      if (allowedRowIds.length === 0) {
        return [] as Record<string, unknown>[];
      }
      const rowIdSql = allowedRowIds.map(() => "?").join(",");
      state.db
        .prepare(`UPDATE ${quoteIdent(table)} SET ${updates.map((column) => `${quoteIdent(column)}=?`).join(", ")} WHERE rowid IN (${rowIdSql})`)
        .run(...updates.map((column) => patch[column]), ...allowedRowIds);
      const updated = state.db
        .prepare(`SELECT * FROM ${quoteIdent(table)} WHERE rowid IN (${rowIdSql})`)
        .all(...allowedRowIds) as Record<string, unknown>[];
      for (const item of updated) {
        this.recordEvent(state.db, table, "update", item, actor);
      }
      return updated;
    });

    const updated = tx();
    if (updated.length > 0) {
      this.durabilizeWal(state, `update:${table}`);
      this.notify(state.projectId);
    }
    return { affected: updated.length, rows: updated };
  }

  deleteRows(projectId: string, tableName: string, filters: Record<string, string>, actor: Actor): Record<string, unknown> {
    const state = this.ensureProject(projectId);
    const table = assertUserIdent(tableName);
    this.requireTable(state.db, table);
    const where = this.whereForOperation(state.db, table, "delete", filters, actor);
    const tx = state.db.transaction(() => {
      const rows = state.db
        .prepare(`SELECT rowid AS _rowid, * FROM ${quoteIdent(table)} WHERE ${where.sql}`)
        .all(...where.params) as Array<Record<string, unknown>>;
      const rowIds = rows.map((row) => Number(row._rowid));
      if (rowIds.length > 0) {
        const rowIdSql = rowIds.map(() => "?").join(",");
        state.db.prepare(`DELETE FROM ${quoteIdent(table)} WHERE rowid IN (${rowIdSql})`).run(...rowIds);
        for (const row of rows) {
          const payload = { ...row };
          delete payload._rowid;
          this.recordEvent(state.db, table, "delete", payload, actor);
        }
      }
      return rows.length;
    });
    const affected = tx();
    if (affected > 0) {
      this.durabilizeWal(state, `delete:${table}`);
      this.notify(state.projectId);
    }
    return { affected };
  }

  createBucket(projectId: string, name: string): Record<string, unknown> {
    const state = this.ensureProject(projectId);
    const bucket = assertUserIdent(name);
    state.db.prepare("INSERT INTO _sdb_buckets(name) VALUES(?) ON CONFLICT(name) DO NOTHING").run(bucket);
    this.durabilizeWal(state, `create_bucket:${bucket}`);
    return { bucket };
  }

  putObject(projectId: string, bucketName: string, keyName: string, data: Buffer, contentType: string, actor: Actor): Record<string, unknown> {
    const state = this.ensureProject(projectId);
    const bucket = assertUserIdent(bucketName);
    const key = safeObjectKey(keyName);
    this.requireBucket(state.db, bucket);
    const etag = createHash("sha256").update(data).digest("hex");
    const now = utcNow();
    this.objectStore.writeBytesAtomic(data, "projects", state.projectId, "storage", bucket, key);
    const tx = state.db.transaction(() => {
      state.db
        .prepare(
          `
          INSERT INTO _sdb_objects(bucket, object_key, size, content_type, etag, owner_id, created_at, updated_at)
          VALUES(?, ?, ?, ?, ?, ?, ?, ?)
          ON CONFLICT(bucket, object_key)
          DO UPDATE SET size=excluded.size, content_type=excluded.content_type, etag=excluded.etag,
                        owner_id=excluded.owner_id, updated_at=excluded.updated_at
          `
        )
        .run(bucket, key, data.length, contentType, etag, actor.sub, now, now);
      const object = state.db
        .prepare("SELECT bucket, object_key AS key, size, content_type, etag, owner_id, created_at, updated_at FROM _sdb_objects WHERE bucket=? AND object_key=?")
        .get(bucket, key) as Record<string, unknown>;
      this.recordEvent(state.db, "_sdb_objects", "storage_put", object, actor);
      return object;
    });
    const object = tx();
    this.durabilizeWal(state, `put_object:${bucket}/${key}`);
    this.notify(state.projectId);
    return object;
  }

  getObject(projectId: string, bucketName: string, keyName: string): { meta: Record<string, unknown>; data: Buffer } {
    const state = this.ensureProject(projectId);
    const bucket = assertUserIdent(bucketName);
    const key = safeObjectKey(keyName);
    const meta = state.db
      .prepare("SELECT bucket, object_key AS key, size, content_type, etag, owner_id, created_at, updated_at FROM _sdb_objects WHERE bucket=? AND object_key=?")
      .get(bucket, key) as Record<string, unknown> | undefined;
    if (!meta) {
      throw new ApiError(404, "object not found");
    }
    return {
      meta,
      data: this.objectStore.readBytes("projects", state.projectId, "storage", bucket, key)
    };
  }

  deleteObject(projectId: string, bucketName: string, keyName: string, actor: Actor): Record<string, unknown> {
    const state = this.ensureProject(projectId);
    const bucket = assertUserIdent(bucketName);
    const key = safeObjectKey(keyName);
    const meta = state.db
      .prepare("SELECT bucket, object_key AS key, size, content_type, etag, owner_id, created_at, updated_at FROM _sdb_objects WHERE bucket=? AND object_key=?")
      .get(bucket, key) as Record<string, unknown> | undefined;
    if (!meta) {
      throw new ApiError(404, "object not found");
    }
    const tx = state.db.transaction(() => {
      state.db.prepare("DELETE FROM _sdb_objects WHERE bucket=? AND object_key=?").run(bucket, key);
      this.recordEvent(state.db, "_sdb_objects", "storage_delete", meta, actor);
    });
    tx();
    this.durabilizeWal(state, `delete_object:${bucket}/${key}`);
    this.objectStore.remove("projects", state.projectId, "storage", bucket, key);
    this.notify(state.projectId);
    return { deleted: true };
  }

  events(projectId: string, since = 0, limit = 100): Record<string, unknown>[] {
    const state = this.ensureProject(projectId);
    const rows = state.db
      .prepare(
        `
        SELECT id, created_at, table_name, operation, row_json, actor_sub, actor_role
        FROM _sdb_outbox
        WHERE id > ?
        ORDER BY id
        LIMIT ?
        `
      )
      .all(Math.trunc(since), Math.max(1, Math.min(Math.trunc(limit), 1000))) as Array<{
      id: number;
      created_at: string;
      table_name: string;
      operation: string;
      row_json: string;
      actor_sub: string | null;
      actor_role: string | null;
    }>;
    return rows.map((row) => ({
      id: row.id,
      created_at: row.created_at,
      table: row.table_name,
      operation: row.operation,
      row: JSON.parse(row.row_json),
      actor_sub: row.actor_sub,
      actor_role: row.actor_role
    }));
  }

  waitForEvents(projectId: string, since: number, timeoutMs: number): Promise<Record<string, unknown>[]> {
    const immediate = this.events(projectId, since);
    if (immediate.length > 0) {
      return Promise.resolve(immediate);
    }
    const eventName = `events:${safeProjectId(projectId)}`;
    return new Promise((resolve) => {
      const timer = setTimeout(() => {
        this.emitter.off(eventName, onEvent);
        resolve(this.events(projectId, since));
      }, timeoutMs);
      const onEvent = () => {
        clearTimeout(timer);
        this.emitter.off(eventName, onEvent);
        resolve(this.events(projectId, since));
      };
      this.emitter.on(eventName, onEvent);
    });
  }

  private ensureMeta(db: Database.Database): void {
    db.exec(`
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
    `);
  }

  private persistSnapshot(state: ProjectState, reason: string): void {
    state.db.pragma("wal_checkpoint(TRUNCATE)");
    const tmp = path.join(state.cacheDir, `snapshot-${process.pid}-${Date.now()}.sqlite`);
    if (existsSync(tmp)) unlinkSync(tmp);
    state.db.exec(`VACUUM INTO ${sqlString(tmp)}`);
    this.objectStore.replaceFileAtomic(tmp, "projects", state.projectId, "database.sqlite");
    const checksum = sha256File(tmp);
    unlinkSync(tmp);
    this.objectStore.remove("projects", state.projectId, "database.sqlite-wal");
    state.walFlushesSinceSnapshot = 0;
    state.lastDurableWalBytes = 0;
    state.lastSnapshotAtMs = Date.now();
    const snapshotBytes = this.objectStore.stat("projects", state.projectId, "database.sqlite").size;
    this.objectStore.writeBytesAtomic(
      Buffer.from(
        JSON.stringify(
          {
            project_id: state.projectId,
            updated_at: utcNow(),
            snapshot_reason: reason,
            snapshot_sha256: checksum,
            snapshot_bytes: snapshotBytes,
            durable_wal_bytes: 0,
            wal_flushes_since_snapshot: 0
          },
          null,
          2
        )
      ),
      "projects",
      state.projectId,
      "manifest.json"
    );
    this.appendChangeLog(state, reason, {
      snapshot_sha256: checksum,
      snapshot_bytes: snapshotBytes,
      durable_wal_bytes: 0,
      wal_flushes_since_snapshot: 0
    });
  }

  private durabilizeWal(state: ProjectState, reason: string): void {
    const walPath = `${state.dbPath}-wal`;
    let walBytes = 0;
    if (existsSync(walPath)) {
      walBytes = statSync(walPath).size;
      const durableWalExists = this.objectStore.exists("projects", state.projectId, "database.sqlite-wal");
      if (!durableWalExists || walBytes < state.lastDurableWalBytes) {
        this.objectStore.replaceFileAtomic(walPath, "projects", state.projectId, "database.sqlite-wal");
      } else if (walBytes > state.lastDurableWalBytes) {
        this.objectStore.appendFileRange(walPath, state.lastDurableWalBytes, "projects", state.projectId, "database.sqlite-wal");
      }
      state.lastDurableWalBytes = walBytes;
    }
    state.walFlushesSinceSnapshot += 1;
    if (this.shouldWriteMetadata(state)) {
      this.writeWalManifest(state, reason, walBytes);
    }
    if (this.shouldSnapshot(state)) {
      this.persistSnapshot(state, `compact_after:${reason}`);
    }
  }

  private shouldWriteMetadata(state: ProjectState): boolean {
    return this.options.metadataEveryOps > 0 && state.walFlushesSinceSnapshot % this.options.metadataEveryOps === 0;
  }

  private writeWalManifest(state: ProjectState, reason: string, walBytes: number): void {
    this.objectStore.writeBytesAtomic(
      Buffer.from(
        JSON.stringify(
          {
            project_id: state.projectId,
            updated_at: utcNow(),
            snapshot_reason: null,
            snapshot_bytes: this.objectStore.stat("projects", state.projectId, "database.sqlite").size,
            durable_wal_bytes: walBytes,
            wal_flushes_since_snapshot: state.walFlushesSinceSnapshot
          },
          null,
          2
        )
      ),
      "projects",
      state.projectId,
      "manifest.json"
    );
    this.appendChangeLog(state, reason, {
      durable_wal_bytes: walBytes,
      wal_flushes_since_snapshot: state.walFlushesSinceSnapshot
    });
  }

  private shouldSnapshot(state: ProjectState): boolean {
    if (this.options.snapshotEveryOps > 0 && state.walFlushesSinceSnapshot >= this.options.snapshotEveryOps) {
      return true;
    }
    return this.options.snapshotEveryMs > 0 && Date.now() - state.lastSnapshotAtMs >= this.options.snapshotEveryMs;
  }

  private appendChangeLog(state: ProjectState, reason: string, fields: Record<string, unknown>): void {
    this.objectStore.appendJsonl(
      {
        at: utcNow(),
        reason,
        ...fields
      },
      "projects",
      state.projectId,
      "change_log.jsonl"
    );
  }

  private whereForOperation(
    db: Database.Database,
    table: string,
    operation: "select" | "update" | "delete",
    filters: Record<string, string>,
    actor: Actor
  ): { sql: string; params: SqlValue[] } {
    const clauses: string[] = [];
    const params: SqlValue[] = [];
    for (const [key, value] of Object.entries(filters)) {
      const column = assertUserIdent(key);
      clauses.push(`${quoteIdent(column)} = ?`);
      params.push(value);
    }
    const policy = compilePolicies(this.policyRules(db, table, operation), actor);
    clauses.push(`(${policy.sql})`);
    params.push(...policy.params);
    return { sql: clauses.join(" AND "), params };
  }

  private policyRules(db: Database.Database, table: string, operation: string): PolicyRule[] {
    const rows = db
      .prepare("SELECT rule_json FROM _sdb_policies WHERE table_name=? AND operation IN (?, 'all') ORDER BY id")
      .all(table, operation) as Array<{ rule_json: string }>;
    return rows.map((row) => JSON.parse(row.rule_json) as PolicyRule);
  }

  private recordEvent(db: Database.Database, table: string, operation: string, row: Record<string, unknown>, actor: Actor): void {
    db.prepare("INSERT INTO _sdb_outbox(table_name, operation, row_json, actor_sub, actor_role) VALUES(?, ?, ?, ?, ?)").run(
      table,
      operation,
      JSON.stringify(row),
      actor.sub,
      actor.role
    );
  }

  private notify(projectId: string): void {
    this.emitter.emit(`events:${projectId}`);
  }

  private requireTable(db: Database.Database, table: string): void {
    const row = db
      .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name=? AND name NOT LIKE '_sdb_%'")
      .get(table);
    if (!row) {
      throw new ApiError(404, `table not found: ${table}`);
    }
  }

  private requireBucket(db: Database.Database, bucket: string): void {
    const row = db.prepare("SELECT name FROM _sdb_buckets WHERE name=?").get(bucket);
    if (!row) {
      throw new ApiError(404, `bucket not found: ${bucket}`);
    }
  }

  private tableColumns(db: Database.Database, table: string): Record<string, unknown>[] {
    return db.prepare(`PRAGMA table_info(${quoteIdent(table)})`).all() as Record<string, unknown>[];
  }
}

export function normalizeRowPayload(data: Record<string, unknown>): Record<string, SqlValue> {
  if (!data || typeof data !== "object" || Array.isArray(data) || Object.keys(data).length === 0) {
    throw new ApiError(400, "row body must be a non-empty object");
  }
  const normalized: Record<string, SqlValue> = {};
  for (const [key, value] of Object.entries(data)) {
    const column = assertUserIdent(key);
    if (typeof value === "boolean") {
      normalized[column] = value ? 1 : 0;
    } else if (typeof value === "number" || typeof value === "string" || value === null) {
      normalized[column] = value;
    } else {
      normalized[column] = JSON.stringify(value);
    }
  }
  return normalized;
}

export function safeProjectId(projectId: string): string {
  if (!PROJECT_RE.test(projectId)) {
    throw new ApiError(400, "project id must match [A-Za-z0-9][A-Za-z0-9_-]{0,63}");
  }
  return projectId;
}

export function assertUserIdent(name: string): string {
  if (!IDENT_RE.test(name) || name.startsWith("_sdb_")) {
    throw new ApiError(400, `invalid user identifier: ${name}`);
  }
  return name;
}

export function safeObjectKey(key: string): string {
  if (!key || key.startsWith("/") || key.includes("\0")) {
    throw new ApiError(400, "invalid object key");
  }
  const parts = key.split("/").filter(Boolean);
  if (parts.some((part) => part === "." || part === "..")) {
    throw new ApiError(400, "object key may not contain . or .. path segments");
  }
  return parts.join("/");
}

function sqlString(value: string): string {
  return `'${value.replaceAll("'", "''")}'`;
}

function sha256File(filePath: string): string {
  return createHash("sha256").update(readFileSync(filePath)).digest("hex");
}

function utcNow(): string {
  return new Date().toISOString().replace(/\.\d{3}Z$/, "Z");
}
