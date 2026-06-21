import { createHmac } from "node:crypto";

const baseUrl = process.argv[2] ?? "http://127.0.0.1:8765";
const jwtSecret = process.env.SDB_JWT_SECRET || "dev-secret-change-me";

function mintToken(sub: string, role = "authenticated", claims: Record<string, unknown> = {}): string {
  const now = Math.floor(Date.now() / 1000);
  const header = { alg: "HS256", typ: "JWT" };
  const payload = { sub, role, claims, iat: now };
  const signingInput = `${b64(header)}.${b64(payload)}`;
  const sig = createHmac("sha256", jwtSecret).update(signingInput).digest("base64url");
  return `${signingInput}.${sig}`;
}

function b64(value: unknown): string {
  return Buffer.from(JSON.stringify(value)).toString("base64url");
}

async function request(method: string, path: string, body?: unknown, token?: string, contentType = "application/json"): Promise<unknown> {
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
  const response = await fetch(`${baseUrl}${path}`, { method, headers, body: payload });
  if (!response.ok) {
    throw new Error(`${method} ${path} failed: ${response.status} ${await response.text()}`);
  }
  const responseType = response.headers.get("content-type") ?? "";
  if (responseType.startsWith("application/json")) {
    return response.json();
  }
  return response.text();
}

async function main(): Promise<void> {
  const service = mintToken("admin", "service_role");
  const alice = mintToken("alice", "authenticated", { orgs: ["demo"] });

  await request("POST", "/v1/projects", { id: "demo" }, service);
  await request("POST", "/v1/projects/demo/tables", {
    name: "notes",
    columns: [
      { name: "owner_id", type: "text", not_null: true },
      { name: "title", type: "text", not_null: true },
      { name: "body", type: "text" }
    ]
  }, service);
  await request("PUT", "/v1/projects/demo/policies", {
    table: "notes",
    operation: "all",
    name: "owner_only",
    rule: { column: "owner_id", equals_claim: "sub" }
  }, service);
  await request("POST", "/v1/projects/demo/tables/notes", { owner_id: "alice", title: "hello", body: "from TypeScript POC" }, alice);
  await request("POST", "/v1/projects/demo/buckets", { name: "files" }, service);
  await request("PUT", "/v1/projects/demo/storage/files/hello.txt", Buffer.from("stored in object-store adapter\n"), alice, "text/plain");

  const rows = await request("GET", "/v1/projects/demo/tables/notes", undefined, alice);
  const events = await request("GET", "/v1/projects/demo/events?since=0", undefined, service);
  await request("POST", "/v1/projects/demo/hibernate", undefined, service);
  const afterHibernate = await request("GET", "/v1/projects/demo/tables/notes", undefined, alice);
  console.log(JSON.stringify({ rows, events, afterHibernate }, null, 2));
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
