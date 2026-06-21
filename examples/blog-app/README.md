# Blog Platform Example

A multi-tenant blog platform that comprehensively verifies **all** serverless-db features in a single run.

## Features Verified

| # | Feature | API Surface |
| --- | --- | --- |
| 1 | Health check | `GET /health` |
| 2 | JWT auth (service_role, authenticated, claims, invalid token rejection) | `POST /v1/tokens` + local JWT minting |
| 3 | Project creation | `POST /v1/projects` |
| 4 | Multi-table schema (posts, comments, tags — various column types, custom PK) | `POST /v1/projects/{id}/tables` |
| 5 | Schema introspection | `GET /v1/projects/{id}/schema` |
| 6 | Policy DSL — all 7 rule types: `allow`, `role_in`, `equals_claim`, `in_claim`, `equals`, `and`, `or` | `PUT /v1/projects/{id}/policies` |
| 7 | CRUD — insert, select with `eq` filters + `limit`, update, delete | `GET/POST/PATCH/DELETE /v1/projects/{id}/tables/{table}` |
| 8 | Policy enforcement — owner-only, org-based, role-based, public read, service_role bypass | (implicit via CRUD) |
| 9 | Storage — bucket creation, text/binary object PUT/GET/DELETE, content-type preservation | `POST /buckets`, `PUT/GET/DELETE /storage/{bucket}/{key}` |
| 10 | Realtime outbox — events polling with insert/update/delete operations | `GET /v1/projects/{id}/events` |
| 11 | SSE realtime — live event stream | `GET /v1/projects/{id}/realtime` |
| 12 | Bookmark consistency — write returns bookmark, read-with-bookmark sees the write | `x-sdb-bookmark` header / `?bookmark=` query |
| 13 | Write idempotency — same key returns same result, different body → 409 | `Idempotency-Key` header |
| 14 | Supabase SDK compatibility — `insert().select()`, `select().eq().limit()`, `update().eq().select()`, `delete().eq().select()` | `/rest/v1/{table}` |
| 15 | Read replica — async catch-up, write forwarding | replica URLs via `SDB_REPLICA_URLS` |
| 16 | Hibernate recovery — data survives hibernate + rehydrate from object store | `POST /hibernate` |
| 17 | Crash recovery — data survives crash, recovered from snapshot + durable WAL | `POST /crash` |

## Data Model

```text
posts     (id, owner_id, org, title, body, published, view_count)
comments  (id, post_id, owner_id, org, content)
tags      (name [PK], color)
```

## Policies

| Table | Operation | Rule | Description |
| --- | --- | --- | --- |
| posts | all | `equals_claim(owner_id, sub)` | Owner can access own posts |
| posts | select | `role_in([service_role])` | Service role can read all |
| posts | select | `equals(published, 1)` | Only published posts visible |
| posts | select | `and(equals_claim(owner_id, sub), equals(published, 1))` | Owner + published |
| comments | all | `in_claim(org, orgs)` | Org-based access |
| comments | delete | `or(equals_claim(owner_id, sub), role_in([service_role]))` | Owner or service_role |
| tags | select | `allow(true)` | Public read |

## Running

### Against local dev server

```bash
# Terminal 1: start the Rust core
npm run core:dev

# Terminal 2: run the example
bash examples/blog-app/run.sh
```

### Against Docker cluster

```bash
# Start the distributed cluster
docker compose -f deploy/docker-compose.distributed.yml up -d --build

# Run the example against the cluster
SDB_BASE_URL=http://127.0.0.1:80 \
SDB_REPLICA_URLS=http://127.0.0.1:8766,http://127.0.0.1:8767 \
SDB_JWT_SECRET=<your-secret-from-.env> \
SDB_ENV=production \
bash examples/blog-app/run.sh
```

### Via npm script

```bash
npm run example:blog
```

With replicas:

```bash
SDB_REPLICA_URLS=http://127.0.0.1:8766,http://127.0.0.1:8767 npm run example:blog
```

## Output

The script prints a real-time test matrix to stdout and writes a detailed Markdown report to `reports/blog-app-verification-report.md`.

```text
=== Blog Platform Example ===
Base URL: http://127.0.0.1:8765
Project:  blog-app
Replicas: none
Mode:     dev (server JWT)

› Tokens & Auth
  ✓ [PASS] mint service_role token — POST /v1/tokens
  ✓ [PASS] mint authenticated token (alice) — POST /v1/tokens — claims: orgs=[acme,beta]
  ...

=== Summary ===
  PASS: 42  FAIL: 0  SKIP: 2  Total: 44
✓ All 42 test(s) passed (2 skipped)
```
