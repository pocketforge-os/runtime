#ifndef PF_UINPUT_DOMAIN_GUARD_H
#define PF_UINPUT_DOMAIN_GUARD_H

#include <ctype.h>
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef O_CLOEXEC
#define O_CLOEXEC 02000000
#endif
#ifndef O_NOFOLLOW
#define O_NOFOLLOW 0400000
#endif

#define PF_UINPUT_REFUSAL_REASON "unsafe_uinput_domain"
#define PF_UINPUT_ALLOW_REASON "safe_uinput_domain"
#define PF_UINPUT_BUILDER_MARKER "/etc/pocketforge/ephemeral-runner.conf"
#define PF_UINPUT_PROBE_TIMEOUT_MS 2000

enum pf_uinput_check { PF_CHECK_NO, PF_CHECK_YES, PF_CHECK_ERROR };
enum pf_uinput_state { PF_STATE_MISSING, PF_STATE_OK, PF_STATE_INVALID, PF_STATE_ERROR };
enum pf_uinput_logind { PF_LOGIND_NOT_PROBED, PF_LOGIND_NO_SEAT, PF_LOGIND_SEAT, PF_LOGIND_UNREADABLE };
enum pf_uinput_domain {
    PF_DOMAIN_PFVD_GUEST, PF_DOMAIN_DUT, PF_DOMAIN_PF_BUILDER_VM,
    PF_DOMAIN_CONTAINER, PF_DOMAIN_VM_WITH_SEAT, PF_DOMAIN_UNTRUSTED_VM,
    PF_DOMAIN_DUT_MISMATCH, PF_DOMAIN_BARE_METAL, PF_DOMAIN_PROBE_ERROR
};

struct pf_uinput_probe {
    enum pf_uinput_check container;
    enum pf_uinput_check vm;
    enum pf_uinput_state dt;
    int current_compatible;
    int legacy_compatible;
    int dt_nonempty;
    enum pf_uinput_state marker;
    enum pf_uinput_logind logind;
};

struct pf_uinput_verdict { int allowed; const char *reason; enum pf_uinput_domain domain; const char *probe; };

static const char *pf_uinput_domain_name(enum pf_uinput_domain domain) {
    static const char *names[] = { "pfvd-guest", "dut", "pf-builder-vm", "container",
        "vm-with-seat", "untrusted-vm", "dut-mismatch", "bare-metal", "probe-error" };
    return names[(int)domain];
}

static struct pf_uinput_verdict pf_uinput_make_verdict(int allowed, enum pf_uinput_domain domain, const char *probe) {
    struct pf_uinput_verdict out = { allowed, allowed ? PF_UINPUT_ALLOW_REASON : PF_UINPUT_REFUSAL_REASON, domain, probe };
    return out;
}

static struct pf_uinput_verdict pf_uinput_domain_evaluate(const struct pf_uinput_probe *p) {
    if (p->container == PF_CHECK_ERROR) return pf_uinput_make_verdict(0, PF_DOMAIN_PROBE_ERROR, "systemd-detect-virt");
    if (p->container == PF_CHECK_YES) return pf_uinput_make_verdict(0, PF_DOMAIN_CONTAINER, NULL);
    if (p->vm == PF_CHECK_ERROR) return pf_uinput_make_verdict(0, PF_DOMAIN_PROBE_ERROR, "systemd-detect-virt");
    if (p->dt == PF_STATE_ERROR) return pf_uinput_make_verdict(0, PF_DOMAIN_PROBE_ERROR, "device-tree-compatible");
    if (p->vm == PF_CHECK_YES && p->current_compatible) return pf_uinput_make_verdict(1, PF_DOMAIN_PFVD_GUEST, NULL);
    if (p->vm == PF_CHECK_NO && (p->current_compatible || p->legacy_compatible)) return pf_uinput_make_verdict(1, PF_DOMAIN_DUT, NULL);
    if (p->vm == PF_CHECK_YES) {
        if (p->marker == PF_STATE_ERROR) return pf_uinput_make_verdict(0, PF_DOMAIN_PROBE_ERROR, "builder-marker");
        if (p->marker != PF_STATE_OK) return pf_uinput_make_verdict(0, PF_DOMAIN_UNTRUSTED_VM, NULL);
        if (p->logind == PF_LOGIND_NO_SEAT) return pf_uinput_make_verdict(1, PF_DOMAIN_PF_BUILDER_VM, NULL);
        if (p->logind == PF_LOGIND_SEAT) return pf_uinput_make_verdict(0, PF_DOMAIN_VM_WITH_SEAT, NULL);
        return pf_uinput_make_verdict(0, PF_DOMAIN_PROBE_ERROR, "loginctl");
    }
    return pf_uinput_make_verdict(0, p->dt == PF_STATE_OK && p->dt_nonempty ? PF_DOMAIN_DUT_MISMATCH : PF_DOMAIN_BARE_METAL, NULL);
}

/* Some strict C11 translation units hide these POSIX declarations. */
extern int kill(pid_t pid, int sig);
extern int usleep(unsigned int usec);

static int pf_uinput_run_capture(char *const argv[], char *output, size_t capacity, int timeout_ms) {
    int pipefd[2];
    if (pipe(pipefd) < 0) return -1;
    pid_t pid = fork();
    if (pid < 0) { close(pipefd[0]); close(pipefd[1]); return -1; }
    if (pid == 0) {
        int nullfd = open("/dev/null", O_RDWR);
        close(pipefd[0]);
        if (nullfd >= 0) { dup2(nullfd, 0); dup2(nullfd, 2); if (nullfd > 2) close(nullfd); }
        dup2(pipefd[1], 1);
        if (pipefd[1] > 2) close(pipefd[1]);
        execvp(argv[0], argv);
        _exit(127);
    }
    close(pipefd[1]);
    int flags = fcntl(pipefd[0], F_GETFL, 0);
    if (flags < 0 || fcntl(pipefd[0], F_SETFL, flags | O_NONBLOCK) < 0) {
        kill(pid, SIGKILL); waitpid(pid, NULL, 0); close(pipefd[0]); return -1;
    }
    size_t used = 0;
    int overflow = 0, status = 0;
    int steps = timeout_ms / 10 + 1;
    for (int step = 0; step < steps; ++step) {
        for (;;) {
            char discard[256];
            char *target = used + 1 < capacity ? output + used : discard;
            size_t room = used + 1 < capacity ? capacity - used - 1 : sizeof discard;
            ssize_t got = read(pipefd[0], target, room);
            if (got > 0) {
                if (target == discard) overflow = 1;
                else used += (size_t)got;
                continue;
            }
            if (got < 0 && errno != EAGAIN && errno != EWOULDBLOCK) overflow = 1;
            break;
        }
        pid_t waited = waitpid(pid, &status, WNOHANG);
        if (waited == pid) {
            for (;;) {
                char discard[256];
                char *target = used + 1 < capacity ? output + used : discard;
                size_t room = used + 1 < capacity ? capacity - used - 1 : sizeof discard;
                ssize_t got = read(pipefd[0], target, room);
                if (got <= 0) break;
                if (target == discard) overflow = 1;
                else used += (size_t)got;
            }
            close(pipefd[0]);
            if (capacity) output[used < capacity ? used : capacity - 1] = 0;
            return !overflow && WIFEXITED(status) ? WEXITSTATUS(status) : -1;
        }
        if (waited < 0) { close(pipefd[0]); return -1; }
        usleep(10000);
    }
    kill(pid, SIGKILL);
    waitpid(pid, NULL, 0);
    close(pipefd[0]);
    errno = ETIMEDOUT;
    return -1;
}

static enum pf_uinput_check pf_uinput_command_check_program(const char *program, const char *kind, int timeout_ms) {
    char *argv[] = { (char *)program, (char *)kind, (char *)"--quiet", NULL };
    char output[1];
    int code = pf_uinput_run_capture(argv, output, sizeof output, timeout_ms);
    return code == 0 ? PF_CHECK_YES : code == 1 ? PF_CHECK_NO : PF_CHECK_ERROR;
}

static enum pf_uinput_check pf_uinput_command_check(const char *kind) {
    return pf_uinput_command_check_program("systemd-detect-virt", kind, PF_UINPUT_PROBE_TIMEOUT_MS);
}

static void pf_uinput_probe_dt(struct pf_uinput_probe *p) {
    FILE *f = fopen("/proc/device-tree/compatible", "rb");
    if (!f) { p->dt = errno == ENOENT ? PF_STATE_MISSING : PF_STATE_ERROR; return; }
    char buf[4096];
    size_t n = fread(buf, 1, sizeof buf, f);
    if (ferror(f) || (!feof(f) && n == sizeof buf)) { p->dt = PF_STATE_ERROR; fclose(f); return; }
    fclose(f);
    p->dt = PF_STATE_OK;
    int tsp = 0, a133 = 0, a100 = 0, old_a133 = 0, old_arm = 0;
    for (size_t off = 0; off < n;) {
        size_t left = n - off, len = 0;
        while (len < left && buf[off + len] != '\0') len++;
        if (len == left) { p->dt = PF_STATE_ERROR; return; }
        if (len) {
            p->dt_nonempty = 1;
            tsp |= strcmp(buf + off, "pocketforge,tsp") == 0;
            a133 |= strcmp(buf + off, "allwinner,sun50i-a133") == 0;
            a100 |= strcmp(buf + off, "allwinner,sun50i-a100") == 0;
            old_a133 |= strcmp(buf + off, "allwinner,a133") == 0;
            old_arm |= strcmp(buf + off, "arm,sun50iw10p1") == 0;
        }
        off += len + 1;
    }
    p->current_compatible = tsp && a133 && a100;
    p->legacy_compatible = old_a133 && old_arm;
}

static enum pf_uinput_state pf_uinput_probe_marker_path(const char *path) {
    struct stat st;
    int fd = open(path, O_RDONLY | O_CLOEXEC | O_NOFOLLOW);
    if (fd < 0) return errno == ENOENT ? PF_STATE_MISSING : errno == ELOOP ? PF_STATE_INVALID : PF_STATE_ERROR;
    if (fstat(fd, &st) < 0) { close(fd); return PF_STATE_ERROR; }
    if (!S_ISREG(st.st_mode) || st.st_uid != 0 || (st.st_mode & 0022)) { close(fd); return PF_STATE_INVALID; }
    close(fd);
    return PF_STATE_OK;
}

static enum pf_uinput_state pf_uinput_probe_marker(void) {
    return pf_uinput_probe_marker_path(PF_UINPUT_BUILDER_MARKER);
}

static int pf_uinput_safe_session_id(const char *id) {
    if (!*id) return 0;
    for (; *id; ++id) if (!isalnum((unsigned char)*id) && *id != '_' && *id != '-' && *id != '.') return 0;
    return 1;
}

static enum pf_uinput_logind pf_uinput_probe_logind(void) {
    char listing[4096];
    char *list_argv[] = { (char *)"loginctl", (char *)"list-sessions", (char *)"--no-legend", (char *)"--no-pager", NULL };
    if (pf_uinput_run_capture(list_argv, listing, sizeof listing, PF_UINPUT_PROBE_TIMEOUT_MS) != 0)
        return PF_LOGIND_UNREADABLE;
    char id[128], *list_save = NULL;
    enum pf_uinput_logind result = PF_LOGIND_NO_SEAT;
    for (char *line = strtok_r(listing, "\r\n", &list_save); line; line = strtok_r(NULL, "\r\n", &list_save)) {
        if (!*line) continue;
        if (sscanf(line, "%127s", id) != 1 || !pf_uinput_safe_session_id(id)) { result = PF_LOGIND_UNREADABLE; break; }
        char shown[2048];
        char *show_argv[] = { (char *)"loginctl", (char *)"show-session", id, (char *)"--no-pager",
            (char *)"-p", (char *)"Type", (char *)"-p", (char *)"Seat", NULL };
        if (pf_uinput_run_capture(show_argv, shown, sizeof shown, PF_UINPUT_PROBE_TIMEOUT_MS) != 0) {
            result = PF_LOGIND_UNREADABLE; break;
        }
        char seat[128] = "";
        int type_count = 0, seat_count = 0;
        char *show_save = NULL;
        for (char *property = strtok_r(shown, "\r\n", &show_save); property;
             property = strtok_r(NULL, "\r\n", &show_save)) {
            if (!strncmp(property, "Type=", 5)) { type_count++; }
            else if (!strncmp(property, "Seat=", 5)) { snprintf(seat, sizeof seat, "%s", property + 5); seat_count++; }
            else { result = PF_LOGIND_UNREADABLE; }
        }
        if (type_count != 1 || seat_count != 1 || result == PF_LOGIND_UNREADABLE) { result = PF_LOGIND_UNREADABLE; break; }
        if (*seat) { result = PF_LOGIND_SEAT; break; }
    }
    return result;
}

static struct pf_uinput_probe pf_uinput_domain_probe(void) {
    struct pf_uinput_probe p;
    memset(&p, 0, sizeof p);
    p.container = pf_uinput_command_check("--container");
    p.vm = p.container == PF_CHECK_NO ? pf_uinput_command_check("--vm") : PF_CHECK_NO;
    pf_uinput_probe_dt(&p);
    p.marker = p.vm == PF_CHECK_YES ? pf_uinput_probe_marker() : PF_STATE_MISSING;
    p.logind = p.vm == PF_CHECK_YES && p.marker == PF_STATE_OK ? pf_uinput_probe_logind() : PF_LOGIND_NOT_PROBED;
    return p;
}

typedef int (*pf_uinput_open_fn)(const char *, int, void *);

static int pf_uinput_guarded_open_probe(const struct pf_uinput_probe *p, const char *path, int flags,
                                        pf_uinput_open_fn opener, void *ctx,
                                        struct pf_uinput_verdict *reported) {
    struct pf_uinput_verdict v = pf_uinput_domain_evaluate(p);
    if (reported) *reported = v;
    if (!v.allowed) { errno = EPERM; return -1; }
    return opener(path, flags, ctx);
}

static inline int pf_uinput_guarded_open(const char *path, int flags, pf_uinput_open_fn opener, void *ctx,
                                         struct pf_uinput_verdict *reported) {
    struct pf_uinput_probe p = pf_uinput_domain_probe();
    return pf_uinput_guarded_open_probe(&p, path, flags, opener, ctx, reported);
}

static inline int pf_uinput_real_open(const char *path, int flags, void *ctx) {
    (void)ctx;
    return open(path, flags);
}

#endif
