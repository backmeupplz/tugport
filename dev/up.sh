#!/bin/sh
# Build and start the dev box: `dev/up.sh [name] [port-offset]`. Source is mounted at /src.
set -e
NAME=${1:-tugport-dev}; OFF=${2:-0}
cd "$(dirname "$0")"
docker build -q -t tugport-dev .
docker rm -f "$NAME" >/dev/null 2>&1 || true
docker run -d --name "$NAME" --hostname "$NAME" --privileged \
  -v "$(cd .. && pwd)":/src -w /src \
  -v tugport-dev-cargo:/usr/local/cargo/registry -v tugport-dev-target:/target -e CARGO_TARGET_DIR=/target \
  -v "$NAME-docker":/var/lib/docker \
  -p $((28080+OFF)):80 -p $((28443+OFF)):443 -p $((28484+OFF)):8484 \
  tugport-dev >/dev/null
echo "$NAME up: http://localhost:$((28484+OFF)) (panel), :$((28080+OFF)) http, :$((28443+OFF)) https"
