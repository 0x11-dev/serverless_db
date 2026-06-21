# Supabase Compatibility Matrix

Generated: 2025-01-24

## Summary

This report tracks the compatibility of Serverless DB with the Supabase PostgREST API surface.

## REST API (PostgREST compatibility)

| Feature | Status | Notes |
|---------|--------|-------|
| `GET /rest/v1/{table}` | ✅ Supported | Select rows with filters, order, limit, offset |
| `POST /rest/v1/{table}` | ✅ Supported | Insert single or array of objects |
| `PATCH /rest/v1/{table}` | ✅ Supported | Update with filters |
| `DELETE /rest/v1/{table}` | ✅ Supported | Delete with filters |
| `OPTIONS /rest/v1/{table}` | ✅ Supported | CORS preflight |

## Query Operators

| Operator | Status | Notes |
|----------|--------|-------|
| `eq` | ✅ Supported | Equality filter |
| `neq` | ✅ Supported | Not-equal filter |
| `gt` | ✅ Supported | Greater-than |
| `gte` | ✅ Supported | Greater-than-or-equal |
| `lt` | ✅ Supported | Less-than |
| `lte` | ✅ Supported | Less-than-or-equal |
| `in` | ✅ Supported | IN list (comma-separated) |
| `is` | ✅ Supported | IS null / not.null / true / false |
| `like` | ✅ Supported | LIKE pattern |
| `or` | ✅ Supported | OR filter (comma-separated conditions) |
| `and` | ✅ Supported | AND filter (comma-separated conditions) |
| `not` | ❌ Not supported | Negation operator |
| `fts` | ❌ Not supported | Full-text search |
| `plfts` | ❌ Not supported | Plain full-text search |
| `phfts` | ❌ Not supported | Phrase full-text search |
| `wfts` | ❌ Not supported | Websearch full-text search |
| `cs` | ❌ Not supported | Contains (array) |
| `cd` | ❌ Not supported | Contained by (array) |
| `ov` | ❌ Not supported | Overlaps (array) |
| `sl` | ❌ Not supported | Strictly left of (range) |
| `sr` | ❌ Not supported | Strictly right of (range) |
| `nxr` | ❌ Not supported | Not extending right of (range) |
| `nxl` | ❌ Not supported | Not extending left of (range) |
| `adj` | ❌ Not supported | Adjacent to (range) |

## Query Parameters

| Parameter | Status | Notes |
|-----------|--------|-------|
| `select` | ✅ Supported | Column projection (comma-separated) |
| `order` | ✅ Supported | `column.asc/desc.nullsfirst/nullslast` |
| `limit` | ✅ Supported | Row limit (max 1000) |
| `offset` | ✅ Supported | Row offset for pagination |
| `count` | ✅ Supported | `exact`, `planned`, `estimated` (parsed) |

## Headers

| Header | Status | Notes |
|--------|--------|-------|
| `Authorization: Bearer {jwt}` | ✅ Supported | JWT auth |
| `apikey: {jwt}` | ✅ Supported | Supabase-style API key header |
| `Prefer: return=representation` | ✅ Supported | Return inserted/updated rows |
| `Prefer: return=minimal` | ✅ Supported | Return 204 No Content |
| `Prefer: count=exact` | ✅ Parsed | Count mode recognized |
| `Range` | ❌ Not supported | Range header for pagination |
| `Accept` | ❌ Not supported | CSV, GeoJSON output formats |

## Response Headers

| Header | Status | Notes |
|--------|--------|-------|
| `Content-Range` | ✅ Supported | For return=minimal responses |
| `x-sdb-bookmark` | ✅ Supported | Custom bookmark for read consistency |
| `Access-Control-Allow-Origin` | ✅ Supported | Configurable via `SDB_CORS_ORIGINS` |

## Error Responses

| Feature | Status | Notes |
|---------|--------|-------|
| PostgREST-style `{code, message, details, hint}` | ✅ Supported | Normalized error format |
| HTTP status codes | ✅ Supported | 400, 401, 403, 404, 409, 425, 500, 502, 503, 504 |

## Auth & RLS

| Feature | Status | Notes |
|---------|--------|-------|
| `anon` role | ✅ Supported | Anonymous access with RLS |
| `authenticated` role | ✅ Supported | Authenticated user with RLS |
| `service_role` role | ✅ Supported | Bypasses RLS, admin access |
| `admin` role | ✅ Supported | Admin access |
| Row-Level Security (RLS) | ✅ Supported | Custom policy DSL with `allow`, `role_in`, `and`, `or`, `column`, `equals_claim`, `in_claim`, `equals` |
| Policy bypass for service_role | ✅ Supported | service_role bypasses all policies |

## Storage

| Feature | Status | Notes |
|---------|--------|-------|
| `PUT /v1/projects/{id}/storage/{bucket}/{key}` | ✅ Supported | Upload object |
| `GET /v1/projects/{id}/storage/{bucket}/{key}` | ✅ Supported | Download object (actor-enforced) |
| `DELETE /v1/projects/{id}/storage/{bucket}/{key}` | ✅ Supported | Delete object |
| Owner-based access control | ✅ Supported | Objects check `owner_id` against actor `sub` |
| `POST /storage/v1/buckets` | ✅ Supported | Supabase Storage bucket create |
| `GET /storage/v1/buckets` | ✅ Supported | Supabase Storage bucket list |
| `GET /storage/v1/buckets/{id}` | ✅ Supported | Supabase Storage bucket get |
| `DELETE /storage/v1/buckets/{id}` | ✅ Supported | Supabase Storage bucket delete (empty only) |
| `POST /storage/v1/object/{bucket}/{key}` | ✅ Supported | Supabase Storage object upload |
| `GET /storage/v1/object/{bucket}/{key}` | ✅ Supported | Supabase Storage object download |
| `PUT /storage/v1/object/{bucket}/{key}` | ✅ Supported | Supabase Storage object update |
| `DELETE /storage/v1/object/{bucket}/{key}` | ✅ Supported | Supabase Storage object delete |
| `POST /storage/v1/object/list/{bucket}` | ✅ Supported | Supabase Storage list objects (prefix, limit, offset) |

## Events & Realtime

| Feature | Status | Notes |
|---------|--------|-------|
| `GET /v1/projects/{id}/events` | ✅ Supported | Admin-only (service_role/admin) |
| `GET /v1/projects/{id}/realtime` | ✅ Supported | SSE stream, admin-only |
| `GET /realtime/v1/stream` | ✅ Supported | Supabase Realtime SSE stream (authenticated+), table filter, `{type, table, schema, record, old}` format |

## Management API

| Feature | Status | Notes |
|---------|--------|-------|
| `POST /v1/tokens` | ✅ Supported | Admin-only, disabled in production |
| `POST /v1/projects` | ✅ Supported | Admin-only |
| `POST /v1/projects/{id}/hibernate` | ✅ Supported | Admin-only |
| `POST /v1/projects/{id}/crash` | ✅ Supported | Admin-only |
| `GET /v1/projects/{id}/schema` | ✅ Supported | Admin-only |
| `POST /v1/projects/{id}/tables` | ✅ Supported | Admin-only |
| `POST /v1/projects/{id}/policies` | ✅ Supported | Admin-only |
| `GET /v1/projects/{id}/policies` | ✅ Supported | Admin-only |
| `POST /v1/projects/{id}/buckets` | ✅ Supported | Admin-only |

## Not Yet Supported

- PostgREST resource embedding (foreign key joins via `select=*,foreign_table(*)`)
- CSV / GeoJSON / XML output formats
- Range operators (`cs`, `cd`, `ov`, `sl`, `sr`, `nxr`, `nxl`, `adj`)
- Full-text search operators (`fts`, `plfts`, `phfts`, `wfts`)
- `NOT` filter negation
- Stored procedure calls (`/rest/v1/rpc/{function}`)
- Supabase Auth (GoTrue) integration
- Supabase Realtime WebSocket subscriptions (SSE seed implemented, WebSocket not yet)
- Supabase Storage signed URLs
