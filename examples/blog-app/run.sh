#!/usr/bin/env bash
#
# Run the blog platform example against a local or Docker-deployed serverless-db cluster.
#
# Usage:
#   # Local dev server (npm run core:dev)
#   bash examples/blog-app/run.sh
#
#   # Docker cluster (primary on :80)
#   SDB_BASE_URL=http://127.0.0.1:80 \
#   SDB_REPLICA_URLS=http://127.0.0.1:8766,http://127.0.0.1:8767 \
#   SDB_JWT_SECRET=your-secret \
#   SDB_ENV=production \
#   bash examples/blog-app/run.sh
#
# Environment variables:
#   SDB_BASE_URL      — primary endpoint (default: http://127.0.0.1:8765)
#   SDB_REPLICA_URLS  — comma-separated replica URLs (default: empty)
#   SDB_JWT_SECRET    — JWT signing secret (default: dev-secret-change-me)
#   SDB_PROJECT_ID    — project ID (default: blog-app)
#   SDB_ENV           — set to "production" to use local JWT minting
#   SDB_REPORT_DIR    — report output directory (default: reports)

set -euo pipefail

cd "$(dirname "$0")/../.."

exec node examples/blog-app/app.mjs
