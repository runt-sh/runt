#!/usr/bin/env bash
# Build the runt initramfs: a cpio whose /init is runt-agent.
#
#   images/initramfs/build.sh AGENT_BINARY [OUT_FILE]
#
# Uses the kernel tree's usr/gen_init_cpio so device nodes can be created
# without root. Run images/kernel/build.sh first.
set -euo pipefail

AGENT="${1:?usage: build.sh AGENT_BINARY [OUT_FILE]}"
CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/runt"
OUT="${2:-$CACHE/initramfs.cpio}"
KERNEL_VERSION="${KERNEL_VERSION:-6.18.55}"
SRC="${BUILD_DIR:-$CACHE/build}/linux-$KERNEL_VERSION"
GEN="$SRC/usr/gen_init_cpio"

if [ ! -x "$GEN" ]; then
  make -s -C "$SRC" usr/gen_init_cpio
fi

spec="$(mktemp)"
trap 'rm -f "$spec"' EXIT
cat >"$spec" <<SPEC
dir /dev 0755 0 0
nod /dev/console 0600 0 0 c 5 1
dir /proc 0755 0 0
dir /sys 0755 0 0
dir /run 0755 0 0
dir /mnt 0755 0 0
file /init $(realpath "$AGENT") 0755 0 0
SPEC

mkdir -p "$(dirname "$OUT")"
"$GEN" "$spec" >"$OUT"
echo "initramfs -> $OUT ($(du -h "$OUT" | cut -f1))"
