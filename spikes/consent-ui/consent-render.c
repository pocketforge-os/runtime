// consent-render.c — tsp-ht0p.1: SPIKE-2, consent-UI-on-gamepad prototype.
//
// Renders the supervisor-drawn permission consent dialog
// ("{APP} wants to use {RESOURCE} — [Deny] [Allow once] [Allow always]") to a
// virtual framebuffer, headless, on a GPU-less host. Dumps the framebuffer as
// P6 PPM for host-side PNG conversion + screen-reviewer verification.
//
// tsp-osr-safe: consumes the pinned recipe from pocketforge-os/sim @ 74ddfbc
// (sim/fb/README.md, sim/fb/fb-render.c). Two safe paths, both used:
//   (1) offscreen: memfd -> SDL_CreateSurfaceFrom -> SDL_CreateSoftwareRenderer
//       (no window, no GL, structurally safe from tsp-osr).
//   (2) pin recipe: non-OPENGL window + SDL_CreateRenderer(win, "software").
//       Called on startup so a regression that breaks the on-window recipe
//       (which the M1.D supervisor will use on-panel) is caught immediately.
//
// HONESTY: portable SDL3 software rasterizer. Proves layout/widget logic +
// renderer-creation recipe ONLY. NOT the on-device libSDL3-pocketforge sunxifb
// backend, NOT PowerVR/dc_sunxi/DE2.0/fb0, NOT real panel rotation. See
// DESIGN.md §6-§7 and sim/fb/README.md "Honesty".
//
// Usage:
//   consent-render --app-name "Weather" --resource "LOCATION"
//                  [--purpose "show nearby weather"]
//                  --focus deny|allow-once|allow-always
//                  --state initial|selected|cancelled
//                  --out state.ppm
//   Optional: --canvas WxH  (default 1280x720)
#define _GNU_SOURCE
#include <SDL3/SDL.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>
#ifdef __linux__
#include <sys/syscall.h>
#endif

#include "font8x13.h"

// ---------- virtual fb (memfd analog of the supervisor->app fb-handoff fd) ----------
static int make_fb(size_t bytes, void **mem) {
    int fd = -1;
#ifdef SYS_memfd_create
    fd = (int)syscall(SYS_memfd_create, "vfb", 0u);
#endif
    if (fd >= 0) {
        if (ftruncate(fd, (off_t)bytes) != 0) { close(fd); fd = -1; }
    }
    if (fd >= 0) {
        *mem = mmap(NULL, bytes, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        if (*mem == MAP_FAILED) { close(fd); fd = -1; }
    }
    if (fd < 0) { *mem = calloc(1, bytes); return -1; }
    return fd;
}

// XRGB8888 -> RGB24, endianness-agnostic.
static void put_rgb(unsigned char *out, const unsigned char *fb, int w, int h) {
    const Uint32 *px = (const Uint32 *)fb;
    for (int i = 0; i < w * h; i++) {
        out[i * 3 + 0] = (px[i] >> 16) & 0xff;
        out[i * 3 + 1] = (px[i] >> 8) & 0xff;
        out[i * 3 + 2] = px[i] & 0xff;
    }
}

static int write_ppm(const char *path, const unsigned char *rgb, int w, int h) {
    FILE *f = fopen(path, "wb");
    if (!f) { perror("fopen ppm"); return -1; }
    fprintf(f, "P6\n%d %d\n255\n", w, h);
    fwrite(rgb, 1, (size_t)w * h * 3, f);
    fclose(f);
    return 0;
}

// ---------- primitive drawing helpers (mirrors sim/skin/skin-render.c) ----------
static void fill(SDL_Renderer *r, int x, int y, int w, int h, int cr, int cg, int cb) {
    SDL_SetRenderDrawColor(r, cr, cg, cb, 255);
    SDL_FRect fr = {(float)x, (float)y, (float)w, (float)h};
    SDL_RenderFillRect(r, &fr);
}

static void outline_thick(SDL_Renderer *r, int x, int y, int w, int h, int t,
                          int cr, int cg, int cb) {
    for (int i = 0; i < t; i++) {
        SDL_SetRenderDrawColor(r, cr, cg, cb, 255);
        SDL_FRect e[4] = {
            {(float)(x + i), (float)(y + i), (float)(w - 2 * i), 1.0f},
            {(float)(x + i), (float)(y + h - 1 - i), (float)(w - 2 * i), 1.0f},
            {(float)(x + i), (float)(y + i), 1.0f, (float)(h - 2 * i)},
            {(float)(x + w - 1 - i), (float)(y + i), 1.0f, (float)(h - 2 * i)},
        };
        SDL_RenderRects(r, e, 4);
    }
}

static void draw_text(SDL_Renderer *r, int x, int y, int scale, const char *s,
                      int cr, int cg, int cb) {
    SDL_SetRenderDrawColor(r, cr, cg, cb, 255);
    int cx = x;
    for (; *s; s++) {
        unsigned char ch = (unsigned char)*s;
        if (ch < FONT_FIRST || ch > FONT_LAST) { cx += (FONT_W + 1) * scale; continue; }
        const unsigned char *g = FONT8X13[ch - FONT_FIRST];
        for (int row = 0; row < FONT_H; row++)
            for (int col = 0; col < FONT_W; col++)
                if (g[row] & (1 << (7 - col))) {
                    SDL_FRect px = {(float)(cx + col * scale), (float)(y + row * scale),
                                    (float)scale, (float)scale};
                    SDL_RenderFillRect(r, &px);
                }
        cx += (FONT_W + 1) * scale;
    }
}

static int text_width(const char *s, int scale) {
    int n = 0; while (*s++) n++;
    return n * (FONT_W + 1) * scale;
}

static void draw_text_centered(SDL_Renderer *r, int cx, int y, int scale, const char *s,
                               int cr, int cg, int cb) {
    int w = text_width(s, scale);
    draw_text(r, cx - w / 2, y, scale, s, cr, cg, cb);
}

// ---------- tsp-osr window-recipe pin (mirrors sim/fb/fb-render.c) ----------
static void pin_tsp_osr_recipe(int w, int h) {
    SDL_SetHint(SDL_HINT_RENDER_DRIVER, "software");
    SDL_Window *win = SDL_CreateWindow("consent-ui-osr-pin", w, h, 0);
    if (!win) {
        fprintf(stderr, "tsp-osr-pin: window create skipped (%s)\n", SDL_GetError());
        return;
    }
    SDL_Renderer *r = SDL_CreateRenderer(win, "software");
    if (!r) {
        fprintf(stderr, "tsp-osr-pin: FAIL renderer NULL (%s)\n", SDL_GetError());
        SDL_DestroyWindow(win);
        return;
    }
    const char *name = SDL_GetRendererName(r);
    fprintf(stderr, "tsp-osr-pin: OK window(no-GL)+SDL_CreateRenderer(\"software\") -> '%s'\n",
            name ? name : "?");
    SDL_DestroyRenderer(r);
    SDL_DestroyWindow(win);
}

// ---------- state machine (DESIGN.md §2) ----------
typedef enum {
    FOCUS_DENY = 0,
    FOCUS_ALLOW_ONCE = 1,
    FOCUS_ALLOW_ALWAYS = 2,
    FOCUS_COUNT = 3,
} Focus;

typedef enum {
    STATE_INITIAL = 0,   // dialog shown, focus indicator drawn, no commit yet
    STATE_SELECTED = 1,  // A pressed on focused button; button drawn "committed" (lit)
    STATE_CANCELLED = 2, // B pressed; dialog collapses to a Deny-equivalent selected state
} State;

static Focus parse_focus(const char *s) {
    if (!strcmp(s, "deny")) return FOCUS_DENY;
    if (!strcmp(s, "allow-once") || !strcmp(s, "allow_once")) return FOCUS_ALLOW_ONCE;
    if (!strcmp(s, "allow-always") || !strcmp(s, "allow_always")) return FOCUS_ALLOW_ALWAYS;
    fprintf(stderr, "unknown focus '%s' (expected deny|allow-once|allow-always)\n", s);
    exit(2);
}

static State parse_state(const char *s) {
    if (!strcmp(s, "initial")) return STATE_INITIAL;
    if (!strcmp(s, "selected")) return STATE_SELECTED;
    if (!strcmp(s, "cancelled")) return STATE_CANCELLED;
    fprintf(stderr, "unknown state '%s' (expected initial|selected|cancelled)\n", s);
    exit(2);
}

static const char *focus_label(Focus f) {
    switch (f) {
        case FOCUS_DENY: return "Deny";
        case FOCUS_ALLOW_ONCE: return "Allow once";
        case FOCUS_ALLOW_ALWAYS: return "Allow always";
        default: return "?";
    }
}

// ---------- the dialog layout (1280x720 landscape, matches screens[0].render_canvas) ----------
//
// Layout is fixed at 1280x720. Rendered in a landscape canvas; the supervisor
// composites it onto whatever fb0 space it owns. Numbers below chosen so the
// screen-reviewer VLM reads text reliably (3-5x scale of the 8x13 bitmap font).
//
// y-band       role
//   0-100    top pad + accent bar
// 120-200    title: "{APP} wants to use {RESOURCE}"
// 220-280    optional purpose line
// 380-500    3 buttons in a row, centered horizontally
// 620-660    hint line ("A: confirm    B: cancel")
static void draw_dialog(SDL_Renderer *r, int W, int H, const char *app_name,
                        const char *resource, const char *purpose,
                        Focus focus, State state) {
    // -- background --
    fill(r, 0, 0, W, H, 24, 24, 28);

    // -- accent bar at the top (signals "system dialog", differentiates from any app frame) --
    fill(r, 0, 0, W, 8, 90, 130, 210);

    // -- title: "{APP} wants to use {RESOURCE}" --
    // 5x scaled bitmap font. Format the string ourselves.
    char title[512];
    snprintf(title, sizeof(title), "%s wants to use %s", app_name, resource);
    draw_text_centered(r, W / 2, 120, 5, title, 235, 240, 250);

    // -- optional purpose line (2x smaller, dimmer color) --
    if (purpose && purpose[0]) {
        draw_text_centered(r, W / 2, 240, 3, purpose, 170, 178, 200);
    }

    // -- three buttons in a row, centered --
    const int BW = 300;         // button width
    const int BH = 120;         // button height
    const int GAP = 40;         // between buttons
    const int TOTAL = 3 * BW + 2 * GAP;   // = 980
    const int Y = 380;
    const int X0 = (W - TOTAL) / 2;       // = 150

    const char *labels[3] = {"Deny", "Allow once", "Allow always"};

    for (int i = 0; i < FOCUS_COUNT; i++) {
        int x = X0 + i * (BW + GAP);

        // classify this button's visual state
        int is_focus = (i == (int)focus);
        int is_committed = (state == STATE_SELECTED && is_focus);
        int is_deny = (i == FOCUS_DENY);
        int is_cancelled_deny = (state == STATE_CANCELLED && is_deny);

        // fill color: dim base; committed = bright; cancelled-collapsed-to-deny = bright red-ish
        int fr, fg, fb;
        if (is_committed) {
            fr = 210; fg = 220; fb = 235;              // bright — "you just committed this"
        } else if (is_cancelled_deny) {
            fr = 190; fg = 90;  fb = 90;               // red-tinted — cancel-collapsed-to-deny
        } else if (is_focus) {
            fr = 68;  fg = 74;  fb = 96;               // slightly-brighter than unfocused
        } else {
            fr = 44;  fg = 48;  fb = 62;               // dim
        }
        fill(r, x, Y, BW, BH, fr, fg, fb);

        // focus outline (thick) — only when NOT committed and NOT cancelled-collapsed
        if (is_focus && !is_committed && !is_cancelled_deny) {
            outline_thick(r, x - 4, Y - 4, BW + 8, BH + 8, 6, 120, 200, 255);
        }
        // committed / cancelled-collapsed get a solid thin frame to look "final"
        if (is_committed || is_cancelled_deny) {
            outline_thick(r, x - 2, Y - 2, BW + 4, BH + 4, 3, 240, 240, 250);
        }

        // label — centered in the button, scale-3 (24px wide chars)
        int label_scale = 3;
        int lw = text_width(labels[i], label_scale);
        int lx = x + (BW - lw) / 2;
        int ly = Y + (BH - FONT_H * label_scale) / 2;
        int tr = 245, tg = 245, tb = 250;
        if (is_committed) { tr = 30;  tg = 30;  tb = 40; }   // dark text on bright fill
        draw_text(r, lx, ly, label_scale, labels[i], tr, tg, tb);
    }

    // -- hint line --
    const char *hint = "A: confirm     B: cancel     Dpad L/R: move focus";
    draw_text_centered(r, W / 2, 620, 2, hint, 140, 148, 170);

    // -- decision banner at bottom for SELECTED/CANCELLED states (aids the VLM verifier) --
    if (state == STATE_SELECTED) {
        char msg[128];
        snprintf(msg, sizeof(msg), "DECISION: %s", focus_label(focus));
        draw_text_centered(r, W / 2, 670, 2, msg, 200, 210, 230);
    } else if (state == STATE_CANCELLED) {
        draw_text_centered(r, W / 2, 670, 2, "DECISION: Deny (via B/cancel)", 220, 140, 140);
    }
}

// ---------- main ----------
int main(int argc, char **argv) {
    int W = 1280, H = 720;
    const char *app_name = "Weather";
    const char *resource = "LOCATION";
    const char *purpose = NULL;
    const char *focus_s = "deny";
    const char *state_s = "initial";
    const char *out = NULL;

    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--canvas") && i + 1 < argc) sscanf(argv[++i], "%dx%d", &W, &H);
        else if (!strcmp(argv[i], "--app-name") && i + 1 < argc) app_name = argv[++i];
        else if (!strcmp(argv[i], "--resource") && i + 1 < argc) resource = argv[++i];
        else if (!strcmp(argv[i], "--purpose") && i + 1 < argc) purpose = argv[++i];
        else if (!strcmp(argv[i], "--focus") && i + 1 < argc) focus_s = argv[++i];
        else if (!strcmp(argv[i], "--state") && i + 1 < argc) state_s = argv[++i];
        else if (!strcmp(argv[i], "--out") && i + 1 < argc) out = argv[++i];
        else {
            fprintf(stderr, "unknown arg '%s'\n", argv[i]);
            return 2;
        }
    }
    if (!out) {
        fprintf(stderr, "consent-render: need --out <path.ppm>\n");
        return 2;
    }
    Focus focus = parse_focus(focus_s);
    State state = parse_state(state_s);

    // dummy video driver — no display, GPU-less; the surface path still works
    SDL_SetHint(SDL_HINT_VIDEO_DRIVER, "dummy");
    if (!SDL_Init(SDL_INIT_VIDEO)) {
        fprintf(stderr, "warn: SDL_Init(VIDEO) failed (%s) - surface path still works\n",
                SDL_GetError());
    }

    // (2) tsp-osr window-recipe pin — proves the on-window recipe the supervisor will use
    pin_tsp_osr_recipe(W, H);

    // (1) offscreen software-render onto a memfd-backed virtual fb
    size_t bytes = (size_t)W * H * 4;
    void *fbmem = NULL;
    int fbfd = make_fb(bytes, &fbmem);
    fprintf(stderr, "virtual fb: %s %dx%d (%zu bytes)\n",
            fbfd >= 0 ? "memfd" : "anon-buffer", W, H, bytes);

    SDL_Surface *surf = SDL_CreateSurfaceFrom(W, H, SDL_PIXELFORMAT_XRGB8888, fbmem, W * 4);
    if (!surf) {
        fprintf(stderr, "FAIL CreateSurfaceFrom: %s\n", SDL_GetError());
        return 3;
    }
    SDL_Renderer *r = SDL_CreateSoftwareRenderer(surf);
    if (!r) {
        fprintf(stderr, "FAIL CreateSoftwareRenderer: %s\n", SDL_GetError());
        return 3;
    }

    draw_dialog(r, W, H, app_name, resource, purpose, focus, state);
    SDL_RenderPresent(r);

    unsigned char *rgb = malloc((size_t)W * H * 3);
    put_rgb(rgb, (unsigned char *)fbmem, W, H);
    if (write_ppm(out, rgb, W, H) != 0) return 4;
    fprintf(stderr, "consent-render: app=%s resource=%s focus=%s state=%s -> %s (%dx%d)\n",
            app_name, resource, focus_s, state_s, out, W, H);

    free(rgb);
    SDL_DestroyRenderer(r);
    SDL_DestroySurface(surf);
    if (fbfd >= 0) { munmap(fbmem, bytes); close(fbfd); } else free(fbmem);
    SDL_Quit();
    return 0;
}
