# Compatibility v2 Matrix

日期：2026-06-21

本文定义 v2 的 Supabase-compatible 验收口径。它基于当前 Rust core 实现和仓库脚本，不把未落地能力写成已完成。

## Tier 定义

| Tier | 使用场景 | 判定口径 |
| --- | --- | --- |
| Tier0 | 当前 POC 和内部 demo | 能跑通现有 demo、blog example、Rust core tests；允许使用自定义 `/v1` API |
| Tier1 | Browser `@supabase/supabase-js` app | 浏览器只拿 anon key，通过 Auth 获得 session；PostgREST、Storage、Realtime 常规用法可运行 |
| Tier2 | Server-side `@supabase/supabase-js` / backend | 服务端使用 service role/admin key；支持管理、幂等、强一致读、Dashboard 后端和运维 API |

不在 Tier1/Tier2 的范围：

- Postgres wire protocol。
- 任意 Postgres extension、PL/pgSQL、trigger。
- 对已有 Supabase/Postgres 项目的无损迁移。
- 单 project 多主写。

## 总体矩阵

| Surface | v2 目标能力 | 当前已实现 | 主要缺口 |
| --- | --- | --- | --- |
| Browser client | `createClient(url, anonKey)`、Auth session、PostgREST、Storage、Realtime、CORS allowlist | `apikey` 或 `Authorization` 可作为 actor；`/rest/v1`、`/auth/v1`、`/storage/v1`、`/realtime/v1/stream` 已有原型 | anon key 生命周期、Auth service 运维边界、Realtime WebSocket/channel、项目级 CORS、browser 禁用 service role |
| Server-side SDK | service role、RLS bypass、admin APIs、idempotent writes、bookmark 强一致读 | `service_role/admin` actor、管理 API、idempotency key、bookmark、replica forwarding 已有 | key rotation、多项目 key scope、Admin Auth API、SSR helpers、服务端 conformance 仍需系统化 |
| GoTrue Auth | email/password、refresh、logout revoke、user update、settings、durable users/sessions | `/auth/v1/signup`、`token`、`logout`、`user`、`settings` 原型；users/sessions/refresh tokens 写入 project SQLite；Argon2id 新 hash，兼容旧 SHA256 | 无 email/OAuth/MFA/password reset/admin users；缺少独立 Auth service 运维边界和 session 观测 |
| PostgREST | CRUD、projection、filters、order/range/count、single/maybeSingle、Prefer、upsert、错误模型 | `/rest/v1/{table}`；projection、eq/neq/gt/gte/lt/lte/in/is/like/ilike、not/and/or、order、limit/offset；basic single/maybeSingle 行为 | upsert conflict 语义、count header、RPC、embedding/join、Postgres 错误细节、schema cache |
| Storage | bucket CRUD、object upload/download/list/delete、owner/policy、signed URL、repair/GC | `/storage/v1/buckets` admin-only；`/storage/v1/object/*` private-by-default；authenticated owner-only；service_role bypass | public bucket、bucket policy DSL、signed URL、multipart/resumable、metadata/object 原子性和后台 repair 不完整 |
| Realtime | Supabase channel subscribe、postgres_changes、broadcast/presence Tier1 子集、policy filtering | `_sdb_outbox`、JSON polling、SSE、`/realtime/v1/stream?table=`；`realtime_protocol.rs` 有 frame 类型 | WebSocket service、channel lifecycle、presence/broadcast、row-level filtering、backfill/ack |
| Dashboard | 项目、schema、Auth、Storage、Realtime、HA、WAL/snapshot、audit 的管理 UI | 未实现；仅有 reports 和 CLI/demo | Console app、Dashboard API、权限模型、审计、可视化和运维工作流 |

## Browser Client

### Tier1 目标

- Browser 使用 anon key 初始化 `createClient(url, anonKey)`。
- `auth.signUp`、`signInWithPassword`、`getUser`、`refreshSession`、`signOut` 可用。
- Auth session 自动进入 PostgREST/Storage/Realtime 请求。
- PostgREST 支持常规 CRUD、filter、order、limit/range、single/maybeSingle。
- Storage 支持 bucket 可见性、object upload/download/list/delete。
- Realtime 支持 table-level subscribe 的 Supabase-compatible API。
- CORS 必须按 project allowlist；生产环境不能返回 wildcard。
- Browser 永远不能使用 service role key，Gateway 需要阻断 browser origin 携带 service role key。

### 当前状态

- `actor()` 支持 `Authorization: Bearer` 和 `apikey` header。
- Supabase preflight 暴露 `authorization, apikey, content-type, prefer, x-client-info, x-sdb-bookmark, x-d1-bookmark, idempotency-key`。
- 非生产且未配置 `SDB_CORS_ORIGINS` 时会允许 `*`。
- `/auth/v1/*`、`/rest/v1/{table}`、`/storage/v1/*`、`/realtime/v1/stream` 已在 Rust HTTP 层注册。

### 缺口

- anon key 还不是 project-scoped managed key，只是 HS256 token 的一种用法。
- Auth 用户和 refresh token 当前写入 project SQLite，可随 snapshot/WAL 恢复；但仍缺少独立 Auth service、key/session 运维和跨节点可观测。
- Realtime 不是 Supabase WebSocket channel。
- CORS 没有项目配置面。
- SDK conformance 需要区分 browser bundle、Node、SSR 三种环境。

## Server-side Supabase JS

### Tier2 目标

- Server 使用 service role key 初始化 `createClient(url, serviceRoleKey)`。
- service role 可绕过 row policy，但所有管理操作写 audit。
- 写请求支持 `Idempotency-Key` / `x-sdb-idempotency-key`，用于 retry 和 replica forwarding。
- 强一致读支持 bookmark/session，并能跨 primary/replica 路由。
- 提供 Auth Admin、Storage Admin、Project Admin、Dashboard backend API。
- 支持 key rotation 和 per-project secret scope。

### 当前状态

- `Actor::is_admin()` 把 `service_role` 和 `admin` 视为管理角色。
- `/v1/projects`、schema、table、policy、bucket、hibernate、crash 当前要求 admin/service_role。
- write idempotency 记录在 `_sdb_idempotency`。
- forwarding client 对 GET/HEAD 和带幂等键的写请求支持 retry。
- `supabase_project_id` 把 `/rest/v1/{table}`、`/auth/v1/*`、`/storage/v1/*` 映射到单个默认 project。

### 缺口

- `control_plane.rs` 有 in-memory API key catalog 原型，但尚未接入 Gateway；仍没有 production project-scoped anon/service-role key 管理、轮换和吊销。
- `/auth/v1/admin/*` 未实现。
- `/rest/v1/rpc/*` 未实现。
- 多 project 的 Supabase-compatible host/path 映射尚未定义；当前默认是单 `--supabase-project-id`。
- Dashboard 后端 API 未实现。

## GoTrue Auth

### Tier1 目标

- email/password sign up 和 sign in。
- refresh token rotation。
- logout 支持 Supabase.js scope：`global`、`local`、`others`。
- `getUser`、`updateUser`、`settings`。
- 用户、身份、refresh token、session 持久化并可跨 data-plane 实例恢复。

### Tier2 目标

- Admin user CRUD。
- 邀请、封禁、删除、重置密码。
- 审计登录事件。
- 可选 OAuth、magic link、MFA、email/SMS provider。

### 当前状态

- `/auth/v1/signup` 创建用户并立即 autoconfirm。
- `/auth/v1/token?grant_type=password` 支持 email/phone + password。
- `/auth/v1/token?grant_type=refresh_token` 支持 refresh。
- `/auth/v1/user` 支持 get/update。
- `/auth/v1/settings` 返回静态配置。
- `auth_store.rs` 已定义 `_sdb_auth_users`、`_sdb_auth_sessions` 和 `_sdb_auth_refresh_tokens`，当前 Auth 原型通过 project SQLite 持久化。
- refresh token 支持 TTL、rotation 和 logout revoke。
- Access token claims 写入 `session_id`。
- 新注册/改密写入 Argon2id password hash；旧 SHA256 hash 仍可登录。
- Access token 是本项目 HS256 JWT，role 为 `authenticated`。

### 缺口

- refresh token 还没有设备列表、session family reuse detection。
- 缺少 email confirmation、password recovery、OAuth、MFA、admin users API。
- Auth 数据进入 project snapshot/WAL 链路，但还没有独立 control-plane durable Auth store 和运维 API。

## PostgREST

### Tier1 目标

- `select("*")` 和 column projection。
- `insert().select()`、`update().eq().select()`、`delete().eq().select()`。
- `eq/neq/gt/gte/lt/lte/in/is/like/ilike/not`。
- chained AND 和 top-level OR。
- `order`、`limit`、`range`、`single`、`maybeSingle`。
- `Prefer: return=minimal/representation`。
- count headers 至少支持 exact。
- 错误响应接近 Supabase/PostgREST 客户端预期。

### Tier2 目标

- upsert conflict resolution，包括 `onConflict` 和 merge/ignore duplicates。
- RPC 子集：映射到显式注册的 server functions，而不是任意 Postgres function。
- schema cache/introspection。
- 更完整的 PostgREST error code 和 header 行为。

### 当前状态

- `postgrest.rs` 已有 projection、filter expr、order、limit、offset、count mode 的解析结构。
- SQL 编译支持 projection、AND/OR/NOT、多个比较操作、IN、IS、LIKE/ILIKE、ORDER BY、LIMIT/OFFSET。
- HTTP 层支持 `.single()` / `.maybeSingle()` 依赖的 object accept header 行为。
- `Prefer: return=minimal` 对 mutation 有 204 分支；representation 返回 rows。
- 错误响应返回 `code/message/details/hint`。

### 缺口

- `prefer_count()` 已存在但 read response 未真正写 `Content-Range` count。
- upsert 目前没有 conflict target 和 merge/ignore semantics；“不存在时 insert”不等于完整 upsert。
- `Range` header 和 `Content-Range` 的 PostgREST 语义不完整。
- 嵌套 select、resource embedding、foreign table join 未实现。
- `/rpc` 未实现。
- SQLite 类型和 Postgres 类型/错误码仍有差异。

## Storage

### Tier1 目标

- bucket create/list/get/delete。
- object upload/download/list/delete。
- content-type、etag、size、owner metadata。
- private bucket 默认策略，后续再补 public bucket 显式配置。
- browser 和 server SDK 均可用。

### Tier2 目标

- signed URL。
- move/copy。
- bucket policy/admin API。
- resumable/multipart roadmap。
- object/metadata repair 和 GC。

### 当前状态

- `/storage/v1/buckets` 支持 create/list，要求 service_role/admin。
- `/storage/v1/buckets/{bucket_id}` 支持 get/delete，要求 service_role/admin。
- `/storage/v1/object/{bucket_id}/{key}` 支持 POST/PUT upload、GET download、DELETE，要求 authenticated 或 service_role/admin。
- `/storage/v1/object/list/{bucket_id}` 支持 prefix、limit、offset，runtime 层按 actor 过滤。
- object bytes 存 object store；metadata 在 `_sdb_objects`。
- 非 admin 访问 object 时会检查 `owner_id == actor.sub`；无 owner 的旧 object 只允许 service/admin。

### 缺口

- bucket 级 public policy 和 bucket policy DSL 不完整。
- 上传先写 object store 再写 metadata；失败恢复需要更完整的 `pending/committed` repair。
- 删除中间态有 `deleting`，但后台 GC/repair 还不是常驻服务。
- signed URL、copy、move、multipart/resumable 未实现。

## Realtime

### Tier1 目标

- Supabase-compatible Realtime channel。
- `postgres_changes` table filter。
- authenticated user 只收到 policy 允许的 row events。
- reconnect 后可按 cursor/bookmark 补事件。

### Tier2 目标

- broadcast。
- presence。
- 服务端 fanout、backpressure、metrics。
- channel auth audit。

### 当前状态

- 所有 insert/update/delete/storage_put/storage_delete 会写 `_sdb_outbox`。
- `/v1/projects/{project_id}/events` 支持 polling，要求 service_role/admin。
- `/v1/projects/{project_id}/realtime` 支持 SSE，要求 service_role/admin。
- `/realtime/v1/stream` 支持 SSE，authenticated/service_role 可访问，支持 `table` query filter。
- SSE payload 包含 `type/table/schema/record/old`。

### 缺口

- `realtime_protocol.rs` 已有 Phoenix/Supabase-like frame 类型和 parser/encoder，但 HTTP 层还没有 WebSocket service。
- 无 channel join/leave、heartbeat、ack。
- 无 broadcast/presence。
- `table` filter 只是简单表名过滤，没有 row-level policy filtering。
- outbox retention、cursor、backfill 策略未定义。

## Dashboard

### Tier2 目标

- Project list/detail：state、primary、replica、region、route revision。
- Schema/table viewer：表、列、数据浏览、policy。
- Auth：users、sessions、refresh token、admin user 操作。
- Storage：bucket/object browser、metadata、signed URL。
- Realtime：channels、outbox lag、subscriber 状态。
- Durability：manifest、generation、bookmark、WAL bytes、snapshot history、restore dry-run。
- HA：failover history、replica lag、endpoint health、manual promote/hibernate/warm。
- Audit：管理操作、Auth 事件、secret rotation。

### 当前状态

- 没有 Dashboard app。
- `project_info` 已能提供一部分运行态信息：bookmark、local/remote commit seq、replica lag、routing、manifest、writer lease、WAL bytes。
- `reports/blog-app-verification-report.md` 是示例报告输出，不是交互式控制台。

### 缺口

- 需要新增 Dashboard frontend 和 Dashboard backend APIs。
- 需要权限模型，区分 owner、developer、viewer、platform admin。
- 需要审计日志和危险操作二次确认。
- 需要可观测数据源，不应只靠单次 HTTP 查询。

## 验收要求

每个矩阵项从 partial 变成 supported 前必须满足：

- 有 Rust core 测试或 integration test。
- 有真实 `@supabase/supabase-js` conformance 覆盖。
- `examples/blog-app/app.mjs` 或 Dashboard e2e 覆盖至少一条端到端路径。
- 文档列出与 Supabase/Postgres 的已知偏差。
- 分布式相关能力需要在 Docker primary + replica 拓扑下验证。

## 当前需要优先处理的漂移

- `README.md` 已更新 Phase 0 安全语义；后续仍需要在控制面/Dashboard 文档中补 project key routing 和多租户权限边界。
- `scripts/supabase-compat-check.mjs` 的兼容范围说明只到 `eq/limit`，已落后于 `postgrest.rs` 的解析能力。
- `scripts/bootstrap-distributed-poc.sh` 已改为本地按 `SDB_JWT_SECRET` 签发 service/user JWT，再用 service_role 调用管理面；后续仍需要把它纳入 Docker 分布式 smoke test。
- `tests/supabase-sdk.test.mjs` 需要与当前 `PolicySpec` 结构核对，避免测试声明覆盖但请求体不匹配。
