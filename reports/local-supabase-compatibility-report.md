# Supabase SDK Compatibility Report

Generated: 2026-06-20T15:52:27.614Z
Primary URL: http://127.0.0.1:18765
Replica URLs: not tested

## Result Matrix

| Capability | Endpoint | Result |
| --- | --- | --- |
| HTTP health | http://127.0.0.1:18765 | PASS |
| insert().select() | http://127.0.0.1:18765 | PASS id=1 |
| select().eq().limit() | http://127.0.0.1:18765 | PASS rows=1 |
| update().eq().select() | http://127.0.0.1:18765 | PASS rows=1 |
| delete().eq().select() | http://127.0.0.1:18765 | PASS rows=1 |

## Compatibility Scope

- Supported: `createClient(url, jwt)` with table CRUD through `/rest/v1/{table}`.
- Supported: `select('*').eq(...).limit(...)`, `insert(object).select()`, `update(...).eq(...).select()`, `delete().eq(...).select()`.
- Supported: JWT in `Authorization: Bearer` or `apikey` header, using this POC's HS256 token format.
- Supported: primary plus async read replicas, including primary-to-replica read catch-up and replica write forwarding.
- Partial: only `eq` filters and `limit` are implemented for the PostgREST surface.
- Not implemented: Supabase Auth API, Storage API compatibility, Realtime protocol compatibility, RPC, joins/embedding, `or`, `in`, `order`, range offsets, count headers, and generated Postgres error codes.

