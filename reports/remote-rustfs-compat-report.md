# Supabase SDK Compatibility Report

Generated: 2026-06-21T02:27:53.953Z
Primary URL: http://127.0.0.1:8765
Replica URLs: http://127.0.0.1:8766, http://127.0.0.1:8767

## Result Matrix

| Capability | Endpoint | Result |
| --- | --- | --- |
| HTTP health | http://127.0.0.1:8765 | PASS |
| insert().select() | http://127.0.0.1:8765 | PASS id=10 |
| select().eq().limit() | http://127.0.0.1:8765 | PASS rows=1 |
| update().eq().select() | http://127.0.0.1:8765 | PASS rows=1 |
| delete().eq().select() | http://127.0.0.1:8765 | PASS rows=1 |
| replica async read catch-up | http://127.0.0.1:8766 | PASS id=11 primary_id=11 |
| replica async read catch-up | http://127.0.0.1:8767 | PASS id=11 primary_id=11 |
| replica write forwarding | http://127.0.0.1:8766 | PASS id=12 |

## Compatibility Scope

- Supported: `createClient(url, jwt)` with table CRUD through `/rest/v1/{table}`.
- Supported: `select('*').eq(...).limit(...)`, `insert(object).select()`, `update(...).eq(...).select()`, `delete().eq(...).select()`.
- Supported: JWT in `Authorization: Bearer` or `apikey` header, using this POC's HS256 token format.
- Supported: primary plus async read replicas, including primary-to-replica read catch-up and replica write forwarding.
- Partial: only `eq` filters and `limit` are implemented for the PostgREST surface.
- Not implemented: Supabase Auth API, Storage API compatibility, Realtime protocol compatibility, RPC, joins/embedding, `or`, `in`, `order`, range offsets, count headers, and generated Postgres error codes.

