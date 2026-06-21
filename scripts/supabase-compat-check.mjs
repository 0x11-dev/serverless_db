#!/usr/bin/env node
import { writeFile } from "node:fs/promises";
import { createClient } from "@supabase/supabase-js";

const baseUrl = process.argv[2] || process.env.SUPABASE_URL || "http://127.0.0.1:8765";
const anonKey = process.argv[3] || process.env.SUPABASE_ANON_KEY;
const subject = process.env.SDB_COMPAT_SUB || "alice";
const replicaUrls = (process.env.SDB_REPLICA_URLS || "")
  .split(",")
  .map((item) => item.trim())
  .filter(Boolean);
const reportPath = process.env.SDB_COMPAT_REPORT || "reports/supabase-compatibility-report.md";

if (!anonKey) {
  throw new Error("SUPABASE_ANON_KEY or argv[3] is required");
}

function clientFor(url) {
  return createClient(url, anonKey, {
    auth: {
      persistSession: false,
      autoRefreshToken: false,
      detectSessionInUrl: false,
    },
    global: {
      headers: {
        "x-client-info": "serverless-db-poc-compat-check",
      },
    },
  });
}

function assertOk(step, result) {
  if (result.error) {
    throw new Error(`${step} failed: ${result.error.message}`);
  }
  return result.data;
}

async function waitForRow(client, title, attempts = 30) {
  let last = null;
  for (let i = 0; i < attempts; i += 1) {
    const result = await client.from("notes").select("*").eq("title", title).limit(1);
    if (result.error) {
      last = result.error.message;
    } else if (Array.isArray(result.data) && result.data.length > 0) {
      return result.data[0];
    }
    await new Promise((resolve) => setTimeout(resolve, 250));
  }
  throw new Error(`row ${title} not visible after polling; last=${last ?? "empty result"}`);
}

async function main() {
  const startedAt = new Date();
  const primary = clientFor(baseUrl);
  const stamp = `${Date.now()}-${Math.random().toString(16).slice(2)}`;
  const title = `sdk-primary-${stamp}`;
  const updatedTitle = `sdk-updated-${stamp}`;
  const forwardedTitle = `sdk-replica-forward-${stamp}`;
  const evidence = [];

  const health = await fetch(`${baseUrl}/health`);
  evidence.push(["HTTP health", baseUrl, health.ok ? "PASS" : `FAIL ${health.status}`]);
  if (!health.ok) {
    throw new Error(`health check failed: ${health.status}`);
  }

  await primary.from("notes").delete().eq("owner_id", subject).select();

  const inserted = assertOk(
    "insert().select()",
    await primary
      .from("notes")
      .insert({ owner_id: subject, title, body: "created through supabase-js" })
      .select(),
  );
  if (!Array.isArray(inserted) || inserted[0]?.title !== title) {
    throw new Error("insert().select() did not return the inserted row");
  }
  evidence.push(["insert().select()", baseUrl, `PASS id=${inserted[0].id}`]);

  const selected = assertOk(
    "select().eq().limit()",
    await primary.from("notes").select("*").eq("title", title).limit(1),
  );
  if (!Array.isArray(selected) || selected.length !== 1) {
    throw new Error("select().eq().limit() did not return exactly one row");
  }
  evidence.push(["select().eq().limit()", baseUrl, `PASS rows=${selected.length}`]);

  const updated = assertOk(
    "update().eq().select()",
    await primary.from("notes").update({ title: updatedTitle }).eq("title", title).select(),
  );
  if (!Array.isArray(updated) || updated[0]?.title !== updatedTitle) {
    throw new Error("update().eq().select() did not return the updated row");
  }
  evidence.push(["update().eq().select()", baseUrl, `PASS rows=${updated.length}`]);

  const deleted = assertOk(
    "delete().eq().select()",
    await primary.from("notes").delete().eq("title", updatedTitle).select(),
  );
  if (!Array.isArray(deleted) || deleted[0]?.title !== updatedTitle) {
    throw new Error("delete().eq().select() did not return the deleted row");
  }
  evidence.push(["delete().eq().select()", baseUrl, `PASS rows=${deleted.length}`]);

  const replicaClients = replicaUrls.map((url) => [url, clientFor(url)]);
  if (replicaClients.length > 0) {
    const row = assertOk(
      "primary insert for replica visibility",
      await primary
        .from("notes")
        .insert({ owner_id: subject, title, body: "replica visibility probe" })
        .select(),
    )[0];
    for (const [url, client] of replicaClients) {
      const visible = await waitForRow(client, title);
      evidence.push([
        "replica async read catch-up",
        url,
        `PASS id=${visible.id} primary_id=${row.id}`,
      ]);
    }

    const [url, replica] = replicaClients[0];
    const forwarded = assertOk(
      "replica write forwarding",
      await replica
        .from("notes")
        .insert({ owner_id: subject, title: forwardedTitle, body: "write through replica" })
        .select(),
    );
    if (!Array.isArray(forwarded) || forwarded[0]?.title !== forwardedTitle) {
      throw new Error("replica write forwarding did not return inserted row");
    }
    await waitForRow(primary, forwardedTitle);
    evidence.push(["replica write forwarding", url, `PASS id=${forwarded[0].id}`]);
  }

  await primary.from("notes").delete().eq("owner_id", subject).select();

  const lines = [
    "# Supabase SDK Compatibility Report",
    "",
    `Generated: ${startedAt.toISOString()}`,
    `Primary URL: ${baseUrl}`,
    `Replica URLs: ${replicaUrls.length ? replicaUrls.join(", ") : "not tested"}`,
    "",
    "## Result Matrix",
    "",
    "| Capability | Endpoint | Result |",
    "| --- | --- | --- |",
    ...evidence.map((row) => `| ${row[0]} | ${row[1]} | ${row[2]} |`),
    "",
    "## Compatibility Scope",
    "",
    "- Supported: `createClient(url, jwt)` with table CRUD through `/rest/v1/{table}`.",
    "- Supported: `select('*').eq(...).limit(...)`, `insert(object).select()`, `update(...).eq(...).select()`, `delete().eq(...).select()`.",
    "- Supported: JWT in `Authorization: Bearer` or `apikey` header, using this POC's HS256 token format.",
    "- Supported: primary plus async read replicas, including primary-to-replica read catch-up and replica write forwarding.",
    "- Partial: only `eq` filters and `limit` are implemented for the PostgREST surface.",
    "- Not implemented: Supabase Auth API, Storage API compatibility, Realtime protocol compatibility, RPC, joins/embedding, `or`, `in`, `order`, range offsets, count headers, and generated Postgres error codes.",
    "",
  ];

  await writeFile(reportPath, `${lines.join("\n")}\n`);
  console.log(lines.join("\n"));
}

main().catch((error) => {
  console.error(error.stack || error.message);
  process.exit(1);
});
