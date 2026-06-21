#!/usr/bin/env bash
set -euo pipefail

s3_container="${SDB_FAULT_S3_CONTAINER:-sdb-s3-fault-s3}"
toxiproxy_container="${SDB_FAULT_TOXIPROXY_CONTAINER:-sdb-s3-fault-toxiproxy}"
network_name="${SDB_FAULT_NETWORK:-sdb-s3-fault-net}"
bucket="${SDB_S3_BUCKET:-serverless-db-poc}"
root_user="${SDB_S3_ROOT_USER:-rustfsadmin}"
root_password="${SDB_S3_ROOT_PASSWORD:-rustfsadmin}"
s3_endpoint="${SDB_S3_ENDPOINT:-http://127.0.0.1:9000}"
proxy_endpoint="${SDB_FAULT_PROXY_ENDPOINT:-http://127.0.0.1:19000}"
toxiproxy_api="${SDB_TOXIPROXY_API:-http://127.0.0.1:8474}"
toxiproxy_image="${SDB_TOXIPROXY_IMAGE:-ghcr.io/shopify/toxiproxy:latest}"
prefix="${SDB_S3_PREFIX:-serverless-db-fault}"
runtime_dir="${SDB_S3_RUNTIME_DIR:-/tmp/sdb-fault-s3}"
rows="${SDB_S3_ROWS:-128}"
latency_ms="${SDB_FAULT_LATENCY_MS:-200}"
jitter_ms="${SDB_FAULT_JITTER_MS:-50}"
proxy_name="s3"

cleanup() {
  docker rm -f "${toxiproxy_container}" "${s3_container}" >/dev/null 2>&1 || true
  docker network rm "${network_name}" >/dev/null 2>&1 || true
}
trap cleanup EXIT

wait_http() {
  local url="$1"
  for _ in $(seq 1 60); do
    if curl -fsS "${url}" >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  curl -fsS "${url}" >/dev/null
}

create_proxy() {
  curl -fsS -X DELETE "${toxiproxy_api}/proxies/${proxy_name}" >/dev/null 2>&1 || true
  curl -fsS -X POST "${toxiproxy_api}/proxies" \
    -H 'content-type: application/json' \
    -d "{\"name\":\"${proxy_name}\",\"listen\":\"0.0.0.0:19000\",\"upstream\":\"${s3_container}:9000\",\"enabled\":true}" \
    >/dev/null
}

add_latency_toxic() {
  curl -fsS -X POST "${toxiproxy_api}/proxies/${proxy_name}/toxics" \
    -H 'content-type: application/json' \
    -d "{\"name\":\"latency_downstream\",\"type\":\"latency\",\"stream\":\"downstream\",\"toxicity\":1.0,\"attributes\":{\"latency\":${latency_ms},\"jitter\":${jitter_ms}}}" \
    >/dev/null
}

delete_latency_toxic() {
  curl -fsS -X DELETE "${toxiproxy_api}/proxies/${proxy_name}/toxics/latency_downstream" >/dev/null
}

run_conformance() {
  local scenario="$1"
  AWS_ACCESS_KEY_ID="${root_user}" \
  AWS_SECRET_ACCESS_KEY="${root_password}" \
  AWS_EC2_METADATA_DISABLED=true \
  AWS_MAX_ATTEMPTS="${AWS_MAX_ATTEMPTS:-1}" \
  cargo run --release --manifest-path rust-core/Cargo.toml --features s3 --bin object_store_conformance -- \
    --object-store s3 \
    --runtime-dir "${runtime_dir}" \
    --namespace "${scenario}-$(date +%s)" \
    --s3-bucket "${bucket}" \
    --s3-prefix "${prefix}" \
    --s3-endpoint "${proxy_endpoint}" \
    --s3-region us-east-1 \
    --s3-force-path-style \
    --rows "${rows}"
}

expect_outage_failure() {
  set +e
  AWS_ACCESS_KEY_ID="${root_user}" \
  AWS_SECRET_ACCESS_KEY="${root_password}" \
  AWS_EC2_METADATA_DISABLED=true \
  AWS_MAX_ATTEMPTS="${AWS_MAX_ATTEMPTS:-1}" \
  cargo run --release --manifest-path rust-core/Cargo.toml --features s3 --bin object_store_conformance -- \
    --object-store s3 \
    --runtime-dir "${runtime_dir}" \
    --namespace "outage-$(date +%s)" \
    --s3-bucket "${bucket}" \
    --s3-prefix "${prefix}" \
    --s3-endpoint "${proxy_endpoint}" \
    --s3-region us-east-1 \
    --s3-force-path-style \
    --rows 1 >/tmp/sdb-fault-outage.log 2>&1
  local status=$?
  set -e
  if [ "${status}" -eq 0 ]; then
    cat /tmp/sdb-fault-outage.log >&2
    echo "expected outage scenario to fail while the S3 proxy is stopped" >&2
    exit 1
  fi
}

cleanup
docker network create "${network_name}" >/dev/null

docker run -d \
  --name "${s3_container}" \
  --network "${network_name}" \
  -p 127.0.0.1:9000:9000 \
  -p 127.0.0.1:9001:9001 \
  -e "RUSTFS_ACCESS_KEY=${root_user}" \
  -e "RUSTFS_SECRET_KEY=${root_password}" \
  rustfs/rustfs:latest /data >/dev/null

docker run -d \
  --name "${toxiproxy_container}" \
  --network "${network_name}" \
  -p 127.0.0.1:8474:8474 \
  -p 127.0.0.1:19000:19000 \
  "${toxiproxy_image}" >/dev/null

wait_http "${s3_endpoint}/health/ready"
wait_http "${toxiproxy_api}/version"
create_proxy

docker run --rm \
  --network "${network_name}" \
  -e AWS_ACCESS_KEY_ID="${root_user}" \
  -e AWS_SECRET_ACCESS_KEY="${root_password}" \
  -e AWS_EC2_METADATA_DISABLED=true \
  amazon/aws-cli \
  --endpoint-url "http://${s3_container}:9000" \
  s3 mb "s3://${bucket}" >/dev/null 2>&1 || true

run_conformance "healthy"
add_latency_toxic
run_conformance "latency-${latency_ms}ms"
delete_latency_toxic

docker stop "${toxiproxy_container}" >/dev/null
expect_outage_failure
docker start "${toxiproxy_container}" >/dev/null
wait_http "${toxiproxy_api}/version"
create_proxy
run_conformance "recovered"

cat <<JSON
{
  "checks": {
    "healthy_proxy_conformance": true,
    "latency_proxy_conformance": true,
    "outage_fails_closed": true,
    "recovered_proxy_conformance": true
  },
  "object_store": "s3",
  "proxy": "toxiproxy",
  "latency_ms": ${latency_ms},
  "jitter_ms": ${jitter_ms}
}
JSON
