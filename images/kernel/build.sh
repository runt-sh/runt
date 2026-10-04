#!/usr/bin/env bash
# Build the runt guest kernel (x86_64 ELF vmlinux) from tinyconfig + runt.config.
#
#   images/kernel/build.sh [OUT_DIR]
#
# Env: KERNEL_VERSION (default below), BUILD_DIR (default ~/.cache/runt/build)
set -euo pipefail

KERNEL_VERSION="${KERNEL_VERSION:-6.18.55}"
HERE="$(cd "$(dirname "$0")" && pwd)"
CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/runt"
BUILD_DIR="${BUILD_DIR:-$CACHE/build}"
OUT_DIR="${1:-$CACHE}"
SRC="$BUILD_DIR/linux-$KERNEL_VERSION"
TARBALL="$BUILD_DIR/linux-$KERNEL_VERSION.tar.xz"

mkdir -p "$BUILD_DIR" "$OUT_DIR"

if [ ! -d "$SRC" ]; then
  if [ ! -f "$TARBALL" ]; then
    major="${KERNEL_VERSION%%.*}"
    curl -fL -o "$TARBALL" \
      "https://cdn.kernel.org/pub/linux/kernel/v${major}.x/linux-$KERNEL_VERSION.tar.xz"
  fi
  tar -C "$BUILD_DIR" -xf "$TARBALL"
fi

cd "$SRC"
make -s tinyconfig
./scripts/kconfig/merge_config.sh -m .config "$HERE/runt.config" >/dev/null
make -s olddefconfig

# Fail loudly if kconfig silently dropped something we asked for.
missing=0
while IFS= read -r line; do
  case "$line" in CONFIG_*=y) ;; *) continue ;; esac
  if ! grep -qx "$line" .config; then
    echo "warning: not set in final config: $line" >&2
    missing=1
  fi
done <"$HERE/runt.config"

make -s -j"$(nproc)" vmlinux
cp vmlinux "$OUT_DIR/vmlinux"
cp .config "$OUT_DIR/vmlinux.config"
echo "kernel $KERNEL_VERSION -> $OUT_DIR/vmlinux ($(du -h "$OUT_DIR/vmlinux" | cut -f1))"
[ "$missing" = 0 ] || echo "note: some requested options were not applied (see warnings)" >&2
