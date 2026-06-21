import { strict as assert } from "node:assert";
import { mkdtempSync, rmSync } from "node:fs";
import type { AddressInfo } from "node:net";
import { tmpdir } from "node:os";
import path from "node:path";
import { performance } from "node:perf_hooks";
import { createHttpServer } from "../src/http.js";
import { ProjectRuntime } from "../src/runtime.js";

type Args = {
  rows: number;
  concurrency: number;
  snapshotEveryOps: number;
  metadataEveryOps: number;
  runtimeDir?: string;
};

const args = parseArgs(process.argv.slice(2));
const tempDir = args.runtimeDir ? null : mkdtempSync(path.join(tmpdir(), "sdb-bench-"));
const runtimeDir = args.runtimeDir ?? path.join(tempDir!, "runtime");
const runtime = new ProjectRuntime(runtimeDir, {
  snapshotEveryOps: args.snapshotEveryOps,
  snapshotEveryMs: 300_000,
  metadataEveryOps: args.metadataEveryOps,
  sqliteSynchronous: "NORMAL"
});
const server = createHttpServer(runtime);

await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
const address = server.address() as AddressInfo;
const baseUrl = `http://127.0.0.1:${address.port}`;

try {
  const projectId = "bench";
  const token = await mintToken("bench-user");
  await request("POST", "/v1/projects", { id: projectId });
  await request("POST", `/v1/projects/${projectId}/tables`, {
    name: "events",
    columns: [
      { name: "owner_id", type: "text", not_null: true },
      { name: "seq", type: "integer", not_null: true },
      { name: "payload", type: "text", not_null: true }
    ]
  });
  await request("PUT", `/v1/projects/${projectId}/policies`, {
    table: "events",
    operation: "all",
    name: "owner_only",
    rule: { column: "owner_id", equals_claim: "sub" }
  });

  const insertLatencies = await runConcurrent(args.rows, args.concurrency, async (idx) => {
    await request(
      "POST",
      `/v1/projects/${projectId}/tables/events`,
      {
        owner_id: "bench-user",
        seq: idx,
        payload: `payload-${idx}-${"x".repeat(96)}`
      },
      token
    );
  });

  const pointReadLatencies = await runConcurrent(Math.min(args.rows, 1000), args.concurrency, async (idx) => {
    await request("GET", `/v1/projects/${projectId}/tables/events?eq.seq=${idx}`, undefined, token);
  });

  const crashStart = performance.now();
  await request("POST", `/v1/projects/${projectId}/crash`);
  const recovered = (await request("GET", `/v1/projects/${projectId}/tables/events?eq.seq=${args.rows - 1}`, undefined, token)) as {
    rows: unknown[];
  };
  const recoveryMs = performance.now() - crashStart;
  assert.equal(recovered.rows.length, 1);

  const info = runtime.projectInfo(projectId);
  const result = {
    rows: args.rows,
    concurrency: args.concurrency,
    snapshot_every_ops: args.snapshotEveryOps,
    metadata_every_ops: args.metadataEveryOps,
    insert: summarize(insertLatencies),
    point_read: summarize(pointReadLatencies),
    crash_recovery_ms: Number(recoveryMs.toFixed(2)),
    object_store: {
      snapshot_bytes: info.snapshot_bytes,
      durable_wal_bytes: info.durable_wal_bytes,
      durable_wal_exists: info.durable_wal_exists,
      manifest: info.manifest
    }
  };
  console.log(JSON.stringify(result, null, 2));
} finally {
  await new Promise<void>((resolve, reject) => server.close((err) => (err ? reject(err) : resolve())));
  if (tempDir) {
    rmSync(tempDir, { recursive: true, force: true });
  }
}

async function mintToken(sub: string): Promise<string> {
  const response = (await request("POST", "/v1/tokens", { sub })) as { token: string };
  return response.token;
}

async function runConcurrent(count: number, concurrency: number, fn: (idx: number) => Promise<void>): Promise<number[]> {
  const latencies: number[] = [];
  let next = 0;
  const started = performance.now();
  await Promise.all(
    Array.from({ length: concurrency }, async () => {
      while (true) {
        const idx = next;
        next += 1;
        if (idx >= count) return;
        const before = performance.now();
        await fn(idx);
        latencies.push(performance.now() - before);
      }
    })
  );
  const elapsed = performance.now() - started;
  Object.defineProperty(latencies, "elapsedMs", { value: elapsed, enumerable: false });
  return latencies;
}

function summarize(values: number[]): Record<string, number> {
  const sorted = [...values].sort((a, b) => a - b);
  const elapsedMs = (values as number[] & { elapsedMs?: number }).elapsedMs ?? 0;
  return {
    count: sorted.length,
    throughput_per_sec: Number((sorted.length / (elapsedMs / 1000)).toFixed(2)),
    elapsed_ms: Number(elapsedMs.toFixed(2)),
    p50_ms: percentile(sorted, 0.5),
    p95_ms: percentile(sorted, 0.95),
    p99_ms: percentile(sorted, 0.99),
    max_ms: Number((sorted.at(-1) ?? 0).toFixed(2))
  };
}

function percentile(sorted: number[], pct: number): number {
  if (sorted.length === 0) return 0;
  const idx = Math.min(sorted.length - 1, Math.ceil(sorted.length * pct) - 1);
  return Number(sorted[idx].toFixed(2));
}

async function request(method: string, route: string, body?: unknown, token?: string): Promise<unknown> {
  const headers: Record<string, string> = {};
  let payload: BodyInit | undefined;
  if (token) headers.authorization = `Bearer ${token}`;
  if (body !== undefined) {
    payload = JSON.stringify(body);
    headers["content-type"] = "application/json";
  }
  const response = await fetch(`${baseUrl}${route}`, { method, headers, body: payload });
  const responseType = response.headers.get("content-type") ?? "";
  const payloadOut = responseType.startsWith("application/json") ? await response.json() : await response.text();
  if (!response.ok) {
    throw new Error(`${method} ${route} failed with ${response.status}: ${JSON.stringify(payloadOut)}`);
  }
  return payloadOut;
}

function parseArgs(argv: string[]): Args {
  const parsed: Args = {
    rows: 2000,
    concurrency: 16,
    snapshotEveryOps: 1000,
    metadataEveryOps: 100
  };
  for (let idx = 0; idx < argv.length; idx += 1) {
    const item = argv[idx];
    if (item === "--rows") parsed.rows = Number(argv[++idx] ?? parsed.rows);
    else if (item === "--concurrency") parsed.concurrency = Number(argv[++idx] ?? parsed.concurrency);
    else if (item === "--snapshot-every-ops") parsed.snapshotEveryOps = Number(argv[++idx] ?? parsed.snapshotEveryOps);
    else if (item === "--metadata-every-ops") parsed.metadataEveryOps = Number(argv[++idx] ?? parsed.metadataEveryOps);
    else if (item === "--runtime-dir") parsed.runtimeDir = argv[++idx];
  }
  return parsed;
}
