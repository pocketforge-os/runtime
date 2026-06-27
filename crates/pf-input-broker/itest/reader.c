// reader.c — minimal evdev reader for the v0 INPUT broker proof (the qemu-tsp leg).
//
// Compiled native (gcc) AND aarch64-static (aarch64-linux-gnu-gcc -static) and run under
// qemu-tsp, so the SAME enforcement is shown in the arm64 target environment the device ships.
// Reads the re-emit node for a window, prints "EV <type> <code> <value>" (SYN skipped), then —
// if a source node is given — reads it briefly and prints "SOURCE_EVENTS <n>", which MUST be 0
// while the broker holds EVIOCGRAB (the app cannot bypass to the raw device).
//
// Usage: reader <reemit-node> [<source-node>] [<ms>]
#include <fcntl.h>
#include <linux/input.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static long now_ms(void) {
  struct timespec t;
  clock_gettime(CLOCK_MONOTONIC, &t);
  return t.tv_sec * 1000L + t.tv_nsec / 1000000L;
}

static int read_window(const char *node, long ms, int print) {
  int fd = open(node, O_RDONLY | O_NONBLOCK);
  if (fd < 0) { printf("OPEN_FAIL %s\n", node); return -1; }
  struct input_event ev[64];
  long deadline = now_ms() + ms;
  int count = 0;
  while (now_ms() < deadline) {
    ssize_t n = read(fd, ev, sizeof ev);
    if (n > 0) {
      int k = n / (int)sizeof(struct input_event);
      for (int i = 0; i < k; i++) {
        if (ev[i].type == EV_SYN) continue;
        if (print) printf("EV %u %u %d\n", ev[i].type, ev[i].code, ev[i].value);
        count++;
      }
    } else {
      struct timespec s = {0, 10 * 1000000L};
      nanosleep(&s, NULL);
    }
  }
  close(fd);
  return count;
}

int main(int argc, char **argv) {
  if (argc < 2) { fprintf(stderr, "usage: reader <reemit-node> [<source-node>] [<ms>]\n"); return 2; }
  const char *reemit = argv[1];
  const char *source = (argc > 2 && argv[2][0] != '\0') ? argv[2] : NULL;
  long ms = (argc > 3) ? atol(argv[3]) : 1800;

  int got = read_window(reemit, ms, 1);
  if (got < 0) return 3;
  fprintf(stderr, "reader: read %d events from re-emit %s\n", got, reemit);

  if (source) {
    int src = read_window(source, 400, 0);
    if (src < 0) return 3;
    printf("SOURCE_EVENTS %d\n", src);
    fprintf(stderr, "reader: grabbed source delivered %d events (must be 0)\n", src);
  }
  fflush(stdout);
  return 0;
}
