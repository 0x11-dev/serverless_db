import { ProjectRuntime } from "./runtime.js";
import { createHttpServer } from "./http.js";

type Args = {
  host: string;
  port: number;
  runtimeDir: string;
  snapshotEveryOps: number;
  snapshotEveryMs: number;
  metadataEveryOps: number;
  sqliteSynchronous: "OFF" | "NORMAL" | "FULL";
};

const args = parseArgs(process.argv.slice(2));
const runtime = new ProjectRuntime(args.runtimeDir, {
  snapshotEveryOps: args.snapshotEveryOps,
  snapshotEveryMs: args.snapshotEveryMs,
  metadataEveryOps: args.metadataEveryOps,
  sqliteSynchronous: args.sqliteSynchronous
});
const server = createHttpServer(runtime);

server.listen(args.port, args.host, () => {
  const address = server.address();
  const port = typeof address === "object" && address ? address.port : args.port;
  console.log(`Serverless DB POC listening on http://${args.host}:${port}`);
});

process.on("SIGINT", () => {
  server.close(() => process.exit(0));
});

function parseArgs(argv: string[]): Args {
  const args: Args = {
    host: "127.0.0.1",
    port: 8765,
    runtimeDir: ".runtime",
    snapshotEveryOps: numberEnv("SDB_SNAPSHOT_EVERY_OPS", 1000),
    snapshotEveryMs: numberEnv("SDB_SNAPSHOT_EVERY_MS", 60_000),
    metadataEveryOps: numberEnv("SDB_METADATA_EVERY_OPS", 100),
    sqliteSynchronous: sqliteSyncEnv()
  };
  for (let idx = 0; idx < argv.length; idx += 1) {
    const item = argv[idx];
    if (item === "--host") args.host = argv[++idx] ?? args.host;
    else if (item === "--port") args.port = Number(argv[++idx] ?? args.port);
    else if (item === "--runtime-dir") args.runtimeDir = argv[++idx] ?? args.runtimeDir;
    else if (item === "--snapshot-every-ops") args.snapshotEveryOps = Number(argv[++idx] ?? args.snapshotEveryOps);
    else if (item === "--snapshot-every-ms") args.snapshotEveryMs = Number(argv[++idx] ?? args.snapshotEveryMs);
    else if (item === "--metadata-every-ops") args.metadataEveryOps = Number(argv[++idx] ?? args.metadataEveryOps);
    else if (item === "--sqlite-synchronous") args.sqliteSynchronous = sqliteSyncValue(argv[++idx] ?? args.sqliteSynchronous);
  }
  return args;
}

function numberEnv(name: string, fallback: number): number {
  const value = Number(process.env[name]);
  return Number.isFinite(value) ? value : fallback;
}

function sqliteSyncEnv(): "OFF" | "NORMAL" | "FULL" {
  return sqliteSyncValue(process.env.SDB_SQLITE_SYNCHRONOUS ?? "NORMAL");
}

function sqliteSyncValue(value: string): "OFF" | "NORMAL" | "FULL" {
  const normalized = value.toUpperCase();
  if (normalized === "OFF" || normalized === "NORMAL" || normalized === "FULL") {
    return normalized;
  }
  throw new Error("sqlite synchronous mode must be OFF, NORMAL, or FULL");
}
