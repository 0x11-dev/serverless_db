# Supabase SDK Compatibility Report

Generated: 2026-06-20T16:30:54.735Z
Primary URL: http://8.147.71.246
Replica URLs: http://127.0.0.1:18766, http://127.0.0.1:18767

## Deployment Notes

- Remote host: `root@8.147.71.246`, deployment path `/opt/serverless-db-poc`.
- Public Supabase-compatible URL: `http://8.147.71.246` mapped to the primary container on host port 80.
- Docker services: RustFS object store, one primary, two read replicas.
- Replica host ports `8766` and `8767` are open on the remote host but not reachable from this workstation through the cloud security group; replica compatibility was verified through SSH tunnels to the same remote services.
- Cold state evidence: RustFS bucket `serverless-db-poc/distributed-poc` contains `manifest.json`, immutable snapshots, WAL objects, writer lease, and lease claims for project `demo`.

## Result Matrix

| Capability | Endpoint | Result |
| --- | --- | --- |
| HTTP health | http://8.147.71.246 | PASS |
| insert().select() | http://8.147.71.246 | PASS id=4 |
| select().eq().limit() | http://8.147.71.246 | PASS rows=1 |
| update().eq().select() | http://8.147.71.246 | PASS rows=1 |
| delete().eq().select() | http://8.147.71.246 | PASS rows=1 |
| replica async read catch-up | http://127.0.0.1:18766 | PASS id=5 primary_id=5 |
| replica async read catch-up | http://127.0.0.1:18767 | PASS id=5 primary_id=5 |
| replica write forwarding | http://127.0.0.1:18766 | PASS id=6 |

## Compatibility Scope

- Supported: `createClient(url, jwt)` with table CRUD through `/rest/v1/{table}`.
- Supported: `select('*').eq(...).limit(...)`, `insert(object).select()`, `update(...).eq(...).select()`, `delete().eq(...).select()`.
- Supported: JWT in `Authorization: Bearer` or `apikey` header, using this POC's HS256 token format.
- Supported: primary plus async read replicas, including primary-to-replica read catch-up and replica write forwarding.
- Partial: only `eq` filters and `limit` are implemented for the PostgREST surface.
- Not implemented: Supabase Auth API, Storage API compatibility, Realtime protocol compatibility, RPC, joins/embedding, `or`, `in`, `order`, range offsets, count headers, and generated Postgres error codes.
