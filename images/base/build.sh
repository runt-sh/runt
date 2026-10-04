#!/usr/bin/env bash
# Build a runt base image: a container image flattened into a compressed erofs.
#
#   images/base/build.sh [CONTAINERFILE_DIR] [OUT_FILE]
#
# Uses rootless podman: build the image, export its flattened filesystem as a
# tar (ownership as seen inside the image), and let mkfs.erofs read the tar
# directly, so nothing is extracted to disk and no root is needed.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
CTX="${1:-$HERE}"
CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/runt"
OUT="${2:-$CACHE/images/base.erofs}"
TAG="localhost/runt-base:build"

podman build --quiet --pull=newer -t "$TAG" -f "$CTX/Containerfile" "$CTX" >/dev/null
cid="$(podman create --quiet "$TAG" /bin/true)"
trap 'podman rm --force "$cid" >/dev/null' EXIT

mkdir -p "$(dirname "$OUT")"
rm -f "$OUT.tmp"
podman export "$cid" | mkfs.erofs --quiet --tar=f -zlz4hc -T0 "$OUT.tmp"
mv "$OUT.tmp" "$OUT"
echo "base image -> $OUT ($(du -h "$OUT" | cut -f1))"
