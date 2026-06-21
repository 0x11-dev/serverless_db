#!/usr/bin/env bash
set -euo pipefail

container_name="${SDB_S3_CONTAINER:-sdb-s3-conformance}"
bucket="${SDB_S3_BUCKET:-serverless-db-poc}"
root_user="${SDB_S3_ROOT_USER:-rustfsadmin}"
root_password="${SDB_S3_ROOT_PASSWORD:-rustfsadmin}"
endpoint="${SDB_S3_ENDPOINT:-http://127.0.0.1:9000}"
prefix="${SDB_S3_PREFIX:-serverless-db-conformance}"
runtime_dir="${SDB_S3_RUNTIME_DIR:-/tmp/sdb-conformance-s3}"
rows="${SDB_S3_ROWS:-128}"

docker rm -f "${container_name}" >/dev/null 2>&1 || true
docker run -d \
  --name "${container_name}" \
  -p 127.0.0.1:9000:9000 \
  -p 127.0.0.1:9001:9001 \
  -e "RUSTFS_ACCESS_KEY=${root_user}" \
  -e "RUSTFS_SECRET_KEY=${root_password}" \
  rustfs/rustfs:latest /data >/dev/null

cleanup() {
  docker rm -f "${container_name}" >/dev/null 2>&1 || true
}
trap cleanup EXIT

for _ in $(seq 1 60); do
  if curl -fsS "${endpoint}/health/ready" >/dev/null 2>&1; then
    break
  fi
  sleep 1
done
curl -fsS "${endpoint}/health/ready" >/dev/null

docker run --rm \
  --network "container:${container_name}" \
  -e AWS_ACCESS_KEY_ID="${root_user}" \
  -e AWS_SECRET_ACCESS_KEY="${root_password}" \
  -e AWS_EC2_METADATA_DISABLED=true \
  amazon/aws-cli \
  --endpoint-url http://127.0.0.1:9000 \
  s3 mb "s3://${bucket}" >/dev/null 2>&1 || true

AWS_ACCESS_KEY_ID="${root_user}" \
AWS_SECRET_ACCESS_KEY="${root_password}" \
AWS_EC2_METADATA_DISABLED=true \
cargo run --release --manifest-path rust-core/Cargo.toml --features s3 --bin object_store_conformance -- \
  --object-store s3 \
  --runtime-dir "${runtime_dir}" \
  --s3-bucket "${bucket}" \
  --s3-prefix "${prefix}" \
  --s3-endpoint "${endpoint}" \
  --s3-region us-east-1 \
  --s3-force-path-style \
  --rows "${rows}"
