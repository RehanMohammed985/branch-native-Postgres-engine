#!/bin/sh
# Run a command inside the development container (Linux + FUSE + Postgres + Rust).
#   docker/dev.sh cargo test
set -e
here=$(cd "$(dirname "$0")/.." && pwd)
docker image inspect banyan-dev >/dev/null 2>&1 || docker build -q -t banyan-dev -f "$here/docker/dev.Dockerfile" "$here/docker"
exec docker run --rm ${DEV_TTY:+-it} --privileged \
  -v "$here":/src \
  -v banyan-cargo-registry:/opt/cargo/registry \
  -v banyan-target:/target \
  -e CARGO_TARGET_DIR=/target \
  ${DEV_ARGS:-} banyan-dev "$@"
