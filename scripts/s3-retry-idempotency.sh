#!/usr/bin/env bash
set -euo pipefail

container_name="${SDB_RETRY_S3_CONTAINER:-sdb-s3-retry}"
bucket="${SDB_S3_BUCKET:-serverless-db-poc}"
root_user="${SDB_S3_ROOT_USER:-rustfsadmin}"
root_password="${SDB_S3_ROOT_PASSWORD:-rustfsadmin}"
s3_endpoint="${SDB_S3_ENDPOINT:-http://127.0.0.1:9000}"
proxy_endpoint="${SDB_RETRY_PROXY_ENDPOINT:-http://127.0.0.1:19101}"
proxy_listen="${SDB_RETRY_PROXY_LISTEN:-127.0.0.1:19101}"
proxy_upstream="${SDB_RETRY_PROXY_UPSTREAM:-127.0.0.1:9000}"
prefix="${SDB_S3_PREFIX:-serverless-db-retry}"
runtime_dir="${SDB_S3_RUNTIME_DIR:-/tmp/sdb-retry-s3}"
rows="${SDB_RETRY_ROWS:-8}"
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
  local target="$2"
  local count_file="$3"
  local ready_file="/tmp/sdb-retry-proxy-${mode}-${target}.ready"
  rm -f "${ready_file}" "${count_file}"
  cleanup_proxy
  rust-core/target/release/s3_fault_proxy \
    --listen "${proxy_listen}" \
    --upstream "${proxy_upstream}" \
    --mode "${mode}" \
    --target "${target}" \
    --transient-failures 1 \
    --fault-count-file "${count_file}" \
    --ready-file "${ready_file}" >/tmp/sdb-retry-proxy-${mode}-${target}.log 2>&1 &
  proxy_pid=$!
  for _ in $(seq 1 50); do
    if [ -f "${ready_file}" ]; then
      return 0
    fi
    sleep 0.1
  done
  cat "/tmp/sdb-retry-proxy-${mode}-${target}.log" >&2 || true
  echo "s3 retry proxy did not become ready for mode ${mode} target ${target}" >&2
  exit 1
}

run_conformance() {
  local scenario="$1"
  AWS_ACCESS_KEY_ID="${root_user}" \
  AWS_SECRET_ACCESS_KEY="${root_password}" \
  AWS_EC2_METADATA_DISABLED=true \
  AWS_RETRY_MODE="${AWS_RETRY_MODE:-standard}" \
  AWS_MAX_ATTEMPTS="${AWS_MAX_ATTEMPTS:-4}" \
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

assert_fault_injected() {
  local count_file="$1"
  local label="$2"
  local count
  count="$(cat "${count_file}" 2>/dev/null || echo 0)"
  if [ "${count}" -lt 1 ]; then
    echo "expected transient fault to be injected for ${label}" >&2
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

targets=(list-objects put-object get-object head-object delete-object)
modes=(transient-503 transient-429)
for mode in "${modes[@]}"; do
  for target in "${targets[@]}"; do
    count_file="/tmp/sdb-retry-${mode}-${target}.count"
    start_proxy "${mode}" "${target}" "${count_file}"
    run_conformance "${mode}-${target}" >/tmp/sdb-retry-${mode}-${target}.log
    assert_fault_injected "${count_file}" "${mode}/${target}"
  done
done

cat <<JSON
{
  "checks": {
    "transient_503_list_objects": true,
    "transient_503_put_object": true,
    "transient_503_get_object": true,
    "transient_503_head_object": true,
    "transient_503_delete_object": true,
    "transient_429_list_objects": true,
    "transient_429_put_object": true,
    "transient_429_get_object": true,
    "transient_429_head_object": true,
    "transient_429_delete_object": true
  },
  "aws_retry_mode": "${AWS_RETRY_MODE:-standard}",
  "aws_max_attempts": "${AWS_MAX_ATTEMPTS:-4}",
  "object_store": "s3",
  "proxy": "s3_fault_proxy"
}
JSON
