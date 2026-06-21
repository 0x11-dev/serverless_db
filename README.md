# Serverless DB POC

这个仓库实现了调研文档里的低成本 Supabase-like POC。当前核心组件已迁移到 Rust：

- `rust-core/`：生产级 data-plane POC，负责 SQLite/WAL、对象存储 durable WAL、snapshot compact、crash recovery、Auth/Policy、HTTP API。对象存储支持本地 filesystem adapter，并提供可选 S3 adapter（兼容 RustFS / AWS S3 等）。
- `src/`：早期 TypeScript POC，保留作 API 参考和对比基线。
- `examples/demo.ts`：客户端 demo，可同时打 Rust core 和 TypeScript server。

## 语言决策

核心组件选择 Rust，而不是 Go。

| 维度 | Rust | Go |
| --- | --- | --- |
| WAL/page-cache/storage runtime | 更适合，零成本抽象和内存安全更贴近存储内核 | 可做，但 GC 和内存布局控制弱一些 |
| SQLite/object-store hot path | 低开销，适合后续 group commit、page cache、S3 writer | 工程效率高，但更偏服务编排 |
| Control plane/scheduler | 可做，但开发效率不如 Go | 很适合 |
| API/SDK/console | 不如 TypeScript | 不如 TypeScript |

结论：data-plane core 用 Rust；未来 control plane 可以用 Go；SDK/console 继续用 TypeScript。

当前能力：

- SQLite 作为 per-project 热数据引擎。
- 本地目录或 S3 兼容对象存储（RustFS / AWS S3 等）作为 object storage，SQLite cache 可删除、可从对象存储 snapshot + WAL segments 恢复。
- D1-like 单主写入：每个 project 写入前获取 object-store writer lease，租约过期接管使用 fencing token + conditional create，旧 runtime 失去租约后必须重新 rehydrate 才能写。
- D1-like bookmark 原型：每次 durable write batch 生成单调 `sdb1-*` bookmark；JSON 读写响应返回 bookmark；读请求可带 `bookmark`，热缓存落后时会从对象存储 rehydrate。
- Read replica 原型：runtime 可用 `--read-replica` 以只读副本启动；副本后台按 object-store manifest 追 primary，读请求可等待 bookmark；配置 `--primary-url` 或 routing registry 后，副本可把写请求和追不上的 bookmarked read 转发到 primary；primary/gateway 也可按 `route_region` 把 unconstrained read 路由到匹配副本。
- REST CRUD + schema introspection。
- HS256 JWT + gateway-enforced policy DSL，覆盖 select/insert/update/delete。
- Storage bucket + object upload/download，文件进入对象存储 adapter。
- Realtime outbox：所有 mutation 写入 `_sdb_outbox`，支持 JSON polling 和 SSE。

Iceberg/离线分析层暂时不在关键路径里，后续可以接在 outbox/export 之后。

## 安装

```bash
npm install
```

Rust core 本身不依赖 npm，可直接用 Cargo：

```bash
cargo test --manifest-path rust-core/Cargo.toml
```

## 运行

```bash
npm run core:dev
```

可调参数：

```bash
npm run core:dev -- \
  --snapshot-every-ops 1000 \
  --metadata-every-ops 100 \
  --group-commit-max-ops 64 \
  --group-commit-delay-ms 2 \
  --writer-queue-capacity 1024 \
  --max-durable-wal-bytes 67108864 \
  --writer-lease-ttl-ms 30000 \
  --sqlite-synchronous NORMAL
```

- `--snapshot-every-ops`：每多少次 committed mutation 后 compact 成新 snapshot；设为 `0` 表示不按操作数自动 compact。
- `--metadata-every-ops`：change-log 降采样频率；durable manifest 会在每次 WAL batch flush 后更新。
- `--group-commit-max-ops`：单次 durable WAL flush 最多合并多少个 mutation。
- `--group-commit-delay-ms`：writer 等待更多 mutation 进入同一 batch 的最大延迟。
- `--writer-queue-capacity`：每个 project writer 的有界队列容量；队列满时写请求返回 `429 project writer queue is full`，避免无界内存增长。
- `--max-durable-wal-bytes`：每个 project 允许的 durable WAL 预算；达到预算后下一批写入会先 compact，compact 失败时返回 `507 project durable WAL budget exceeded`。设为 `0` 表示关闭。
- `--writer-lease-ttl-ms`：每个 project 的 object-store writer lease TTL。设为 `0` 表示关闭分布式单主 fencing，仅适合单进程本地验证。
- `--read-replica`：以只读副本模式启动。副本不会创建项目或接受写入；读路径会比较 object-store manifest bookmark，落后时重新 rehydrate。
- `--replica-refresh-interval-ms`：read replica 后台 refresh loop 周期，默认 `1000`；设为 `0` 表示关闭后台刷新，只在读请求时同步刷新。
- `--replica-bookmark-wait-timeout-ms`：read replica 收到带 bookmark 的读请求时等待追赶的最长时间，默认 `5000`。
- `--primary-url`：read replica 的 primary HTTP endpoint，例如 `https://db-primary.example.com` 或 `http://127.0.0.1:8765`。forwarding client 使用 `reqwest` + rustls，支持 HTTP/HTTPS、连接池、连接/请求超时、trace header、GET/HEAD transient retry，以及带 `Idempotency-Key` / `x-sdb-idempotency-key` 的写请求 transient retry；没有幂等键的写请求固定单次转发。
- `--routing-region`：当前 runtime 的逻辑 region，用于 routing registry 输出、served-by metadata 和 replica selection。
- `--routing-endpoint role,region,url`：声明 routing registry endpoint，可重复传入；`role` 当前支持 `primary` / `replica`。HTTP forwarding 优先使用 registry 中的 `primary` endpoint，未配置时退回 `--primary-url`；primary/gateway 收到 unconstrained read 时会优先选择 `route_region` / `x-sdb-region` 匹配的 replica endpoint。
- `--forward-connect-timeout-ms`：forwarding client 建连超时，默认 `1000`。
- `--forward-request-timeout-ms`：forwarding client 单请求总超时，默认 `5000`。
- `--forward-max-attempts`：GET/HEAD 或带幂等键写请求转发遇到 408/429/502/503/504 或网络/超时时的最大尝试次数，默认 `3`；非幂等写请求固定单次尝试。
- `--forward-retry-backoff-ms`：GET/HEAD 转发 retry 的线性退避基准，默认 `25`。
- `--routing-endpoint-failure-threshold`：endpoint 连续 transient failure 多少次后短期开路，默认 `2`。
- `--routing-endpoint-cooldown-ms`：endpoint 开路后跳过 replica selection 的时间，默认 `1000`。
- `--sqlite-synchronous`：SQLite `OFF` / `NORMAL` / `FULL`。
- `--supabase-project-id`：Supabase SDK 兼容入口 `/rest/v1/{table}` 映射到的默认单项目 id，默认 `demo`。

只读副本模式额外加 `--read-replica`。

S3 adapter 需要启用 Cargo feature：

```bash
cargo run --release --manifest-path rust-core/Cargo.toml --features s3 -- \
  --object-store s3 \
  --s3-bucket serverless-db-poc \
  --s3-prefix dev \
  --s3-endpoint http://127.0.0.1:9000 \
  --s3-region us-east-1 \
  --s3-force-path-style
```

对象存储 conformance：

```bash
npm run core:store-conformance -- --object-store local --runtime-dir /tmp/sdb-conformance-local --rows 128
```

真实 RustFS/S3 adapter conformance：

```bash
npm run core:s3-conformance
```

`core:s3-conformance` 会启动临时 RustFS 容器、创建 bucket、运行 S3 adapter conformance，并在退出时删除容器。若 Docker daemon 未运行，可先启动 Docker Desktop 或 `colima start --runtime docker`。

真实 RustFS/S3 网络故障注入：

```bash
npm run core:s3-fault-injection
```

`core:s3-fault-injection` 会启动临时 RustFS + Toxiproxy，验证 healthy proxy、下行延迟、proxy outage fail-closed、proxy recovered 四个场景。默认延迟为 `200ms +/- 50ms`，可通过 `SDB_FAULT_LATENCY_MS` / `SDB_FAULT_JITTER_MS` 调整。

真实 RustFS/S3 HTTP 级故障注入：

```bash
npm run core:s3-http-fault-injection
```

`core:s3-http-fault-injection` 会启动临时 RustFS 和 Rust `s3_fault_proxy`，验证 healthy proxy、HTTP 503、HTTP 429、HTTP 408、partial response、recovered proxy。503/429/408 会被 S3 adapter 识别为 service error，partial response 会被识别为 truncated body。

真实 RustFS/S3 retry 幂等矩阵：

```bash
npm run core:s3-retry-idempotency
```

`core:s3-retry-idempotency` 会对 list/put/get/head/delete 分别注入一次 transient 503 和 transient 429，并验证 conformance 仍通过。S3 adapter 在 SDK retry 外还有一层对象存储级短重试，默认 `SDB_S3_ADAPTER_MAX_ATTEMPTS=3`；脚本用 `AWS_RETRY_MODE=standard` 和 `AWS_MAX_ATTEMPTS=4` 运行。

进程级 crash matrix：

```bash
npm run core:crash-matrix -- --runtime-dir /tmp/sdb-crash-matrix
```

`core:crash-matrix` 会用父子进程验证真实退出语义：SQLite commit 后但 durable WAL 前退出不会恢复；WAL segment PUT 后未写 manifest 的写入不会恢复；manifest PUT 后的写入会恢复；snapshot upload、snapshot manifest、local cache replacement、snapshot 后 WAL 删除、project cache 删除阶段退出后都能按 manifest source-of-truth 恢复。

Supabase SDK 兼容入口：

```bash
eval "$(./scripts/bootstrap-distributed-poc.sh http://127.0.0.1:8765)"
npm run supabase:compat
```

`/rest/v1/{table}` 当前支持 supabase-js 的 table CRUD：`select('*').eq(...).limit(...)`、`insert(object).select()`、`update(...).eq(...).select()`、`delete().eq(...).select()`。该入口复用 Rust core 的 JWT、policy、writer idempotency、replica forwarding 和 bookmark read path；Auth/Storage/Realtime 的 Supabase 协议兼容仍未实现。

远端分布式 Docker 集群：

```bash
docker compose -f deploy/docker-compose.distributed.yml up -d --build
eval "$(./scripts/bootstrap-distributed-poc.sh http://127.0.0.1:8765)"
SDB_REPLICA_URLS=http://127.0.0.1:8766,http://127.0.0.1:8767 \
npm run supabase:compat
```

Compose 会启动 RustFS、一个 primary、两个 read replica。primary 暴露 `8765`，replica 分别暴露 `8766` / `8767`，三者共享同一个 S3 prefix 来验证 cold snapshot/WAL、异步 replica catch-up、replica 写转发和 routing health。

健康检查：

```bash
curl http://127.0.0.1:8765/health
```

跑一遍 demo：

```bash
npm run demo
```

Blog Platform 综合验证示例（覆盖全部功能）：

```bash
# 本地 dev server
npm run example:blog

# Docker 集群
docker compose -f deploy/docker-compose.distributed.yml up -d --build
docker compose -f deploy/docker-compose.distributed.yml run --rm blog-example
```

该示例模拟一个多租户博客平台，验证 health、JWT auth、多表 schema、Policy DSL 全部 7 种 rule、CRUD + 策略隔离、Storage 对象上传/下载/删除、Realtime outbox + SSE、Bookmark 一致性读、写幂等、Supabase SDK 兼容、Read replica 异步追赶 + 写转发、Hibernate/Crash 恢复。报告输出到 `reports/blog-app-verification-report.md`。

## 测试

```bash
npm run core:test
```

测试覆盖 owner policy、hibernate 恢复、未优雅退出的 durable WAL 恢复、Storage roundtrip、group commit 并发写入、writer queue backpressure、writer lease blocking、TTL takeover、并发 takeover 单赢家、durable WAL byte budget、超过 SQLite 默认 auto-checkpoint 阈值的大 WAL segment 恢复、manifest 损坏、segment 缺失、checksum mismatch 的 fail-closed 恢复，以及 WAL/manifest/snapshot PUT 失败注入。进程级退出语义由 `npm run core:crash-matrix` 覆盖。

Bookmark/session 原型测试覆盖写响应单调 bookmark、落后热 runtime 带 bookmark 读取时 rehydrate、请求未出现的 future bookmark 返回 425。

Read replica 原型测试覆盖副本写入返回 405、普通读请求按最新 manifest 同步刷新、后台 refresh loop 降低 lag、带 future bookmark 的读请求等待 primary durable 后返回，以及 HTTP 层通过 `--primary-url` 或 routing registry 把写请求和追不上的 bookmarked read 转发到 primary。Routing 测试覆盖 primary/gateway 按 region 选择 replica、`session=first-primary` 绕过 replica、read replica 收到 first-primary read 时转发 primary，以及 replica endpoint transient failure 后回落 primary 并短期开路。Forwarding client 测试覆盖 HTTPS/base-path URL 构造、GET transient retry、trace header 透传、非幂等写不重试，以及带幂等键写请求可重试。Writer 幂等测试覆盖同 key 重放不重复插入、同 key 不同请求 hash 返回 409。

TypeScript 旧实现仍可用：

```bash
npm test
npm run dev
```

### Supabase JS SDK 兼容性测试

使用真实 `@supabase/supabase-js` SDK 验证 PostgREST CRUD/筛选/变换、GoTrue Auth（注册/登录/登出/刷新/用户更新）、RLS 策略隔离、Storage 上传/下载/列表/删除：

```bash
# 终端 1：启动 Rust 服务
npm run core:dev

# 终端 2：运行 SDK 测试
npm run test:sdk
```

49 项测试覆盖：PostgREST 基本 CRUD（select/insert/update/delete/upsert/single）、筛选器（eq/neq/gt/gte/lt/lte/in/like/ilike/is/not/链式 AND）、变换（order/limit/range/maybeSingle）、RLS 策略隔离（anon/authenticated 读写权限、用户只能操作自己的行）、GoTrue Auth（signUp/signInWithPassword/signOut/getUser/refreshSession/updateUser/重复注册拒绝/错误密码拒绝/不存在用户拒绝/user_metadata/auth settings）、Storage（bucket 创建/列表/删除、文件上传/下载/列表/删除）、Auth+PostgREST 集成。

## 性能验证

Rust core 压测测 data-plane hot path：

```bash
npm run core:bench -- \
  --rows 2000 \
  --concurrency 16 \
  --snapshot-every-ops 1000 \
  --metadata-every-ops 100 \
  --group-commit-max-ops 64 \
  --group-commit-delay-ms 2 \
  --payload-bytes 96
```

TypeScript 旧实现的压测走真实 HTTP server：

```bash
npm run bench -- --rows 2000 --concurrency 16 --snapshot-every-ops 1000 --metadata-every-ops 100
```

当前 POC 的生产级持久化路径是：

1. generation-based immutable SQLite snapshot 落对象存储，manifest 指向当前 snapshot。
2. SQLite 保持 WAL mode，并关闭 auto-checkpoint；checkpoint 只能由 snapshot compact 显式触发，避免 WAL generation 被 SQLite 自动复位。
3. 每个 project 有 object-store writer lease；热 runtime 记录 fencing token。租约过期后，新 runtime 只有成功 conditional-create 下一个 token claim 才能接管，旧 runtime 看到租约被别人接管会返回 423。
4. 每个 project 有单独 writer coordinator，所有 mutation 进入有界 writer queue；队列满时返回 429，给上游做 backpressure。
5. writer 串行执行 SQLite transaction，并按 `group-commit-max-ops` / `group-commit-delay-ms` 合并 durable WAL flush。
6. batch WAL segment PUT 成功后立即写 durable manifest。manifest 包含 generation、snapshot checksum、WAL segment id/offset/len/checksum、GC watermark。
7. manifest durable 成功后才回复该 batch 内成功提交的请求。
8. 当 durable WAL 达到 byte budget，下一批写入会先 compact；compact 失败时该批返回 507，不继续拉长恢复链。
9. 达到 snapshot cadence 后用 `VACUUM INTO` 生成新 snapshot；snapshot 上传和新 manifest 提交成功后，才替换本地 cache 并删除旧 WAL prefix。snapshot 上传失败不会破坏旧 snapshot + WAL chain。
10. cold open 时本地 cache 不作为 source of truth；runtime 会丢弃本地 `main.sqlite/-wal/-shm`，按 manifest 校验 snapshot + WAL segments 后重建。
11. hibernate/crash 模拟会 stop/join project writer，确保 SQLite connection 关闭后再删除 cache，避免同进程测试里的旧连接和新 rehydrate 竞争。

## Cloudflare D1 对标

Cloudflare D1 的核心边界是 SQLite + 单库单主写入：单个 D1 database 由单个 Durable Object 承载，查询按队列串行处理；读副本是异步复制，必须通过 Sessions API/bookmark 才能获得顺序一致性。

当前 POC 已对齐的部分：

- 单主写入：writer coordinator + writer lease/fencing，防止两个热 runtime 同时确认同一 project 的写入。
- Session bookmark 原型：write batch 成功 durable 后返回单调 bookmark；`GET /tables/{table}?bookmark=...` 或 `x-d1-bookmark` / `x-sdb-bookmark` 可要求读路径至少追到该 bookmark。
- Read replica 原型：`--read-replica` runtime 从 object-store manifest 追 primary 的最新 durable state，后台 refresh loop 维护本地缓存；`project_info` 暴露 `replica.local_commit_seq`、`remote_commit_seq`、`lag_commits`、`routing` registry 和 `routing.endpoint_health`；配置 `--primary-url` 或 `--routing-endpoint primary,...` 后支持 write forwarding 和 read fallback；配置 `--routing-endpoint replica,...` 后，primary/gateway 可按 region 把 unconstrained read 路由到 replica。JSON 响应带 D1-like `meta.served_by_region` / `meta.served_by_primary`。Forwarding client 已支持 HTTP/HTTPS、连接池、超时、读重试、trace header、replica endpoint 短期开路，以及带幂等键写请求重试。
- Backpressure：writer queue 满返回 429，durable WAL budget 超限返回 507。
- 可丢弃 compute：本地 cache 不是 source of truth，冷启动从 snapshot + WAL manifest 校验恢复。
- 对象存储故障：RustFS/S3 conformance、网络故障、HTTP 级故障和 transient retry 矩阵均可运行。

仍未对齐的部分：

- D1 Sessions API/bookmark：已有 primary/replica 的最小 bookmark 协议、等待策略、routing registry、primary fallback、显式 region-aware replica routing、served-by metadata、生产化 HTTP forwarding 基础和写幂等键重放，但还没有完整 session SDK、跨请求 session store 和线上 lag observability。
- Global read replicas：已有单进程内 endpoint health/cooldown，但还没有自动 replica placement、跨节点 lag-aware endpoint health、集中式 circuit breaker 和跨 region 动态 routing。
- Time Travel/PITR：当前只有当前 manifest + snapshot/WAL 恢复，没有 30 天 bookmark timeline、恢复/undo restore API。
- Durable Object 级强一致私有存储：当前以 S3/object store 为 durable source，lease 依赖 conditional create；生产版仍需跨 region 时钟、claim GC、真实云对象存储语义压测。

本机基线结果（2026-06-19，本地 filesystem 模拟 object store）：

| 实现 | 配置 | Insert 吞吐 | Insert p50 | Insert p95 | Read 吞吐 | Crash recovery | Object-store WAL |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Rust core + group commit + durable manifest + WAL budget | `concurrency=16`, `payload=96B`, `snapshotEveryOps=1000`, `metadataEveryOps=100`, `groupCommit=64/2ms`, `maxDurableWal=64MiB` | 1121.1/s | 13.14 ms | 19.98 ms | 14799/s | 66.34 ms | 13.0 MB |
| Rust core + group commit | `concurrency=16`, `payload=96B`, `snapshotEveryOps=1000`, `metadataEveryOps=100`, `groupCommit=64/2ms` | 1652.9/s | 8.81 ms | 12.63 ms | 15356/s | 70.26 ms | 13.0 MB |
| Rust core + group commit | `concurrency=16`, `payload=512B`, `snapshotEveryOps=1000`, `metadataEveryOps=100`, `groupCommit=64/2ms` | 1500.2/s | 9.74 ms | 13.25 ms | 12922/s | 82.94 ms | 14.7 MB |
| Rust core, pre-group-commit | `snapshotEveryOps=1000`, `metadataEveryOps=100` | 226.9/s | 4.03 ms | 6.52 ms | 11849/s | 19.76 ms | 24 KB |
| TypeScript HTTP | `snapshotEveryOps=1000`, `metadataEveryOps=100` | 214/s | 71.66 ms | 103.08 ms | 2396/s | 16.54 ms | 24 KB |
| TypeScript HTTP | `snapshotEveryOps=0`, `metadataEveryOps=100` | 223/s | 70.88 ms | 87.68 ms | 2620/s | 205.86 ms | 26 MB |

结论：group commit 把 Rust core 写入吞吐从约 `226/s` 提升到约 `1500-1650/s`；启用每 batch durable manifest 后，本机实测约 `830-1120/s`。这是正确性成本：恢复不再依赖 prefix list，而是按 manifest checksum fail-closed。生产版本需要继续优化 manifest 写放大，例如 segment metadata 分片、manifest checkpoint 合并、异步 GC。

## API 摘要

### Token

```bash
curl -s -X POST http://127.0.0.1:8765/v1/tokens \
  -H 'content-type: application/json' \
  -d '{"sub":"alice","claims":{"orgs":["demo"]}}'
```

### Project / Schema

```bash
curl -s -X POST http://127.0.0.1:8765/v1/projects \
  -H 'content-type: application/json' \
  -d '{"id":"demo"}'

curl -s -X POST http://127.0.0.1:8765/v1/projects/demo/tables \
  -H 'content-type: application/json' \
  -d '{"name":"notes","columns":[{"name":"owner_id","type":"text","not_null":true},{"name":"title","type":"text","not_null":true}]}'

curl -s http://127.0.0.1:8765/v1/projects/demo/schema
```

### Policy DSL

允许当前 token 的 `sub` 访问 `owner_id` 等于自己的行：

```json
{
  "table": "notes",
  "operation": "all",
  "name": "owner_only",
  "rule": { "column": "owner_id", "equals_claim": "sub" }
}
```

支持的 rule：

- `{ "allow": true }`
- `{ "role_in": ["authenticated"] }`
- `{ "column": "owner_id", "equals_claim": "sub" }`
- `{ "column": "org_id", "in_claim": "orgs" }`
- `{ "and": [ ... ] }`
- `{ "or": [ ... ] }`

### CRUD

```bash
TOKEN="$(curl -s -X POST http://127.0.0.1:8765/v1/tokens -H 'content-type: application/json' -d '{"sub":"alice"}' | node -e 'let s=""; process.stdin.on("data",d=>s+=d); process.stdin.on("end",()=>console.log(JSON.parse(s).token))')"

curl -s -X POST http://127.0.0.1:8765/v1/projects/demo/tables/notes \
  -H "authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' \
  -d '{"owner_id":"alice","title":"hello"}'

curl -s http://127.0.0.1:8765/v1/projects/demo/tables/notes \
  -H "authorization: Bearer $TOKEN"
```

带 bookmark 的一致性读：

```bash
BOOKMARK="$(curl -s -X POST http://127.0.0.1:8765/v1/projects/demo/tables/notes \
  -H "authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' \
  -d '{"owner_id":"alice","title":"consistent"}' \
  | node -e 'let s=""; process.stdin.on("data",d=>s+=d); process.stdin.on("end",()=>console.log(JSON.parse(s).bookmark))')"

curl -s "http://127.0.0.1:8765/v1/projects/demo/tables/notes?bookmark=$BOOKMARK" \
  -H "authorization: Bearer $TOKEN"
```

### Storage

```bash
curl -s -X POST http://127.0.0.1:8765/v1/projects/demo/buckets \
  -H 'content-type: application/json' \
  -d '{"name":"files"}'

curl -s -X PUT http://127.0.0.1:8765/v1/projects/demo/storage/files/hello.txt \
  -H "authorization: Bearer $TOKEN" \
  -H 'content-type: text/plain' \
  --data-binary 'hello'

curl -s http://127.0.0.1:8765/v1/projects/demo/storage/files/hello.txt \
  -H "authorization: Bearer $TOKEN"
```

#### Supabase Storage API (`/storage/v1`)

```bash
# Create bucket
curl -s -X POST http://127.0.0.1:8765/storage/v1/buckets \
  -H "apikey: $TOKEN" \
  -H 'content-type: application/json' \
  -d '{"name":"files"}'

# List buckets
curl -s http://127.0.0.1:8765/storage/v1/buckets -H "apikey: $TOKEN"

# Upload object
curl -s -X POST http://127.0.0.1:8765/storage/v1/object/files/hello.txt \
  -H "apikey: $TOKEN" \
  -H 'content-type: text/plain' \
  --data-binary 'hello world'

# Download object
curl -s http://127.0.0.1:8765/storage/v1/object/files/hello.txt -H "apikey: $TOKEN"

# List objects
curl -s -X POST http://127.0.0.1:8765/storage/v1/object/list/files \
  -H "apikey: $TOKEN" \
  -H 'content-type: application/json' \
  -d '{"limit":10,"offset":0}'

# Delete object
curl -s -X DELETE http://127.0.0.1:8765/storage/v1/object/files/hello.txt -H "apikey: $TOKEN"
```

#### GoTrue-compatible Auth API (`/auth/v1`)

```bash
# Sign up with email + password
curl -s -X POST http://127.0.0.1:8765/auth/v1/signup \
  -H "apikey: $ANON_KEY" \
  -H 'content-type: application/json' \
  -d '{"email":"user@example.com","password":"secret123"}'

# Sign in with password
curl -s -X POST http://127.0.0.1:8765/auth/v1/token?grant_type=password \
  -H "apikey: $ANON_KEY" \
  -H 'content-type: application/json' \
  -d '{"email":"user@example.com","password":"secret123"}'

# Get current user
curl -s http://127.0.0.1:8765/auth/v1/user \
  -H "apikey: $ANON_KEY" \
  -H "authorization: Bearer $ACCESS_TOKEN"

# Update user
curl -s -X PUT http://127.0.0.1:8765/auth/v1/user \
  -H "apikey: $ANON_KEY" \
  -H "authorization: Bearer $ACCESS_TOKEN" \
  -H 'content-type: application/json' \
  -d '{"data":{"key":"value"}}'

# Refresh session
curl -s -X POST http://127.0.0.1:8765/auth/v1/token?grant_type=refresh_token \
  -H "apikey: $ANON_KEY" \
  -H 'content-type: application/json' \
  -d '{"refresh_token":"$REFRESH_TOKEN"}'

# Sign out (revoke current session)
curl -s -X POST http://127.0.0.1:8765/auth/v1/logout \
  -H "apikey: $ANON_KEY" \
  -H "authorization: Bearer $ACCESS_TOKEN"

# Auth settings
curl -s http://127.0.0.1:8765/auth/v1/settings -H "apikey: $ANON_KEY"
```

### Realtime / Outbox

Polling：

```bash
curl -s http://127.0.0.1:8765/v1/projects/demo/events?since=0
```

SSE：

```bash
curl -N http://127.0.0.1:8765/v1/projects/demo/realtime?since=0
```

#### Supabase Realtime SSE (`/realtime/v1/stream`)

```bash
# SSE stream (authenticated users, optional table filter)
curl -N "http://127.0.0.1:8765/realtime/v1/stream?since=0&table=posts" \
  -H "apikey: $TOKEN"
```

### Scale-to-zero 模拟

`hibernate` 会 close connection 并删除 local cache。下一次请求会从对象存储 snapshot rehydrate：

```bash
curl -s -X POST http://127.0.0.1:8765/v1/projects/demo/hibernate
curl -s http://127.0.0.1:8765/v1/projects/demo/tables/notes -H "authorization: Bearer $TOKEN"
```

未优雅退出验证：

```bash
curl -s -X POST http://127.0.0.1:8765/v1/projects/demo/crash
curl -s http://127.0.0.1:8765/v1/projects/demo/tables/notes -H "authorization: Bearer $TOKEN"
```

`crash` 不做强制 snapshot，只关闭连接并删除 local cache，用来验证 snapshot + durable WAL 恢复。

## 当前边界

- 对象存储 adapter 已支持本地 filesystem 和可选 S3 feature（兼容 RustFS / AWS S3 等）；HTTP 读路径已通过 `AsyncObjectStore` + `spawn_blocking` 实现异步 IO，避免阻塞 tokio 运行时。当前验证覆盖 local conformance、真实 RustFS/S3 conformance、Toxiproxy 延迟/断流/恢复场景、HTTP 503/429/408/partial response、transient 503/429 retry 幂等矩阵、WAL/manifest/snapshot PUT 阶段的本地故障注入，以及 9 场景进程级 crash matrix。真实 S3 multi-part failure、跨 region 行为仍待补。
- durable path 使用 SQLite WAL segment + checksum manifest 追加到对象存储；已有 writer queue 和 durable WAL byte budget，writer lease 有后台续约线程、过期 claim GC 和 lease 冲突审计日志，但还不是多副本 quorum storage，GC 目前只删除 WAL prefix，尚未实现历史 snapshot 保留策略和异步 GC。
- policy 是受限 DSL，不兼容任意 Postgres RLS expression。
- SQLite 写路径仍是单库单写，适合低成本多小库，不适合单大库高并发写。
- 没有实现 Postgres wire protocol、PostgREST、extensions、PL/pgSQL。
