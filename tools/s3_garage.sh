#!/usr/bin/env bash
# A throwaway single-node Garage (S3-compatible) for the `storage-s3` tests
# (#1015). Works with docker or podman.
#
#   tools/s3_garage.sh up     start it, create a bucket and a key, print the
#                             environment the tests read (eval it)
#   tools/s3_garage.sh down   remove it
#
#   eval "$(tools/s3_garage.sh up)"
#   cargo test --features storage-s3 --lib s3_garage -- --include-ignored
#   tools/s3_garage.sh down
#
# Garage ignores If-Match / If-None-Match on PutObject, so these tests
# exercise the SingleWriter commit mode; conditional-put behaviour is
# covered against the in-memory store (and MinIO/AWS by hand).
set -euo pipefail

NAME=${GARAGE_NAME:-db-core-garage}
IMAGE=${GARAGE_IMAGE:-docker.io/dxflrs/garage:v2.1.0}
# Not Garage's stock 3900: a developer's own Garage usually owns that.
PORT=${GARAGE_PORT:-39000}
RT=${CONTAINER_RUNTIME:-$(command -v podman >/dev/null 2>&1 && echo podman || echo docker)}
BUCKET=${GARAGE_BUCKET:-db-core-test}

garage() { "$RT" exec "$NAME" /garage "$@"; }

case "${1:-}" in
up)
  dir=$(mktemp -d)
  mkdir -p "$dir/meta" "$dir/data"
  cat >"$dir/garage.toml" <<TOML
metadata_dir = "/meta"
data_dir = "/data"
db_engine = "sqlite"
replication_factor = 1
rpc_bind_addr = "[::]:3901"
rpc_public_addr = "127.0.0.1:3901"
rpc_secret = "$(openssl rand -hex 32)"
[s3_api]
s3_region = "garage"
api_bind_addr = "[::]:3900"
TOML
  "$RT" rm -f "$NAME" >/dev/null 2>&1 || true
  if curl -s -o /dev/null --max-time 2 "http://127.0.0.1:$PORT/"; then
    echo "port $PORT is already in use; set GARAGE_PORT" >&2
    exit 1
  fi
  "$RT" run -d --name "$NAME" -p "127.0.0.1:$PORT:3900" \
    -v "$dir/garage.toml:/etc/garage.toml" -v "$dir/meta:/meta" -v "$dir/data:/data" \
    "$IMAGE" >/dev/null
  for _ in $(seq 1 60); do
    garage status >/dev/null 2>&1 && break
    sleep 1
  done
  id=$(garage status 2>/dev/null | awk '/127.0.0.1:3901/{print $1}')
  garage layout assign -z dc1 -c 1G "$id" >/dev/null
  garage layout apply --version 1 >/dev/null
  garage bucket create "$BUCKET" >/dev/null
  key=$(garage key create db-core-key)
  access=$(echo "$key" | awk '/Key ID:/{print $3}')
  secret=$(echo "$key" | awk '/Secret key:/{print $3}')
  garage bucket allow --read --write --owner "$BUCKET" --key db-core-key >/dev/null
  # Garage answers S3 requests a moment after the layout applies.
  for _ in $(seq 1 30); do
    curl -s -o /dev/null "http://127.0.0.1:$PORT/" && break
    sleep 1
  done
  cat <<ENV
export DB_CORE_S3_ENDPOINT=http://127.0.0.1:$PORT
export DB_CORE_S3_REGION=garage
export DB_CORE_S3_BUCKET=$BUCKET
export AWS_ACCESS_KEY_ID=$access
export AWS_SECRET_ACCESS_KEY=$secret
ENV
  ;;
down)
  "$RT" rm -f "$NAME" >/dev/null 2>&1 || true
  ;;
*)
  echo "usage: $0 up|down" >&2
  exit 2
  ;;
esac
