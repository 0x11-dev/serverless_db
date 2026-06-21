# Realtime v2 WebSocket 协议设计

## 范围

本文说明 Supabase-compatible Realtime WebSocket 的可复用协议层。当前可执行入口仍保持现有 polling/SSE：

- `GET /v1/projects/{project_id}/events`
- `GET /v1/projects/{project_id}/realtime`
- `GET /realtime/v1/stream`

本阶段不新增 axum WebSocket route，不改变 Auth、ControlPlane、Dashboard，也不改变现有 SSE 行为。Rust 协议模块只负责 serde 类型、Phoenix frame 解析/编码，以及 outbox 事件到 Supabase Realtime payload 的转换；未来 route 可以复用它，但它本身不依赖 `http.rs` 或 `runtime.rs`。

## Phoenix Envelope

Supabase realtime-js v2 使用 Phoenix serializer。WebSocket JSON text frame 是 5 元素数组：

```json
[join_ref, ref, topic, event, payload]
```

协议模块也接受 object-form envelope，便于服务端测试和内部工具构造：

```json
{
  "join_ref": "1",
  "ref": "1",
  "topic": "realtime:public:posts",
  "event": "phx_join",
  "payload": {}
}
```

最小事件集：

| Event | 方向 | 作用 |
| --- | --- | --- |
| `phx_join` | client to server | 加入 channel，并提交 `config.postgres_changes` 订阅过滤器。 |
| `heartbeat` | client to server | 在 `phoenix` topic 上保活连接。 |
| `broadcast` | client to server / server to clients | 传递 channel broadcast payload。 |
| `postgres_changes` | server to clients | 下发匹配订阅 id 的数据库变更。 |
| `phx_reply` | server to client | 对 join、heartbeat、broadcast 或带 ref 的错误做 ack。 |
| `phx_error` | server to client | channel 级错误。 |

## Join 与订阅确认

客户端 join payload：

```json
{
  "access_token": "...",
  "config": {
    "broadcast": { "ack": false, "self": false },
    "presence": { "key": "", "enabled": false },
    "postgres_changes": [
      { "event": "INSERT", "schema": "public", "table": "posts", "filter": "id=eq.1" }
    ],
    "private": false
  }
}
```

join reply 必须保持 filter 顺序，并把同一组 filter 加上服务端生成的 `id` 返回：

```json
{
  "status": "ok",
  "response": {
    "postgres_changes": [
      {
        "id": "pg:public:posts:INSERT:id=eq.1:0",
        "event": "INSERT",
        "schema": "public",
        "table": "posts",
        "filter": "id=eq.1"
      }
    ]
  }
}
```

realtime-js 会按数组下标校验 server reply 中的 filter 是否和 client filter 一致，然后保存每个返回的 `id`。后续 `postgres_changes` frame 必须在 `payload.ids` 中带上匹配的 id，否则 SDK 不会触发对应 callback。

## `_sdb_outbox` 到 `postgres_changes`

现有 runtime 会把 mutation 记录到 `_sdb_outbox`：

| `_sdb_outbox` 字段 | Realtime payload 字段 |
| --- | --- |
| `id` | 本阶段不直接暴露；后续可以作为 replay cursor metadata。 |
| `created_at` | `payload.data.commit_timestamp` |
| `table_name` | `payload.data.table` |
| `operation` | `payload.data.type`，取值为 `INSERT`、`UPDATE`、`DELETE` |
| `row_json` | insert/update 映射到 `payload.data.record`，delete 映射到 `payload.data.old_record` |
| `actor_sub`、`actor_role` | 不放入 Supabase payload；route 可用于鉴权或审计。 |

发送给 channel 的 WebSocket message 形状：

```json
[
  null,
  null,
  "realtime:public:posts",
  "postgres_changes",
  {
    "ids": ["pg:public:posts:INSERT:*:0"],
    "data": {
      "schema": "public",
      "table": "posts",
      "type": "INSERT",
      "commit_timestamp": "2026-06-21 12:00:00",
      "columns": [{ "name": "id", "type": "jsonb" }],
      "record": { "id": 1 },
      "old_record": null,
      "errors": []
    }
  }
]
```

当前继承自 `_sdb_outbox` 的限制：

- UPDATE 只存更新后的 row，因此 `old_record` 暂为 `null`；要做到完整兼容，需要 runtime 记录 before-image。
- DELETE 把删除前 row 放到 `old_record`，并使用 `record: null`。
- column metadata 目前从 JSON object key 推断，类型统一为 `jsonb`；后续 WebSocket route 应从 `PRAGMA table_info` 或 schema cache 补齐更接近 Postgres 的列类型。
- 行级 filter matching 应在 fanout 前完成。协议模块只负责 payload 形状，不负责授权或过滤。

## 后续 WebSocket Route 形状

未来 `GET /realtime/v1/websocket` route 应按以下方式接入：

1. 复用现有 auth 逻辑校验 `apikey` 或 `access_token`。
2. 使用 `realtime_protocol::parse_frame` 解析 Phoenix frame。
3. 收到 `phx_join` 时，鉴权 channel 和 `postgres_changes` filter，然后用 `join_reply` 返回订阅 id。
4. 收到 `heartbeat` 时，用 `heartbeat_reply` 返回 `phx_reply`。
5. 通过现有 runtime event API 或内部 typed outbox cursor 读取 `_sdb_outbox`。
6. 对每个 outbox event 做 channel/filter 匹配，用 `outbox_event_to_postgres_changes` 转换，再用 `encode_frame` 发给客户端。
7. 在 SDK WebSocket 覆盖和跨机器 fanout 达到生产标准前，保留现有 SSE endpoints。

## 多机器 Fanout

单进程路径可以直接 poll/wait `_sdb_outbox`，但多机器 WebSocket 必须有共享 fanout log：客户端 socket 可能在任意节点上，而写入可能由另一个节点提交。

| 方案 | 优势 | 代价 | 适用性 |
| --- | --- | --- | --- |
| NATS JetStream | 低延迟 pub/sub + durable stream，支持 pull consumer 和按 sequence replay，运维复杂度低于 Kafka。 | 长期留存和分析生态弱于 Kafka；需要设计 subject 来隔离租户。 | 推荐作为 Tier2 realtime fanout 默认方案。 |
| Redis Streams | 如果已有 Redis，接入和本地运维最简单；consumer group 足够支撑小规模单地域部署。 | memory-first 压力明显；retention trim 容易误配；跨地域持久性较弱。 | 适合 Tier1/Tier2 过渡和本地/dev 部署。 |
| Redpanda | Kafka-compatible，高吞吐、强 retention/replay、partition 模型成熟。 | 对当前 POC 偏重；topic/partition 规划会变成 ControlPlane 职责。 | 适合高吞吐、长留存、审计/replay 需求明确的阶段。 |

建议：生产 Tier2 默认选 NATS JetStream。每个已提交 `_sdb_outbox` row 在 SQLite transaction commit 后发布一条 fanout event，subject 可按 project/table 分层，例如 `sdb.project.{project_id}.table.{table}.changes`。WebSocket 节点按 project 建 durable consumer，或按节点建 ephemeral filtered consumer。Redis Streams 保留为轻量 local/dev 方案；Redpanda 留给高规模与 Kafka 生态明确需要的部署。

## 边界

协议层不决定：

- Auth policy 或租户成员关系。
- 哪些 project/table 开启 Realtime。
- ControlPlane 生命周期、调度、计费或 Dashboard 状态。
- WebSocket route 的 backpressure、连接数限制和关闭策略。

这些属于后续 Auth、ControlPlane 和 runtime integration 工作。当前模块只解析/编码 Phoenix frame，并把 outbox event JSON 转成 Supabase-compatible payload。
