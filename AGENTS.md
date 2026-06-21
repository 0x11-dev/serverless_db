# Agent Guidelines

## Project Overview

Serverless DB POC — a Supabase-like serverless database with SQLite per-project hot data engine, object-store durable WAL/snapshot, JWT auth, policy DSL, storage, realtime outbox, read replicas, and Supabase SDK compatibility.

- **Rust core** (`rust-core/`): production data-plane (HTTP API, SQLite, object store, auth, policy, writer lease, replica forwarding)
- **TypeScript POC** (`src/`): early prototype, kept as API reference baseline
- **Examples** (`examples/`): `demo.ts` (basic demo), `blog-app/` (comprehensive feature verification)

## Architecture Quick Reference

| Component | Location |
| --- | --- |
| Rust HTTP routes | `rust-core/src/http.rs` |
| Rust runtime (SQLite, WAL, snapshot, writer) | `rust-core/src/runtime.rs` |
| Rust auth (JWT HS256) | `rust-core/src/auth.rs` |
| Rust policy DSL | `rust-core/src/policy.rs` |
| Rust object store (local + S3) | `rust-core/src/object_store.rs` |
| Rust PostgREST compat | `rust-core/src/postgrest.rs` |
| TS HTTP routes | `src/http.ts` |
| TS runtime | `src/runtime.ts` |
| Docker distributed deploy | `deploy/docker-compose.distributed.yml` |
| Conformance / fault scripts | `scripts/` |
| Bench | `bench/perf.ts` (TS), `rust-core/src/bin/bench.rs` (Rust) |

## Mandatory Rules

### 1. Every new feature MUST include test coverage

When adding or modifying a feature, update or add tests before implementation is considered complete.

- **Rust core tests**: `cargo test --manifest-path rust-core/Cargo.toml` — add unit/integration tests in `rust-core/src/` or `rust-core/tests/`.
- **TypeScript tests**: `npm test` — update `tests/poc.test.ts`.
- **Conformance / fault injection**: if the feature touches object store or S3 adapter, update or add scripts in `scripts/`.
- **Crash matrix**: if the feature touches durable WAL, snapshot, or recovery path, add a scenario to `rust-core/src/bin/crash_matrix.rs`.

### 2. Every new feature MUST be covered by a demo or example

When adding or modifying a feature, update the demo/example so the new capability is exercised end-to-end.

- **Basic demo**: `examples/demo.ts` — update for simple API additions.
- **Comprehensive example**: `examples/blog-app/app.mjs` — update when adding new API surface, policy rule type, storage capability, realtime feature, replica behavior, recovery scenario, or SDK compat surface.
- **Docker integration**: if the feature is relevant to distributed deployment, update `deploy/docker-compose.distributed.yml` and the `blog-example` service.

### 3. Update README when API surface changes

When adding or changing an HTTP route, policy rule, CLI flag, or environment variable, update `README.md` accordingly.

### 4. Keep changes minimal and focused

- Prefer minimal upstream fixes over downstream workarounds.
- Follow existing code style (Rust: `cargo fmt`, TypeScript: existing conventions).
- Do not add comments or documentation unless explicitly requested.

## Feature Checklist

Use this checklist when implementing a new feature:

- [ ] Implementation in Rust core (and/or TypeScript POC if applicable)
- [ ] Unit/integration test added
- [ ] `examples/blog-app/app.mjs` updated to exercise the feature
- [ ] `README.md` updated if API surface changed
- [ ] `deploy/docker-compose.distributed.yml` updated if deployment-relevant
- [ ] `cargo test --manifest-path rust-core/Cargo.toml` passes
- [ ] `npm run example:blog` passes against local dev server

## Running Tests & Demos

```bash
# Rust core tests
npm run core:test

# TypeScript tests
npm test

# Local dev server
npm run core:dev

# Basic demo
npm run demo

# Comprehensive example
npm run example:blog

# Docker cluster + example
docker compose -f deploy/docker-compose.distributed.yml up -d --build
docker compose -f deploy/docker-compose.distributed.yml run --rm blog-example
```
