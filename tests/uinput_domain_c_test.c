#define _GNU_SOURCE
#include "../guard/pf_uinput_domain_guard.h"

static int calls;
static int fake_open(const char *path, int flags, void *ctx) { (void)path; (void)flags; (void)ctx; calls++; return 71; }
static enum pf_uinput_check check(const char *s) { return !strcmp(s,"yes") ? PF_CHECK_YES : !strcmp(s,"no") ? PF_CHECK_NO : PF_CHECK_ERROR; }
static enum pf_uinput_state state(const char *s) { return !strcmp(s,"ok") || !strcmp(s,"valid") ? PF_STATE_OK : !strcmp(s,"missing") ? PF_STATE_MISSING : !strcmp(s,"invalid") ? PF_STATE_INVALID : PF_STATE_ERROR; }
static enum pf_uinput_logind logind(const char *s) { return !strcmp(s,"no-seat") ? PF_LOGIND_NO_SEAT : strstr(s,"-seat") ? PF_LOGIND_SEAT : !strcmp(s,"not-probed") ? PF_LOGIND_NOT_PROBED : PF_LOGIND_UNREADABLE; }

int main(int argc, char **argv) {
    if (argc != 2) return 2;
    FILE *f = fopen(argv[1], "r"); if (!f) return 2;
    char line[2048]; if (!fgets(line, sizeof line, f)) return 2;
    int rows = 0;
    while (fgets(line, sizeof line, f)) {
        char *field[10], *save = NULL, *p = strtok_r(line, "\t\r\n", &save);
        for (int i = 0; i < 10; i++) { if (!p) return 3; field[i] = p; p = strtok_r(NULL, "\t\r\n", &save); }
        struct pf_uinput_probe probe = { check(field[1]), check(field[2]), state(field[3]),
            strstr(field[4],"pocketforge,tsp") && strstr(field[4],"allwinner,sun50i-a133") && strstr(field[4],"allwinner,sun50i-a100"),
            strstr(field[4],"allwinner,a133") && strstr(field[4],"arm,sun50iw10p1"), strcmp(field[4],"-") != 0,
            state(field[5]), logind(field[6]) };
        struct pf_uinput_verdict got = pf_uinput_domain_evaluate(&probe);
        if (got.allowed != !strcmp(field[7],"allow") || strcmp(got.reason,field[8]) || strcmp(pf_uinput_domain_name(got.domain),field[9])) return 4;
        calls = 0;
        struct pf_uinput_verdict guarded;
        int fd = pf_uinput_guarded_open_probe(&probe, "mock", 0, fake_open, NULL, &guarded);
        if (got.allowed) { if (fd != 71 || calls != 1) return 5; }
        else { if (fd != -1 || errno != EPERM || calls != 0 || strcmp(guarded.reason, PF_UINPUT_REFUSAL_REASON)) return 6; }
        rows++;
    }
    fclose(f);
    printf("uinput-domain-c: PASS fixture_rows=%d opener=mocked\n", rows);
    return rows == 18 ? 0 : 7;
}
