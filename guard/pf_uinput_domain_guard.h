#ifndef PF_UINPUT_DOMAIN_GUARD_H
#define PF_UINPUT_DOMAIN_GUARD_H

#include <ctype.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef O_CLOEXEC
#define O_CLOEXEC 0
#endif

/* Some strict C11 translation units hide these POSIX declarations. */
extern FILE *popen(const char *command, const char *type);
extern int pclose(FILE *stream);

#define PF_UINPUT_REFUSAL_REASON "unsafe_uinput_domain"
#define PF_UINPUT_ALLOW_REASON "safe_uinput_domain"
#define PF_UINPUT_BUILDER_MARKER "/etc/pocketforge/ephemeral-runner.conf"

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

struct pf_uinput_verdict { int allowed; const char *reason; enum pf_uinput_domain domain; };

static const char *pf_uinput_domain_name(enum pf_uinput_domain domain) {
    static const char *names[] = { "pfvd-guest", "dut", "pf-builder-vm", "container",
        "vm-with-seat", "untrusted-vm", "dut-mismatch", "bare-metal", "probe-error" };
    return names[(int)domain];
}

static struct pf_uinput_verdict pf_uinput_make_verdict(int allowed, enum pf_uinput_domain domain) {
    struct pf_uinput_verdict out = { allowed, allowed ? PF_UINPUT_ALLOW_REASON : PF_UINPUT_REFUSAL_REASON, domain };
    return out;
}

static struct pf_uinput_verdict pf_uinput_domain_evaluate(const struct pf_uinput_probe *p) {
    if (p->container == PF_CHECK_ERROR) return pf_uinput_make_verdict(0, PF_DOMAIN_PROBE_ERROR);
    if (p->container == PF_CHECK_YES) return pf_uinput_make_verdict(0, PF_DOMAIN_CONTAINER);
    if (p->vm == PF_CHECK_ERROR || p->dt == PF_STATE_ERROR) return pf_uinput_make_verdict(0, PF_DOMAIN_PROBE_ERROR);
    if (p->vm == PF_CHECK_YES && p->current_compatible) return pf_uinput_make_verdict(1, PF_DOMAIN_PFVD_GUEST);
    if (p->vm == PF_CHECK_NO && (p->current_compatible || p->legacy_compatible)) return pf_uinput_make_verdict(1, PF_DOMAIN_DUT);
    if (p->vm == PF_CHECK_YES) {
        if (p->marker == PF_STATE_ERROR) return pf_uinput_make_verdict(0, PF_DOMAIN_PROBE_ERROR);
        if (p->marker != PF_STATE_OK) return pf_uinput_make_verdict(0, PF_DOMAIN_UNTRUSTED_VM);
        if (p->logind == PF_LOGIND_NO_SEAT) return pf_uinput_make_verdict(1, PF_DOMAIN_PF_BUILDER_VM);
        if (p->logind == PF_LOGIND_SEAT) return pf_uinput_make_verdict(0, PF_DOMAIN_VM_WITH_SEAT);
        return pf_uinput_make_verdict(0, PF_DOMAIN_PROBE_ERROR);
    }
    return pf_uinput_make_verdict(0, p->dt == PF_STATE_OK && p->dt_nonempty ? PF_DOMAIN_DUT_MISMATCH : PF_DOMAIN_BARE_METAL);
}

static enum pf_uinput_check pf_uinput_command_check(const char *kind) {
    pid_t pid = fork();
    if (pid < 0) return PF_CHECK_ERROR;
    if (pid == 0) {
        int nullfd = open("/dev/null", O_RDWR);
        if (nullfd >= 0) { dup2(nullfd, 0); dup2(nullfd, 1); dup2(nullfd, 2); if (nullfd > 2) close(nullfd); }
        execlp("systemd-detect-virt", "systemd-detect-virt", kind, "--quiet", (char *)NULL);
        _exit(127);
    }
    int status;
    if (waitpid(pid, &status, 0) != pid || !WIFEXITED(status)) return PF_CHECK_ERROR;
    int code = WEXITSTATUS(status);
    return code == 0 ? PF_CHECK_YES : code == 1 ? PF_CHECK_NO : PF_CHECK_ERROR;
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

static enum pf_uinput_state pf_uinput_probe_marker(void) {
    struct stat st;
    if (stat(PF_UINPUT_BUILDER_MARKER, &st) < 0) return errno == ENOENT ? PF_STATE_MISSING : PF_STATE_ERROR;
    if (!S_ISREG(st.st_mode) || st.st_uid != 0 || (st.st_mode & 0022)) return PF_STATE_INVALID;
    int fd = open(PF_UINPUT_BUILDER_MARKER, O_RDONLY | O_CLOEXEC);
    if (fd < 0) return PF_STATE_ERROR;
    close(fd);
    return PF_STATE_OK;
}

static int pf_uinput_safe_session_id(const char *id) {
    if (!*id) return 0;
    for (; *id; ++id) if (!isalnum((unsigned char)*id) && *id != '_' && *id != '-' && *id != '.') return 0;
    return 1;
}

static enum pf_uinput_logind pf_uinput_probe_logind(void) {
    FILE *list = popen("loginctl list-sessions --no-legend --no-pager", "r");
    if (!list) return PF_LOGIND_UNREADABLE;
    char line[1024], id[128];
    enum pf_uinput_logind result = PF_LOGIND_NO_SEAT;
    while (fgets(line, sizeof line, list)) {
        if (sscanf(line, "%127s", id) != 1 || !pf_uinput_safe_session_id(id)) { result = PF_LOGIND_UNREADABLE; break; }
        char command[256];
        snprintf(command, sizeof command, "loginctl show-session %s --no-pager -p Type -p Seat", id);
        FILE *show = popen(command, "r");
        if (!show) { result = PF_LOGIND_UNREADABLE; break; }
        char type[64] = "", seat[128] = "";
        int type_count = 0, seat_count = 0;
        while (fgets(line, sizeof line, show)) {
            line[strcspn(line, "\r\n")] = 0;
            if (!strncmp(line, "Type=", 5)) { snprintf(type, sizeof type, "%s", line + 5); type_count++; }
            else if (!strncmp(line, "Seat=", 5)) { snprintf(seat, sizeof seat, "%s", line + 5); seat_count++; }
            else { result = PF_LOGIND_UNREADABLE; }
        }
        int status = pclose(show);
        if (status != 0 || type_count != 1 || seat_count != 1 || result == PF_LOGIND_UNREADABLE) { result = PF_LOGIND_UNREADABLE; break; }
        if (*seat && (!strcmp(type, "x11") || !strcmp(type, "wayland") || !strcmp(type, "tty"))) { result = PF_LOGIND_SEAT; break; }
    }
    int status = pclose(list);
    return status == 0 ? result : PF_LOGIND_UNREADABLE;
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
