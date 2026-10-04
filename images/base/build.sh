#!/usr/bin/env bash
# Build a runt base image: an OCI image flattened into a compressed erofs.
#
#   images/base/build.sh [IMAGE_REF] [OUT_FILE]
#
# Default IMAGE_REF is docker.io/library/debian:trixie-slim. Runs unprivileged:
# mkfs.erofs reads the layer tarball directly, so file ownership is kept
# without extracting anything to disk. Only single-layer images for now.
set -euo pipefail

REF="${1:-docker.io/library/debian:trixie-slim}"
CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/runt"
OUT="${2:-$CACHE/images/base.erofs}"
ARCH="${ARCH:-amd64}"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

skopeo copy --quiet --override-arch "$ARCH" --override-os linux \
  "docker://$REF" "oci:$work/oci:img"

layers=$(python3 - "$work/oci" <<'PY'
import json, sys, os
root = sys.argv[1]
def blob(d):
    alg, h = d.split(":")
    return os.path.join(root, "blobs", alg, h)
idx = json.load(open(os.path.join(root, "index.json")))
man = json.load(open(blob(idx["manifests"][0]["digest"])))
for l in man["layers"]:
    print(blob(l["digest"]))
PY
)

n=$(echo "$layers" | wc -l)
if [ "$n" -ne 1 ]; then
  echo "error: multi-layer images not supported yet ($n layers)" >&2
  exit 1
fi

mkdir -p "$(dirname "$OUT")"
rm -f "$OUT"
mkfs.erofs --quiet --tar=f --ungzip -zlz4hc -T0 "$OUT" $layers
echo "base image $REF -> $OUT ($(du -h "$OUT" | cut -f1))"
