# Long-term Architecture Research: D1, Turso, Neon, and Adjacent Systems

日期：2026-06-21

## 结论

当前 repo 的长期演进，不应在 D1、Turso Cloud、Neon、Aurora DSQL 之间做单选复制。更合适的路线是：

**用 D1 的产品一致性模型 + Turso 的 SQLite/object-store 成本模型 + Neon 的存储计算分离原则，继续演进当前 Rust `lite-serverless` data plane；如果未来要完整 Supabase/Postgres parity，则另建 `postgres-serverless` tier，不把 SQLite core 硬改成 Postgres。**

换成架构语言：

1. `lite-serverless` 继续走当前方向：per-project SQLite/libSQL hot engine、单 project 单主写、对象存储 durable WAL/snapshot、异步 read replica、bookmark/session 读一致性、compute 可丢弃。
2. durable layer 要从当前 POC 的 snapshot + WAL manifest 演进到更明确的 segment/page-cache 体系：对象存储保存 immutable history，本地 NVMe/磁盘只做 cache，写入 ack 前必须进入 durable log。
3. control/coordination 不能长期只靠对象存储条件写。需要独立强一致 metadata/lease store，用于 primary epoch、routing revision、failover、API key、quota、audit。
4. Supabase-compatible API surface 保留为 gateway/service 层目标，但明确不是 Postgres wire / extension / PL/pgSQL 兼容。需要 exact Postgres 行为的客户进入单独 Postgres tier。
5. Turso Sync / local-first 是可选扩展，不是当前 server-side Supabase-like 平台的主一致性模型。

## 当前仓库基线

当前实现已经比单纯文档 POC 更接近 D1/Turso 低成本层：

- Rust core 已经以 SQLite 作为 per-project 热数据引擎。
- 本地 cache 不是 source of truth；对象存储中的 generation snapshot、WAL segment、manifest 才是 durable source。
- 写路径已有 per-project writer queue、group commit、durable manifest、writer lease/fencing、backpressure。
- read replica 已有 manifest refresh、bookmark wait、primary forwarding、region routing 原型。
- API 面已有 Supabase-like PostgREST/Auth/Storage/Realtime 子集。

这意味着长期决策的核心不是“选不用 SQLite”，而是“这个 SQLite/object-store tier 是否应该继续承担主产品路线，以及 Postgres parity 放在哪里”。

## 候选架构判断

| 架构 | 核心模型 | 适合作为长期主线吗 | 原因 |
| --- | --- | --- | --- |
| Cloudflare D1-like | SQLite 语义、单库单线程/单主、read replica、Sessions/bookmark、Workers/Durable Objects 生态 | **适合借鉴一致性和产品边界，不适合原样复制** | D1 明确服务很多小库和 Workers 生态，单库 10 GB、单库单线程、写入仍走 primary。它证明了当前 repo 的方向成立，但它不是通用 Supabase parity 架构。 |
| Turso/libSQL Cloud-like | SQLite-compatible/libSQL、S3/S3 Express durable、local cache、embedded replicas、sync、branching | **最适合借鉴 durable economics 和 sync/replica 方向** | Turso 的 diskless/S3-backed、compute 可替换、本地读/同步模型和当前 repo 高度接近。但 Turso Sync 的 local-first/last-push-wins 语义不应成为 server-side API 的默认事务模型。 |
| Neon-like Serverless Postgres | Postgres compute ephemeral、WAL quorum、pageserver、object storage immutable history、COW branching | **最适合 Postgres/Supabase parity，但不是当前 SQLite core 的自然下一步** | 如果目标是完整 Supabase 兼容，Neon-like 是可信路线。代价是分布式 WAL/page storage、Postgres 运维、cache miss、quorum 和 tenant isolation，复杂度远高于当前 POC。 |
| Supabase classic | Managed Postgres + Auth/Storage/Realtime/API | **适合作为 API/service surface 参照，不适合作为低成本 data-plane 参照** | 它的优势是 Postgres 生态和平台完整度；劣势是每 project compute/storage 经济性，不符合“海量长期空闲小项目”的核心目标。 |
| Aurora DSQL / CockroachDB-like distributed SQL | 分布式 SQL、强一致、多区域/多 AZ、自动分片或 active-active | **不适合作为当前 repo 主线** | 它们解决高可用和全局写扩展，但代价、语义、产品形态和“SQLite per-project 低成本层”不同。Aurora DSQL 也不是完整 PostgreSQL，应用需要适配 OCC、事务/DDL/feature 限制。 |
| DuckDB/Arrow/Iceberg | 对象存储上的分析/冷数据层 | **只适合作为派生层** | 适合 export、history、analytics、vector/offline，不适合作为 app-facing OLTP 写路径。 |

## 外部方案要点

### Cloudflare D1

D1 当前官方定位是 managed serverless SQL database，提供 SQLite SQL semantics、Time Travel、Global Read Replication、Workers/HTTP API access。D1 也明确强调可以创建大量数据库，按 query/storage 计费，而不是按每库实例计费。[^d1-overview]

和当前 repo 最相关的事实：

- D1 读副本是异步复制，读副本可能落后；Sessions API 通过 bookmark 提供 session 内顺序一致性。[^d1-replica]
- 写请求仍转发到 primary；read replication 只改善读延迟和读吞吐。[^d1-replica]
- 单个 D1 database 天然 single-threaded，逐个处理 query；过载时排队，队列满会返回 overloaded。[^d1-faq]
- paid 计划单库上限是 10 GB，且不能继续增大。[^d1-limits]

对我们的结论：

- **要学 D1 的 bookmark/session contract**。这比“读副本随机一致”更适合作为对外 SDK 语义。
- **要学 D1 的小库/多库产品定位**。当前 repo 的 per-project SQLite 很适合这个方向。
- **不要把它包装成通用大库**。单 project 仍然是单主写，适合小中型项目和低写冲突租户，不适合单大库高并发写。
- **不要把 D1 的 Durable Object 依赖当成我们自己的 durable layer**。我们没有 Cloudflare 内部 DO 存储能力，必须把强一致 lease/route 和 bulk object store 分层。

### Turso Cloud / libSQL

Turso Cloud 目前官方说明是 SQLite-compatible/libSQL 平台，下一代 Turso Database engine 仍在 alpha 并计划未来集成进 Turso Cloud。它提供 replication/sync、branching、analytics、BYOK 等能力。[^turso-cloud]

和当前 repo 最相关的事实：

- Turso Cloud durability 文档说明，AWS regions 使用 diskless architecture，数据由 S3-Express One Zone + S3 支撑；commit 在数据安全进入 S3 或 S3 Express 后才 ack，compute 节点可来可走，本地磁盘只是 cache。[^turso-durability]
- Embedded Replicas 的默认模型是本地读、写发往 remote primary，再同步回本地；它们适合 VM/VPS/mobile，但 serverless 无文件系统场景不能用，且同步时不要同时打开本地 DB。[^turso-embedded]
- Turso Sync 使用 local database + remote URL + auth token；push 发送本地变更，冲突策略是 last push wins；offline-first 场景可本地写，网络恢复后 push/pull。[^turso-sync]
- Partial Sync 支持不下载完整数据库，查询触达缺页时再从云端 lazy fetch page，并支持 segment/prefetch 来降低冷启动和随机 IO 成本。[^turso-partial]

对我们的结论：

- **Turso 的 durable economics 最值得学**：对象存储做 source of truth，本地磁盘只是 cache，commit ack 必须等 durable log。
- **Partial Sync/page lazy fetch 是我们 cold start 优化的下一阶段**：当前按 snapshot + WAL 全量 rehydrate，长期应演进为 page/segment 级恢复和热集预取。
- **Embedded/local-first 不应作为默认 API 一致性模型**：Supabase-like server API 更需要 primary-acknowledged transactions、idempotency、bookmark read；local-first conflict resolution 可以作为 SDK/offline 产品扩展。
- **libSQL 可以作为兼容/replication 技术选项评估，但不能替代 gateway policy**：RLS、Auth、Storage metadata、Realtime contract 仍由平台层承担。

### Neon

Neon 是更接近“完整 Supabase/Postgres parity”的参照。官方架构说明把 compute 视为 ephemeral/replaceable，storage durable/replicated/shared，WAL 是 source of truth，object storage 是基础。[^neon-architecture]

和当前 repo 最相关的事实：

- Neon 写路径是 Postgres 生成 WAL，WAL 发给多个 safekeepers，quorum ack 后 commit；page materialization 异步发生，不在事务 critical path。[^neon-write]
- 读路径避免直接读 object storage：优先 RAM/local NVMe，cache miss 时向 pageserver 请求指定 LSN 的 page，pageserver 可用 base page + WAL 重建。[^neon-write]
- object storage 保存 immutable history，但不被 compute 直接访问。[^neon-pageserver]
- compute failure 可通过 stateless compute 重新分配恢复；read replicas 读取同一 durable source，不需要额外复制存储，scale-to-zero 也不会引入传统 replica lag。[^neon-ha][^neon-replicas]

对我们的结论：

- **如果目标是 exact Supabase/Postgres，应该建立 `postgres-serverless` tier**，不要继续把 SQLite gateway 说成 Postgres。
- **Neon 的“WAL quorum + pageserver + object storage”是长期 durable layer 的上限模型**。我们不需要一步做到 Postgres/pageserver，但要避免让 object storage latency 进入热读写路径。
- **当前 SQLite tier 可以借鉴它的分层原则**：write ack 只等 durable log，不等 snapshot/materialization；object store 是历史底座，不是 hot query path；branch/PITR/restore 尽量变成 metadata operation。

### Aurora DSQL / Distributed SQL

Aurora DSQL 官方定位是 serverless distributed relational database，针对事务工作负载，PostgreSQL-compatible，active-active 架构提供 single-region 99.99% 和 multi-region 99.999% availability。[^aurora-dsql]

但它和当前目标不完全同类：

- 它是分布式 SQL/active-active，解决的是 always-available、强一致、多 AZ/多区域读写。
- 它不是完整 PostgreSQL：迁移文档明确要求应用适配不同 DDL、referential integrity、trigger-like 逻辑、OCC 冲突重试、事务限制等。[^aurora-compat]
- 它适合作为高可用企业 tier 的参照，不适合作为“海量小 SQLite project、长期空闲、对象存储低成本”的 data-plane 主线。

## 推荐长期架构

### 产品分层

保留并明确三层产品形态：

| Tier | 名称 | 引擎 | 对外承诺 |
| --- | --- | --- | --- |
| Tier A | `lite-serverless` | SQLite/libSQL-compatible Rust data-plane | Supabase-like SDK/API 子集、低成本、海量小库、scale-to-zero、read replica、bookmark consistency |
| Tier B | `postgres-serverless` | Neon-like 或外部 Postgres-compatible engine | 真 Postgres/Supabase compatibility，PostgREST/RLS/extensions/SQL 生态优先 |
| Tier C | `analytics-cold` | Object store + Parquet/Iceberg + DuckDB/Arrow workers | 历史、日志、analytics、export、向量/离线任务 |

当前 repo 应继续优先推进 Tier A。Tier B 是未来产品线或集成路线，不应污染 Tier A 的实现边界。

### Data plane

`lite-serverless` 的长期 data plane：

1. 每个 project 一个 SQLite/libSQL database history。
2. 每个 project 单 writer coordinator，所有 mutation 串行入 SQLite transaction，可 group commit。
3. ack write 前，WAL/log segment 必须进入 durable storage，并写入 CAS-protected manifest 或 log index。
4. 本地 cache 只保存 hot SQLite/page/segment，不作为 source of truth。
5. read replica 从 durable manifest/log 追 primary；带 bookmark 的读必须等待到目标 seq、fallback primary 或返回明确错误。
6. snapshot/compact 异步化，失败不能破坏旧 snapshot + WAL chain。
7. 后续从 full snapshot rehydrate 演进到 page/segment lazy restore，降低冷启动和大库恢复成本。

### Durable layer

分两层：

| 层 | 用途 | 长期选择 |
| --- | --- | --- |
| Bulk durable history | SQLite snapshots、WAL/log segments、Storage object、export files | S3-compatible object store，写入 immutable objects，配 retention/PITR/GC |
| Coordination metadata | primary lease、epoch、route revision、project registry、API keys、quota、audit index | 强一致 KV/SQL store，必须支持 CAS/transaction |

关键约束：

- object store 适合 blob/history，不适合单独承担选主和路由仲裁。
- manifest 必须带 `previous_generation`、`commit_seq`、`owner_id`、`fencing_token`、`primary_epoch`，并用版本 CAS 提交。
- failover 时先 rehydrate，再获得 writer lease/fencing token，最后发布新 routing revision。

### API / consistency contract

对外 API 应采用 D1-style session/read consistency，而不是隐藏 replica lag：

- 默认读：可走就近 replica，返回 `served_by_region`、`served_by_primary`、`bookmark`。
- `first-primary`：首读/强一致读走 primary。
- `bookmark`：客户端提交上次 bookmark，读路径必须至少追到该 seq。
- replica 追不上：按策略 fallback primary，或返回 `425/503` + retry metadata。
- 写请求：必须支持 idempotency key；跨 failover 重试不应重复提交。

### Auth / policy / storage

SQLite tier 不能让客户端绕过 gateway：

- RLS/policy enforcement 在 gateway/query layer 或受控 runtime 内执行。
- raw SQL 必须分级：browser/anon 禁止通用 raw SQL；server-side/admin raw SQL 也要经过 AST/policy/allowlist。
- Auth keys、sessions、API keys、project catalog 不应长期藏在单个 project SQLite 热路径里，至少 metadata/route/key lifecycle 要进入 control plane store。
- Storage object bytes 走 object store；metadata 状态机必须可恢复：`pending -> committed -> deleting -> deleted`。

## 演进路线

### P0：产品声明收敛

- README 和 docs 明确 `lite-serverless` 不是 Postgres replacement。
- 兼容矩阵继续使用 Tier0/Tier1/Tier2，不宣称完整 Supabase parity。
- 将“D1-like bookmark + Turso-like object-store WAL + Neon-like separation”作为主线写进 architecture docs。

### P1：强一致 control/coordination

- 引入 durable Project Registry / Lease Store。
- failover/routing revision 使用 CAS/transaction。
- object-store writer lease 降级为 data-plane fencing 辅助，不再是唯一仲裁。

### P2：D1-style session API 完整化

- 定义 SDK-visible session/bookmark contract。
- 统一 `x-d1-bookmark`、`x-sdb-bookmark`、`session=first-primary` 行为。
- dashboard/metrics 暴露 replica lag、bookmark wait、primary fallback、served-by metadata。

### P3：Turso/Neon-style durable history 优化

- WAL segment index / manifest checkpoint 合并，减少每 batch manifest 写放大。
- 支持历史 retention、PITR、branch/restore API。
- 从全量 snapshot restore 演进到 segment/page lazy fetch 和热集预取。
- 增加真实 S3/S3 Express/RustFS 跨 region 语义压测。

### P4：scale-to-zero 和 HA 产品化

- control plane 驱动 `Cold -> Warming -> HotPrimary/HotReplica -> Draining` 状态机。
- gateway 支持同步 wakeup 或 202/retry。
- failover 明确 RTO、错误码、idempotency retry 指南。

### P5：Postgres tier 决策

只有当产品目标明确进入完整 Supabase/Postgres 兼容，才启动：

- 自研 Neon-like Postgres storage/compute 分离；或
- 集成 Neon/Aurora/Supabase Postgres 作为 `postgres-serverless` 后端；或
- 明确放弃 Postgres parity，只经营 `lite-serverless`。

## 最终建议

**长期主线选择当前 D1/Turso-style `lite-serverless`，但按 Neon 的分层原则补齐 durable/control plane。**

这条路线和当前 repo 最匹配：能利用已经完成的 Rust SQLite runtime、object-store WAL/snapshot、bookmark、read replica 和 Supabase-like API；同时不会对用户承诺做不到的 Postgres 语义。

具体取舍：

- 借 D1：session/bookmark、一库单主、读副本 consistency contract、小库 scale-out 产品边界。
- 借 Turso：S3-backed durability、compute/cache 可替换、embedded/partial sync、branching/PITR 的存储经济性。
- 借 Neon：WAL/source-of-truth、compute stateless、object storage immutable history、hot path 不直接依赖 object storage latency。
- 不借：D1 的 Cloudflare-only Durable Object 运行环境、Turso Sync 的默认冲突语义作为 server API、Neon 的完整 Postgres storage engine 复杂度、Aurora/Cockroach 的全局分布式 SQL 作为当前 POC 主线。

[^d1-overview]: Cloudflare D1 overview, https://developers.cloudflare.com/d1/
[^d1-replica]: Cloudflare D1 Global read replication, https://developers.cloudflare.com/d1/best-practices/read-replication/
[^d1-limits]: Cloudflare D1 limits, https://developers.cloudflare.com/d1/platform/limits/
[^d1-faq]: Cloudflare D1 FAQ, https://developers.cloudflare.com/d1/reference/faq/
[^turso-cloud]: Turso Cloud documentation, https://docs.turso.tech/turso-cloud
[^turso-durability]: Turso Cloud durability guarantees, https://docs.turso.tech/cloud/durability
[^turso-embedded]: Turso Embedded Replicas, https://docs.turso.tech/features/embedded-replicas/introduction
[^turso-sync]: Turso Sync usage, https://docs.turso.tech/sync/usage
[^turso-partial]: Turso Partial Sync, https://docs.turso.tech/sync/partial
[^neon-architecture]: Neon architecture overview, https://neon.com/docs/introduction/architecture-overview
[^neon-write]: Neon write/read path overview, https://neon.com/docs/introduction/architecture-overview
[^neon-pageserver]: Neon pageserver and object storage architecture, https://neon.com/docs/introduction/architecture-overview
[^neon-ha]: Neon high availability, https://neon.com/docs/introduction/high-availability
[^neon-replicas]: Neon read replicas, https://neon.com/docs/introduction/read-replicas
[^aurora-dsql]: Amazon Aurora DSQL overview, https://docs.aws.amazon.com/aurora-dsql/latest/userguide/what-is-aurora-dsql.html
[^aurora-compat]: Aurora DSQL PostgreSQL migration/compatibility guide, https://docs.aws.amazon.com/aurora-dsql/latest/userguide/working-with-postgresql-compatibility-migration-guide.html
