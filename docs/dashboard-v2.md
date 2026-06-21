# Dashboard v2 Console Skeleton

Dashboard v2 是给 Serverless DB POC 使用的管理 console 骨架。当前实现只新增前端与 typed mock adapter，不接入 Rust data-plane，也不修改 `rust-core/src/http.rs` 或 `rust-core/src/runtime.rs`。

## 目标

- 第一屏直接进入可操作 console：左侧导航、顶部搜索/角色切换、项目状态表、右侧 inspector。
- 覆盖管理员和普通用户都需要感知的核心区域：Projects、API keys、Auth users、Tables、Storage buckets、Realtime、System / Workers。
- UI 只依赖 `DashboardControlPlaneClient` facade；当前 `MockAdminClient` 提供静态数据，后续替换为 control-plane admin API 时不需要重写页面结构。
- 明确角色差异：管理员看到全部项目、服务密钥、Auth 全量用户和 worker 状态；普通用户只能看到自己的项目，API keys/Auth users 页面保留骨架但敏感字段被隐藏，System / Workers 为 admin-only。

## 文件结构

```text
dashboard/
  index.html
  vite.config.ts
  tsconfig.json
  src/
    App.tsx
    main.tsx
    styles.css
    api/
      mockAdminClient.ts
      types.ts
  tests/
    dashboard.test.ts
```

## 信息架构

| 页面 | 管理员视图 | 普通用户视图 |
| --- | --- | --- |
| Projects | 全部项目、状态、region、bookmark、storage、replica lag | 仅 owner 项目 |
| API keys | anon、service_role、replica token 与轮换状态 | anon 可见，service_role 行保留但 secret/last-used/rotation 隐藏 |
| Auth users | 项目内用户、role、provider、last seen | 仅当前用户 |
| Tables | 可见项目的表、列数、行数、policy 数、realtime 开关 | 仅 owner 项目表 |
| Storage buckets | bucket、对象数、容量、公开状态、retention | 仅 owner 项目 bucket |
| Realtime | outbox stream、subscriber、event id、lag | 仅 owner 项目 stream |
| System / Workers | writer、compactor、replica、outbox worker 状态 | restricted skeleton |

## Adapter Contract

当前 facade 位于 `dashboard/src/api/types.ts`：

```ts
export interface DashboardControlPlaneClient {
  getNavigation(role: DashboardRole): Promise<NavigationItem[]>;
  getDashboardSnapshot(role: DashboardRole): Promise<DashboardSnapshot>;
}
```

后续真实 control plane 可以先实现同一接口，再逐步替换 mock 数据来源。建议控制面 API 不直接复用 data-plane service role token；应提供单独 admin auth、审计日志、RBAC、密钥脱敏和 rate limit。

## 本地命令

```bash
npm run dashboard:test
npm run dashboard:typecheck
npm run dashboard:build
npm run dashboard:dev
```

`dashboard:test` 覆盖页面骨架完整性、管理员/普通用户权限差异、service_role key 脱敏和 worker admin-only 边界。`dashboard:build` 只构建前端静态产物，不需要启动 Rust core。

## 后续接入点

1. 新增 control-plane admin API，聚合 projects、keys、auth、schema、storage、realtime、worker health。
2. 将 `MockAdminClient` 替换为 HTTP client，并保留当前 facade 类型作为编译期契约。
3. 对 API keys、Auth users、System / Workers 加入写操作前，先补齐 RBAC、审计日志、CSRF/Origin 策略和密钥一次性 reveal 流程。
4. 将项目状态与 Rust core 已有 `project_info`、routing health、bookmark、replica lag 数据对齐。
