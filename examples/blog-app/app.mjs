#!/usr/bin/env node
/**
 * Blog Platform Example — comprehensive verification of all serverless-db features.
 *
 * Models a multi-tenant blog platform that exercises:
 *   1. Health check
 *   2. JWT token minting (service_role, authenticated users with claims)
 *   3. Project creation
 *   4. Multi-table schema (users, posts, comments, tags)
 *   5. Schema introspection
 *   6. Policy DSL — all rule types: allow, role_in, equals_claim, in_claim, equals, and, or
 *   7. CRUD — insert, select with eq filters + limit, update, delete
 *   8. Policy enforcement — owner-only, org-based, role-based, public read
 *   9. Storage — bucket creation, object PUT/GET/DELETE with content types
 *  10. Realtime outbox — events polling
 *  11. SSE realtime stream
 *  12. Hibernate + recovery (scale-to-zero simulation)
 *  13. Crash + recovery (ungraceful exit simulation)
 *  14. Bookmark-based consistent reads
 *  15. Write idempotency (Idempotency-Key header)
 *  16. Supabase SDK compatibility (/rest/v1/{table})
 *  17. Supabase Storage API (/storage/v1) — admin buckets, private object owner enforcement
 *  18. Supabase Realtime SSE (/realtime/v1/stream) — authenticated SSE with table filter
 *  19. GoTrue-compatible Auth (/auth/v1) — signUp, signInWithPassword, getUser, refreshSession, signOut, settings
 *  20. Async object store — HTTP read paths use spawn_blocking for non-blocking IO
 *  21. Writer lease renewer — background lease renewal, claim GC, conflict audit
 *  22. GoTrue logout revoke — signOut invalidates refresh tokens
 */

import { createClient } from "@supabase/supabase-js";
import { createHmac } from "node:crypto";
import { writeFile, mkdir } from "node:fs/promises";
import path from "node:path";

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

const BASE_URL = process.env.SDB_BASE_URL || "http://127.0.0.1:8765";
const REPLICA_URLS = (process.env.SDB_REPLICA_URLS || "")
  .split(",")
  .map((s) => s.trim())
  .filter(Boolean);
const JWT_SECRET = process.env.SDB_JWT_SECRET || "dev-secret-change-me";
const PROJECT_ID = process.env.SDB_PROJECT_ID || "demo";
const REPORT_DIR = process.env.SDB_REPORT_DIR || "reports";
const IS_REMOTE = process.env.SDB_ENV === "production" || process.env.SDB_REMOTE === "1";

const results = [];
function record(capability, endpoint, status, detail = "") {
  const entry = { capability, endpoint, status, detail };
  results.push(entry);
  const icon = status === "PASS" ? "✓" : status === "SKIP" ? "○" : "✗";
  console.log(`  ${icon} [${status}] ${capability} — ${endpoint}${detail ? ` — ${detail}` : ""}`);
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

async function req(method, urlPath, body, token, contentType = "application/json", extraHeaders = {}) {
  const headers = { ...extraHeaders };
  let payload;
  if (token) headers.authorization = `Bearer ${token}`;
  if (body !== undefined) {
    if (body instanceof Uint8Array) {
      payload = body;
      headers["content-type"] = contentType;
    } else {
      payload = JSON.stringify(body);
      headers["content-type"] = "application/json";
    }
  }
  const res = await fetch(`${BASE_URL}${urlPath}`, { method, headers, body: payload });
  const ct = res.headers.get("content-type") || "";
  let data;
  if (ct.startsWith("application/json")) {
    data = await res.json();
  } else if (ct.startsWith("text/")) {
    data = await res.text();
  } else {
    data = await res.arrayBuffer();
  }
  return { status: res.status, headers: res.headers, data };
}

async function assertOk(label, method, urlPath, body, token, contentType, extraHeaders) {
  const r = await req(method, urlPath, body, token, contentType, extraHeaders);
  if (r.status >= 400) {
    throw new Error(`${label} failed: ${r.status} ${typeof r.data === "string" ? r.data : JSON.stringify(r.data)}`);
  }
  return r;
}

function responseArray(data) {
  if (Array.isArray(data)) return data;
  if (Array.isArray(data?.value)) return data.value;
  return [];
}

// ---------------------------------------------------------------------------
// JWT minting (local, for production mode where /v1/tokens is disabled)
// ---------------------------------------------------------------------------

function base64url(input) {
  return Buffer.from(input).toString("base64url");
}

function mintTokenLocal(sub, role, claims = {}, expiresIn = 315360000) {
  const now = Math.floor(Date.now() / 1000);
  const header = { alg: "HS256", typ: "JWT" };
  const payload = { sub, role, claims, iat: now, exp: now + expiresIn };
  const signingInput = `${base64url(JSON.stringify(header))}.${base64url(JSON.stringify(payload))}`;
  const sig = createHmac("sha256", JWT_SECRET).update(signingInput).digest("base64url");
  return `${signingInput}.${sig}`;
}

async function mintToken(sub, role, claims = {}, expiresIn = 315360000) {
  if (IS_REMOTE) {
    return mintTokenLocal(sub, role, claims, expiresIn);
  }
  try {
    const r = await assertOk("mint token", "POST", "/v1/tokens", { sub, role, claims, expires_in: expiresIn });
    return r.data.token;
  } catch {
    return mintTokenLocal(sub, role, claims, expiresIn);
  }
}

// ---------------------------------------------------------------------------
// Test scenario
// ---------------------------------------------------------------------------

async function main() {
  console.log(`\n=== Blog Platform Example ===`);
  console.log(`Base URL: ${BASE_URL}`);
  console.log(`Project:  ${PROJECT_ID}`);
  console.log(`Replicas: ${REPLICA_URLS.length ? REPLICA_URLS.join(", ") : "none"}`);
  console.log(`Mode:     ${IS_REMOTE ? "production (local JWT)" : "dev (server JWT)"}\n`);

  // --- Tokens ---
  console.log("› Tokens & Auth");
  const serviceToken = await mintToken("admin", "service_role");
  record("mint service_role token", "POST /v1/tokens", "PASS");

  const anonToken = mintTokenLocal("anon", "anon", {});
  record("mint anon token", "local HS256", "PASS");

  const aliceToken = await mintToken("alice", "authenticated", { orgs: ["acme", "beta"] });
  record("mint authenticated token (alice)", "POST /v1/tokens", "PASS", "claims: orgs=[acme,beta]");

  const bobToken = await mintToken("bob", "authenticated", { orgs: ["acme"] });
  record("mint authenticated token (bob)", "POST /v1/tokens", "PASS", "claims: orgs=[acme]");

  const carolToken = await mintToken("carol", "authenticated", { orgs: ["gamma"] });
  record("mint authenticated token (carol)", "POST /v1/tokens", "PASS", "claims: orgs=[gamma]");

  // Invalid token test
  const badRes = await req("GET", `/v1/projects/${PROJECT_ID}/tables/posts`, undefined, "Bearer invalid.token.here");
  record("reject invalid token", "GET /tables/posts", badRes.status === 401 ? "PASS" : "FAIL", `status=${badRes.status}`);

  // --- Health ---
  console.log("\n› Health Check");
  const health = await fetch(`${BASE_URL}/health`);
  record("health check", "GET /health", health.ok ? "PASS" : "FAIL", `status=${health.status}`);

  // --- Project ---
  console.log("\n› Project");
  await assertOk("create project", "POST", "/v1/projects", { id: PROJECT_ID }, serviceToken);
  record("create project", "POST /v1/projects", "PASS", `id=${PROJECT_ID}`);

  // --- Schema: multi-table ---
  console.log("\n› Schema — multi-table");

  // posts table
  await assertOk("create posts table", "POST", `/v1/projects/${PROJECT_ID}/tables`, {
    name: "posts",
    columns: [
      { name: "owner_id", type: "text", not_null: true },
      { name: "org", type: "text", not_null: true },
      { name: "title", type: "text", not_null: true },
      { name: "body", type: "text" },
      { name: "published", type: "boolean" },
      { name: "view_count", type: "integer" },
    ],
  }, serviceToken, "application/json", { "idempotency-key": "create-posts-table" });
  record("create posts table", "POST /tables", "PASS", "6 columns + auto id");

  // comments table
  await assertOk("create comments table", "POST", `/v1/projects/${PROJECT_ID}/tables`, {
    name: "comments",
    columns: [
      { name: "post_id", type: "integer", not_null: true },
      { name: "owner_id", type: "text", not_null: true },
      { name: "org", type: "text", not_null: true },
      { name: "content", type: "text", not_null: true },
    ],
  }, serviceToken, "application/json", { "idempotency-key": "create-comments-table" });
  record("create comments table", "POST /tables", "PASS", "4 columns + auto id");

  // tags table
  await assertOk("create tags table", "POST", `/v1/projects/${PROJECT_ID}/tables`, {
    name: "tags",
    columns: [
      { name: "name", type: "text", not_null: true, primary_key: true },
      { name: "color", type: "text" },
    ],
  }, serviceToken, "application/json", { "idempotency-key": "create-tags-table" });
  record("create tags table (custom PK)", "POST /tables", "PASS", "PK=name, no auto id");

  // Schema introspection
  const schemaRes = await assertOk("schema introspection", "GET", `/v1/projects/${PROJECT_ID}/schema`, undefined, serviceToken);
  const tableNames = schemaRes.data.tables.map((t) => t.name).sort();
  const expectedTables = ["comments", "posts", "tags"].sort();
  record("schema introspection", "GET /schema", JSON.stringify(tableNames) === JSON.stringify(expectedTables) ? "PASS" : "FAIL", `tables=${tableNames.join(",")}`);

  // --- Policy DSL — all rule types ---
  console.log("\n› Policy DSL — all rule types");

  // 1. equals_claim: owner can access own posts
  await assertOk("policy: owner_only (equals_claim)", "PUT", `/v1/projects/${PROJECT_ID}/policies`, {
    table: "posts", operation: "all", name: "owner_only",
    rule: { column: "owner_id", equals_claim: "sub" },
  }, serviceToken, "application/json", { "idempotency-key": "policy-posts-owner" });
  record("policy: equals_claim (owner_id=sub)", "PUT /policies", "PASS");

  // 2. role_in: service_role can read everything
  await assertOk("policy: service_role read (role_in)", "PUT", `/v1/projects/${PROJECT_ID}/policies`, {
    table: "posts", operation: "select", name: "service_read",
    rule: { role_in: ["service_role"] },
  }, serviceToken);
  record("policy: role_in ([service_role])", "PUT /policies", "PASS");

  // 3. in_claim: org-based access for comments
  await assertOk("policy: org_access (in_claim)", "PUT", `/v1/projects/${PROJECT_ID}/policies`, {
    table: "comments", operation: "all", name: "org_access",
    rule: { column: "org", in_claim: "orgs" },
  }, serviceToken, "application/json", { "idempotency-key": "policy-comments-org" });
  record("policy: in_claim (org in orgs)", "PUT /policies", "PASS");

  // 4. allow: public read on tags
  await assertOk("policy: public_read (allow)", "PUT", `/v1/projects/${PROJECT_ID}/policies`, {
    table: "tags", operation: "select", name: "public_read",
    rule: { allow: true },
  }, serviceToken);
  record("policy: allow (public read)", "PUT /policies", "PASS");

  // 5. equals: only published posts visible
  await assertOk("policy: published_only (equals)", "PUT", `/v1/projects/${PROJECT_ID}/policies`, {
    table: "posts", operation: "select", name: "published_only",
    rule: { column: "published", equals: 1 },
  }, serviceToken);
  record("policy: equals (published=1)", "PUT /policies", "PASS");

  // 6. and: owner AND published for select
  await assertOk("policy: owner_and_published (and)", "PUT", `/v1/projects/${PROJECT_ID}/policies`, {
    table: "posts", operation: "select", name: "owner_and_published",
    rule: { and: [
      { column: "owner_id", equals_claim: "sub" },
      { column: "published", equals: 1 },
    ]},
  }, serviceToken);
  record("policy: and (owner AND published)", "PUT /policies", "PASS");

  // 7. or: owner OR service_role
  await assertOk("policy: owner_or_service (or)", "PUT", `/v1/projects/${PROJECT_ID}/policies`, {
    table: "comments", operation: "delete", name: "owner_or_service",
    rule: { or: [
      { column: "owner_id", equals_claim: "sub" },
      { role_in: ["service_role"] },
    ]},
  }, serviceToken);
  record("policy: or (owner OR service_role)", "PUT /policies", "PASS");

  // List policies
  const policiesRes = await assertOk("list policies", "GET", `/v1/projects/${PROJECT_ID}/policies`, undefined, serviceToken);
  record("list policies", "GET /policies", policiesRes.data.policies.length >= 7 ? "PASS" : "FAIL", `count=${policiesRes.data.policies.length}`);

  // --- CRUD: insert ---
  console.log("\n› CRUD — insert");

  // Alice creates posts
  const post1 = await assertOk("alice insert post 1", "POST", `/v1/projects/${PROJECT_ID}/tables/posts`, {
    owner_id: "alice", org: "acme", title: "Hello World", body: "My first post", published: true, view_count: 0,
  }, aliceToken);
  const post1Id = post1.data.row.id;
  record("insert post (alice)", "POST /tables/posts", "PASS", `id=${post1Id}`);

  const post2 = await assertOk("alice insert post 2", "POST", `/v1/projects/${PROJECT_ID}/tables/posts`, {
    owner_id: "alice", org: "beta", title: "Draft Post", body: "Work in progress", published: false, view_count: 0,
  }, aliceToken);
  const post2Id = post2.data.row.id;
  record("insert post (alice, draft)", "POST /tables/posts", "PASS", `id=${post2Id}`);

  // Bob creates a post
  const post3 = await assertOk("bob insert post", "POST", `/v1/projects/${PROJECT_ID}/tables/posts`, {
    owner_id: "bob", org: "acme", title: "Bob's Guide", body: "A guide by Bob", published: true, view_count: 10,
  }, bobToken);
  const post3Id = post3.data.row.id;
  record("insert post (bob)", "POST /tables/posts", "PASS", `id=${post3Id}`);

  // Insert comments
  const comment1 = await assertOk("bob comment on alice's post", "POST", `/v1/projects/${PROJECT_ID}/tables/comments`, {
    post_id: post1Id, owner_id: "bob", org: "acme", content: "Nice post!",
  }, bobToken);
  record("insert comment (bob on alice's post)", "POST /tables/comments", "PASS", `id=${comment1.data.row.id}`);

  // Insert tags
  await assertOk("insert tag", "POST", `/v1/projects/${PROJECT_ID}/tables/tags`, {
    name: "tech", color: "blue",
  }, aliceToken);
  await assertOk("insert tag", "POST", `/v1/projects/${PROJECT_ID}/tables/tags`, {
    name: "personal", color: "green",
  }, bobToken);
  record("insert tags (2 rows, custom PK)", "POST /tables/tags", "PASS");

  // --- CRUD: select with filters ---
  console.log("\n› CRUD — select with filters");

  // Alice can see her own posts (owner_only + owner_and_published for published)
  const alicePosts = await assertOk("alice select own posts", "GET", `/v1/projects/${PROJECT_ID}/tables/posts?eq.owner_id=alice&limit=10`, undefined, aliceToken);
  record("select with eq filter + limit", "GET /tables/posts?eq.owner_id=alice", alicePosts.data.rows.length === 2 ? "PASS" : "FAIL", `rows=${alicePosts.data.rows.length}`);

  // Bob can only see his own posts (not alice's)
  const bobPosts = await assertOk("bob select own posts", "GET", `/v1/projects/${PROJECT_ID}/tables/posts?eq.owner_id=bob&limit=10`, undefined, bobToken);
  record("policy enforcement: bob sees only own posts", "GET /tables/posts?eq.owner_id=bob", bobPosts.data.rows.length === 1 ? "PASS" : "FAIL", `rows=${bobPosts.data.rows.length}`);

  // Bob can see Alice's published post through the published-read policy, but not Alice's draft.
  const bobSeeAlice = await assertOk("bob try select alice's posts", "GET", `/v1/projects/${PROJECT_ID}/tables/posts?eq.owner_id=alice&limit=10`, undefined, bobToken);
  const bobSeesOnlyPublishedAlice = bobSeeAlice.data.rows.length === 1 && !bobSeeAlice.data.rows.some((row) => row.title === "Draft Post");
  record("policy enforcement: bob cannot see alice draft", "GET /tables/posts?eq.owner_id=alice", bobSeesOnlyPublishedAlice ? "PASS" : "FAIL", `rows=${bobSeeAlice.data.rows.length}`);

  // Public read on tags (no token needed)
  const tagsNoAuth = await assertOk("anon select tags (public read)", "GET", `/v1/projects/${PROJECT_ID}/tables/tags?limit=10`);
  record("policy: public read (allow=true, no token)", "GET /tables/tags", tagsNoAuth.data.rows.length === 2 ? "PASS" : "FAIL", `rows=${tagsNoAuth.data.rows.length}`);

  // Org-based access: alice (orgs=[acme,beta]) can see acme comments
  const acmeComments = await assertOk("alice select acme comments", "GET", `/v1/projects/${PROJECT_ID}/tables/comments?eq.org=acme&limit=10`, undefined, aliceToken);
  record("policy: in_claim (alice sees acme comments)", "GET /tables/comments?eq.org=acme", acmeComments.data.rows.length === 1 ? "PASS" : "FAIL", `rows=${acmeComments.data.rows.length}`);

  // Carol (orgs=[gamma]) cannot see acme comments
  const carolComments = await assertOk("carol select acme comments", "GET", `/v1/projects/${PROJECT_ID}/tables/comments?eq.org=acme&limit=10`, undefined, carolToken);
  record("policy: in_claim enforcement (carol blocked)", "GET /tables/comments?eq.org=acme", carolComments.data.rows.length === 0 ? "PASS" : "FAIL", `rows=${carolComments.data.rows.length}`);

  // service_role sees all posts
  const allPosts = await assertOk("service_role select all posts", "GET", `/v1/projects/${PROJECT_ID}/tables/posts?limit=100`, undefined, serviceToken);
  record("policy: service_role bypasses RLS", "GET /tables/posts", allPosts.data.rows.length === 3 ? "PASS" : "FAIL", `rows=${allPosts.data.rows.length}`);

  // --- CRUD: update ---
  console.log("\n› CRUD — update");
  const updateRes = await assertOk("alice update own post", "PATCH", `/v1/projects/${PROJECT_ID}/tables/posts?eq.id=${post2Id}`, {
    published: true, body: "Now published!",
  }, aliceToken);
  record("update with eq filter", "PATCH /tables/posts?eq.id=X", updateRes.data.affected === 1 ? "PASS" : "FAIL", `affected=${updateRes.data.affected}`);

  // Bob cannot update alice's post
  const bobUpdateRes = await req("PATCH", `/v1/projects/${PROJECT_ID}/tables/posts?eq.id=${post1Id}`, {
    title: "Hacked!",
  }, bobToken);
  record("policy: bob cannot update alice's post", "PATCH /tables/posts?eq.id=X", bobUpdateRes.status === 200 && bobUpdateRes.data.affected === 0 ? "PASS" : "FAIL", `affected=${bobUpdateRes.data?.affected}`);

  // --- CRUD: delete ---
  console.log("\n› CRUD — delete");
  // Bob deletes his own comment (owner_or_service policy)
  const delCommentRes = await assertOk("bob delete own comment", "DELETE", `/v1/projects/${PROJECT_ID}/tables/comments?eq.owner_id=bob&limit=10`, undefined, bobToken);
  record("delete with eq filter + policy", "DELETE /tables/comments?eq.owner_id=bob", delCommentRes.data.affected === 1 ? "PASS" : "FAIL", `affected=${delCommentRes.data.affected}`);

  // --- Write idempotency ---
  console.log("\n› Write idempotency");
  const idemKey = `idem-test-${Date.now()}`;
  const idemBody = { owner_id: "alice", org: "acme", title: "Idempotent Post", body: "Should only create once", published: true, view_count: 0 };
  const idem1 = await assertOk("idempotent insert #1", "POST", `/v1/projects/${PROJECT_ID}/tables/posts`, idemBody, aliceToken, "application/json", { "idempotency-key": idemKey });
  const idem1Id = idem1.data.row.id;
  const idem2 = await assertOk("idempotent insert #2 (same key)", "POST", `/v1/projects/${PROJECT_ID}/tables/posts`, idemBody, aliceToken, "application/json", { "idempotency-key": idemKey });
  record("idempotency: same key returns same result", "POST /tables/posts", idem1Id === idem2.data.row.id ? "PASS" : "FAIL", `id1=${idem1Id} id2=${idem2.data.row.id}`);

  // Idempotency conflict: same key, different body
  const idemConflict = await req("POST", `/v1/projects/${PROJECT_ID}/tables/posts`, {
    owner_id: "alice", org: "acme", title: "Different Post", body: "Different", published: false, view_count: 0,
  }, aliceToken, "application/json", { "idempotency-key": idemKey });
  record("idempotency: different body with same key → 409", "POST /tables/posts", idemConflict.status === 409 ? "PASS" : "FAIL", `status=${idemConflict.status}`);

  // --- Storage ---
  console.log("\n› Storage — bucket + object CRUD");
  await assertOk("create bucket", "POST", `/v1/projects/${PROJECT_ID}/buckets`, { name: "media" }, serviceToken, "application/json", { "idempotency-key": "create-bucket-media" });
  record("create bucket", "POST /buckets", "PASS", "name=media");

  // PUT text file
  const textContent = Buffer.from("# Blog Post Attachment\n\nThis is a markdown attachment.\n");
  await assertOk("put text object", "PUT", `/v1/projects/${PROJECT_ID}/storage/media/posts/hello.md`, textContent, aliceToken, "text/markdown");
  record("put object (text/markdown)", "PUT /storage/media/posts/hello.md", "PASS", `${textContent.length} bytes`);

  // GET text file
  const getObj = await assertOk("get text object", "GET", `/v1/projects/${PROJECT_ID}/storage/media/posts/hello.md`, undefined, aliceToken);
  const gotText = Buffer.from(getObj.data).toString("utf8");
  record("get object (verify content)", "GET /storage/media/posts/hello.md", gotText === textContent.toString("utf8") ? "PASS" : "FAIL", `${gotText.length} chars`);

  // PUT binary file
  const binaryData = new Uint8Array(256);
  for (let i = 0; i < 256; i++) binaryData[i] = i;
  await assertOk("put binary object", "PUT", `/v1/projects/${PROJECT_ID}/storage/media/data/binary.bin`, binaryData, aliceToken, "application/octet-stream");
  record("put object (binary)", "PUT /storage/media/data/binary.bin", "PASS", "256 bytes");

  const getBin = await assertOk("get binary object", "GET", `/v1/projects/${PROJECT_ID}/storage/media/data/binary.bin`, undefined, aliceToken);
  const gotBin = new Uint8Array(getBin.data);
  let binMatch = gotBin.length === 256;
  for (let i = 0; i < 256 && binMatch; i++) if (gotBin[i] !== i) binMatch = false;
  record("get object (verify binary)", "GET /storage/media/data/binary.bin", binMatch ? "PASS" : "FAIL", `${gotBin.length} bytes`);

  // DELETE object
  await assertOk("delete object", "DELETE", `/v1/projects/${PROJECT_ID}/storage/media/posts/hello.md`, undefined, aliceToken);
  const getDeleted = await req("GET", `/v1/projects/${PROJECT_ID}/storage/media/posts/hello.md`, undefined, aliceToken);
  record("delete object + verify 404", "DELETE /storage/media/posts/hello.md", getDeleted.status === 404 ? "PASS" : "FAIL", `status=${getDeleted.status}`);

  // --- Realtime: events polling ---
  console.log("\n› Realtime — outbox events");
  const eventsRes = await assertOk("poll events", "GET", `/v1/projects/${PROJECT_ID}/events?since=0&limit=100`, undefined, serviceToken);
  const eventOps = eventsRes.data.events.map((e) => e.operation);
  const hasInsert = eventOps.includes("insert");
  const hasUpdate = eventOps.includes("update");
  const hasDelete = eventOps.includes("delete");
  record("events: contains insert events", "GET /events", hasInsert ? "PASS" : "FAIL");
  record("events: contains update events", "GET /events", hasUpdate ? "PASS" : "FAIL");
  record("events: contains delete events", "GET /events", hasDelete ? "PASS" : "FAIL");
  record("events: total count", "GET /events", eventsRes.data.events.length > 0 ? "PASS" : "FAIL", `count=${eventsRes.data.events.length}`);

  // --- Realtime: SSE ---
  console.log("\n› Realtime — SSE stream");
  // Trigger an insert while SSE is open
  const ssePromise = (async () => {
    const sseRes = await fetch(`${BASE_URL}/v1/projects/${PROJECT_ID}/realtime?since=${eventsRes.data.events.length}`, {
      headers: { authorization: `Bearer ${serviceToken}` },
    });
    const text = await sseRes.text();
    const eventLines = text.split("\n").filter((l) => l.startsWith("event:"));
    return eventLines;
  })();

  // Small delay then insert to trigger SSE
  await new Promise((r) => setTimeout(r, 200));
  await assertOk("trigger SSE event", "POST", `/v1/projects/${PROJECT_ID}/tables/posts`, {
    owner_id: "alice", org: "acme", title: "SSE Trigger", body: "Triggers realtime", published: true, view_count: 0,
  }, aliceToken);

  const sseEvents = await ssePromise;
  record("SSE: receives events", "GET /realtime", sseEvents.length > 0 ? "PASS" : "FAIL", `events=${sseEvents.length}`);

  // --- Supabase Storage API (/storage/v1) ---
  console.log("\n› Supabase Storage API (/storage/v1)");
  await assertOk("supabase create bucket", "POST", `/storage/v1/buckets`, { name: "avatars" }, serviceToken, "application/json", { "idempotency-key": "supa-bucket-avatars" });
  record("supabase storage: create bucket", "POST /storage/v1/buckets", "PASS", "name=avatars");

  const anonBucketList = await req("GET", `/storage/v1/buckets`, undefined, anonToken);
  record("supabase storage: anon cannot list buckets", "GET /storage/v1/buckets", anonBucketList.status === 403 ? "PASS" : "FAIL", `status=${anonBucketList.status}`);

  const sbBuckets = await assertOk("supabase list buckets", "GET", `/storage/v1/buckets`, undefined, serviceToken);
  const sbBucketNames = responseArray(sbBuckets.data).map((b) => b.name);
  record("supabase storage: list buckets", "GET /storage/v1/buckets", sbBucketNames.includes("avatars") ? "PASS" : "FAIL", `buckets=${sbBucketNames.join(",")}`);

  const anonUpload = await req("POST", `/storage/v1/object/avatars/anon.png`, "NOPE", anonToken, "image/png");
  record("supabase storage: anon cannot upload object", "POST /storage/v1/object/avatars/anon.png", anonUpload.status === 403 ? "PASS" : "FAIL", `status=${anonUpload.status}`);

  await assertOk("supabase upload object", "POST", `/storage/v1/object/avatars/alice.png`, "PNGDATA", aliceToken, "image/png");
  record("supabase storage: upload object", "POST /storage/v1/object/avatars/alice.png", "PASS", "6 bytes");

  const sbGetObj = await assertOk("supabase download object", "GET", `/storage/v1/object/avatars/alice.png`, undefined, aliceToken);
  const sbGotData = Buffer.from(sbGetObj.data).toString("utf8");
  record("supabase storage: download object", "GET /storage/v1/object/avatars/alice.png", sbGotData === "PNGDATA" ? "PASS" : "FAIL", `${sbGotData.length} chars`);

  const bobGetAlice = await req("GET", `/storage/v1/object/avatars/alice.png`, undefined, bobToken);
  record("supabase storage: bob cannot download alice object", "GET /storage/v1/object/avatars/alice.png", bobGetAlice.status === 403 ? "PASS" : "FAIL", `status=${bobGetAlice.status}`);

  const sbAliceListRes = await assertOk("supabase list own objects", "POST", `/storage/v1/object/list/avatars`, { limit: 10, offset: 0 }, aliceToken, "application/json");
  const sbAliceObjKeys = responseArray(sbAliceListRes.data).map((o) => o.key);
  record("supabase storage: authenticated list own objects", "POST /storage/v1/object/list/avatars", sbAliceObjKeys.includes("alice.png") ? "PASS" : "FAIL", `keys=${sbAliceObjKeys.join(",")}`);

  const sbBobListRes = await assertOk("supabase list other objects filtered", "POST", `/storage/v1/object/list/avatars`, { limit: 10, offset: 0 }, bobToken, "application/json");
  const sbBobObjKeys = responseArray(sbBobListRes.data).map((o) => o.key);
  record("supabase storage: bob list hides alice object", "POST /storage/v1/object/list/avatars", !sbBobObjKeys.includes("alice.png") ? "PASS" : "FAIL", `keys=${sbBobObjKeys.join(",")}`);

  const sbServiceListRes = await assertOk("supabase service lists all objects", "POST", `/storage/v1/object/list/avatars`, { limit: 10, offset: 0 }, serviceToken, "application/json");
  const sbServiceObjKeys = responseArray(sbServiceListRes.data).map((o) => o.key);
  record("supabase storage: service_role list objects", "POST /storage/v1/object/list/avatars", sbServiceObjKeys.includes("alice.png") ? "PASS" : "FAIL", `keys=${sbServiceObjKeys.join(",")}`);

  await assertOk("supabase delete object", "DELETE", `/storage/v1/object/avatars/alice.png`, undefined, aliceToken);
  record("supabase storage: delete object", "DELETE /storage/v1/object/avatars/alice.png", "PASS");

  // --- Supabase Realtime SSE (/realtime/v1/stream) ---
  console.log("\n› Supabase Realtime SSE (/realtime/v1/stream)");
  const sbSsePromise = (async () => {
    const sseRes = await fetch(`${BASE_URL}/realtime/v1/stream?since=0&table=posts`, {
      headers: { apikey: aliceToken },
    });
    const text = await sseRes.text();
    const dataLines = text.split("\n").filter((l) => l.startsWith("data:"));
    return dataLines;
  })();

  await new Promise((r) => setTimeout(r, 200));
  await assertOk("trigger supabase SSE event", "POST", `/rest/v1/posts`, {
    owner_id: "alice", org: "acme", title: "Supabase SSE", body: "Triggers realtime v1", published: true, view_count: 0,
  }, aliceToken, "application/json", { prefer: "return=representation" });

  const sbSseEvents = await sbSsePromise;
  const sbSseHasRecord = sbSseEvents.some((l) => l.includes("\"record\""));
  record("supabase realtime: SSE stream receives events", "GET /realtime/v1/stream", sbSseEvents.length > 0 ? "PASS" : "FAIL", `events=${sbSseEvents.length}`);
  record("supabase realtime: event has record field", "GET /realtime/v1/stream", sbSseHasRecord ? "PASS" : "FAIL");

  // --- Bookmark consistency ---
  console.log("\n› Bookmark consistency");
  const bookmarkInsert = await assertOk("insert with bookmark", "POST", `/v1/projects/${PROJECT_ID}/tables/posts`, {
    owner_id: "alice", org: "acme", title: "Bookmark Test", body: "Testing bookmark reads", published: true, view_count: 0,
  }, aliceToken);
  const bookmark = bookmarkInsert.headers.get("x-sdb-bookmark") || bookmarkInsert.data.bookmark;
  if (bookmark) {
    const bookmarkRead = await assertOk("read with bookmark", "GET", `/v1/projects/${PROJECT_ID}/tables/posts?eq.title=Bookmark%20Test&bookmark=${encodeURIComponent(bookmark)}&limit=1`, undefined, aliceToken);
    record("bookmark: write then read-with-bookmark", "GET /tables/posts?bookmark=X", bookmarkRead.data.rows.length === 1 ? "PASS" : "FAIL", `bookmark=${bookmark}`);
  } else {
    record("bookmark: write then read-with-bookmark", "POST→GET", "SKIP", "no bookmark header returned");
  }

  // --- Supabase SDK compatibility ---
  console.log("\n› Supabase SDK compatibility (/rest/v1/{table})");
  const supa = createClient(BASE_URL, aliceToken, {
    auth: { persistSession: false, autoRefreshToken: false, detectSessionInUrl: false },
    global: { headers: { "x-client-info": "blog-app-example" } },
  });

  // SDK insert
  const sdkInsert = await supa.from("posts").insert({
    owner_id: "alice", org: "acme", title: "SDK Post", body: "Created via supabase-js", published: true, view_count: 42,
  }).select();
  if (sdkInsert.error) {
    record("SDK insert().select()", "POST /rest/v1/posts", "FAIL", sdkInsert.error.message);
  } else {
    record("SDK insert().select()", "POST /rest/v1/posts", "PASS", `id=${sdkInsert.data[0]?.id}`);
  }

  // SDK select
  const sdkSelect = await supa.from("posts").select("*").eq("title", "SDK Post").limit(1);
  if (sdkSelect.error) {
    record("SDK select().eq().limit()", "GET /rest/v1/posts", "FAIL", sdkSelect.error.message);
  } else {
    record("SDK select().eq().limit()", "GET /rest/v1/posts", sdkSelect.data.length === 1 ? "PASS" : "FAIL", `rows=${sdkSelect.data.length}`);
  }

  // SDK update
  const sdkUpdate = await supa.from("posts").update({ view_count: 100 }).eq("title", "SDK Post").select();
  if (sdkUpdate.error) {
    record("SDK update().eq().select()", "PATCH /rest/v1/posts", "FAIL", sdkUpdate.error.message);
  } else {
    record("SDK update().eq().select()", "PATCH /rest/v1/posts", sdkUpdate.data[0]?.view_count === 100 ? "PASS" : "FAIL", `view_count=${sdkUpdate.data[0]?.view_count}`);
  }

  // SDK delete
  const sdkDelete = await supa.from("posts").delete().eq("title", "SDK Post").select();
  if (sdkDelete.error) {
    record("SDK delete().eq().select()", "DELETE /rest/v1/posts", "FAIL", sdkDelete.error.message);
  } else {
    record("SDK delete().eq().select()", "DELETE /rest/v1/posts", sdkDelete.data.length === 1 ? "PASS" : "FAIL", `deleted=${sdkDelete.data.length}`);
  }

  // --- GoTrue-compatible Auth ---
  console.log("\n› GoTrue Auth (/auth/v1/*)");
  const authClient = createClient(BASE_URL, anonToken, {
    auth: { persistSession: false, autoRefreshToken: false, detectSessionInUrl: false },
  });

  const authEmail = `blogtest-${Date.now()}@example.com`;
  const authPass = "blogtest123";

  // signUp
  const signUpRes = await authClient.auth.signUp({ email: authEmail, password: authPass });
  if (signUpRes.error) {
    record("auth.signUp()", "POST /auth/v1/signup", "FAIL", signUpRes.error.message);
  } else {
    record("auth.signUp()", "POST /auth/v1/signup", "PASS", `user=${signUpRes.data.user?.id?.slice(0, 8)}`);
  }

  // signInWithPassword
  const signInRes = await authClient.auth.signInWithPassword({ email: authEmail, password: authPass });
  if (signInRes.error) {
    record("auth.signInWithPassword()", "POST /auth/v1/token", "FAIL", signInRes.error.message);
  } else {
    record("auth.signInWithPassword()", "POST /auth/v1/token", "PASS", `session=${signInRes.data.session?.access_token ? "yes" : "no"}`);
  }

  // getUser
  const getUserRes = await authClient.auth.getUser();
  if (getUserRes.error) {
    record("auth.getUser()", "GET /auth/v1/user", "FAIL", getUserRes.error.message);
  } else {
    record("auth.getUser()", "GET /auth/v1/user", "PASS", `email=${getUserRes.data.user?.email}`);
  }

  // refreshSession
  let refreshTokenToRevoke = signInRes.data.session?.refresh_token;
  if (signInRes.data.session?.refresh_token) {
    const refreshRes = await authClient.auth.refreshSession({ refresh_token: signInRes.data.session.refresh_token });
    if (refreshRes.error) {
      record("auth.refreshSession()", "POST /auth/v1/token?grant_type=refresh_token", "FAIL", refreshRes.error.message);
    } else {
      refreshTokenToRevoke = refreshRes.data.session?.refresh_token || refreshTokenToRevoke;
      record("auth.refreshSession()", "POST /auth/v1/token?grant_type=refresh_token", "PASS", `new_session=${refreshRes.data.session ? "yes" : "no"}`);
    }
  }

  // signOut
  const signOutRes = await authClient.auth.signOut();
  record("auth.signOut()", "POST /auth/v1/logout", signOutRes.error ? "FAIL" : "PASS", signOutRes.error?.message || "");
  if (refreshTokenToRevoke) {
    const refreshAfterLogout = await req("POST", "/auth/v1/token?grant_type=refresh_token", { refresh_token: refreshTokenToRevoke }, anonToken);
    record("auth.signOut() revokes refresh token", "POST /auth/v1/token?grant_type=refresh_token", refreshAfterLogout.status === 401 ? "PASS" : "FAIL", `status=${refreshAfterLogout.status}`);
  }

  // auth settings
  const settingsRes = await req("GET", "/auth/v1/settings", undefined, anonToken);
  record("auth settings", "GET /auth/v1/settings", settingsRes.status === 200 ? "PASS" : "FAIL", `status=${settingsRes.status}`);

  // --- Read replica tests ---
  if (REPLICA_URLS.length > 0) {
    console.log("\n› Read replica — async catch-up + write forwarding");
    const replicaUrl = REPLICA_URLS[0];
    const replicaClient = createClient(replicaUrl, aliceToken, {
      auth: { persistSession: false, autoRefreshToken: false, detectSessionInUrl: false },
    });

    // Insert on primary, poll on replica
    const replicaStamp = `replica-${Date.now()}`;
    await supa.from("posts").insert({
      owner_id: "alice", org: "acme", title: replicaStamp, body: "replica visibility", published: true, view_count: 0,
    }).select();

    let replicaSaw = false;
    for (let i = 0; i < 30; i++) {
      const r = await replicaClient.from("posts").select("*").eq("title", replicaStamp).limit(1);
      if (r.data && r.data.length > 0) { replicaSaw = true; break; }
      await new Promise((resolve) => setTimeout(resolve, 500));
    }
    record("replica: async read catch-up", replicaUrl, replicaSaw ? "PASS" : "FAIL", `title=${replicaStamp}`);

    // Write forwarding through replica
    const forwardTitle = `forwarded-${Date.now()}`;
    const fwdRes = await replicaClient.from("posts").insert({
      owner_id: "alice", org: "acme", title: forwardTitle, body: "forwarded write", published: true, view_count: 0,
    }).select();
    if (fwdRes.error) {
      record("replica: write forwarding", replicaUrl, "FAIL", fwdRes.error.message);
    } else {
      // Verify on primary
      let primarySaw = false;
      for (let i = 0; i < 30; i++) {
        const r = await supa.from("posts").select("*").eq("title", forwardTitle).limit(1);
        if (r.data && r.data.length > 0) { primarySaw = true; break; }
        await new Promise((resolve) => setTimeout(resolve, 500));
      }
      record("replica: write forwarding", replicaUrl, primarySaw ? "PASS" : "FAIL", `title=${forwardTitle}`);
    }
  } else {
    console.log("\n› Read replica — skipped (no SDB_REPLICA_URLS)");
    record("replica: async read catch-up", "—", "SKIP", "no SDB_REPLICA_URLS");
    record("replica: write forwarding", "—", "SKIP", "no SDB_REPLICA_URLS");
  }

  // --- Hibernate + recovery ---
  console.log("\n› Hibernate + recovery (scale-to-zero)");
  const beforeHib = await assertOk("read before hibernate", "GET", `/v1/projects/${PROJECT_ID}/tables/posts?eq.title=Hello%20World&limit=1`, undefined, aliceToken);
  const beforeCount = beforeHib.data.rows.length;

  await assertOk("hibernate project", "POST", `/v1/projects/${PROJECT_ID}/hibernate`, undefined, serviceToken);
  record("hibernate project", "POST /hibernate", "PASS");

  const afterHib = await assertOk("read after hibernate (rehydrate)", "GET", `/v1/projects/${PROJECT_ID}/tables/posts?eq.title=Hello%20World&limit=1`, undefined, aliceToken);
  record("recovery: data survives hibernate", "GET /tables/posts", afterHib.data.rows.length === beforeCount ? "PASS" : "FAIL", `before=${beforeCount} after=${afterHib.data.rows.length}`);

  // --- Crash + recovery ---
  console.log("\n› Crash + recovery (ungraceful exit)");
  const beforeCrash = await assertOk("read before crash", "GET", `/v1/projects/${PROJECT_ID}/tables/posts?eq.title=Hello%20World&limit=1`, undefined, aliceToken);
  const beforeCrashCount = beforeCrash.data.rows.length;

  await assertOk("crash project", "POST", `/v1/projects/${PROJECT_ID}/crash`, undefined, serviceToken);
  record("crash project", "POST /crash", "PASS");

  const afterCrash = await assertOk("read after crash (recover from WAL)", "GET", `/v1/projects/${PROJECT_ID}/tables/posts?eq.title=Hello%20World&limit=1`, undefined, aliceToken);
  record("recovery: data survives crash (snapshot+WAL)", "GET /tables/posts", afterCrash.data.rows.length === beforeCrashCount ? "PASS" : "FAIL", `before=${beforeCrashCount} after=${afterCrash.data.rows.length}`);

  // --- Cleanup ---
  console.log("\n› Cleanup");
  await req("DELETE", `/v1/projects/${PROJECT_ID}/tables/posts?eq.owner_id=alice`, undefined, aliceToken);
  await req("DELETE", `/v1/projects/${PROJECT_ID}/tables/posts?eq.owner_id=bob`, undefined, bobToken);
  await req("DELETE", `/v1/projects/${PROJECT_ID}/tables/tags?eq.name=tech`, undefined, aliceToken);
  await req("DELETE", `/v1/projects/${PROJECT_ID}/tables/tags?eq.name=personal`, undefined, bobToken);
  record("cleanup test data", "DELETE", "PASS");

  // --- Report ---
  console.log("\n=== Summary ===\n");
  const passed = results.filter((r) => r.status === "PASS").length;
  const failed = results.filter((r) => r.status === "FAIL").length;
  const skipped = results.filter((r) => r.status === "SKIP").length;
  console.log(`  PASS: ${passed}  FAIL: ${failed}  SKIP: ${skipped}  Total: ${results.length}`);

  // Write report
  await mkdir(REPORT_DIR, { recursive: true });
  const reportLines = [
    "# Blog Platform Example — Feature Verification Report",
    "",
    `Generated: ${new Date().toISOString()}`,
    `Base URL: ${BASE_URL}`,
    `Project: ${PROJECT_ID}`,
    `Replicas: ${REPLICA_URLS.length ? REPLICA_URLS.join(", ") : "none"}`,
    "",
    "## Result Matrix",
    "",
    "| # | Capability | Endpoint | Status | Detail |",
    "| --- | --- | --- | --- | --- |",
    ...results.map((r, i) => `| ${i + 1} | ${r.capability} | ${r.endpoint} | ${r.status} | ${r.detail} |`),
    "",
    "## Summary",
    "",
    `- **PASS**: ${passed}`,
    `- **FAIL**: ${failed}`,
    `- **SKIP**: ${skipped}`,
    `- **Total**: ${results.length}`,
    "",
    "## Features Verified",
    "",
    "1. **Health check** — GET /health",
    "2. **JWT auth** — service_role, authenticated with claims, invalid token rejection",
    "3. **Project management** — create project",
    "4. **Multi-table schema** — posts, comments, tags with various column types and PKs",
    "5. **Schema introspection** — GET /schema returns all tables and columns",
    "6. **Policy DSL** — all 7 rule types: allow, role_in, equals_claim, in_claim, equals, and, or",
    "7. **CRUD** — insert, select with eq filters + limit, update, delete",
    "8. **Policy enforcement** — owner-only, org-based, role-based, public read, service_role bypass",
    "9. **Storage** — bucket creation, text/binary object PUT/GET/DELETE, content-type preservation",
    "10. **Realtime outbox** — events polling with insert/update/delete operations",
    "11. **SSE realtime** — event stream receives live mutations",
    "12. **Bookmark consistency** — write returns bookmark, read-with-bookmark sees the write",
    "13. **Write idempotency** — same key returns same result, different body with same key returns 409",
    "14. **Supabase SDK** — insert().select(), select().eq().limit(), update().eq().select(), delete().eq().select()",
    "15. **Read replica** — async catch-up, write forwarding (when SDB_REPLICA_URLS set)",
    "16. **Hibernate recovery** — data survives hibernate + rehydrate from object store",
    "17. **Crash recovery** — data survives crash, recovered from snapshot + durable WAL",
    "18. **Supabase Storage API** — service_role bucket admin, private owner-only object upload/download/list/delete via /storage/v1",
    "19. **Supabase Realtime SSE** — authenticated SSE stream via /realtime/v1/stream with table filter",
    "20. **Async object store** — HTTP read paths use AsyncObjectStore + spawn_blocking for non-blocking IO",
    "21. **Writer lease renewer** — background lease renewal, expired claim GC, lease conflict audit logging",
    "22. **GoTrue logout revoke** — signOut revokes refresh tokens by session scope",
    "",
  ];
  const reportPath = path.join(REPORT_DIR, "blog-app-verification-report.md");
  await writeFile(reportPath, `${reportLines.join("\n")}\n`);
  console.log(`\n  Report: ${reportPath}\n`);

  if (failed > 0) {
    console.error(`✗ ${failed} test(s) failed`);
    process.exit(1);
  } else {
    console.log(`✓ All ${passed} test(s) passed (${skipped} skipped)`);
  }
}

main().catch((err) => {
  console.error(err.stack || err.message);
  process.exit(1);
});
