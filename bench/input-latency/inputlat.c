// inputlat.c — on-silicon latency of the EVIOCGRAB->uinput re-emit input path (tsp-e1b.7).
//
// THE QUESTION (tsp-e1b.6 / R-B): the v0 INPUT broker enforces by EVIOCGRAB-ing the real
// evdev source and re-emitting a remapped stream via a uinput device the app reads. R-B put
// the per-event cost of that interposition at ~0.15 ms/event. This bench measures that number
// on REAL A133 silicon so tsp-e1b.6's hardware gate is answered with a figure, not an estimate.
//
// It is a SELF-CONTAINED static binary that mirrors pf-input-broker's grab->re-emit HOT LOOP in
// C, so it runs on stock CrossMix with nothing installed but /dev/uinput (UINPUT/EVDEV are =y on
// the vendor 4.9 kernel). It does NOT run the Rust daemon (whose *functional* enforcement was
// already proven under qemu-tsp in tsp-e1b.6); it exercises the IDENTICAL kernel primitives
// (uinput source + EVIOCGRAB + uinput re-emit) that the silicon/driver latency actually depends
// on. Topology (one process, one fork):
//
//     app injects BTN_SOUTH  -->  [SOURCE uinput dev] --(kernel)--> broker EVIOCGRABs source
//                                                                        |  passthrough write
//     app times arrival      <--  [RE-EMIT uinput dev] <--(kernel)-- broker uinput re-emit
//
// The app writes a state-changing key event into the SOURCE and blocks reading the RE-EMIT node;
// the forked broker reads it off the grabbed source and writes it to the re-emit device; the
// app's (t_arrive - t_inject) IS the broker interposition latency per event. Alternating
// press/release each iteration guarantees the kernel delivers every event (no dup-drop).
//
// Root required (uinput + reading the event nodes). Two passes, mirroring bench.c's JSON shape:
//   reemit_burst   — inject back-to-back (synchronous), warm path latency at max rate.
//   reemit_60hz    — inject paced to ~16.667 ms/event, latency + p99 jitter at frame cadence.
//
// Build: aarch64 musl-static (see ../Makefile).  Run: ./inputlat [iters]  (default 20000).
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <poll.h>
#include <sys/ioctl.h>
#include <sys/wait.h>
#include <linux/input.h>
#include <linux/uinput.h>

static const int KEYS[] = { BTN_SOUTH, BTN_EAST, BTN_WEST, BTN_NORTH };

static inline uint64_t now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}

static int cmp_u64(const void *a, const void *b) {
    uint64_t x = *(const uint64_t *)a, y = *(const uint64_t *)b;
    return x < y ? -1 : x > y ? 1 : 0;
}
static uint64_t pct(const uint64_t *s, size_t n, double p) {
    if (n == 0) return 0;
    size_t idx = (size_t)(p / 100.0 * (double)(n - 1) + 0.5);
    if (idx >= n) idx = n - 1;
    return s[idx];
}
static void stats(const char *name, uint64_t *lat, size_t n, int last) {
    qsort(lat, n, sizeof *lat, cmp_u64);
    double sum = 0;
    for (size_t i = 0; i < n; i++) sum += (double)lat[i];
    double mean = sum / (double)n;
    fprintf(stderr, "[%s] n=%zu mean=%.0f p50=%llu p95=%llu p99=%llu p999=%llu max=%llu (ns)\n",
            name, n, mean,
            (unsigned long long)pct(lat, n, 50.0), (unsigned long long)pct(lat, n, 95.0),
            (unsigned long long)pct(lat, n, 99.0), (unsigned long long)pct(lat, n, 99.9),
            (unsigned long long)lat[n - 1]);
    printf("  {\"name\":\"%s\",\"n\":%zu,\"mean_ns\":%.1f,\"p50_ns\":%llu,\"p95_ns\":%llu,"
           "\"p99_ns\":%llu,\"p999_ns\":%llu,\"max_ns\":%llu}%s\n",
           name, n, mean,
           (unsigned long long)pct(lat, n, 50.0), (unsigned long long)pct(lat, n, 95.0),
           (unsigned long long)pct(lat, n, 99.0), (unsigned long long)pct(lat, n, 99.9),
           (unsigned long long)lat[n - 1], last ? "" : ",");
}

// Create a uinput EV_KEY device carrying KEYS; return the uinput write-fd, fill evnode with the
// /dev/input/eventN the kernel materializes for it.
static int make_uinput(const char *name, char *evnode, size_t evnode_sz) {
    int fd = open("/dev/uinput", O_RDWR);
    if (fd < 0) { perror("open /dev/uinput"); return -1; }
    if (ioctl(fd, UI_SET_EVBIT, EV_KEY) < 0) { perror("UI_SET_EVBIT EV_KEY"); return -1; }
    if (ioctl(fd, UI_SET_EVBIT, EV_SYN) < 0) { perror("UI_SET_EVBIT EV_SYN"); return -1; }
    for (size_t i = 0; i < sizeof KEYS / sizeof KEYS[0]; i++)
        if (ioctl(fd, UI_SET_KEYBIT, KEYS[i]) < 0) { perror("UI_SET_KEYBIT"); return -1; }
    struct uinput_setup us;
    memset(&us, 0, sizeof us);
    us.id.bustype = BUS_USB; us.id.vendor = 0x1209; us.id.product = 0xe1b7;
    snprintf(us.name, sizeof us.name, "%s", name);
    if (ioctl(fd, UI_DEV_SETUP, &us) < 0) { perror("UI_DEV_SETUP"); return -1; }
    if (ioctl(fd, UI_DEV_CREATE) < 0) { perror("UI_DEV_CREATE"); return -1; }

    // Resolve the event node via UI_GET_SYSNAME ("inputN") -> /sys/.../inputN/eventM.
    char sysname[64] = {0};
    if (ioctl(fd, UI_GET_SYSNAME(sizeof sysname), sysname) < 0) { perror("UI_GET_SYSNAME"); return -1; }
    char sysdir[256];
    snprintf(sysdir, sizeof sysdir, "/sys/devices/virtual/input/%s", sysname);
    evnode[0] = '\0';
    for (int tries = 0; tries < 200 && evnode[0] == '\0'; tries++) {
        DIR *d = opendir(sysdir);
        if (d) {
            struct dirent *e;
            while ((e = readdir(d))) {
                if (strncmp(e->d_name, "event", 5) == 0) {
                    snprintf(evnode, evnode_sz, "/dev/input/%.100s", e->d_name);
                    break;
                }
            }
            closedir(d);
        }
        if (evnode[0] == '\0') { struct timespec s = {0, 5 * 1000000L}; nanosleep(&s, NULL); }
    }
    if (evnode[0] == '\0') { fprintf(stderr, "make_uinput: no event node under %s\n", sysdir); return -1; }
    // Let the node settle + become readable.
    for (int tries = 0; tries < 200; tries++) {
        if (access(evnode, R_OK) == 0) break;
        struct timespec s = {0, 5 * 1000000L}; nanosleep(&s, NULL);
    }
    return fd;
}

static void emit(int fd, uint16_t type, uint16_t code, int32_t value) {
    struct input_event ev;
    memset(&ev, 0, sizeof ev);
    ev.type = type; ev.code = code; ev.value = value;
    if (write(fd, &ev, sizeof ev) != (ssize_t)sizeof ev) { perror("uinput write"); }
}

// The forked broker: grab the source, re-emit its events verbatim via a fresh uinput device,
// report the re-emit node to the parent over `wpipe`, then pump forever.
static void run_broker(const char *source_node, int wpipe) {
    int src = open(source_node, O_RDONLY);
    if (src < 0) { perror("broker: open source"); _exit(20); }
    if (ioctl(src, EVIOCGRAB, 1) < 0) { perror("broker: EVIOCGRAB"); _exit(21); }
    char reemit_node[128];
    int re = make_uinput("pf-inputlat-reemit", reemit_node, sizeof reemit_node);
    if (re < 0) { _exit(22); }
    // Report the re-emit node (newline-terminated) to the parent.
    dprintf(wpipe, "%s\n", reemit_node);
    close(wpipe);
    // Hot loop: read grabbed source events, write them to the re-emit device unchanged.
    struct input_event ev[64];
    for (;;) {
        ssize_t n = read(src, ev, sizeof ev);
        if (n <= 0) { if (n < 0 && errno == EINTR) continue; break; }
        int k = (int)(n / (ssize_t)sizeof(struct input_event));
        for (int i = 0; i < k; i++) {
            if (write(re, &ev[i], sizeof ev[i]) != (ssize_t)sizeof ev[i]) { /* keep going */ }
        }
    }
    _exit(0);
}

// The app side: inject state-changing key events into the source, time their arrival on re-emit.
// pace_ns == 0 => back-to-back (burst); else sleep to that per-event cadence.
static void measure(const char *name, int source_fd, int reemit_fd,
                    size_t iters, size_t warmup, uint64_t pace_ns, int last) {
    uint64_t *lat = malloc(iters * sizeof *lat);
    if (!lat) { perror("malloc"); exit(2); }
    size_t got = 0, dropped = 0;
    int key = KEYS[0];
    struct input_event rb[64];
    struct pollfd pf = { .fd = reemit_fd, .events = POLLIN };
    for (size_t i = 0; i < warmup + iters; i++) {
        // Alternate press/release, PRESS FIRST: a key event whose value equals the current key
        // state is dropped by the kernel input core, so iteration 0 must be a press (fresh device
        // = released) or the matching re-emit never arrives.
        int32_t value = (int32_t)((i & 1) ^ 1);
        uint64_t t0 = now_ns();
        emit(source_fd, EV_KEY, (uint16_t)key, value);
        emit(source_fd, EV_SYN, SYN_REPORT, 0);
        // Wait (bounded) for the matching EV_KEY re-emit to arrive.
        uint64_t arrive = 0;
        for (;;) {
            int pr = poll(&pf, 1, 200);              // 200 ms ceiling — a dropped event can't hang the bench
            if (pr <= 0) break;                      // timeout/err => count as dropped, move on
            ssize_t n = read(reemit_fd, rb, sizeof rb);
            if (n <= 0) { if (n < 0 && errno == EINTR) continue; break; }
            uint64_t t1 = now_ns();
            int k = (int)(n / (ssize_t)sizeof(struct input_event));
            for (int j = 0; j < k; j++)
                if (rb[j].type == EV_KEY && rb[j].code == key) { arrive = t1; break; }
            if (arrive) break;
        }
        if (i >= warmup) {
            if (arrive) lat[got++] = arrive - t0;
            else dropped++;
        }
        if (pace_ns) {
            uint64_t spent = now_ns() - t0;
            if (spent < pace_ns) {
                struct timespec s = { (time_t)((pace_ns - spent) / 1000000000ull),
                                      (long)((pace_ns - spent) % 1000000000ull) };
                nanosleep(&s, NULL);
            }
        }
    }
    if (dropped) fprintf(stderr, "[%s] WARN dropped=%zu of %zu (no re-emit within 200ms)\n", name, dropped, iters);
    if (got == 0) {
        fprintf(stderr, "[%s] FATAL no events made the round trip\n", name);
        printf("  {\"name\":\"%s\",\"n\":0,\"error\":\"no_events\"}%s\n", name, last ? "" : ",");
    } else {
        stats(name, lat, got, last);
    }
    free(lat);
}

int main(int argc, char **argv) {
    size_t iters = (argc > 1) ? (size_t)strtoul(argv[1], NULL, 10) : 20000;
    size_t warmup = iters / 20 + 500;

    char source_node[128];
    int source_fd = make_uinput("pf-inputlat-source", source_node, sizeof source_node);
    if (source_fd < 0) { fprintf(stderr, "FATAL: cannot create source uinput (need root + /dev/uinput)\n"); return 3; }
    fprintf(stderr, "source = %s\n", source_node);

    int pfd[2];
    if (pipe(pfd) != 0) { perror("pipe"); return 3; }
    pid_t pid = fork();
    if (pid < 0) { perror("fork"); return 3; }
    if (pid == 0) { close(pfd[0]); run_broker(source_node, pfd[1]); _exit(0); }
    close(pfd[1]);

    // Read the re-emit node the broker created.
    char reemit_node[128] = {0};
    { size_t off = 0; char c;
      while (off + 1 < sizeof reemit_node) { ssize_t r = read(pfd[0], &c, 1); if (r <= 0) break; if (c == '\n') break; reemit_node[off++] = c; }
      reemit_node[off] = '\0'; }
    close(pfd[0]);
    if (reemit_node[0] == '\0') { fprintf(stderr, "FATAL: broker did not report a re-emit node\n"); kill(pid, SIGKILL); return 3; }
    fprintf(stderr, "reemit = %s\n", reemit_node);

    int reemit_fd = open(reemit_node, O_RDONLY);
    if (reemit_fd < 0) { perror("open reemit"); kill(pid, SIGKILL); return 3; }

    fprintf(stderr, "inputlat: iters=%zu warmup=%zu (EVIOCGRAB source -> uinput re-emit)\n", iters, warmup);
    printf("{\n \"bench\":\"input-latency\",\"path\":\"EVIOCGRAB->uinput-reemit\",\"frame_budget_ns\":16666667,\"iters\":%zu,\n", iters);
    printf(" \"measurements\":[\n");
    measure("reemit_burst", source_fd, reemit_fd, iters, warmup, 0, 0);
    measure("reemit_60hz", source_fd, reemit_fd, iters, warmup, 16666667ull, 1);
    printf(" ]\n}\n");

    close(reemit_fd);
    kill(pid, SIGKILL);
    int st; waitpid(pid, &st, 0);
    ioctl(source_fd, UI_DEV_DESTROY);
    close(source_fd);
    return 0;
}
