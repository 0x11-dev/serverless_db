import assert from "node:assert/strict";
import { MockAdminClient, getNavigationForRole } from "../src/api/mockAdminClient";
import { REQUIRED_PAGE_IDS } from "../src/api/types";

const client = new MockAdminClient();

const adminNavigation = getNavigationForRole("admin");
assert.deepEqual(
  adminNavigation.map((item) => item.id),
  REQUIRED_PAGE_IDS,
  "dashboard must expose every required console page",
);
assert.ok(adminNavigation.every((item) => item.access === "full"), "admin should have full navigation access");

const userNavigation = getNavigationForRole("user");
assert.deepEqual(
  userNavigation.map((item) => item.id),
  REQUIRED_PAGE_IDS,
  "ordinary users still see the console skeleton and role gates",
);
assert.equal(userNavigation.find((item) => item.id === "system_workers")?.access, "restricted");
assert.equal(userNavigation.find((item) => item.id === "api_keys")?.access, "limited");

const adminSnapshot = await client.getDashboardSnapshot("admin");
const userSnapshot = await client.getDashboardSnapshot("user");

assert.ok(adminSnapshot.projects.length > userSnapshot.projects.length, "admin sees all projects");
assert.ok(userSnapshot.projects.every((project) => project.ownerId === "alice"), "user sees only owned projects");
assert.ok(adminSnapshot.apiKeys.some((key) => key.type === "service_role" && key.secretPreview !== "restricted"));

const userServiceKey = userSnapshot.apiKeys.find((key) => key.type === "service_role");
assert.equal(userServiceKey?.secretPreview, "restricted", "service role key is redacted for ordinary users");
assert.equal(userSnapshot.authUsers.length, 1, "ordinary user sees only their account row");
assert.equal(userSnapshot.workers.length, 0, "worker internals are admin only");

console.log("dashboard facade tests passed");
