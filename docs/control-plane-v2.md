# Control Plane v2 Skeleton

日期：2026-06-21

## 本轮边界

本轮只新增 `rust-core/src/control_plane.rs` 的 in-memory 控制面契约和单元测试，不接入现有 HTTP 路由，不改变 Auth、Realtime、Dashboard 或 data-plane runtime 行为。

该模块表达四个最小职责：

- project catalog：记录 `project_id`、对象存储 prefix、默认 region、active/cold serving state。
- API key routing：把 browser client 的 anon key、server-side SDK 的 service-role key 解析到 `project_id`，并显式返回 `service_role` 和 `browser_safe` 标记。
- worker endpoint registry：记录 worker id、region、URL、健康状态，以及是否能承接 cold project wakeup。
- placement decision：对 active project 返回 primary/replica route，对 cold project 返回 wakeup worker。

当前实现是进程内 `InMemoryControlPlane`，只用于契约测试和后续接线前的接口稳定。它不负责签发 JWT、不验证 HTTP header、不绕过或修改 policy DSL，也不读取 Realtime outbox。

## 和现有 data plane 的关系

现有 `RuntimeOptions.routing_endpoints`、`--primary-url`、`--read-replica` 和 writer lease 仍是 data plane 本地配置。控制面未来替代的是这些静态 registry 和项目生命周期决策，不替代对象存储里的 writer lease fencing。

后续接线建议保持分层：

- Gateway/Auth 层先用 API key 解析 `project_id` 和 key kind。
- Auth/Policy 层决定 anon/authenticated/service-role 的权限语义。
- Control Plane 只决定请求该发往哪个 worker，或是否触发 cold project wakeup。
- Runtime 继续用 object-store manifest、WAL、snapshot、writer lease 保障单主写和恢复。

## 后续持久化实现

### etcd / Consul

适合先落地 worker registry 和租约型 membership：

- `/projects/{project_id}`：project catalog、object store prefix、active/cold state、primary worker、replica workers。
- `/api-keys/{key_hash}`：key id、project id、key kind、状态和轮换窗口。
- `/workers/{worker_id}`：endpoint、region、capabilities、heartbeat lease。
- `/placements/{project_id}`：带 revision 的 placement 决策，gateway 按 revision 缓存。

failover 更新必须使用 compare-and-swap：只有当前 primary worker lease 失效，且新 worker 已成功 cold open 并拿到 writer lease fencing token 后，才能提交新的 placement revision。

### FoundationDB

适合需要强事务 catalog、key rotation 和 placement history 的版本：

- project、API key、worker、placement 放在同一个 tenant tuple space。
- API key 解析、project 状态读取、placement revision 可以在一个只读事务内完成。
- failover 使用事务条件写入：检查旧 primary epoch、写新 primary epoch、追加 audit event 一起提交。
- 可保留最近 N 个 placement revision，支持 gateway 快速判断缓存是否过期。

## Failover / Wakeup 流程

active primary failover：

1. Worker heartbeat 过期后，控制面把 endpoint 标记为 unhealthy。
2. 调度器选择同 region 或 fallback region 的候选 worker。
3. 候选 worker 从对象存储 rehydrate project，并尝试获取 writer lease。
4. 只有 writer lease fencing token 获取成功后，控制面用 CAS/事务把 project primary 切到候选 worker。
5. Gateway 收到新 placement revision 后把写请求发往新 primary；旧 primary 即使恢复，也会因 writer lease fencing 失效而不能继续写。

cold project wakeup：

1. 项目 idle 后 runtime flush durable WAL/snapshot，控制面把 serving state 改成 `Cold`，不保留 active worker。
2. 首个请求通过 anon/service key 解析到 project。
3. placement 对 cold state 返回 `WakeProject`，gateway 可同步等待 worker cold open，或返回 202/重试提示。
4. worker rehydrate 完成后，控制面把 project 改为 `Active` 并发布新 placement revision。

单 project scale-to-zero 的关键约束是：cold state 必须保留对象存储 prefix 和 key routing，但不能依赖任何单机内存状态；恢复源只能是 manifest、snapshot 和 WAL。
