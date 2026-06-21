# Supabase-like Serverless Database 可行性调研

> 日期：2026-06-20  
> 范围：调研能否用 SQLite/libSQL、DuckDB、Arrow、Parquet/Iceberg、S3/对象存储等非 Postgres 技术，提供类似 Supabase 的服务，并做到 compute 可归零、冷数据落 S3、降低空闲成本。

## 核心结论

有机会，但不应定位为“一比一替代 Postgres 版 Supabase”。

如果目标是完整 Supabase 兼容，最现实的路线仍然是 Postgres-compatible compute + 存储计算分离，类似 Neon：保留 Postgres 的 SQL 语义、WAL、MVCC、RLS、extensions、协议和生态，把 durable history / cold pages 放到对象存储，让 compute 变成可销毁、可拉起的执行层。Supabase 官方架构也明确把 Postgres 作为核心，而不是一层抽象；Auth、Storage、Realtime、Edge Functions 都建立在数据库能力上。

SQLite/libSQL 可以支撑一个更便宜的“海量小库、长期空闲”的产品层，特别适合 per-tenant、per-user、per-agent 的场景。它的优势是本地文件速度快、运维轻、配合 WAL/segment replication 可以把持久状态放进对象存储。但它天然缺少 Postgres RLS、roles、logical replication、extensions、stored procedures、多写并发能力和 PostgREST 生态。

Arrow/DuckDB/Parquet/Iceberg 更适合冷数据、日志、事件、历史分析、导出、离线向量处理。它们不是 OLTP 替代品。Arrow 是列式内存格式和传输协议族，不是数据库；DuckDB 很适合在本地或 S3 文件上做分析 SQL，但写并发模型偏单进程；Iceberg/Delta 解决的是对象存储上的表元数据、快照和事务一致性，不适合直接承载毫秒级行级应用写入。

建议的产品方向是混合架构：

1. 热 OLTP / control plane：Postgres-compatible serverless engine，或者给低成本层使用受限的 SQLite/libSQL。
2. 冷数据层：S3 + Parquet/Iceberg，由 DuckDB/Arrow Flight SQL/Postgres FDW 等查询。
3. API 兼容层：提供 Supabase-like REST/Auth/Realtime/Storage SDK 体验，但用明确 tier 区分能力边界，不宣称透明 Postgres parity。

## Supabase 实际提供了什么

Supabase 不只是 hosted Postgres，而是围绕 Postgres primitives 组成的后端平台：

- Database：每个项目是完整 Postgres，不是 Postgres abstraction。官方文档把 Postgres 定义为 Supabase core，Auth、Storage、Realtime、Edge Functions 都依赖数据库。
- Auto API：REST API 由 PostgREST 从数据库 schema 自动生成，schema、constraints、permissions、RLS 共同决定 API 行为。
- Auth 与授权：Supabase Auth 把用户数据存在项目 Postgres 的 `auth` schema，并用 JWT + Postgres Row Level Security 做逐行授权。
- Realtime：数据库变更流依赖 Postgres replication slot、publication 和 WAL；database broadcast 也读取 `realtime.messages` 表的 WAL。
- Storage：文件是 S3-compatible storage，但元数据和访问控制与 Postgres/RLS 集成。Supabase 当前还提供 Analytics Buckets（Iceberg 表）和 Vector Buckets（S3-backed vector storage）。
- Extensions 与高级 SQL：pgvector、PostGIS、cron、triggers、functions、FDW、roles、schemas、SQL editor/ops 工具都是开发者体验的一部分。
- 成本模型：Supabase paid project 每个项目有独立 Postgres compute/server，按运行时间收费，和实际数据库使用量弱相关；paused project 不计 compute，但 Pro 项目当前不能像 Free 项目那样常规 pause。

所以“提供同样服务”不是 CRUD over SQL，而是要覆盖授权语义、数据库原生 API、schema introspection、Realtime 行为和 Postgres 生态。

## 候选技术评估

### 1. SQLite / libSQL / Turso-style 架构

适合：

- 小中型项目，写冲突低。
- per-tenant / per-user 数据库分片。
- 大量长期空闲项目，不希望每个项目持有 VM/container。
- local-first 或 edge-read 场景。
- 低成本 project-per-user、project-per-agent 产品形态。

优势：

- SQLite 是 embedded database，不需要独立数据库 server process。
- WAL mode 支持 snapshot isolation，并允许读写同时发生。
- libSQL/Turso-style embedded replicas 可以本地读、远端 primary 写。
- Turso 的公开架构已经验证了一种方向：SQLite 数据分段，WAL durable 到 S3/S3 Express，本地磁盘只是 cache，compute 节点可替换，数据可从对象存储 lazy fetch。

硬限制：

- SQLite 文件锁模型允许多读，但写路径本质上单写。Cloudflare D1 文档把产品层限制写得更直白：单个 D1 database 是 single-threaded，逐个处理 query；paid Workers 单库上限 10 GB。
- 没有原生 Postgres RLS/roles/policies。Supabase-like 服务必须在 gateway/query compiler 层强制执行 policy，并禁止绕过 gateway 直连 DB。
- 没有 Postgres extensions、PL/pgSQL functions、Postgres trigger 兼容、FDW、PostgREST 兼容和 logical replication slots。
- 多租户模型只有在数据天然能拆成很多小 DB 文件时才成立；单个大库、写密集库不适合。

判断：

SQLite/libSQL 适合做 Supabase-like 低成本层，不适合做完整 Supabase parity。产品定位应是“带 Supabase 风格 SDK 的 serverless relational app DB”，而不是“Postgres-compatible Supabase replacement”。

### 2. DuckDB + Arrow + Parquet/Iceberg on S3

适合：

- 冷数据和历史数据。
- 日志、事件、analytics、feature store、离线/向量预处理。
- 不常驻 warehouse compute 的 S3 SQL 查询。
- 列式扫描、聚合、大结果集传输（Arrow/Flight SQL）。

优势：

- DuckDB `httpfs` extension 支持读、写、glob S3-compatible object storage 上的文件。
- Arrow 提供跨语言列式内存格式，Flight SQL 提供基于 Arrow 的 SQL 数据库交互协议。
- Iceberg/Delta Lake 提供对象存储上的 table metadata/transaction layer。Iceberg 适合 immutable data/metadata files 和 S3 类对象存储；Delta Lake 提供 ACID transactions、scalable metadata、batch/stream 统一、schema enforcement、time travel、upsert/delete 等能力。
- Supabase 自己也已经往这个方向扩展：Analytics Buckets 使用 Iceberg tables，并可通过 Iceberg FDW 从 Postgres 查询。

硬限制：

- Arrow 不是数据库。它解决的是表示和传输，不解决授权、mutation、indexing、transaction、serving。
- DuckDB 写并发偏单 read-write process，不是一个天然的多租户 OLTP server。
- Iceberg/Delta commit 需要 catalog/locking/transaction coordinator。Iceberg 文档明确提到，在 S3 这类不提供 file-write mutual exclusion 的存储上，file-system catalog 需要额外 lock。
- 行级写入、毫秒级 point lookup、Realtime row change stream、app-facing RLS 都不是这些技术的天然强项。

判断：

DuckDB/Arrow/Iceberg 是优秀的冷数据与分析层，应补充热 OLTP 数据库，而不是替代它。

### 3. Serverless Postgres / Neon-like 架构

适合：

- 完整 Supabase/Postgres 兼容层。
- 客户明确需要真实 Postgres 行为。
- 现有 Supabase/Postgres 应用迁移。

优势：

- 保留 Postgres 语义，同时让 compute ephemeral。
- Neon 把 Postgres 拆成 compute 和 durable storage。compute 运行 Postgres 且不拥有 durable state；WAL 由 storage services 复制；object storage 保存 long-term immutable history，并被设计成不在 hot query path 上。
- 这种架构支持 scale-to-zero compute、copy-on-write branching、instant restore、冷数据进对象存储，同时保留 SQL、MVCC、locks、indexes 和绝大多数 Postgres 行为。

硬限制：

- 构建难度远高于 SQLite gateway 或 DuckDB query service。
- 需要分布式 WAL/page storage layer、quorum durability、page reconstruction、cache management、tenant isolation 和 Postgres 运维能力。
- cold start 和 cache miss 有真实延迟成本；需要 RAM/NVMe cache、预热池和 admission control 才能做好 p95/p99。

判断：

如果“同样的服务”指 Postgres 兼容和通用性能，这基本是唯一可信路线。

## 能力对齐矩阵

| Supabase 能力 | Postgres 依赖 | SQLite/libSQL 路线 | DuckDB/Arrow/Iceberg 路线 | 可行性 |
| --- | --- | --- | --- | --- |
| SQL CRUD | tables、indexes、constraints、transactions | 可做，但 SQL 语义有差异 | 读/分析可做，OLTP 弱 | 部分 |
| Auto REST API | PostgREST introspection、schema、grants、RLS | 需要自研 gateway | 需要自研 gateway | 子集中等 |
| Auth users | `auth` schema、triggers/FKs、JWT claims | 重建 schema + gateway auth | 引擎外重建 | 中等 |
| RLS | 原生 Postgres policies | query rewrite / gateway enforcement | query rewrite / gateway enforcement | 高风险 |
| Realtime DB changes | WAL、publications、replication slots | WAL/libSQL change stream 或 outbox | table snapshots，不适合低延迟 | 部分 |
| Broadcast/Presence | Realtime service + RLS | 独立 pubsub | 独立 pubsub | 中等 |
| Storage files | S3 + Postgres metadata/RLS | S3 + SQLite metadata + gateway RLS | 适合 object data | 中等 |
| pgvector | Postgres extension | sqlite-vec/libSQL vector 或外部服务 | vector bucket/lakehouse/vector engine | 部分 |
| PostGIS/extensions | Postgres extension ecosystem | 基本不行 | 基本不行 | 低 |
| Functions/triggers | PL/pgSQL/triggers | 只有 SQLite triggers，无 PL/pgSQL | 无通用 OLTP trigger model | 低 |
| Direct DB access | Native Postgres protocol | 除非 proxy/translation，否则不兼容 | 不兼容 | 低 |
| PITR/branching | WAL/PITR/storage snapshots | 对象存储 WAL/segment 可做 | analytics table format 原生较强 | 中等 |
| Scale to zero | 需要 detached durable storage | DB file/log 在对象存储时天然适合 | query worker 天然适合 | 高 |

## 推荐架构

### 产品形态

明确拆成三类 database class：

1. `postgres-serverless`：完整 Supabase/Postgres-compatible tier。若需要 exact parity，应走 Postgres-compatible storage/compute separation。
2. `lite-serverless`：SQLite/libSQL-backed 低成本 tier。提供 Supabase-like API、Auth、Storage、Realtime 子集和强空闲经济性，但不提供 direct Postgres compatibility。
3. `analytics-cold`：S3 + Iceberg/Parquet + DuckDB/Arrow query service，服务冷数据、历史表、日志、事件、向量和导出。

### Data Plane

`lite-serverless`：

- 每个 project/tenant 一个 chunked SQLite/libSQL database segment 集合，存于对象存储。
- WAL fragments 在 ack write 前 durable 到 S3/S3 Express 或等价对象存储。
- 本地 NVMe 只作为 cache，不作为 system of record。
- cold start 时 lazy fetch pages/segments，并缓存 hot working set。
- 每库单 writer coordinator，配 queue/backpressure 和明确写入限制。
- 从 committed WAL frames 或 application outbox table 产生 change events。

`analytics-cold`：

- 大型 append-heavy tables 用 Parquet + Iceberg metadata 落对象存储。
- 用 REST catalog 管 schema、snapshot、partition metadata。
- 根据产品表面选择 DuckDB workers、Arrow Flight SQL 或 Postgres FDW 查询。
- 按 tenant/time 分区，并异步 compact。

### API Plane

- 可以从 schema metadata 生成 REST endpoints，但 backing engine 不是 Postgres 时不要宣称 PostgREST。
- Policy enforcement 集中在 gateway：
  - 解析 SQL/API request 为 AST。
  - JWT 解析成 role/claims。
  - 将 row policies rewrite 成 predicate。
  - 强制 column allowlist 和 mutation check。
  - 在 `lite-serverless` 中禁止不受控 raw SQL。
- Realtime 支持：
  - Broadcast/Presence 作为独立 WebSocket service。
  - row changes 从 WAL/outbox 发出，保证 at-least-once delivery 和 per-table/per-tenant ordering。
  - 文档化与 Postgres logical replication 的语义差异。

### Control Plane

- project metadata、schemas、auth config、policy definitions、storage buckets、vector indexes、billing、observability 放在独立强一致 control DB。
- tenant data plane 必须 disposable：任意 worker 都可被替换，并从对象存储 rehydrate。
- 每租户加 admission control：max DB size、write QPS、queue depth、cold start budget、max scan bytes、max result bytes。

## 性能预期

可能匹配或优于 Supabase 的部分：

- 空闲项目成本。
- 冷归档存储成本。
- 小型 per-tenant app，写入温和。
- read-heavy 且数据已缓存的 local/edge workload。
- columnar cold data 的分析扫描。

不应承诺匹配 Supabase/Postgres 的部分：

- 单库通用 multi-writer OLTP。
- 复杂 Postgres SQL、extensions、functions、triggers、FDWs、PostGIS。
- Native Postgres protocol 与生态兼容。
- 允许绕过 gateway 时的 RLS 正确性。
- cold start 或 cold-page cache miss 后的低延迟读取。
- 依赖 Postgres logical replication 的 Realtime 语义。

## 推荐 PoC

建议做 6-8 周 PoC，目标是验证受限低成本层，不是完整 Postgres 替代。

### MVP 功能

- 一个 project = 一个 SQLite/libSQL database file/log stream。
- 对象存储 durable WAL/segments，本地只做 cache。
- 表级 REST CRUD + schema introspection。
- JWT auth + gateway-enforced row policies，先限制为 policy DSL。
- storage bucket metadata table + S3 object upload/download。
- insert/update/delete events 的 Realtime outbox。
- cold table export 到 Parquet/Iceberg，并提供 DuckDB query endpoint。

### 成功标准

- idle cost per project 接近对象存储 + metadata 成本。
- 小库 cold start p95 达到约定 SLO，例如 control-plane routing 后 500 ms 内，不含用户网络。
- 小数据集 hot point-read p95 能和 Supabase Micro 级别竞争。
- 每库 sustained write throughput 有明确预算和 backpressure，不隐瞒 single-writer 边界。
- gateway RLS test suite 证明 REST/query API 无绕过路径。
- S3/object-store corruption/replacement 测试证明可从 WAL/segments 恢复。

### 终止标准

- policy enforcement 必须兼容任意 Postgres RLS SQL。
- 用户要求 direct Postgres wire protocol。
- 单项目写入 workload 需要高并发 multi-writer。
- 必需 extensions 包括 pgvector/PostGIS/FDW/PL languages 且要求兼容。
- 不常驻 compute 时 cold start/cache miss 延迟无法满足产品 SLO。

## 当前 POC 验证结果

本仓库已经把 `lite-serverless` data-plane 核心落到 Rust：

- 每 project 一个 SQLite database cache，本地 cache 可删除。
- generation-based immutable snapshot 和 WAL segments 进入 object store adapter。
- durable manifest 是恢复的 checkpoint index，包含 generation、snapshot checksum、WAL segment id/offset/len/checksum、GC watermark；恢复时按 manifest 校验，损坏则 fail closed。
- object store adapter 支持本地 filesystem，并可通过 `s3` feature 跑真实 RustFS/S3 conformance。
- SQLite `wal_autocheckpoint=0`，checkpoint 只由 snapshot compact 显式控制，避免 WAL generation 自动复位破坏 segment chain。
- 每 project 一个 writer coordinator，串行执行 transaction，并用 group commit 合并 durable WAL flush；writer queue 已有容量上限，队列满时返回 429 做 backpressure；durable WAL 有 byte budget，超预算时先 compact，compact 失败返回 507。
- 每 project 增加 object-store writer lease 和 fencing token。租约未过期时第二个 runtime 写入返回 423；租约过期后接管必须先 conditional-create 下一个 token claim，保证并发接管只有一个赢家；旧热 runtime 看到租约已被别人接管后必须重新 rehydrate 才能写。
- 增加 D1-like bookmark 原型。每个成功 durable 的 write batch 递增 `commit_seq` 并写入 manifest bookmark；JSON 读写响应返回 `sdb1-*` bookmark；读请求带 bookmark 时，如果热 runtime 落后会丢弃 cache 并从 object store manifest 重新恢复，object store 仍落后则返回 425。
- 增加 read replica runtime mode。`--read-replica` 副本不创建项目、不获取 writer lease、不接受本地写入；后台 refresh loop 按 object-store manifest bookmark 追 primary，普通读可同步刷新，带 bookmark 读可等待副本追到请求版本；配置 `--primary-url` 或 routing registry 后支持 write forwarding 和 bookmarked read fallback；primary/gateway 可按 `route_region` / `x-sdb-region` 选择匹配 replica 承接 unconstrained read，并在 JSON 响应返回 `meta.served_by_region` / `meta.served_by_primary`。
- cold open 时对象存储是唯一 source of truth，runtime 会丢弃本地 cache，并按 manifest 从 snapshot + WAL segments 校验重建。
- hibernate/crash 模拟会 stop/join writer，避免旧 SQLite connection 与新 rehydrate 竞争。

### Cloudflare D1 对标状态

Cloudflare D1 给这个 POC 最有价值的参照不是“SQLite API”，而是产品化约束：

- 单库单主写入：Cloudflare D1 limits 文档说明，单个 D1 database 本质上 single-threaded，逐个处理 query；每个 D1 database 背后是单个 Durable Object，read replication 时每个 replica 是另一个 Durable Object。
- 明确 backpressure：D1 并发过高时会先排队，队列满后返回 overloaded error。这个 POC 对应实现是 per-project bounded writer queue，满队列返回 429。
- 读副本不等于强一致：D1 read replicas 是从 primary 异步复制，replica 可能落后；必须用 Sessions API/bookmark，让 read replica 至少追到传入 bookmark，才能给 session 提供 sequential consistency。
- 写入仍走 primary：D1 read replica 收到写请求会转发给 primary；这个 POC 已在 HTTP 层实现 `--primary-url` / routing registry write forwarding，读路径也有显式 region-aware replica routing；forwarding client 已替换为 `reqwest` + rustls，具备 HTTP/HTTPS、连接池、超时、读重试、trace header、本地 endpoint health/cooldown，以及带 `Idempotency-Key` 的写请求 retry；仍缺正式服务发现、跨节点 lag-aware 动态 routing、集中式 circuit breaker 和更完整的幂等键 GC/TTL。
- Time Travel/PITR：D1 Time Travel 可恢复到最近 30 天内任意分钟，并用 bookmark 表示数据库状态。这个 POC 当前只有当前 manifest 指向的 snapshot + WAL chain 恢复，不具备历史 bookmark timeline。

本轮 POC 已加强到的可靠性边界：

1. 单主 writer fencing：`writer-lease.json` 记录当前 owner/fencing token/expiry；`writer-lease-claims/{token}.json` 用 object-store conditional create 保护过期接管的单赢家。
2. 热状态防旧写：`ProjectState` 记录最近见过的 fencing token。一个 runtime 一旦被另一个 runtime 接管，后续写入返回 423，而不是在 TTL 再次过期后悄悄夺回。
3. Bookmark/session 最小协议：manifest 持久化 `commit_seq/bookmark`；读写 JSON 响应返回 bookmark；`bookmark` query/header 可要求当前 runtime 至少恢复到对应逻辑版本。
4. Read replica 最小协议：只读副本用 object-store manifest 作为 replication cursor，后台 refresh loop 维护本地 cache，普通读可追最新 durable manifest，带 bookmark 读可等待；副本本地不提交写入，HTTP 层可把写请求转发到 primary。
5. Forwarding client 基础生产化：runtime 复用 `reqwest` 连接池；endpoint 支持 HTTP/HTTPS 和 base path；连接/请求超时可配置；GET/HEAD 对 408/429/502/503/504、网络错误和超时做有限 retry；写请求默认不自动 retry；转发透传 `Authorization`、`traceparent`、`x-request-id`，并增加 `x-sdb-forward-*` trace header。
6. 写请求幂等键：HTTP 写请求可带 `Idempotency-Key` / `x-sdb-idempotency-key`；primary writer 在 `_sdb_idempotency` 持久化 request hash、bookmark 和响应 JSON；同 key 同 hash 重放旧响应，不重复执行 mutation；同 key 不同 hash 返回 409；forwarding client 只有在写请求带幂等键时才对 transient failure 做 retry。
7. Replica endpoint health：转发最终出现 408/429/502/503/504 或网络/超时后记录 consecutive failure；达到阈值后 endpoint 在 cooldown 窗口内不参与 replica selection；unconstrained read 的 replica route 失败会回落 primary；`project_info.routing.endpoint_health` 暴露当前开路状态。
8. 对象存储 source of truth：本地 SQLite cache 只做可丢弃热缓存，恢复必须经过 manifest checksum、WAL segment checksum。
9. 故障矩阵：已有 process-level crash matrix、RustFS/S3 conformance、网络/HTTP 故障注入、transient retry 幂等矩阵。

仍不能声称等价 D1 的部分：

1. Bookmark 已经能标识 primary durable 状态，read replica 也能按 manifest refresh、等待指定 bookmark，并暴露 local/remote commit seq 与 lag；HTTP 层已有 primary write forwarding、bookmarked read fallback、最小 routing registry、显式 region-aware replica selection、served-by metadata、HTTP/HTTPS forwarding client、读重试、trace header、单进程 endpoint health/cooldown 和写幂等键；但还没有线上 lag 聚合、跨节点 health-aware routing、集中式 circuit breaker、正式服务发现、幂等键 TTL/GC 和外部幂等键审计。
2. 还没有完整 D1 Sessions 对象语义，例如 `first-primary` / `first-unconstrained` 的客户端 session 生命周期、跨请求 session store、driver SDK。
3. 还没有 30 天 PITR、restore bookmark、undo restore、历史 snapshot/WAL retention policy。
4. writer lease 当前依赖对象存储 conditional create。S3 adapter 用 `If-None-Match: *`，本地 adapter 用 `create_new`；生产版还需要跨 region 时钟偏差、claim 文件 GC、真实云厂商限流/一致性差异压测。
5. 还没有 control-plane placement、租户迁移、primary failover election、multi-AZ durability quorum；这些是把 POC 推向 D1 级服务的下一阶段。

2026-06-19 本机 filesystem object store 基准：

| 场景 | Insert 吞吐 | Insert p50 | Insert p95 | Point read 吞吐 | Crash recovery |
| --- | ---: | ---: | ---: | ---: | ---: |
| 2000 rows, concurrency 16, payload 96B, group commit 64/2ms, durable manifest per batch, WAL budget 64MiB | 1121.1/s | 13.14 ms | 19.98 ms | 14799/s | 66.34 ms |
| 2000 rows, concurrency 16, payload 96B, group commit 64/2ms | 1652.9/s | 8.81 ms | 12.63 ms | 15356/s | 70.26 ms |
| 2000 rows, concurrency 16, payload 512B, group commit 64/2ms | 1500.2/s | 9.74 ms | 13.25 ms | 12922/s | 82.94 ms |

2026-06-19 已通过的对象存储验证：

- local adapter conformance：put/read/overwrite/list/delete/delete_prefix + runtime crash recovery smoke。
- RustFS/S3 adapter conformance：用临时 RustFS 容器验证同一组对象语义和 runtime crash recovery smoke。
- RustFS/S3 network fault injection：用 Toxiproxy 验证 healthy proxy、`200ms +/- 50ms` 下行延迟、proxy outage fail-closed、proxy recovered 后再次 crash recovery。
- RustFS/S3 HTTP fault injection：用 Rust `s3_fault_proxy` 验证 healthy proxy、HTTP 503、HTTP 429、HTTP 408、partial response、recovered proxy；429/503/408 均进入 service error，partial response 进入 truncated body error。
- RustFS/S3 retry idempotency matrix：对 list/put/get/head/delete 分别注入一次 transient 503 和 transient 429，验证 conformance 仍通过；S3 adapter 在 SDK retry 外有对象存储级短重试，默认 `SDB_S3_ADAPTER_MAX_ATTEMPTS=3`。
- process-level crash matrix：子进程在 SQLite commit 后 durable WAL 前、WAL segment PUT、WAL manifest PUT、snapshot upload、snapshot manifest PUT、local cache replacement、snapshot 后 WAL delete_prefix、project cache delete 阶段成功后立刻退出；父进程冷恢复验证未 durable/未 manifest 的写入不恢复，manifest/snapshot source-of-truth 已提交的状态可恢复。
- fail-closed recovery tests：manifest JSON 损坏、manifest segment 缺失、segment checksum mismatch 均返回错误，不走不校验 fallback。
- object-store PUT failure tests：WAL segment PUT 失败和 manifest PUT 失败都不会确认写入；snapshot PUT 失败不会破坏旧 snapshot + WAL chain，后续写入和 crash recovery 仍可恢复。
- backpressure test：writer queue 容量受限时，并发写入会收到 429，不会进入无界内存队列。
- WAL budget tests：durable WAL 超预算会先触发 compact；compact 失败时返回 507，并且不会继续拉长恢复链。

这证明低成本 SQLite tier 的热路径可行，但仍不是生产可用数据库。下一步要验证的生产级风险：

1. Replica routing 生产化：把当前显式 endpoint registry 和单进程 endpoint health 扩展为 control-plane placement、跨节点 health-aware endpoint、lag-aware routing、集中式 circuit breaker，以及无法追上时 fallback primary 的策略化控制。
2. Forwarding client 继续增强：增加服务发现、mTLS/内部身份、retry budget、tracing/metrics sink，以及幂等键 TTL/GC 和审计。
3. D1 Sessions SDK 原型：把当前 query/header bookmark 协议封装成 `withSession("first-primary" | "first-unconstrained" | bookmark)`，维护跨请求 latest bookmark。
4. Time Travel 原型：保留历史 manifest/snapshot/WAL，提供 `bookmark -> manifest generation` 映射、restore/undo restore API 和 retention GC。
5. 真实 S3 网络故障注入扩展：当前已覆盖 Toxiproxy 延迟、断流、恢复、HTTP 503/429/408/partial response，以及 transient 503/429 retry 幂等矩阵；下一步补 multi-part failure、跨 region 行为和真实云厂商限流差异。
6. crash matrix 扩展：当前已覆盖 SQLite commit 后 durable WAL 前、对象存储阶段、local cache replacement 和 cache delete 的进程退出；下一步补 SQLite transaction commit 前、local file copy 中途、父进程收到 ack 前后的 kill。
7. manifest 写放大优化：segment metadata 分片、manifest checkpoint 合并、历史 snapshot 保留和异步 GC。
8. admission control 扩展：当前已限制 writer queue depth 和 durable WAL bytes；下一步补 snapshot compact CPU/IO、cold start 并发、max scan bytes 和 result bytes。
9. SLO：冷启动 p50/p95/p99、hot read p95、write p95、恢复时间随 snapshot/WAL 大小增长曲线。
10. 安全：policy DSL 的 bypass test、raw SQL 禁用边界、Storage object metadata 与对象内容的一致性。

## Source Notes

- Supabase architecture：Postgres 是 core；Auth 集成 Postgres RLS；PostgREST 把 Postgres 暴露成 REST；Realtime 依赖 Postgres changes 和 WAL；Storage metadata 集成 Postgres。
- Supabase compute billing：每个 project 有 dedicated Postgres instance/server；paused project 不计 compute usage，但 paid Pro project 当前不能常规 pause。
- SQLite：WAL mode 给 snapshot isolation 和 reader/writer 并发，但文件锁模型仍有单写路径。
- DuckDB：`httpfs` 支持 S3-compatible object storage；写并发限制在 single writer process 范围内。
- Arrow：列式内存格式和 Flight SQL protocol，不是 OLTP engine。
- Iceberg/Delta：对象存储 table format 解决冷分析数据的一致性和 metadata，不解决 app-facing OLTP serving。
- Neon/Turso：都证明了“object storage as durable source of truth + local cache/ephemeral compute”是可信架构；Neon 是 Postgres-compatible 版本，Turso 是 SQLite/libSQL 版本。
- Cloudflare D1：关键参照是 single-threaded per-database execution、Durable Object primary、read replica + Sessions/bookmark 顺序一致性、Time Travel/PITR，而不是无限扩展的通用 SQLite。

## References

- Supabase architecture: https://supabase.com/docs/guides/getting-started/architecture
- Supabase database overview: https://supabase.com/docs/guides/database/overview
- Supabase REST API: https://supabase.com/docs/guides/api
- Supabase RLS: https://supabase.com/docs/guides/database/postgres/row-level-security
- Supabase Realtime architecture: https://supabase.com/docs/guides/realtime/architecture
- Supabase Storage overview: https://supabase.com/docs/guides/storage
- Supabase Analytics Buckets query with Postgres: https://supabase.com/docs/guides/storage/analytics/query-with-postgres
- Supabase Vector Buckets: https://supabase.com/docs/guides/storage/vector/introduction
- Supabase compute usage: https://supabase.com/docs/guides/platform/manage-your-usage/compute
- PostgREST docs: https://postgrest.org/
- SQLite isolation: https://sqlite.org/isolation.html
- SQLite locking: https://sqlite.org/lockingv3.html
- SQLite WAL: https://sqlite.org/wal.html
- DuckDB S3 API support: https://duckdb.org/docs/lts/core_extensions/httpfs/s3api
- DuckDB concurrency: https://duckdb.org/docs/current/connect/concurrency
- Apache Arrow FAQ: https://arrow.apache.org/faq/
- Arrow Flight SQL: https://arrow.apache.org/docs/format/FlightSql.html
- Apache Iceberg spec: https://iceberg.apache.org/spec/
- Apache Iceberg AWS integration/locking: https://iceberg.apache.org/docs/latest/aws/
- Delta Lake docs: https://docs.delta.io/
- Neon architecture: https://neon.com/docs/introduction/architecture-overview
- Neon storage: https://neon.com/storage
- Turso embedded replicas: https://docs.turso.tech/features/embedded-replicas/introduction
- Turso durability/S3 architecture: https://turso.tech/blog/how-does-the-turso-cloud-keep-your-data-durable-and-safe
- Cloudflare D1 overview: https://developers.cloudflare.com/d1/
- Cloudflare D1 limits: https://developers.cloudflare.com/d1/platform/limits/
- Cloudflare D1 global read replication: https://developers.cloudflare.com/d1/best-practices/read-replication/
- Cloudflare D1 Worker API with Sessions/bookmarks: https://developers.cloudflare.com/d1/worker-api/d1-database/
- Cloudflare D1 Time Travel and backups: https://developers.cloudflare.com/d1/reference/time-travel/
- Cloudflare D1 read replication implementation blog: https://blog.cloudflare.com/d1-read-replication-beta/
- Cloudflare SQLite-backed Durable Object Storage: https://developers.cloudflare.com/durable-objects/api/sqlite-storage-api/
