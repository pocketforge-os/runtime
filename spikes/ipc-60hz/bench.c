// bench.c — SPIKE-1 (tsp-e1b.1): broker IPC overhead per input frame at 60 Hz.
//
// THE QUESTION (briefing §D.2 / infra-101 §1): if the broker serves input as a
// portal/Binder-style round-trip PER EVENT, does that blow the ~16.667 ms frame
// budget at 60 Hz? If yes, the hot input path CANNOT be call-per-sample and must
// collapse to a SHARED evdev/uinput fd handed into the app's namespace (R-B:
// uinput+EVIOCGRAB). This benchmark measures both costs on one host so the
// go/no-go is a number, not a guess.
//
// It measures, on the SAME host, back to back:
//   (A) PER-EVENT RPC: an AF_UNIX SOCK_STREAM round-trip with a length-prefixed
//       message (u32 len + payload, the wire shape .2 commits to) — client writes
//       a request, the broker (a forked peer) reads it and writes a reply, client
//       reads the reply. This is the cost of ONE broker hop per input event.
//   (B) SHARED-FD hot path: the app does read(fd, ev, 24) of one kernel
//       input_event from an always-full pipe (a writer peer keeps it fed) — the
//       per-event cost the uinput+EVIOCGRAB shared-fd path pays the APP. No reply,
//       no round-trip; the broker's write is off the app's critical path.
//
// Honesty: the protobuf encode/decode of a tiny message adds a few hundred ns of
// USERSPACE work on top of (A)'s syscall round-trip; (A) is the dominant,
// host-portable cost (2 context switches + ~4 syscalls). Run on an x86 build host
// this is a LOWER BOUND; the A133 4xA53 @<=2.0GHz number is the authoritative,
// HARDWARE-GATED figure (see RESULTS.md). We report the x86 number + an A53-scaled
// estimate so the design decision can be made off-device today.
//
// Build: cc -O2 -o bench bench.c   (see Makefile)
// Run:   ./bench [iters]           (default 200000; JSON to stdout, log to stderr)
#define _GNU_SOURCE
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <sys/socket.h>
#include <sys/wait.h>

#define REQ_PAYLOAD 24   // a kernel input_event is 24 bytes on 64-bit
#define RESP_PAYLOAD 8   // a small status/handle reply
#define EVENT_SZ 24      // shared-fd: one input_event per read()

static inline uint64_t now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}

static int read_full(int fd, void *buf, size_t n) {
    size_t got = 0;
    while (got < n) {
        ssize_t r = read(fd, (char *)buf + got, n - got);
        if (r > 0) got += (size_t)r;
        else if (r < 0 && errno == EINTR) continue;
        else return -1;
    }
    return 0;
}
static int write_full(int fd, const void *buf, size_t n) {
    size_t put = 0;
    while (put < n) {
        ssize_t w = write(fd, (const char *)buf + put, n - put);
        if (w > 0) put += (size_t)w;
        else if (w < 0 && errno == EINTR) continue;
        else return -1;
    }
    return 0;
}

static int cmp_u64(const void *a, const void *b) {
    uint64_t x = *(const uint64_t *)a, y = *(const uint64_t *)b;
    return x < y ? -1 : x > y ? 1 : 0;
}

// percentile from a SORTED array (nearest-rank)
static uint64_t pct(const uint64_t *s, size_t n, double p) {
    if (n == 0) return 0;
    size_t idx = (size_t)(p / 100.0 * (double)(n - 1) + 0.5);
    if (idx >= n) idx = n - 1;
    return s[idx];
}

static void stats(const char *name, uint64_t *lat, size_t n) {
    qsort(lat, n, sizeof *lat, cmp_u64);
    double sum = 0;
    for (size_t i = 0; i < n; i++) sum += (double)lat[i];
    double mean = sum / (double)n;
    fprintf(stderr, "[%s] n=%zu mean=%.0f p50=%llu p99=%llu p999=%llu max=%llu (ns)\n",
            name, n, mean,
            (unsigned long long)pct(lat, n, 50.0), (unsigned long long)pct(lat, n, 99.0),
            (unsigned long long)pct(lat, n, 99.9), (unsigned long long)lat[n - 1]);
    // JSON line per measurement (consumed by RESULTS.md / CI)
    printf("  {\"name\":\"%s\",\"n\":%zu,\"mean_ns\":%.1f,\"p50_ns\":%llu,"
           "\"p99_ns\":%llu,\"p999_ns\":%llu,\"max_ns\":%llu},\n",
           name, n, mean,
           (unsigned long long)pct(lat, n, 50.0), (unsigned long long)pct(lat, n, 99.0),
           (unsigned long long)pct(lat, n, 99.9), (unsigned long long)lat[n - 1]);
}

// (A) PER-EVENT RPC round-trip over AF_UNIX SOCK_STREAM, length-prefixed.
static void bench_rpc(size_t iters, size_t warmup) {
    int sv[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) != 0) { perror("socketpair"); exit(2); }
    pid_t pid = fork();
    if (pid < 0) { perror("fork"); exit(2); }
    if (pid == 0) {                 // ---- broker peer: read req, write resp, forever ----
        close(sv[0]);
        uint32_t len; char buf[64];
        for (;;) {
            if (read_full(sv[1], &len, 4) != 0) break;
            if (len > sizeof buf) break;
            if (read_full(sv[1], buf, len) != 0) break;
            uint32_t rlen = RESP_PAYLOAD; char resp[RESP_PAYLOAD] = {0};
            if (write_full(sv[1], &rlen, 4) != 0) break;
            if (write_full(sv[1], resp, RESP_PAYLOAD) != 0) break;
        }
        _exit(0);
    }
    close(sv[1]);                   // ---- client: time write(req)+read(resp) ----
    uint64_t *lat = malloc(iters * sizeof *lat);
    char req[REQ_PAYLOAD]; memset(req, 0xab, sizeof req);
    char rbuf[64];
    for (size_t i = 0; i < warmup + iters; i++) {
        uint64_t t0 = now_ns();
        uint32_t len = REQ_PAYLOAD;
        write_full(sv[0], &len, 4);
        write_full(sv[0], req, REQ_PAYLOAD);
        uint32_t rlen;
        read_full(sv[0], &rlen, 4);
        read_full(sv[0], rbuf, rlen);
        uint64_t dt = now_ns() - t0;
        if (i >= warmup) lat[i - warmup] = dt;
    }
    stats("rpc_roundtrip", lat, iters);
    free(lat);
    close(sv[0]);
    int st; waitpid(pid, &st, 0);
}

// (B) SHARED-FD hot path: time read() of one 24-byte event from an always-full pipe.
static void bench_sharedfd(size_t iters, size_t warmup) {
    int p[2];
    if (pipe(p) != 0) { perror("pipe"); exit(2); }
    pid_t pid = fork();
    if (pid < 0) { perror("fork"); exit(2); }
    if (pid == 0) {                 // ---- writer peer: keep the pipe fed ----
        close(p[0]);
        char ev[EVENT_SZ]; memset(ev, 0x5a, sizeof ev);
        for (;;) if (write_full(p[1], ev, EVENT_SZ) != 0) break;
        _exit(0);
    }
    close(p[1]);                    // ---- app: time each read() of one event ----
    uint64_t *lat = malloc(iters * sizeof *lat);
    char ev[EVENT_SZ];
    for (size_t i = 0; i < warmup + iters; i++) {
        uint64_t t0 = now_ns();
        read_full(p[0], ev, EVENT_SZ);
        uint64_t dt = now_ns() - t0;
        if (i >= warmup) lat[i - warmup] = dt;
    }
    stats("sharedfd_read", lat, iters);
    free(lat);
    close(p[0]);
    kill(pid, SIGKILL);
    int st; waitpid(pid, &st, 0);
}

int main(int argc, char **argv) {
    size_t iters = (argc > 1) ? (size_t)strtoul(argv[1], NULL, 10) : 200000;
    size_t warmup = iters / 20 + 1000;
    fprintf(stderr, "SPIKE-1 ipc-60hz: iters=%zu warmup=%zu (req=%dB resp=%dB event=%dB)\n",
            iters, warmup, REQ_PAYLOAD, RESP_PAYLOAD, EVENT_SZ);
    printf("{\n \"spike\":\"ipc-60hz\",\"frame_budget_ns\":16666667,\"iters\":%zu,\n", iters);
    printf(" \"measurements\":[\n");
    bench_rpc(iters, warmup);
    bench_sharedfd(iters, warmup);
    printf("  {\"_\":\"end\"}\n ]\n}\n");
    return 0;
}
