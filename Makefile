# Developer entry points. VM assets go to $XDG_CACHE_HOME/runt (default ~/.cache/runt),
# which is where `runt` looks for them.

CACHE      ?= $(or $(XDG_CACHE_HOME),$(HOME)/.cache)/runt
AGENT_TGT  ?= x86_64-unknown-linux-musl
AGENT_BIN   = target/$(AGENT_TGT)/release/runt-agent

.PHONY: all build agent kernel initramfs image assets test test-vm lint fmt clean-assets

all: build assets

build:
	cargo build --release -p runt

agent:
	cargo build --release -p runt-agent --target $(AGENT_TGT)

kernel:
	images/kernel/build.sh $(CACHE)

# The initramfs needs the kernel tree (for gen_init_cpio), so build the kernel first.
initramfs: agent
	images/initramfs/build.sh $(AGENT_BIN) $(CACHE)/initramfs.cpio

image:
	images/base/build.sh images/base $(CACHE)/images/base.erofs

assets: kernel initramfs image

test:
	cargo test --workspace

# Boots real VMs: needs /dev/kvm, libkrun and `make assets`.
test-vm: build
	cargo test -p runt --release -- --ignored --test-threads=1

lint:
	cargo fmt --all --check
	cargo clippy --workspace --all-targets -- -D warnings
	cargo clippy -p runt-agent --target $(AGENT_TGT) --all-targets -- -D warnings

fmt:
	cargo fmt --all

clean-assets:
	rm -f $(CACHE)/vmlinux $(CACHE)/vmlinux.config $(CACHE)/initramfs.cpio $(CACHE)/images/base.erofs
