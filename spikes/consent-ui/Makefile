# SPIKE-2 consent-ui — build the prototype against the sim's static SDL3-render.
# Runs on modelmaker (mm@10.0.40.90); consumes /home/mm/sim-build/sdl3-render/x86 built by
# pocketforge-os/sim/fb/build-sdl3-render.sh — this spike does NOT rebuild SDL3.
#
# Also works on any host with SDL3 installed system-wide (falls back to pkg-config).
CC       ?= cc
CFLAGS   ?= -O2 -Wall -Wextra -std=c11
SDLR     ?= /home/mm/sim-build/sdl3-render

# Prefer the pinned static SDL3-render from sim; fall back to pkg-config if absent.
ifeq ($(wildcard $(SDLR)/x86/lib/libSDL3.a),$(SDLR)/x86/lib/libSDL3.a)
  SDL_CFLAGS := -I$(SDLR)/x86/include
  SDL_LIBS   := $(SDLR)/x86/lib/libSDL3.a -lm -lpthread -ldl
else
  SDL_CFLAGS := $(shell pkg-config --cflags sdl3 2>/dev/null)
  SDL_LIBS   := $(shell pkg-config --libs sdl3 2>/dev/null)
endif

.PHONY: all clean
all: build/consent-render

build/consent-render: consent-render.c font8x13.h
	@mkdir -p build
	$(CC) $(CFLAGS) $(SDL_CFLAGS) -o $@ consent-render.c $(SDL_LIBS)

clean:
	rm -rf build
