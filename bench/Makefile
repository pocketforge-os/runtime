# bench/Makefile — tsp-e1b.7: static aarch64 benches for on-silicon A133 latency numbers.
#
# Builds two fully-static aarch64 binaries that run on stock CrossMix (vendor glibc/BusyBox —
# assume nothing installed):
#   ipcbench   — the SPIKE-1 broker-IPC micro-benchmark (reuses ../spikes/ipc-60hz/bench.c, tsp-e1b.1)
#   inputlat   — the EVIOCGRAB->uinput re-emit input-path latency bench (tsp-e1b.7, mirrors tsp-e1b.6)
#
# Default CC is the bootlin aarch64 musl-static cross toolchain staged on modelmaker. Override
# CC_AARCH64 to point at any aarch64 gcc; pass STATIC=-static (default) for a no-dependency binary.
CC_AARCH64 ?= /mnt/pocketforge-tmp/pf-musl-toolchain/aarch64--musl--stable-2025.08-1/bin/aarch64-linux-gcc
STATIC     ?= -static
CFLAGS     ?= -O2 -Wall -Wextra -std=c11
IPC_SRC    := ../spikes/ipc-60hz/bench.c
INPUT_SRC  := input-latency/inputlat.c

.PHONY: all clean native
all: build/ipcbench build/inputlat

build/ipcbench: $(IPC_SRC)
	@mkdir -p build
	$(CC_AARCH64) $(CFLAGS) $(STATIC) -o $@ $<

build/inputlat: $(INPUT_SRC)
	@mkdir -p build
	$(CC_AARCH64) $(CFLAGS) $(STATIC) -o $@ $<

# native x86 build of inputlat for a quick local functional sanity run (root-required).
native: build/inputlat.native
build/inputlat.native: $(INPUT_SRC)
	@mkdir -p build
	cc $(CFLAGS) -o $@ $<

clean:
	rm -rf build
