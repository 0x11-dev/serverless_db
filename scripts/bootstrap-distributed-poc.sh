#!/usr/bin/env bash
set -euo pipefail

base_url="${1:-${SDB_BASE_URL:-http://127.0.0.1:8765}}"
project_id="${SDB_PROJECT_ID:-demo}"
subject="${SDB_COMPAT_SUB:-alice}"
role="${SDB_COMPAT_ROLE:-authenticated}"
expires_in="${SDB_COMPAT_EXPIRES_IN:-315360000}"

json_get_token='import json,sys; print(json.load(sys.stdin)["token"])'

token_response="$(
  curl -fsS -X POST "${base_url}/v1/tokens" \
    -H 'content-type: application/json' \
    -d "{\"sub\":\"${subject}\",\"role\":\"${role}\",\"claims\":{},\"expires_in\":${expires_in}}"
)"
token="$(printf '%s' "${token_response}" | python3 -c "${json_get_token}")"

curl -fsS -X POST "${base_url}/v1/projects" \
  -H 'content-type: application/json' \
  -d "{\"id\":\"${project_id}\"}" >/dev/null

curl -fsS -X POST "${base_url}/v1/projects/${project_id}/tables" \
  -H 'content-type: application/json' \
  -H "authorization: Bearer ${token}" \
  -H 'idempotency-key: bootstrap-notes-table' \
  -d '{
    "name": "notes",
    "columns": [
      {"name": "owner_id", "type": "text", "not_null": true},
      {"name": "title", "type": "text", "not_null": true},
      {"name": "body", "type": "text"}
    ]
  }' >/dev/null

curl -fsS -X PUT "${base_url}/v1/projects/${project_id}/policies" \
  -H 'content-type: application/json' \
  -H "authorization: Bearer ${token}" \
  -H 'idempotency-key: bootstrap-notes-owner-policy' \
  -d '{
    "table": "notes",
    "operation": "all",
    "name": "owner_only",
    "rule": {"column": "owner_id", "equals_claim": "sub"}
  }' >/dev/null

printf 'export SUPABASE_URL=%q\n' "${base_url}"
printf 'export SUPABASE_ANON_KEY=%q\n' "${token}"
printf 'export SDB_PROJECT_ID=%q\n' "${project_id}"
printf 'export SDB_COMPAT_SUB=%q\n' "${subject}"
