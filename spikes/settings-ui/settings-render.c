// settings-render.c — tsp-xubv.3: gamepad-navigable settings UI prototype.
//
// Renders the E4 preference list (reduceMotion / hapticsEnabled / monoAudio
// toggles + the brightness scalar row) as a full-screen, gamepad-only settings
// screen to a virtual framebuffer, headless, on a GPU-less host. Dumps the
// framebuffer as P6 PPM for host-side PNG conversion + screen-reviewer
// verification. This is the RENDER + LAYOUT half; driver.py encodes the
// navigation grammar and liveness/ proves the store→observer behavior.
//
// The navigation idiom is REUSED from spikes/consent-ui/DESIGN.md §2 (rotated
// onto the vertical row axis) — see spikes/settings-ui/DESIGN.md §1. Drawing
// helpers + font + the tsp-osr recipe pin are shared with consent-render.c.
//
// tsp-osr-safe: consumes the pinned recipe from pocketforge-os/sim
// (sim/fb/README.md). Two safe paths, both used:
//   (1) offscreen: memfd -> SDL_CreateSurfaceFrom -> SDL_CreateSoftwareRenderer
//   (2) pin recipe: non-OPENGL window + SDL_CreateRenderer(win, "software"),
//       called on startup so a regression that breaks the on-window recipe
//       (which the M1.D supervisor will use on-panel) is caught immediately.
//
// HONESTY: portable SDL3 software rasterizer. Proves layout/widget logic +
// renderer-creation recipe ONLY. NOT the on-device libSDL3-pocketforge sunxifb
// backend, NOT PowerVR/dc_sunxi/DE2.0/fb0, NOT real panel rotation. See
// DESIGN.md §5, §7 and sim/fb/README.md "Honesty".
//
// Usage:
//   settings-render --profile a133|a523
//                   --reduce-motion on|off --haptics on|off --mono on|off
//                   --brightness N            (0..100)
//                   --focus reduceMotion|hapticsEnabled|monoAudio|brightness
//                   --out state.ppm
//   Optional: --canvas WxH  (default 1280x720)
//
// On profile a133 the descriptor has NO rumble actuator, so the Haptics row
// renders honest-absent (greyed, "unavailable") and is not a focus stop — the
// presence half of the E4 unification (DESIGN.md §3). Presence is passed in by
// the driver, which reads it from the E1 descriptor; the renderer never invents
// a toggle for hardware that isn't there.
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

// ---------- primitive drawing helpers (shared shape with consent-render.c) ----------
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

// ---------- tsp-osr window-recipe pin (mirrors sim/fb/fb-render.c + consent-render.c) ----------
static void pin_tsp_osr_recipe(int w, int h) {
    SDL_SetHint(SDL_HINT_RENDER_DRIVER, "software");
    SDL_Window *win = SDL_CreateWindow("settings-ui-osr-pin", w, h, 0);
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

// ---------- the preference list (schema order, matches pf_prefs::SCHEMA) ----------
typedef enum {
    ROW_REDUCE_MOTION = 0,
    ROW_HAPTICS = 1,
    ROW_MONO_AUDIO = 2,
    ROW_BRIGHTNESS = 3,
    ROW_COUNT = 4,
} Row;

typedef enum { KIND_BOOL, KIND_SCALAR } Kind;

typedef struct {
    const char *key;    // schema camelCase key, for --focus parsing
    const char *label;  // human label
    Kind kind;
} RowSpec;

static const RowSpec ROWS[ROW_COUNT] = {
    {"reduceMotion",   "Reduce motion", KIND_BOOL},
    {"hapticsEnabled", "Haptics",       KIND_BOOL},
    {"monoAudio",      "Mono audio",    KIND_BOOL},
    {"brightness",     "Brightness",    KIND_SCALAR},
};

static Row parse_focus(const char *s) {
    for (int i = 0; i < ROW_COUNT; i++)
        if (!strcmp(s, ROWS[i].key)) return (Row)i;
    fprintf(stderr, "unknown focus '%s' (expected a schema key)\n", s);
    exit(2);
}

static int parse_onoff(const char *s) {
    if (!strcmp(s, "on") || !strcmp(s, "true") || !strcmp(s, "1")) return 1;
    if (!strcmp(s, "off") || !strcmp(s, "false") || !strcmp(s, "0")) return 0;
    fprintf(stderr, "unknown on/off '%s'\n", s);
    exit(2);
}

// ---------- the settings-list layout (1280x720 landscape) ----------
//
//   y-band        role
//     0-8         top accent bar (signals "system surface")
//    40-120       title "Settings"
//   180..         four rows, ROW_PITCH apart
//   660-690       hint line (input legend)
static void draw_settings(SDL_Renderer *r, int W, int H, int a133,
                          const int bool_vals[3], int brightness, Row focus) {
    const int LEFT = 120;             // label x
    const int RIGHT = W - 120;        // widget right edge
    const int ROW_Y0 = 190;
    const int ROW_PITCH = 96;
    const int ROW_H = 72;

    // -- background + top accent bar --
    fill(r, 0, 0, W, H, 24, 24, 28);
    fill(r, 0, 0, W, 8, 90, 130, 210);

    // -- title --
    draw_text_centered(r, W / 2, 50, 5, "Settings", 235, 240, 250);

    // map bool rows to their bool_vals[] slot (reduceMotion=0, haptics=1, mono=2)
    for (int i = 0; i < ROW_COUNT; i++) {
        int y = ROW_Y0 + i * ROW_PITCH;
        int is_focus = (i == (int)focus);
        // haptics is honest-ABSENT on the a133 (no rumble actuator in the E1 descriptor)
        int is_absent = (a133 && i == ROW_HAPTICS);

        // -- focus band (thick outline + brightened fill) — reused idiom, never on an absent row --
        if (is_focus && !is_absent) {
            fill(r, LEFT - 30, y - 12, W - 2 * (LEFT - 30), ROW_H, 60, 66, 88);
            outline_thick(r, LEFT - 34, y - 16, W - 2 * (LEFT - 34), ROW_H + 8, 5,
                          120, 200, 255);
        }

        // -- label --
        int lr = 230, lg = 236, lb = 248;
        if (is_absent) { lr = 96; lg = 100; lb = 112; }   // greyed
        draw_text(r, LEFT, y + 6, 3, ROWS[i].label, lr, lg, lb);

        // -- value widget --
        if (is_absent) {
            const char *msg = "- unavailable";
            int tw = text_width(msg, 3);
            draw_text(r, RIGHT - tw, y + 6, 3, msg, 96, 100, 112);
            continue;
        }

        if (ROWS[i].kind == KIND_BOOL) {
            // slot: reduceMotion->0, haptics->1, mono->2
            int slot = (i == ROW_REDUCE_MOTION) ? 0 : (i == ROW_HAPTICS ? 1 : 2);
            int on = bool_vals[slot];
            const char *txt = on ? "[ ON  ]" : "[ OFF ]";
            const int PW = 7 * (FONT_W + 1) * 3;   // pill text width @ scale 3
            int px = RIGHT - PW;
            // pill background: bright when ON, dim when OFF (shape+text carry it too)
            if (on) fill(r, px - 12, y - 2, PW + 24, 44, 70, 120, 90);
            else    fill(r, px - 12, y - 2, PW + 24, 44, 60, 52, 52);
            int tr = on ? 230 : 200, tg = on ? 245 : 205, tb = on ? 230 : 205;
            draw_text(r, px, y + 6, 3, txt, tr, tg, tb);
        } else {
            // brightness bar: 10 cells + numeric. Clamp defensively.
            int b = brightness; if (b < 0) b = 0; if (b > 100) b = 100;
            int filled = (b + 5) / 10;             // round to nearest 10% cell
            char bar[16];
            for (int c = 0; c < 10; c++) bar[c] = (c < filled) ? '#' : '-';
            bar[10] = '\0';
            char widget[48];
            snprintf(widget, sizeof(widget), "[%s] %3d", bar, b);
            int tw = text_width(widget, 3);
            draw_text(r, RIGHT - tw, y + 6, 3, widget, 210, 220, 235);
        }
    }

    // -- hint line (input legend) --
    const char *hint = "Up/Down: move     A: toggle     L/R: adjust     B: back";
    draw_text_centered(r, W / 2, 664, 2, hint, 140, 148, 170);

    // -- device-profile banner (aids the VLM verifier + records the presence half) --
    char prof[64];
    snprintf(prof, sizeof(prof), "device: %s%s", a133 ? "a133" : "a523",
             a133 ? " (no rumble motor)" : "");
    draw_text_centered(r, W / 2, 700, 2, prof, 150, 158, 180);
}

// ---------- main ----------
int main(int argc, char **argv) {
    int W = 1280, H = 720;
    int a133 = 0;
    int reduce_motion = 0, haptics = 1, mono = 0;   // schema defaults
    int brightness = 100;
    const char *focus_s = "reduceMotion";
    const char *out = NULL;

    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--canvas") && i + 1 < argc) sscanf(argv[++i], "%dx%d", &W, &H);
        else if (!strcmp(argv[i], "--profile") && i + 1 < argc) {
            const char *p = argv[++i];
            if (!strcmp(p, "a133")) a133 = 1;
            else if (!strcmp(p, "a523")) a133 = 0;
            else { fprintf(stderr, "unknown profile '%s'\n", p); return 2; }
        }
        else if (!strcmp(argv[i], "--reduce-motion") && i + 1 < argc) reduce_motion = parse_onoff(argv[++i]);
        else if (!strcmp(argv[i], "--haptics") && i + 1 < argc) haptics = parse_onoff(argv[++i]);
        else if (!strcmp(argv[i], "--mono") && i + 1 < argc) mono = parse_onoff(argv[++i]);
        else if (!strcmp(argv[i], "--brightness") && i + 1 < argc) brightness = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--focus") && i + 1 < argc) focus_s = argv[++i];
        else if (!strcmp(argv[i], "--out") && i + 1 < argc) out = argv[++i];
        else { fprintf(stderr, "unknown arg '%s'\n", argv[i]); return 2; }
    }
    if (!out) { fprintf(stderr, "settings-render: need --out <path.ppm>\n"); return 2; }
    Row focus = parse_focus(focus_s);
    int bool_vals[3] = {reduce_motion, haptics, mono};

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
    if (!surf) { fprintf(stderr, "FAIL CreateSurfaceFrom: %s\n", SDL_GetError()); return 3; }
    SDL_Renderer *r = SDL_CreateSoftwareRenderer(surf);
    if (!r) { fprintf(stderr, "FAIL CreateSoftwareRenderer: %s\n", SDL_GetError()); return 3; }

    draw_settings(r, W, H, a133, bool_vals, brightness, focus);
    SDL_RenderPresent(r);

    unsigned char *rgb = malloc((size_t)W * H * 3);
    put_rgb(rgb, (unsigned char *)fbmem, W, H);
    if (write_ppm(out, rgb, W, H) != 0) return 4;
    fprintf(stderr, "settings-render: profile=%s rm=%d haptics=%d mono=%d bright=%d focus=%s -> %s (%dx%d)\n",
            a133 ? "a133" : "a523", reduce_motion, haptics, mono, brightness, focus_s, out, W, H);

    free(rgb);
    SDL_DestroyRenderer(r);
    SDL_DestroySurface(surf);
    if (fbfd >= 0) { munmap(fbmem, bytes); close(fbfd); } else free(fbmem);
    SDL_Quit();
    return 0;
}
