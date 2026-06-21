import { createServer, type IncomingMessage, type Server, type ServerResponse } from "node:http";
import { URL } from "node:url";
import { AuthError, actorFromAuthorization, mintToken } from "./auth.js";
import { ApiError, ProjectRuntime } from "./runtime.js";

type JsonObject = Record<string, unknown>;

export function createHttpServer(runtime: ProjectRuntime): Server {
  return createServer(async (req, res) => {
    try {
      await dispatch(runtime, req, res);
    } catch (err) {
      if (err instanceof ApiError) {
        sendJson(res, err.status, { error: err.message });
      } else if (err instanceof AuthError) {
        sendJson(res, 401, { error: err.message });
      } else if (err instanceof SyntaxError) {
        sendJson(res, 400, { error: "invalid JSON body" });
      } else {
        sendJson(res, 500, { error: `internal error: ${err instanceof Error ? err.message : String(err)}` });
      }
    }
  });
}

async function dispatch(runtime: ProjectRuntime, req: IncomingMessage, res: ServerResponse): Promise<void> {
  if (req.method === "OPTIONS") {
    res.writeHead(204, corsHeaders());
    res.end();
    return;
  }

  const base = `http://${req.headers.host ?? "127.0.0.1"}`;
  const url = new URL(req.url ?? "/", base);
  const segments = url.pathname.split("/").filter(Boolean).map(decodeURIComponent);
  const method = req.method ?? "GET";

  if (method === "GET" && segments.join("/") === "health") {
    sendJson(res, 200, { ok: true });
    return;
  }

  if (method === "POST" && same(segments, ["v1", "tokens"])) {
    const body = await readJson(req);
    const token = mintToken({
      sub: String(body.sub ?? ""),
      role: String(body.role ?? "authenticated"),
      claims: isRecord(body.claims) ? body.claims : {},
      expiresIn: typeof body.expires_in === "number" ? body.expires_in : undefined
    });
    sendJson(res, 200, { token });
    return;
  }

  if (method === "POST" && same(segments, ["v1", "projects"])) {
    const body = await readJson(req);
    sendJson(res, 201, runtime.createProject(String(body.id ?? body.project_id ?? "")));
    return;
  }

  if (segments.length >= 3 && segments[0] === "v1" && segments[1] === "projects") {
    await projectRoute(runtime, req, res, method, segments[2], segments.slice(3), url);
    return;
  }

  throw new ApiError(404, "route not found");
}

async function projectRoute(
  runtime: ProjectRuntime,
  req: IncomingMessage,
  res: ServerResponse,
  method: string,
  projectId: string,
  tail: string[],
  url: URL
): Promise<void> {
  if (method === "POST" && same(tail, ["hibernate"])) {
    sendJson(res, 200, runtime.hibernate(projectId));
    return;
  }
  if (method === "POST" && same(tail, ["crash"])) {
    sendJson(res, 200, runtime.crashProject(projectId));
    return;
  }
  if (method === "GET" && same(tail, ["schema"])) {
    sendJson(res, 200, runtime.schema(projectId));
    return;
  }
  if (method === "POST" && same(tail, ["tables"])) {
    sendJson(res, 201, runtime.createTable(projectId, (await readJson(req)) as never));
    return;
  }
  if (method === "PUT" && same(tail, ["policies"])) {
    sendJson(res, 200, runtime.setPolicy(projectId, (await readJson(req)) as never));
    return;
  }
  if (method === "GET" && same(tail, ["policies"])) {
    sendJson(res, 200, { policies: runtime.listPolicies(projectId) });
    return;
  }
  if (method === "POST" && same(tail, ["buckets"])) {
    const body = await readJson(req);
    sendJson(res, 201, runtime.createBucket(projectId, String(body.name ?? "")));
    return;
  }
  if (method === "GET" && same(tail, ["events"])) {
    sendJson(res, 200, {
      events: runtime.events(projectId, numberParam(url, "since", 0), numberParam(url, "limit", 100))
    });
    return;
  }
  if (method === "GET" && same(tail, ["realtime"])) {
    await sendSse(runtime, req, res, projectId, numberParam(url, "since", 0));
    return;
  }
  if (tail.length === 2 && tail[0] === "tables") {
    await tableRoute(runtime, req, res, method, projectId, tail[1], url);
    return;
  }
  if (tail.length >= 3 && tail[0] === "storage") {
    await storageRoute(runtime, req, res, method, projectId, tail[1], tail.slice(2).join("/"));
    return;
  }
  throw new ApiError(404, "project route not found");
}

async function tableRoute(
  runtime: ProjectRuntime,
  req: IncomingMessage,
  res: ServerResponse,
  method: string,
  projectId: string,
  table: string,
  url: URL
): Promise<void> {
  const actor = actorFromAuthorization(req.headers.authorization);
  const filters = eqFilters(url);
  if (method === "GET") {
    sendJson(res, 200, { rows: runtime.selectRows(projectId, table, filters, actor, numberParam(url, "limit", 100)) });
    return;
  }
  if (method === "POST") {
    sendJson(res, 201, { row: runtime.insertRow(projectId, table, await readJson(req), actor) });
    return;
  }
  if (method === "PATCH") {
    sendJson(res, 200, runtime.updateRows(projectId, table, filters, await readJson(req), actor));
    return;
  }
  if (method === "DELETE") {
    sendJson(res, 200, runtime.deleteRows(projectId, table, filters, actor));
    return;
  }
  throw new ApiError(405, "method not allowed");
}

async function storageRoute(
  runtime: ProjectRuntime,
  req: IncomingMessage,
  res: ServerResponse,
  method: string,
  projectId: string,
  bucket: string,
  key: string
): Promise<void> {
  const actor = actorFromAuthorization(req.headers.authorization);
  if (method === "PUT") {
    const object = runtime.putObject(projectId, bucket, key, await readBytes(req), req.headers["content-type"] ?? "application/octet-stream", actor);
    sendJson(res, 201, { object });
    return;
  }
  if (method === "GET") {
    const { meta, data } = runtime.getObject(projectId, bucket, key);
    res.writeHead(200, {
      ...corsHeaders(),
      "content-type": String(meta.content_type),
      "content-length": String(data.length),
      etag: String(meta.etag)
    });
    res.end(data);
    return;
  }
  if (method === "DELETE") {
    sendJson(res, 200, runtime.deleteObject(projectId, bucket, key, actor));
    return;
  }
  throw new ApiError(405, "method not allowed");
}

async function sendSse(runtime: ProjectRuntime, req: IncomingMessage, res: ServerResponse, projectId: string, since: number): Promise<void> {
  res.writeHead(200, {
    ...corsHeaders(),
    "content-type": "text/event-stream",
    "cache-control": "no-cache",
    connection: "keep-alive"
  });
  let current = since;
  const deadline = Date.now() + 30_000;
  while (!req.destroyed && Date.now() < deadline) {
    const events = await runtime.waitForEvents(projectId, current, 10_000);
    for (const event of events) {
      current = Math.max(current, Number(event.id));
      res.write(`id: ${event.id}\nevent: ${event.operation}\ndata: ${JSON.stringify(event)}\n\n`);
    }
    if (events.length > 0) break;
  }
  res.end();
}

function sendJson(res: ServerResponse, status: number, body: JsonObject): void {
  const data = Buffer.from(JSON.stringify(body));
  res.writeHead(status, {
    ...corsHeaders(),
    "content-type": "application/json",
    "content-length": String(data.length)
  });
  res.end(data);
}

function corsHeaders(): Record<string, string> {
  return {
    "access-control-allow-origin": "*",
    "access-control-allow-methods": "GET,POST,PUT,PATCH,DELETE,OPTIONS",
    "access-control-allow-headers": "Authorization,Content-Type"
  };
}

async function readJson(req: IncomingMessage): Promise<JsonObject> {
  const body = await readBytes(req);
  if (body.length === 0) return {};
  const parsed = JSON.parse(body.toString("utf8"));
  if (!isRecord(parsed)) {
    throw new ApiError(400, "JSON body must be an object");
  }
  return parsed;
}

function readBytes(req: IncomingMessage): Promise<Buffer> {
  return new Promise((resolve, reject) => {
    const chunks: Buffer[] = [];
    req.on("data", (chunk: Buffer) => chunks.push(chunk));
    req.on("end", () => resolve(Buffer.concat(chunks)));
    req.on("error", reject);
  });
}

function eqFilters(url: URL): Record<string, string> {
  const filters: Record<string, string> = {};
  for (const [key, value] of url.searchParams.entries()) {
    if (key.startsWith("eq.")) {
      filters[key.slice(3)] = value;
    }
  }
  return filters;
}

function numberParam(url: URL, key: string, fallback: number): number {
  const raw = url.searchParams.get(key);
  if (raw === null) return fallback;
  const value = Number(raw);
  return Number.isFinite(value) ? value : fallback;
}

function same(a: string[], b: string[]): boolean {
  return a.length === b.length && a.every((item, idx) => item === b[idx]);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
