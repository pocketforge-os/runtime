# SPIKE-1 ipc-60hz micro-benchmark (tsp-e1b.1).
# Build + run on modelmaker (mm@10.0.40.90, the build host) — device-free.
CC      ?= cc
CFLAGS  ?= -O2 -Wall -Wextra -std=c11
ITERS   ?= 200000

build/bench: bench.c
	@mkdir -p build
	$(CC) $(CFLAGS) -o $@ $<

.PHONY: run clean
run: build/bench
	./build/bench $(ITERS) > build/results.json 2> build/results.log
	@cat build/results.log

clean:
	rm -rf build
