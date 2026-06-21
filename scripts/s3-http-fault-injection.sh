#!/usr/bin/env bash
set -euo pipefail

container_name="${SDB_HTTP_FAULT_S3_CONTAINER:-sdb-s3-http-fault}"
bucket="${SDB_S3_BUCKET:-serverless-db-poc}"
root_user="${SDB_S3_ROOT_USER:-rustfsadmin}"
root_password="${SDB_S3_ROOT_PASSWORD:-rustfsadmin}"
s3_endpoint="${SDB_S3_ENDPOINT:-http://127.0.0.1:9000}"
proxy_endpoint="${SDB_HTTP_FAULT_PROXY_ENDPOINT:-http://127.0.0.1:19100}"
proxy_listen="${SDB_HTTP_FAULT_PROXY_LISTEN:-127.0.0.1:19100}"
proxy_upstream="${SDB_HTTP_FAULT_PROXY_UPSTREAM:-127.0.0.1:9000}"
prefix="${SDB_S3_PREFIX:-serverless-db-http-fault}"
runtime_dir="${SDB_S3_RUNTIME_DIR:-/tmp/sdb-http-fault-s3}"
healthy_rows="${SDB_HTTP_FAULT_HEALTHY_ROWS:-32}"
fault_rows="${SDB_HTTP_FAULT_ROWS:-1}"
proxy_pid=""

cleanup_proxy() {
  if [ -n "${proxy_pid}" ]; then
    kill "${proxy_pid}" >/dev/null 2>&1 || true
    wait "${proxy_pid}" >/dev/null 2>&1 || true
    proxy_pid=""
  fi
}

cleanup() {
  cleanup_proxy
  docker rm -f "${container_name}" >/dev/null 2>&1 || true
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

start_proxy() {
  local mode="$1"
  local ready_file="/tmp/sdb-http-fault-proxy-${mode}.ready"
  rm -f "${ready_file}"
  cleanup_proxy
  rust-core/target/release/s3_fault_proxy \
    --listen "${proxy_listen}" \
    --upstream "${proxy_upstream}" \
    --mode "${mode}" \
    --ready-file "${ready_file}" &
  proxy_pid=$!
  for _ in $(seq 1 50); do
    if [ -f "${ready_file}" ]; then
      return 0
    fi
    sleep 0.1
  done
  echo "s3 fault proxy did not become ready for mode ${mode}" >&2
  exit 1
}

run_conformance() {
  local endpoint="$1"
  local scenario="$2"
  local rows="$3"
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
    --s3-endpoint "${endpoint}" \
    --s3-region us-east-1 \
    --s3-force-path-style \
    --rows "${rows}"
}

expect_fault_failure() {
  local mode="$1"
  set +e
  run_conformance "${proxy_endpoint}" "${mode}" "${fault_rows}" >/tmp/sdb-http-fault-${mode}.log 2>&1
  local status=$?
  set -e
  if [ "${status}" -eq 0 ]; then
    cat "/tmp/sdb-http-fault-${mode}.log" >&2
    echo "expected S3 HTTP fault mode ${mode} to fail" >&2
    exit 1
  fi
}

docker rm -f "${container_name}" >/dev/null 2>&1 || true
docker run -d \
  --name "${container_name}" \
  -p 127.0.0.1:9000:9000 \
  -p 127.0.0.1:9001:9001 \
  -e "RUSTFS_ACCESS_KEY=${root_user}" \
  -e "RUSTFS_SECRET_KEY=${root_password}" \
  rustfs/rustfs:latest /data >/dev/null

wait_http "${s3_endpoint}/health/ready"

docker run --rm \
  --network "container:${container_name}" \
  -e AWS_ACCESS_KEY_ID="${root_user}" \
  -e AWS_SECRET_ACCESS_KEY="${root_password}" \
  -e AWS_EC2_METADATA_DISABLED=true \
  amazon/aws-cli \
  --endpoint-url http://127.0.0.1:9000 \
  s3 mb "s3://${bucket}" >/dev/null 2>&1 || true

cargo build --release --manifest-path rust-core/Cargo.toml --bin s3_fault_proxy >/dev/null

start_proxy healthy
run_conformance "${proxy_endpoint}" "healthy-proxy" "${healthy_rows}" >/tmp/sdb-http-fault-healthy.log

for mode in status-503 status-429 status-408 partial-response; do
  start_proxy "${mode}"
  expect_fault_failure "${mode}"
done

start_proxy healthy
run_conformance "${proxy_endpoint}" "recovered-proxy" "${healthy_rows}" >/tmp/sdb-http-fault-recovered.log

cat <<JSON
{
  "checks": {
    "healthy_proxy_conformance": true,
    "status_503_fails_closed": true,
    "status_429_fails_closed": true,
    "status_408_fails_closed": true,
    "partial_response_fails_closed": true,
    "recovered_proxy_conformance": true
  },
  "object_store": "s3",
  "proxy": "s3_fault_proxy"
}
JSON
