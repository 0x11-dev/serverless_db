#!/usr/bin/env bash
set -euo pipefail

base_url="${1:-${SDB_BASE_URL:-http://127.0.0.1:8765}}"
project_id="${SDB_PROJECT_ID:-demo}"
subject="${SDB_COMPAT_SUB:-alice}"
role="${SDB_COMPAT_ROLE:-authenticated}"
expires_in="${SDB_COMPAT_EXPIRES_IN:-315360000}"

mint_token() {
  local sub="$1"
  local token_role="$2"
  local claims_json="$3"
  local token_expires_in="$4"
  node -e '
    const { createHmac } = require("node:crypto");
    const [sub, role, claimsJson, expiresInRaw] = process.argv.slice(1);
    const secret = process.env.SDB_JWT_SECRET || (process.env.SDB_ENV === "production" ? "" : "dev-secret-change-me");
    if (!secret) {
      throw new Error("SDB_JWT_SECRET is required when SDB_ENV=production");
    }
    const now = Math.floor(Date.now() / 1000);
    const payload = {
      sub,
      role,
      claims: JSON.parse(claimsJson),
      iat: now,
      exp: now + Number(expiresInRaw)
    };
    const b64 = (value) => Buffer.from(JSON.stringify(value)).toString("base64url");
    const signingInput = `${b64({ alg: "HS256", typ: "JWT" })}.${b64(payload)}`;
    const sig = createHmac("sha256", secret).update(signingInput).digest("base64url");
    process.stdout.write(`${signingInput}.${sig}`);
  ' "${sub}" "${token_role}" "${claims_json}" "${token_expires_in}"
}

service_token="$(mint_token "admin" "service_role" '{}' "${expires_in}")"
token="$(mint_token "${subject}" "${role}" '{}' "${expires_in}")"

curl -fsS -X POST "${base_url}/v1/projects" \
  -H 'content-type: application/json' \
  -H "authorization: Bearer ${service_token}" \
  -d "{\"id\":\"${project_id}\"}" >/dev/null

curl -fsS -X POST "${base_url}/v1/projects/${project_id}/tables" \
  -H 'content-type: application/json' \
  -H "authorization: Bearer ${service_token}" \
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
  -H "authorization: Bearer ${service_token}" \
  -H 'idempotency-key: bootstrap-notes-owner-policy' \
  -d '{
    "table": "notes",
    "operation": "all",
    "name": "owner_only",
    "rule": {"column": "owner_id", "equals_claim": "sub"}
  }' >/dev/null

printf 'export SUPABASE_URL=%q\n' "${base_url}"
printf 'export SUPABASE_ANON_KEY=%q\n' "${token}"
printf 'export SDB_SERVICE_ROLE_KEY=%q\n' "${service_token}"
printf 'export SDB_PROJECT_ID=%q\n' "${project_id}"
printf 'export SDB_COMPAT_SUB=%q\n' "${subject}"
