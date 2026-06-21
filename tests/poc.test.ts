import { strict as assert } from "node:assert";
import { mkdtempSync, rmSync } from "node:fs";
import type { AddressInfo } from "node:net";
import { tmpdir } from "node:os";
import path from "node:path";
import { createHttpServer } from "../src/http.js";
import { ProjectRuntime } from "../src/runtime.js";

type Harness = Awaited<ReturnType<typeof startHarness>>;

async function startHarness() {
  const dir = mkdtempSync(path.join(tmpdir(), "sdb-poc-"));
  const runtime = new ProjectRuntime(path.join(dir, "runtime"));
  const server = createHttpServer(runtime);
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address() as AddressInfo;
  const baseUrl = `http://127.0.0.1:${address.port}`;
  return {
    baseUrl,
    async close() {
      await new Promise<void>((resolve, reject) => server.close((err) => (err ? reject(err) : resolve())));
      rmSync(dir, { recursive: true, force: true });
    }
  };
}

async function request(h: Harness, method: string, route: string, body?: unknown, token?: string, contentType = "application/json") {
  const headers: Record<string, string> = {};
  let payload: BodyInit | undefined;
  if (token) headers.authorization = `Bearer ${token}`;
  if (body !== undefined) {
    if (body instanceof Uint8Array) {
      payload = body as BodyInit;
      headers["content-type"] = contentType;
    } else {
      payload = JSON.stringify(body);
      headers["content-type"] = "application/json";
    }
  }
  const response = await fetch(`${h.baseUrl}${route}`, { method, headers, body: payload });
  const responseType = response.headers.get("content-type") ?? "";
  const payloadOut = responseType.startsWith("application/json") ? await response.json() : await response.arrayBuffer();
  if (!response.ok) {
    const error = new Error(`${method} ${route} failed with ${response.status}`);
    Object.assign(error, { status: response.status, payload: payloadOut });
    throw error;
  }
  return payloadOut;
}

async function token(h: Harness, sub: string, role = "authenticated", claims: Record<string, unknown> = {}): Promise<string> {
  const response = (await request(h, "POST", "/v1/tokens", { sub, role, claims })) as { token: string };
  return response.token;
}

async function createNotesProject(h: Harness): Promise<void> {
  await request(h, "POST", "/v1/projects", { id: "demo" });
  await request(h, "POST", "/v1/projects/demo/tables", {
    name: "notes",
    columns: [
      { name: "owner_id", type: "text", not_null: true },
      { name: "title", type: "text", not_null: true }
    ]
  });
  await request(h, "PUT", "/v1/projects/demo/policies", {
    table: "notes",
    operation: "all",
    name: "owner_only",
    rule: { column: "owner_id", equals_claim: "sub" }
  });
}

async function testOwnerPolicy(): Promise<void> {
  const h = await startHarness();
  try {
    await createNotesProject(h);
    const alice = await token(h, "alice");
    const bob = await token(h, "bob");
    const inserted = (await request(h, "POST", "/v1/projects/demo/tables/notes", { owner_id: "alice", title: "secret" }, alice)) as {
      row: { owner_id: string };
    };
    assert.equal(inserted.row.owner_id, "alice");

    await assert.rejects(
      () => request(h, "POST", "/v1/projects/demo/tables/notes", { owner_id: "bob", title: "bad" }, alice),
      (err: unknown) => (err as { status?: number }).status === 403
    );

    const aliceRows = (await request(h, "GET", "/v1/projects/demo/tables/notes", undefined, alice)) as { rows: unknown[] };
    const bobRows = (await request(h, "GET", "/v1/projects/demo/tables/notes", undefined, bob)) as { rows: unknown[] };
    assert.equal(aliceRows.rows.length, 1);
    assert.equal(bobRows.rows.length, 0);
  } finally {
    await h.close();
  }
}

async function testHibernateRecovery(): Promise<void> {
  const h = await startHarness();
  try {
    await createNotesProject(h);
    const alice = await token(h, "alice");
    await request(h, "POST", "/v1/projects/demo/tables/notes", { owner_id: "alice", title: "durable" }, alice);
    const events = (await request(h, "GET", "/v1/projects/demo/events?since=0")) as { events: unknown[] };
    assert(events.events.length >= 1);
    await request(h, "POST", "/v1/projects/demo/hibernate");
    const recovered = (await request(h, "GET", "/v1/projects/demo/tables/notes", undefined, alice)) as { rows: Array<{ title: string }> };
    assert.equal(recovered.rows[0].title, "durable");
  } finally {
    await h.close();
  }
}

async function testCrashRecoveryFromDurableWal(): Promise<void> {
  const h = await startHarness();
  try {
    await createNotesProject(h);
    const alice = await token(h, "alice");
    await request(h, "POST", "/v1/projects/demo/tables/notes", { owner_id: "alice", title: "wal-durable" }, alice);
    await request(h, "POST", "/v1/projects/demo/crash");
    const recovered = (await request(h, "GET", "/v1/projects/demo/tables/notes", undefined, alice)) as { rows: Array<{ title: string }> };
    assert.equal(recovered.rows[0].title, "wal-durable");
  } finally {
    await h.close();
  }
}

async function testStorageRoundtrip(): Promise<void> {
  const h = await startHarness();
  try {
    await request(h, "POST", "/v1/projects", { id: "demo" });
    const alice = await token(h, "alice");
    await request(h, "POST", "/v1/projects/demo/buckets", { name: "files" });
    const meta = (await request(h, "PUT", "/v1/projects/demo/storage/files/hello.txt", Buffer.from("hello"), alice, "text/plain")) as {
      object: { size: number };
    };
    assert.equal(meta.object.size, 5);
    const blob = (await request(h, "GET", "/v1/projects/demo/storage/files/hello.txt", undefined, alice)) as ArrayBuffer;
    assert.equal(Buffer.from(blob).toString("utf8"), "hello");
  } finally {
    await h.close();
  }
}

async function testUpdateDelete(): Promise<void> {
  const h = await startHarness();
  try {
    await createNotesProject(h);
    const alice = await token(h, "alice");
    await request(h, "POST", "/v1/projects/demo/tables/notes", { owner_id: "alice", title: "a" }, alice);
    const updated = (await request(h, "PATCH", "/v1/projects/demo/tables/notes?eq.title=a", { title: "b" }, alice)) as {
      affected: number;
    };
    assert.equal(updated.affected, 1);
    const rows = (await request(h, "GET", "/v1/projects/demo/tables/notes", undefined, alice)) as { rows: Array<{ title: string }> };
    assert.equal(rows.rows[0].title, "b");
    const deleted = (await request(h, "DELETE", "/v1/projects/demo/tables/notes?eq.title=b", undefined, alice)) as { affected: number };
    assert.equal(deleted.affected, 1);
  } finally {
    await h.close();
  }
}

const tests = [
  ["owner policy filters and rejects rows", testOwnerPolicy],
  ["hibernate restores from object-store snapshot", testHibernateRecovery],
  ["crash restores from durable object-store WAL", testCrashRecoveryFromDurableWal],
  ["storage roundtrip", testStorageRoundtrip],
  ["filter update and delete", testUpdateDelete]
] as const;

for (const [name, fn] of tests) {
  const start = Date.now();
  await fn();
  console.log(`ok - ${name} (${Date.now() - start}ms)`);
}
