# Architecture v2: Supabase-compatible Serverless DB Platform

日期：2026-06-21

本文基于当前仓库实现写作，重点参考 `AGENTS.md`、`README.md`、`package.json`、`rust-core/src/http.rs`、`rust-core/src/runtime.rs`。本文描述 v2 目标架构，不把规划误标成已实现能力。

## 当前实现基线

当前仓库已经有一个可运行的 Rust data-plane POC：

- HTTP 层在 `rust-core/src/http.rs`，暴露 `/v1/projects/*` 管理 API、`/rest/v1/{table}` PostgREST-like API、`/auth/v1/*` GoTrue-like API、`/storage/v1/*` Storage-like API、`/realtime/v1/stream` SSE 入口。
- 运行时在 `rust-core/src/runtime.rs`，每个 project 使用一个本地 SQLite WAL cache；对象存储中的 generation snapshot、WAL segment、manifest 是 durable source of truth。
- 写路径是单 project 单主写入：project writer queue 串行执行 mutation，group commit 后追加 WAL segment，写 manifest 成功后才回复。writer lease 使用 object-store `put_bytes_if_absent` 和 fencing token，后台续约。
- 读副本模式通过 `--read-replica` 启动，不接受本地写入；副本按 manifest refresh，带 bookmark 的读会等待副本追上，追不上可转发 primary。primary/gateway 可以按 `route_region` 或 `x-sdb-region` 选择 replica。
- Storage 当前把 object bytes 写入同一个 object-store adapter，把元数据写入 project SQLite 的 `_sdb_objects`。
- Realtime 当前是 `_sdb_outbox` + polling/SSE；`realtime_protocol.rs` 已有 Phoenix-style frame 数据结构，但还不是 Supabase Realtime WebSocket service。
- GoTrue-like Auth 当前已通过 `auth_store.rs` 写入 project SQLite 的 `_sdb_auth_users` 和 `_sdb_auth_refresh_tokens`，随 project snapshot/WAL 持久化；它仍不是独立、多项目、可运维的 Auth service。
- `control_plane.rs` 已有 in-memory project catalog、API key 和 placement 原型，但尚未接入 HTTP gateway、部署拓扑或生产级强一致存储。
- Dashboard、自动 project placement、自动 primary failover 尚未实现。

当前 Docker 分布式拓扑是 `rustfs + primary + replica-a + replica-b`。它验证了共享对象存储、异步副本追赶、写转发和 region routing，但 primary 仍是静态配置，不是自动选主。

## v2 目标

v2 的目标是把当前 POC 推进为一个 Supabase-compatible 的低成本 serverless DB 平台：

- **Browser client 支持**：真实 `@supabase/supabase-js` browser 用法可以用 anon key、Auth session、PostgREST、Storage、Realtime 跑通，且 service role 永远不能暴露给浏览器。
- **Server-side SDK 支持**：Node/server 环境可以使用 service role key、幂等写入、强一致读、admin/control APIs 和 SSR session helper 的兼容子集。
- **单 project scale-to-zero**：没有请求时释放本地 SQLite cache、writer worker、replica refresh worker 和租约；下一次请求按 manifest 冷启动恢复。
- **分布式多机器 HA**：project 可以在多台 data-plane 节点之间迁移；primary 故障时由 control plane 选择候选节点，候选节点从 durable layer 恢复并接管 writer lease。
- **Tier1/Tier2 兼容全部实现**：以 `docs/compatibility-v2.md` 的矩阵为验收标准，所有 Tier1/Tier2 必需项要有测试、示例和明确行为。
- **GoTrue-compatible Auth**：在当前 project SQLite Auth store 基础上，补齐 durable session/key 生命周期、admin API 和多实例运维边界。
- **Dashboard**：提供项目、表、策略、Auth、Storage、Realtime、replica lag、WAL/snapshot、failover 和审计的运维界面。

## 非目标

v2 不把 SQLite tier 包装成完整 Postgres：

- 不实现 Postgres wire protocol。
- 不实现任意 Postgres extension、PL/pgSQL、trigger、foreign data wrapper。
- 不承诺现有 Supabase/Postgres 项目无损迁移。
- 不做单 project 多主写入。单 project 仍保持单主写，靠 replica 扩展读。
- 不把 Iceberg/Arrow/DuckDB 放入 OLTP 写路径。分析层只能从 outbox/export 派生。
- 不把对象存储 list/overwrite 语义当作唯一的生产级协调机制。

## 长期架构取向

长期主线见 `docs/long-term-architecture-research.md`。结论是继续演进当前 D1/Turso-style `lite-serverless` data plane：借 D1 的 session/bookmark 一致性合同，借 Turso 的 SQLite/object-store 成本模型，按 Neon 的存储计算分离原则补齐 durable/control plane。若未来要完整 Supabase/Postgres parity，应作为独立 `postgres-serverless` tier，而不是把 SQLite core 包装成 Postgres。

## 组件边界

| 组件 | v2 职责 | 当前仓库状态 | v2 需要补齐 |
| --- | --- | --- | --- |
| API Gateway | TLS、CORS、anon/service-role key 校验、project routing、rate limit、兼容 headers、request audit | 目前并入 Rust HTTP 层 | 独立网关或 Rust gateway 模式，按 project 配置 CORS 和 key |
| Control Plane | project lifecycle、placement、scale-to-zero、failover、replica topology、quota、secret rotation | `control_plane.rs` 有 in-memory 原型，未接入服务 | 建议 Go 服务或独立 Rust gateway，使用强一致元数据存储 |
| Auth Service | GoTrue-compatible users、sessions、refresh tokens、key issuance、admin users API | `/auth/v1/*` 原型，`auth_store.rs` 持久化到 project SQLite | 密码哈希、logout revoke、admin API、key/session 生命周期、email/OAuth/MFA roadmap |
| Data Plane Primary | SQLite writer、policy enforcement、WAL/snapshot/manifest、Storage metadata、outbox | Rust core 已有 | 租约 CAS、failover epoch、metrics、per-project resource limits |
| Data Plane Replica | manifest refresh、bookmark wait、read serving、write forwarding | Rust core 已有原型 | lag-aware routing、自动 placement、读 SLA |
| Durable Object Store | snapshot、WAL segment、Storage object、change log、审计记录 | local + S3/RustFS adapter | production S3 semantics validation、retention、GC、PITR |
| Coordination Store | primary lease、routing revision、project state、failover epoch | 当前只用 object-store lease 原型 | 需要强一致 CAS store |
| Realtime Service | 将 outbox 变为 Supabase Realtime 订阅语义 | polling/SSE 原型，`realtime_protocol.rs` 有 frame 结构 | WebSocket service、channel/topic、presence/broadcast、row policy filtering |
| Dashboard | 项目/数据/策略/Auth/Storage/HA 运维 UI | 未实现 | TypeScript console + control APIs |
| Observability | metrics、trace、audit、SLO、容量账单 | `project_info` 暴露部分 lag/routing | OpenTelemetry/Prometheus、日志和告警 |

## 请求路径

### Browser Auth + Data

1. Browser 使用 anon key 调用 Gateway。
2. Gateway 校验 project、CORS、anon key，转发 `/auth/v1/*` 到 Auth Service。
3. Auth Service 返回 access token 和 refresh token；access token 进入 `Authorization: Bearer`。
4. Browser 调用 `/rest/v1/{table}`、`/storage/v1/*`、`/realtime/v1/*`。
5. Gateway 根据 project registry 选择 primary 或 replica；写请求和强一致读走 primary，默认读可走 replica。
6. Data plane 用 JWT actor 和 policy DSL/RLS 兼容层过滤行，再访问 SQLite cache。

### Server-side SDK

1. Server 使用 service role key 调用 Gateway。
2. Gateway 标记 request 为 privileged，不允许从 browser origin 使用 service role。
3. Server-side PostgREST/Storage/Auth admin API 可绕过用户 RLS，但所有写入必须带 request id 或 idempotency key。
4. 对写入、用户管理、bucket 管理和 Dashboard 操作写 audit log。

### 写路径

1. Gateway 路由写请求到当前 project primary；如果误到 replica，replica 只允许转发到 primary。
2. Primary 校验 actor、policy、idempotency key。
3. Mutation 进入 project writer queue；队列满返回 429。
4. Writer 按 batch 执行 SQLite transaction，记录 outbox/idempotency。
5. Writer 读取 SQLite WAL 增量，上传 WAL segment 到 object store。
6. Writer 写 manifest，manifest 包含 generation、snapshot checksum、WAL segment checksum、commit_seq、bookmark、owner_id、fencing token。
7. manifest durable 成功后返回结果和 `sdb1-*` bookmark。

### 读路径

1. 默认读可以由 Gateway 选就近 replica。
2. 带 `bookmark`、`x-sdb-bookmark`、`x-d1-bookmark` 或 `session=bookmark` 的读必须至少追到指定 seq。
3. Replica 若落后，先按 manifest refresh；超时后按策略 fallback primary 或返回 425。
4. `session=first-primary` 强制走 primary。
5. 响应带 served-by metadata，便于 Dashboard 和客户端定位一致性行为。

### Storage

1. object bytes 写入 durable object store。
2. `_sdb_objects` 记录 bucket、key、size、content_type、etag、owner_id、state。
3. v2 必须把 object 写入和 metadata 状态机做成可恢复流程：`pending -> committed -> deleting -> deleted`。
4. GC/repair worker 负责清理 orphan object 和 stuck deleting metadata。

### Realtime

1. Data plane mutation 写 `_sdb_outbox`。
2. Realtime Service 读取 outbox，按 topic/table/filter 和 actor policy 做过滤。
3. Browser 使用 Supabase-compatible channel protocol；当前 SSE 可作为内部调试或 Tier0 fallback。

### Dashboard

1. Dashboard 只调用 control/admin API，不直接访问 data-plane 私有接口。
2. Dashboard 后端使用 service role/admin token，所有敏感操作写 audit log。
3. Dashboard 需要展示 project state、primary/replica、lag、current bookmark、WAL bytes、snapshot generation、Auth users、buckets、policy、failover history。

## Scale-to-zero 状态机

当前实现已经有 `hibernate(project_id)` 和 `crash_project(project_id)` 原型。v2 需要把它变成 control-plane 驱动的生命周期状态机。

| 状态 | 含义 | 进入条件 | 退出条件 |
| --- | --- | --- | --- |
| `Absent` | 没有 project manifest | 新项目尚未创建 | admin 创建 project |
| `Cold` | object store 有 manifest/snapshot/WAL，本机无 SQLite cache | hibernate 后、节点重启后、调度到新节点 | 收到读写请求或预热指令 |
| `Warming` | 正在下载 snapshot、校验 checksum、回放 WAL | `Cold` 收到请求 | 成功打开 SQLite connection |
| `HotPrimary` | 有 writer queue、lease renewer、SQLite WAL cache | primary warm 成功并获得 writer lease | idle timeout、迁移、failover、节点退出 |
| `HotReplica` | 有只读 SQLite cache、replica refresh worker | replica warm 成功 | idle timeout、replica 下线 |
| `Draining` | 停止接新写，等待 writer batch 完成，必要时 compact snapshot | scale-to-zero、迁移、主动降级 | `Cold` 或 `Fenced` |
| `Fenced` | 本地 runtime 发现 lease 已被其他 owner 接管 | 旧 primary 过期或 failover 后继续写 | evict cache 后重新按 manifest warm |
| `Recovering` | crash 后按 manifest 恢复 | `crash_project`、进程退出、cache 损坏 | `HotPrimary` 或 `HotReplica` |

关键约束：

- `Cold` 到 `HotPrimary` 必须先按 manifest 恢复，再获取 writer lease；不能复用旧 cache 作为 source of truth。
- `HotPrimary` 到 `Cold` 的优雅路径必须先 stop writer，再 persist snapshot，然后删除本地 cache 并释放 lease。
- `crash_project` 类似非优雅退出：不强制 snapshot，只删除 cache；下一次请求必须从 snapshot + durable WAL 恢复。
- scale-to-zero 不应删除 durable manifest、snapshot、WAL、Storage object、Auth/control metadata。
- Auth 当前已经随 project SQLite snapshot/WAL 持久化；v2 仍需要把 key、session 运维、admin API 和跨节点 Auth 可观测从 data-plane 热路径中拆出来。

## 分布式 HA 与 Failover

### 当前能力

- `deploy/docker-compose.distributed.yml` 已有一个 primary 和两个 read replica，共享同一个 RustFS/S3 prefix。
- primary 暴露 replica routing endpoint，replica 配置 primary URL。
- read replica 可以异步追 manifest，也可以把写请求转发到 primary。
- endpoint health 只在单 runtime 内存中记录，当前没有全局健康视图。
- primary 角色不是自动选举；primary 故障时不会自动改路由。

### v2 设计

新增 Project Registry 和 Lease Store：

- `project_id`
- `desired_state`: `cold`、`hot-primary`、`hot-replica`
- `primary_node_id`
- `primary_epoch`
- `routing_revision`
- `replica_nodes`
- `last_manifest_generation`
- `last_bookmark`
- `health`
- `quota`

Failover 流程：

1. Gateway 或 Control Plane 发现 primary health 超时，停止把新写请求路由到旧 primary。
2. Control Plane 从 healthy 节点中选择 candidate，创建 `failover_epoch`。
3. Candidate 按 object-store manifest 进入 `Warming`，校验 snapshot 和 WAL checksum。
4. Candidate 在 Coordination Store 中 CAS 获取 project primary lease，并得到新的 fencing token。
5. Candidate 写入新的 route revision，Gateway 开始把写请求发到新 primary。
6. 旧 primary 如果恢复，因 fencing token/epoch 落后，写入必须返回 423 或主动 evict cache。
7. Replicas 继续按新 manifest refresh；落后读按 bookmark contract fallback 或 425。

写可用性语义：

- failover 期间写请求可以短暂 503/425/423，客户端必须使用 idempotency key 重试。
- 读请求可继续走 stale replica，但带 bookmark 的读必须等到目标 seq 或 fallback primary。
- v2 不承诺零中断写入，承诺 bounded RTO 和不产生 split-brain acknowledged write。

Split-brain 防线：

- Coordination Store CAS lease 是第一道防线。
- manifest 必须带 `owner_id`、`fencing_token`、`primary_epoch`、`previous_generation`。
- manifest 写入需要 If-Match/版本 CAS；当前普通 object-store put 只能作为 POC。
- Data plane 每次 durabilize 前重新确认 lease；失去 lease 后返回 423。
- Gateway 使用 route revision，拒绝旧 revision 节点继续作为 primary。

## Durable Layer 选择

v2 采用两层 durable layer：

| 层 | 选择 | 用途 | 原因 |
| --- | --- | --- | --- |
| Bulk durable data | S3-compatible object store | SQLite snapshot、WAL segment、Storage object、change log、backup/export | 成本低、容量弹性好、适合 immutable blob |
| Coordination/control metadata | 强一致 KV 或 SQL store | primary lease、routing revision、project registry、Auth users/sessions、keys、quota、audit index | 需要 CAS、事务、可观测和多实例一致性 |

具体建议：

- 单 region MVP 可用 Postgres/RDS 或 etcd 存 control plane 元数据和 lease，要求行版本 CAS。
- 多 region 生产版使用明确的 consensus 系统或云厂商强一致数据库。不能只依赖 S3 list 和普通 overwrite 做选主。
- 对象存储仍保留 `put_bytes_if_absent` 用于辅助 claim/audit，但不作为唯一的 failover 仲裁。
- SQLite user data 仍在 project snapshot/WAL 中，不把用户表迁入 control-plane Postgres。
- Auth/control metadata 必须独立持久化，避免 project hibernate 或 data-plane 重启导致用户/session 丢失。

## 兼容性分级

v2 的“兼容全部实现”按 Tier 定义验收，不宣称完整 Supabase/Postgres parity。

| Tier | 目标用户 | 必须支持 | 明确不包含 |
| --- | --- | --- | --- |
| Tier0 | 当前 POC / 内部验证 | `/v1` API、基础 `/rest/v1`、Storage/Auth/Realtime 原型、Docker 分布式验证 | 对外兼容承诺 |
| Tier1 | Browser Supabase app | anon key、email/password Auth、session refresh、PostgREST CRUD/filter/transform、Storage bucket/object、Realtime table subscribe、project CORS | service role、admin user API、Postgres wire、SQL migrations |
| Tier2 | Server-side supabase-js / backend | service role、RLS bypass、admin Auth/Storage APIs、idempotent writes、strong reads/bookmark、management APIs、SSR token handling、Dashboard backend APIs | 任意 Postgres function、extension、PL/pgSQL |

所有 Tier1/Tier2 项必须有：

- 真实 `@supabase/supabase-js` 测试。
- Rust core 集成测试。
- `examples/blog-app/app.mjs` 或后续 Dashboard e2e 覆盖。
- 明确错误码和兼容偏差说明。

## 落地阶段

### Phase 0: Contract freeze

- 以 `docs/compatibility-v2.md` 冻结 Tier1/Tier2 范围。
- 清理 README、`scripts/supabase-compat-check.mjs`、`tests/supabase-sdk.test.mjs` 与当前 Rust 代码的漂移。
- 建立 compatibility report，区分 supported、partial、missing、out-of-scope。

### Phase 1: 安全和 Auth 持久化

- durable Auth store：users、identities、refresh tokens、sessions、keys。
- anon/service-role/admin key 生命周期和 rotation。
- `SDB_ENV=production` 下禁用开发 token minting。
- project-level CORS allowlist。
- 密码哈希改为 Argon2/bcrypt，logout 需要 revoke refresh token。

### Phase 2: Supabase SDK Tier1/Tier2 补齐

- PostgREST CRUD、projection、filters、order、range、count、single/maybeSingle、Prefer header、upsert conflict 行为。
- server-side service role、admin API 和幂等重试语义。
- 所有能力进入 SDK conformance 测试和 blog example。

### Phase 3: Storage 和 Realtime 生产化

- Storage metadata 状态机、GC/repair worker、bucket policy、signed URL roadmap。
- Realtime 从 SSE 原型升级到 Supabase channel protocol 的 Tier1 子集。
- Realtime 按 actor/policy 过滤，不泄露 outbox。

### Phase 4: Control Plane 和 scale-to-zero

- Project Registry、placement、idle detector、warm pool。
- per-project state machine 和 audit。
- hibernate/crash 从调试接口变为受控 lifecycle operation。
- cold start 指标：snapshot size、WAL bytes、rehydrate latency。

### Phase 5: HA/failover

- Coordination Store CAS lease。
- primary_epoch、route revision、manifest CAS。
- failover RTO 测试、stale primary fencing 测试。
- multi-node chaos tests：primary kill、object-store transient、replica lag、network partition。

### Phase 6: Dashboard

- 项目列表、schema/table viewer、SQL-like data browser。
- Auth users/session viewer。
- Storage bucket/object browser。
- Realtime/outbox inspection。
- HA 状态：primary、replica lag、bookmark、WAL bytes、snapshot generation、failover history。
- 运维操作：hibernate、warm、failover、restore dry-run、secret rotation。
