# The guest half of `runt build`, run as root in a throwaway build VM.
#
#   prepare K      mount the base image (vda), the build VM's own disk (vdb)
#                  for scratch space, and K cached layers (/runt/layers/I.erofs)
#   run I CWD CMD [K=V...]
#                  run CMD in CWD, with the recipe's environment, on a root
#                  made of layers 0..I-1 over the base, and pack what it
#                  changed into /runt/io/I.erofs
#   copy I TO      extract /runt/io/I.tar into TO the same way
#
# Layer I's directory is /runt/build/l/I. Steps run chrooted, so the image
# never sees this VM's own tools or files. A failed step just exits: the
# build VM is thrown away.
set -eu
__b=/runt/build
__root=$__b/root
__io=/runt/io

# overlayfs lists lower layers top first. The paths are relative to $__b:
# mount(8) hands the list to the kernel as one fsconfig string, which may
# not exceed 256 bytes.
__lowers() {
    __l=base
    __j=0
    while [ "$__j" -lt "$1" ]; do
        __l=l/$__j:$__l
        __j=$((__j + 1))
    done
    echo "$__l"
}

__mount_root() {
    __u=$__b/disk/runt-build/$1
    rm -rf "$__u"
    mkdir -p "$__u/upper" "$__u/work" "$__root"
    # Only plain whiteouts and opaque directories in layers: no redirects,
    # metacopy or index, which would tie a layer to this mount.
    (cd "$__b" && mount -t overlay overlay -o "lowerdir=$(__lowers "$1"),upperdir=disk/runt-build/$1/upper,workdir=disk/runt-build/$1/work,redirect_dir=off,metacopy=off,index=off,xino=off" "$__root")
    mount -t proc proc "$__root/proc"
    mount -t sysfs sysfs "$__root/sys"
    mount --rbind /dev "$__root/dev"
    for __f in /etc/resolv.conf /etc/hosts; do
        if [ -f "$__root$__f" ] && [ ! -L "$__root$__f" ]; then
            mount --bind "$__f" "$__root$__f"
        fi
    done
}

__pack() {
    # Leftover processes (daemons a step started) would keep the root busy.
    for __p in /proc/[0-9]*; do
        if [ "$(readlink "$__p/root" 2>/dev/null)" = "$__root" ]; then
            kill -9 "${__p#/proc/}" 2>/dev/null || true
        fi
    done
    umount -R "$__root"
    __u=$__b/disk/runt-build/$1
    rm -rf "$__u/work"
    mkfs.erofs --quiet -zlz4hc "$__io/$1.erofs.tmp" "$__u/upper"
    sync "$__io/$1.erofs.tmp"
    mv "$__io/$1.erofs.tmp" "$__io/$1.erofs"
    ln -sfn "$__u/upper" "$__b/l/$1"
}

__cmd=$1
shift
case $__cmd in
prepare)
    mkdir -p "$__b/base" "$__b/disk" "$__b/l"
    mount -t erofs -o ro /dev/vda "$__b/base"
    mount /dev/vdb "$__b/disk"
    __i=0
    while [ "$__i" -lt "$1" ]; do
        mkdir -p "$__b/l/$__i"
        mount -t erofs -o ro "/runt/layers/$__i.erofs" "$__b/l/$__i"
        __i=$((__i + 1))
    done
    ;;
run)
    __i=$1 __cwd=$2 __run=$3
    shift 3
    __mount_root "$__i"
    chroot "$__root" /bin/sh -c '
        while [ "$1" != -- ]; do export "$1"; shift; done
        mkdir -p "$2" && cd "$2" && exec /bin/sh -c "$3"' sh "$@" -- "$__cwd" "$__run" </dev/null
    __pack "$__i"
    ;;
copy)
    __mount_root "$1"
    chroot "$__root" /bin/sh -c 'mkdir -p "$1" && tar -xf - -C "$1"' sh "$2" <"$__io/$1.tar"
    __pack "$1"
    ;;
*)
    echo "runt build: unknown command $__cmd" >&2
    exit 2
    ;;
esac
