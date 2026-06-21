#!/usr/bin/env node
/**
 * Supabase JS SDK Compatibility Test Suite
 *
 * Tests the serverless-db Rust core against the real @supabase/supabase-js SDK,
 * covering PostgREST CRUD, filters, transforms, RLS/policy enforcement,
 * GoTrue-compatible auth (signup/signin/signout/getUser/refresh), and Storage.
 *
 * Usage:
 *   npm run core:dev          # start the Rust server
 *   npm run test:sdk          # run this test suite
 */

import { createClient } from "@supabase/supabase-js";
import { createHmac } from "node:crypto";
import { test, describe, before, after, beforeEach, afterEach } from "node:test";
import { strictEqual, notStrictEqual, ok, deepStrictEqual, fail } from "node:assert";

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

const BASE_URL = process.env.SDB_BASE_URL || "http://127.0.0.1:8765";
const JWT_SECRET = process.env.SDB_JWT_SECRET || "dev-secret-change-me";
const PROJECT_ID = process.env.SDB_PROJECT_ID || "demo";
const ANON_KEY = mintJwtLocal("anon", "anon", {}, 315360000);
let clientCounter = 0;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function base64url(input) {
  return Buffer.from(input).toString("base64url");
}

function mintJwtLocal(sub, role, claims = {}, expiresIn = 315360000) {
  const now = Math.floor(Date.now() / 1000);
  const header = { alg: "HS256", typ: "JWT" };
  const payload = { sub, role, claims, iat: now, exp: now + expiresIn };
  const signingInput = `${base64url(JSON.stringify(header))}.${base64url(JSON.stringify(payload))}`;
  const sig = createHmac("sha256", JWT_SECRET).update(signingInput).digest("base64url");
  return `${signingInput}.${sig}`;
}

async function mintToken(sub, role, claims = {}, expiresIn = 315360000) {
  try {
    const res = await fetch(`${BASE_URL}/v1/tokens`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ sub, role, claims, expires_in: expiresIn }),
    });
    if (res.ok) {
      const data = await res.json();
      return data.token;
    }
  } catch {}
  return mintJwtLocal(sub, role, claims, expiresIn);
}

async function adminReq(method, path, body) {
  const serviceToken = await mintToken("admin", "service_role");
  const headers = { authorization: `Bearer ${serviceToken}`, "content-type": "application/json" };
  const res = await fetch(`${BASE_URL}${path}`, {
    method,
    headers,
    body: body !== undefined ? JSON.stringify(body) : undefined,
  });
  const ct = res.headers.get("content-type") || "";
  const data = ct.includes("json") ? await res.json() : await res.text();
  return { status: res.status, data };
}

function makeClient(token) {
  clientCounter += 1;
  return createClient(BASE_URL, token, {
    auth: { persistSession: false, autoRefreshToken: false },
    global: { headers: { "x-forwarded-for": `sdk-client-${clientCounter}` } },
  });
}

function uniqueEmail() {
  return `test-${Date.now()}-${Math.random().toString(36).slice(2, 8)}@example.com`;
}

async function refreshWithToken(refreshToken) {
  return fetch(`${BASE_URL}/auth/v1/token?grant_type=refresh_token`, {
    method: "POST",
    headers: {
      apikey: ANON_KEY,
      "content-type": "application/json",
      "x-forwarded-for": `sdk-refresh-${Date.now()}-${Math.random()}`,
    },
    body: JSON.stringify({ refresh_token: refreshToken }),
  });
}

async function storageReq(method, path, token, body, contentType = "application/json") {
  const headers = { apikey: token };
  if (token) {
    headers.authorization = `Bearer ${token}`;
  }
  let payload;
  if (body !== undefined) {
    payload = body;
    headers["content-type"] = contentType;
  }
  return fetch(`${BASE_URL}${path}`, { method, headers, body: payload });
}

// ---------------------------------------------------------------------------
// Test suite
// ---------------------------------------------------------------------------

describe("Supabase JS SDK Compatibility Tests", () => {
  let serviceToken;
  let supabaseAnon;
  let supabaseService;

  before(async () => {
    serviceToken = await mintToken("admin", "service_role");
    supabaseAnon = makeClient(ANON_KEY);
    supabaseService = makeClient(serviceToken);

    // Create project
    await adminReq("POST", "/v1/projects", { id: PROJECT_ID });

    // Create tables for PostgREST tests
    await adminReq("POST", `/v1/projects/${PROJECT_ID}/tables`, {
      name: "users",
      columns: [
        { name: "id", type: "integer", primary_key: true, auto_increment: true, not_null: true },
        { name: "username", type: "text", primary_key: false, auto_increment: false, not_null: true },
        { name: "status", type: "text", primary_key: false, auto_increment: false, not_null: false },
        { name: "age", type: "integer", primary_key: false, auto_increment: false, not_null: false },
        { name: "data", type: "text", primary_key: false, auto_increment: false, not_null: false },
      ],
    });

    await adminReq("POST", `/v1/projects/${PROJECT_ID}/tables`, {
      name: "todos",
      columns: [
        { name: "id", type: "integer", primary_key: true, auto_increment: true, not_null: true },
        { name: "task", type: "text", primary_key: false, auto_increment: false, not_null: true },
        { name: "is_complete", type: "text", primary_key: false, auto_increment: false, not_null: false },
        { name: "user_id", type: "text", primary_key: false, auto_increment: false, not_null: false },
      ],
    });

    // Set up policies: anon can read, authenticated can read+write own rows
    await adminReq("PUT", `/v1/projects/${PROJECT_ID}/policies`, {
      table: "todos",
      rules: [
        { kind: "allow", action: "select", role: "anon" },
        { kind: "allow", action: "select", role: "authenticated" },
        { kind: "allow", action: "insert", role: "authenticated", check: "equals_claim", column: "user_id", claim: "sub" },
        { kind: "allow", action: "update", role: "authenticated", check: "equals_claim", column: "user_id", claim: "sub" },
        { kind: "allow", action: "delete", role: "authenticated", check: "equals_claim", column: "user_id", claim: "sub" },
      ],
    });

    await adminReq("PUT", `/v1/projects/${PROJECT_ID}/policies`, {
      table: "users",
      rules: [
        { kind: "allow", action: "select", role: "anon" },
        { kind: "allow", action: "select", role: "authenticated" },
        { kind: "allow", action: "insert", role: "service_role" },
        { kind: "allow", action: "delete", role: "service_role" },
      ],
    });

    // Seed users table
    const seedToken = await mintToken("admin", "service_role");
    const seedClient = makeClient(seedToken);
    await seedClient.from("users").insert([
      { username: "supabot", status: "ONLINE", age: 1 },
      { username: "kiwicopple", status: "OFFLINE", age: 25 },
      { username: "awailas", status: "ONLINE", age: 25 },
      { username: "dragarcia", status: "ONLINE", age: 20 },
    ]);
  });

  // =========================================================================
  // PostgREST: Basic CRUD
  // =========================================================================

  describe("PostgREST — Basic CRUD", () => {
    test("select * from users returns seeded data", async () => {
      const { data, error } = await supabaseAnon.from("users").select("*");
      strictEqual(error, null);
      ok(Array.isArray(data));
      ok(data.length >= 4);
    });

    test("select specific columns", async () => {
      const { data, error } = await supabaseAnon.from("users").select("username,status");
      strictEqual(error, null);
      ok(Array.isArray(data));
      ok(data[0].username !== undefined);
      ok(data[0].status !== undefined);
      ok(data[0].age === undefined, "age should not be selected");
    });

    test("insert + select + update + delete cycle", async () => {
      const stamp = `crud-${Date.now()}`;

      // Insert
      const { data: inserted, error: insertErr } = await supabaseService
        .from("users")
        .insert({ username: stamp, status: "ONLINE", age: 42 })
        .select();
      strictEqual(insertErr, null);
      ok(Array.isArray(inserted));
      strictEqual(inserted[0].username, stamp);

      // Select with eq filter
      const { data: selected, error: selectErr } = await supabaseAnon
        .from("users")
        .select("*")
        .eq("username", stamp);
      strictEqual(selectErr, null);
      strictEqual(selected.length, 1);
      strictEqual(selected[0].age, 42);

      // Update
      const { data: updated, error: updateErr } = await supabaseService
        .from("users")
        .update({ status: "OFFLINE" })
        .eq("username", stamp)
        .select();
      strictEqual(updateErr, null);
      strictEqual(updated[0].status, "OFFLINE");

      // Delete
      const { data: deleted, error: deleteErr } = await supabaseService
        .from("users")
        .delete()
        .eq("username", stamp)
        .select();
      strictEqual(deleteErr, null);
      strictEqual(deleted[0].username, stamp);

      // Verify deletion
      const { data: afterDelete } = await supabaseAnon
        .from("users")
        .select("*")
        .eq("username", stamp);
      strictEqual(afterDelete.length, 0);
    });

    test("insert with .single() returns object not array", async () => {
      const stamp = `single-${Date.now()}`;
      const { data, error } = await supabaseService
        .from("users")
        .insert({ username: stamp, status: "ONLINE", age: 1 })
        .select()
        .single();
      strictEqual(error, null);
      ok(!Array.isArray(data), "single() should return object");
      strictEqual(data.username, stamp);

      // Cleanup
      await supabaseService.from("users").delete().eq("username", stamp);
    });

    test("upsert inserts when not exists", async () => {
      const stamp = `upsert-${Date.now()}`;
      const { data, error } = await supabaseService
        .from("users")
        .upsert({ username: stamp, status: "ONLINE", age: 99 })
        .select();
      strictEqual(error, null);
      strictEqual(data[0].username, stamp);
      strictEqual(data[0].age, 99);

      await supabaseService.from("users").delete().eq("username", stamp);
    });
  });

  // =========================================================================
  // PostgREST: Filters
  // =========================================================================

  describe("PostgREST — Filters", () => {
    before(async () => {
      // Ensure clean state with known data
      await supabaseService.from("users").delete().neq("username", "supabot");
      await supabaseService.from("users").delete().neq("username", "kiwicopple");
      await supabaseService.from("users").delete().neq("username", "awailas");
      await supabaseService.from("users").delete().neq("username", "dragarcia");

      await supabaseService.from("users").insert([
        { username: "supabot", status: "ONLINE", age: 1 },
        { username: "kiwicopple", status: "OFFLINE", age: 25 },
        { username: "awailas", status: "ONLINE", age: 25 },
        { username: "dragarcia", status: "ONLINE", age: 20 },
      ]);
    });

    test("eq filter", async () => {
      const { data, error } = await supabaseAnon.from("users").select("*").eq("status", "ONLINE");
      strictEqual(error, null);
      ok(data.every((u) => u.status === "ONLINE"));
      ok(data.length >= 3);
    });

    test("neq filter", async () => {
      const { data, error } = await supabaseAnon.from("users").select("*").neq("status", "ONLINE");
      strictEqual(error, null);
      ok(data.every((u) => u.status !== "ONLINE"));
    });

    test("gt filter", async () => {
      const { data, error } = await supabaseAnon.from("users").select("*").gt("age", 20);
      strictEqual(error, null);
      ok(data.every((u) => u.age > 20));
    });

    test("gte filter", async () => {
      const { data, error } = await supabaseAnon.from("users").select("*").gte("age", 20);
      strictEqual(error, null);
      ok(data.every((u) => u.age >= 20));
    });

    test("lt filter", async () => {
      const { data, error } = await supabaseAnon.from("users").select("*").lt("age", 25);
      strictEqual(error, null);
      ok(data.every((u) => u.age < 25));
    });

    test("lte filter", async () => {
      const { data, error } = await supabaseAnon.from("users").select("*").lte("age", 20);
      strictEqual(error, null);
      ok(data.every((u) => u.age <= 20));
    });

    test("in filter", async () => {
      const { data, error } = await supabaseAnon
        .from("users")
        .select("*")
        .in("username", ["supabot", "kiwicopple"]);
      strictEqual(error, null);
      ok(data.length >= 2);
      ok(data.every((u) => ["supabot", "kiwicopple"].includes(u.username)));
    });

    test("like filter", async () => {
      const { data, error } = await supabaseAnon.from("users").select("*").like("username", "supa%");
      strictEqual(error, null);
      ok(data.every((u) => u.username.startsWith("supa")));
    });

    test("ilike filter", async () => {
      const { data, error } = await supabaseAnon.from("users").select("*").ilike("username", "SUPA%");
      strictEqual(error, null);
      ok(data.length >= 1);
    });

    test("is filter (null check)", async () => {
      const { data, error } = await supabaseAnon.from("users").select("*").is("data", null);
      strictEqual(error, null);
      ok(Array.isArray(data));
    });

    test("not filter", async () => {
      const { data, error } = await supabaseAnon
        .from("users")
        .select("*")
        .not("status", "eq", "ONLINE");
      strictEqual(error, null);
      ok(data.every((u) => u.status !== "ONLINE"));
    });

    test("chained filters (AND)", async () => {
      const { data, error } = await supabaseAnon
        .from("users")
        .select("*")
        .eq("status", "ONLINE")
        .gt("age", 20);
      strictEqual(error, null);
      ok(data.every((u) => u.status === "ONLINE" && u.age > 20));
    });
  });

  // =========================================================================
  // PostgREST: Transforms
  // =========================================================================

  describe("PostgREST — Transforms", () => {
    test("order ascending", async () => {
      const { data, error } = await supabaseAnon
        .from("users")
        .select("*")
        .order("username", { ascending: true });
      strictEqual(error, null);
      ok(data.length >= 2);
      ok(data[0].username <= data[1].username, "should be sorted ascending");
    });

    test("order descending", async () => {
      const { data, error } = await supabaseAnon
        .from("users")
        .select("*")
        .order("username", { ascending: false });
      strictEqual(error, null);
      ok(data[0].username >= data[1].username, "should be sorted descending");
    });

    test("limit", async () => {
      const { data, error } = await supabaseAnon.from("users").select("*").limit(2);
      strictEqual(error, null);
      ok(data.length <= 2);
    });

    test("range", async () => {
      const { data, error } = await supabaseAnon.from("users").select("*").range(0, 1);
      strictEqual(error, null);
      ok(data.length <= 2);
    });

    test("limit + order combined", async () => {
      const { data, error } = await supabaseAnon
        .from("users")
        .select("*")
        .order("age", { ascending: false })
        .limit(1);
      strictEqual(error, null);
      ok(data.length === 1);
    });

    test("maybeSingle returns object or null", async () => {
      const { data, error } = await supabaseAnon
        .from("users")
        .select("*")
        .eq("username", "supabot")
        .maybeSingle();
      strictEqual(error, null);
      ok(data !== null);
      strictEqual(data.username, "supabot");
    });

    test("maybeSingle returns null for no match", async () => {
      const { data, error } = await supabaseAnon
        .from("users")
        .select("*")
        .eq("username", "nonexistent-user-12345")
        .maybeSingle();
      strictEqual(error, null);
      strictEqual(data, null);
    });
  });

  // =========================================================================
  // PostgreSQL RLS / Policy DSL
  // =========================================================================

  describe("PostgreSQL RLS — Policy enforcement", () => {
    let user1Token, user2Token;
    let user1Client, user2Client;
    let user1Id, user2Id;
    let user1TodoId, user2TodoId;

    before(async () => {
      user1Id = `u1-${Date.now()}`;
      user2Id = `u2-${Date.now()}`;
      user1Token = await mintToken(user1Id, "authenticated", {});
      user2Token = await mintToken(user2Id, "authenticated", {});
      user1Client = makeClient(user1Token);
      user2Client = makeClient(user2Token);

      // Clean todos
      await supabaseService.from("todos").delete().neq("id", -1);

      // Insert todos for each user
      const { data: t1 } = await user1Client
        .from("todos")
        .insert({ task: "User 1 Todo", is_complete: "false", user_id: user1Id })
        .select();
      user1TodoId = t1[0].id;

      const { data: t2 } = await user2Client
        .from("todos")
        .insert({ task: "User 2 Todo", is_complete: "false", user_id: user2Id })
        .select();
      user2TodoId = t2[0].id;
    });

    test("anon can read todos (allow select for anon)", async () => {
      const { data, error } = await supabaseAnon.from("todos").select("*");
      strictEqual(error, null);
      ok(Array.isArray(data));
    });

    test("authenticated user can read their own todo", async () => {
      const { data, error } = await user1Client
        .from("todos")
        .select("*")
        .eq("id", user1TodoId)
        .single();
      strictEqual(error, null);
      strictEqual(data.task, "User 1 Todo");
    });

    test("authenticated user can create their own todo", async () => {
      const { data, error } = await user1Client
        .from("todos")
        .insert({ task: "New User 1 Todo", is_complete: "true", user_id: user1Id })
        .select()
        .single();
      strictEqual(error, null);
      strictEqual(data.task, "New User 1 Todo");

      // Cleanup
      await user1Client.from("todos").delete().eq("id", data.id);
    });

    test("authenticated user can update their own todo", async () => {
      const { data, error } = await user1Client
        .from("todos")
        .update({ task: "Updated User 1 Todo" })
        .eq("id", user1TodoId)
        .select()
        .single();
      strictEqual(error, null);
      strictEqual(data.task, "Updated User 1 Todo");
    });

    test("authenticated user can delete their own todo", async () => {
      const { data, error } = await user1Client
        .from("todos")
        .insert({ task: "To Delete", is_complete: "false", user_id: user1Id })
        .select()
        .single();
      const deleteId = data.id;

      const { error: delErr } = await user1Client.from("todos").delete().eq("id", deleteId);
      strictEqual(delErr, null);

      const { data: after } = await supabaseService.from("todos").select("*").eq("id", deleteId);
      strictEqual(after.length, 0);
    });

    after(async () => {
      await supabaseService.from("todos").delete().neq("id", -1);
    });
  });

  // =========================================================================
  // GoTrue-compatible Auth
  // =========================================================================

  describe("GoTrue Auth — signup / signin / signout / getUser", () => {
    test("signUp with email+password creates user and returns session", async () => {
      const email = uniqueEmail();
      const password = "testpass123";

      const client = makeClient(ANON_KEY);
      const { data, error } = await client.auth.signUp({ email, password });

      strictEqual(error, null);
      ok(data.user, "user should be defined");
      ok(data.session, "session should be defined");
      ok(data.session.access_token, "access_token should be present");
      ok(data.session.refresh_token, "refresh_token should be present");
      strictEqual(data.user.email, email);
    });

    test("signInWithPassword returns session for valid credentials", async () => {
      const email = uniqueEmail();
      const password = "testpass123";

      const signupClient = makeClient(ANON_KEY);
      await signupClient.auth.signUp({ email, password });

      const signinClient = makeClient(ANON_KEY);
      const { data, error } = await signinClient.auth.signInWithPassword({ email, password });

      strictEqual(error, null);
      ok(data.user, "user should be defined");
      ok(data.session, "session should be defined");
      strictEqual(data.user.email, email);
    });

    test("signInWithPassword fails for invalid credentials", async () => {
      const email = uniqueEmail();
      const password = "testpass123";

      const signupClient = makeClient(ANON_KEY);
      await signupClient.auth.signUp({ email, password });

      const signinClient = makeClient(ANON_KEY);
      const { data, error } = await signinClient.auth.signInWithPassword({
        email,
        password: "wrongpassword",
      });

      ok(error, "error should be present for invalid credentials");
      ok(!data.session, "session should be null on failure");
    });

    test("signInWithPassword fails for non-existent user", async () => {
      const client = makeClient(ANON_KEY);
      const { data, error } = await client.auth.signInWithPassword({
        email: `nonexistent-${Date.now()}@example.com`,
        password: "password123",
      });

      ok(error, "error should be present for non-existent user");
      ok(!data.session, "session should be null");
    });

    test("signUp with duplicate email fails", async () => {
      const email = uniqueEmail();
      const password = "testpass123";

      const client1 = makeClient(ANON_KEY);
      await client1.auth.signUp({ email, password });

      const client2 = makeClient(ANON_KEY);
      const { data, error } = await client2.auth.signUp({ email, password });

      ok(error, "error should be present for duplicate signup");
    });

    test("getUser returns current user after signIn", async () => {
      const email = uniqueEmail();
      const password = "testpass123";

      const client = makeClient(ANON_KEY);
      await client.auth.signUp({ email, password });
      await client.auth.signInWithPassword({ email, password });

      const { data, error } = await client.auth.getUser();

      strictEqual(error, null);
      ok(data.user, "user should be defined");
      strictEqual(data.user.email, email);
    });

    test("signOut clears the session", async () => {
      const email = uniqueEmail();
      const password = "testpass123";

      const client = makeClient(ANON_KEY);
      await client.auth.signUp({ email, password });
      await client.auth.signInWithPassword({ email, password });

      const { error: signOutError } = await client.auth.signOut();
      strictEqual(signOutError, null);
    });

    test("signOut revokes the current refresh token", async () => {
      const email = uniqueEmail();
      const password = "testpass123";

      const client = makeClient(ANON_KEY);
      const { data: signupData, error: signupError } = await client.auth.signUp({ email, password });
      strictEqual(signupError, null);
      const refreshToken = signupData.session.refresh_token;

      const { error: signOutError } = await client.auth.signOut();
      strictEqual(signOutError, null);

      const refreshRes = await refreshWithToken(refreshToken);
      strictEqual(refreshRes.status, 401);
    });

    test("signOut with local scope preserves other sessions", async () => {
      const email = uniqueEmail();
      const password = "testpass123";

      const client1 = makeClient(ANON_KEY);
      const { data: signupData, error: signupError } = await client1.auth.signUp({ email, password });
      strictEqual(signupError, null);
      const session1RefreshToken = signupData.session.refresh_token;

      const client2 = makeClient(ANON_KEY);
      const { data: signinData, error: signinError } = await client2.auth.signInWithPassword({ email, password });
      strictEqual(signinError, null);
      const session2RefreshToken = signinData.session.refresh_token;

      const { error: signOutError } = await client1.auth.signOut({ scope: "local" });
      strictEqual(signOutError, null);

      const session1Refresh = await refreshWithToken(session1RefreshToken);
      strictEqual(session1Refresh.status, 401);

      const session2Refresh = await refreshWithToken(session2RefreshToken);
      strictEqual(session2Refresh.status, 200);
      const refreshed = await session2Refresh.json();
      ok(refreshed.refresh_token, "other session should still refresh");
    });

    test("refreshSession with valid refresh token returns new session", async () => {
      const email = uniqueEmail();
      const password = "testpass123";

      const client = makeClient(ANON_KEY);
      const { data: signupData } = await client.auth.signUp({ email, password });
      const refreshToken = signupData.session.refresh_token;

      const { data, error } = await client.auth.refreshSession({
        refresh_token: refreshToken,
      });

      strictEqual(error, null);
      ok(data.session, "new session should be returned");
      ok(data.session.access_token, "new access_token should be present");
      ok(data.session.refresh_token, "new refresh_token should be present");
    });

    test("signUp with user_metadata stores data", async () => {
      const email = uniqueEmail();
      const password = "testpass123";

      const client = makeClient(ANON_KEY);
      const { data, error } = await client.auth.signUp({
        email,
        password,
        options: { data: { first_name: "Test", last_name: "User" } },
      });

      strictEqual(error, null);
      ok(data.user);
      ok(data.user.user_metadata);
      strictEqual(data.user.user_metadata.first_name, "Test");
      strictEqual(data.user.user_metadata.last_name, "User");
    });

    test("auth settings endpoint returns config", async () => {
      const res = await fetch(`${BASE_URL}/auth/v1/settings`, {
        headers: { apikey: ANON_KEY },
      });
      strictEqual(res.status, 200);
      const data = await res.json();
      ok(data.hasOwnProperty("disable_signup"));
      ok(data.hasOwnProperty("mailer_autoconfirm"));
    });

    test("updateUser changes user data", async () => {
      const email = uniqueEmail();
      const password = "testpass123";

      const client = makeClient(ANON_KEY);
      await client.auth.signUp({ email, password });
      await client.auth.signInWithPassword({ email, password });

      const { data, error } = await client.auth.updateUser({
        data: { updated: true },
      });

      strictEqual(error, null);
      ok(data.user);
      strictEqual(data.user.user_metadata.updated, true);
    });
  });

  // =========================================================================
  // Storage API
  // =========================================================================

  describe("Storage API — bucket + file operations", () => {
    let bucketName;

    before(async () => {
      bucketName = `test_bucket_${Date.now()}`;
      const res = await fetch(`${BASE_URL}/storage/v1/buckets`, {
        method: "POST",
        headers: {
          authorization: `Bearer ${serviceToken}`,
          "content-type": "application/json",
        },
        body: JSON.stringify({ name: bucketName }),
      });
      ok(res.status === 200 || res.status === 201, `expected 200 or 201, got ${res.status}`);
    });

    test("list buckets", async () => {
      const res = await fetch(`${BASE_URL}/storage/v1/buckets`, {
        headers: { authorization: `Bearer ${serviceToken}` },
      });
      strictEqual(res.status, 200);
      const data = await res.json();
      ok(Array.isArray(data));
      ok(data.some((b) => b.name === bucketName || b.id === bucketName));
    });

    test("anonymous requests cannot manage buckets", async () => {
      const createRes = await storageReq(
        "POST",
        "/storage/v1/buckets",
        ANON_KEY,
        JSON.stringify({ name: `anon_bucket_${Date.now()}` }),
      );
      strictEqual(createRes.status, 403);

      const listRes = await storageReq("GET", "/storage/v1/buckets", ANON_KEY);
      strictEqual(listRes.status, 403);

      const getRes = await storageReq("GET", `/storage/v1/buckets/${bucketName}`, ANON_KEY);
      strictEqual(getRes.status, 403);

      const deleteRes = await storageReq("DELETE", `/storage/v1/buckets/${bucketName}`, ANON_KEY);
      strictEqual(deleteRes.status, 403);
    });

    test("anonymous requests cannot access objects", async () => {
      await storageReq(
        "POST",
        `/storage/v1/object/${bucketName}/anon-negative.txt`,
        serviceToken,
        "private",
        "text/plain",
      );

      const uploadRes = await storageReq(
        "POST",
        `/storage/v1/object/${bucketName}/anon-upload.txt`,
        ANON_KEY,
        "nope",
        "text/plain",
      );
      strictEqual(uploadRes.status, 403);

      const listRes = await storageReq(
        "POST",
        `/storage/v1/object/list/${bucketName}`,
        ANON_KEY,
        JSON.stringify({ limit: 100, offset: 0 }),
      );
      strictEqual(listRes.status, 403);

      const downloadRes = await storageReq(
        "GET",
        `/storage/v1/object/${bucketName}/anon-negative.txt`,
        ANON_KEY,
      );
      strictEqual(downloadRes.status, 403);

      const deleteRes = await storageReq(
        "DELETE",
        `/storage/v1/object/${bucketName}/anon-negative.txt`,
        ANON_KEY,
      );
      strictEqual(deleteRes.status, 403);

      const cleanupRes = await storageReq(
        "DELETE",
        `/storage/v1/object/${bucketName}/anon-negative.txt`,
        serviceToken,
      );
      ok(cleanupRes.status === 200 || cleanupRes.status === 204, `cleanup status: ${cleanupRes.status}`);
    });

    test("upload file (raw body)", async () => {
      const res = await fetch(`${BASE_URL}/storage/v1/object/${bucketName}/test-file.txt`, {
        method: "POST",
        headers: {
          authorization: `Bearer ${serviceToken}`,
          "content-type": "text/plain",
        },
        body: "Hello, Storage!",
      });
      ok(res.status === 200 || res.status === 201, `upload status: ${res.status}`);
    });

    test("download file", async () => {
      const res = await fetch(`${BASE_URL}/storage/v1/object/${bucketName}/test-file.txt`, {
        headers: { authorization: `Bearer ${serviceToken}` },
      });
      strictEqual(res.status, 200);
      const text = await res.text();
      strictEqual(text, "Hello, Storage!");
    });

    test("list files in bucket", async () => {
      const res = await fetch(`${BASE_URL}/storage/v1/object/list/${bucketName}`, {
        method: "POST",
        headers: {
          authorization: `Bearer ${serviceToken}`,
          "content-type": "application/json",
        },
        body: JSON.stringify({ limit: 100, offset: 0 }),
      });
      strictEqual(res.status, 200);
      const data = await res.json();
      ok(Array.isArray(data));
      ok(data.some((f) => f.key === "test-file.txt" || f.name === "test-file.txt"));
    });

    test("remove file", async () => {
      const res = await fetch(`${BASE_URL}/storage/v1/object/${bucketName}/test-file.txt`, {
        method: "DELETE",
        headers: { authorization: `Bearer ${serviceToken}` },
      });
      ok(res.status === 200 || res.status === 204, `delete status: ${res.status}`);
    });

    test("authenticated users can access only their own objects", async () => {
      const alice = makeClient(ANON_KEY);
      const bob = makeClient(ANON_KEY);
      const { data: aliceSignup } = await alice.auth.signUp({
        email: uniqueEmail(),
        password: "testpass123",
      });
      const { data: bobSignup } = await bob.auth.signUp({
        email: uniqueEmail(),
        password: "testpass123",
      });
      const aliceToken = aliceSignup.session.access_token;
      const bobToken = bobSignup.session.access_token;
      const key = `alice-owned-${Date.now()}.txt`;

      const uploadRes = await storageReq(
        "POST",
        `/storage/v1/object/${bucketName}/${key}`,
        aliceToken,
        "owned by alice",
        "text/plain",
      );
      ok(uploadRes.status === 200 || uploadRes.status === 201, `upload status: ${uploadRes.status}`);

      const aliceDownload = await storageReq("GET", `/storage/v1/object/${bucketName}/${key}`, aliceToken);
      strictEqual(aliceDownload.status, 200);
      strictEqual(await aliceDownload.text(), "owned by alice");

      const aliceList = await storageReq(
        "POST",
        `/storage/v1/object/list/${bucketName}`,
        aliceToken,
        JSON.stringify({ limit: 100, offset: 0 }),
      );
      strictEqual(aliceList.status, 200);
      const aliceObjects = await aliceList.json();
      ok(aliceObjects.some((f) => f.key === key || f.name === key));

      const bobDownload = await storageReq("GET", `/storage/v1/object/${bucketName}/${key}`, bobToken);
      strictEqual(bobDownload.status, 403);

      const bobList = await storageReq(
        "POST",
        `/storage/v1/object/list/${bucketName}`,
        bobToken,
        JSON.stringify({ limit: 100, offset: 0 }),
      );
      strictEqual(bobList.status, 200);
      const bobObjects = await bobList.json();
      ok(!bobObjects.some((f) => f.key === key || f.name === key));

      const bobDelete = await storageReq("DELETE", `/storage/v1/object/${bucketName}/${key}`, bobToken);
      strictEqual(bobDelete.status, 403);

      const aliceDelete = await storageReq("DELETE", `/storage/v1/object/${bucketName}/${key}`, aliceToken);
      ok(aliceDelete.status === 200 || aliceDelete.status === 204, `delete status: ${aliceDelete.status}`);
    });

    test("get bucket info", async () => {
      const res = await fetch(`${BASE_URL}/storage/v1/buckets/${bucketName}`, {
        headers: { authorization: `Bearer ${serviceToken}` },
      });
      strictEqual(res.status, 200);
    });

    test("delete bucket", async () => {
      const res = await fetch(`${BASE_URL}/storage/v1/buckets/${bucketName}`, {
        method: "DELETE",
        headers: { authorization: `Bearer ${serviceToken}` },
      });
      ok(res.status === 200 || res.status === 204, `delete bucket status: ${res.status}`);
    });
  });

  // =========================================================================
  // Auth + PostgREST integration (RLS with real auth users)
  // =========================================================================

  describe("Auth + PostgREST integration", () => {
    test("authenticated user from auth.signUp can query PostgREST", async () => {
      const email = uniqueEmail();
      const password = "testpass123";

      const client = makeClient(ANON_KEY);
      const { data: signupData } = await client.auth.signUp({ email, password });
      ok(signupData.session, "signup should return session");

      // Use the access_token from auth to query PostgREST
      const authedClient = makeClient(signupData.session.access_token);
      const { data, error } = await authedClient.from("users").select("*").limit(1);
      strictEqual(error, null);
      ok(Array.isArray(data));
    });

    test("anon key can read public data", async () => {
      const { data, error } = await supabaseAnon.from("users").select("*").limit(1);
      strictEqual(error, null);
      ok(Array.isArray(data));
    });
  });
});
