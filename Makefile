CC     ?= cc
CSTD   ?= c23
LDFLAGS ?=
LDFLAGS += -lutil                 # forkpty() for the native multiplexer

GNUMAKEFLAGS += -j$(shell nproc 2>/dev/null || echo 1)

WARN_FLAGS := -Wall -Wextra -Wpedantic -Wshadow -Wconversion \
              -Wformat=2 -Wundef

DEBUG_CFLAGS   := $(WARN_FLAGS) -g3 -O0 -fsanitize=address,undefined
RELEASE_CFLAGS := $(WARN_FLAGS) -g0 -O3 -flto
DEBUG_LDFLAGS  := -fsanitize=address,undefined
RELEASE_LDFLAGS := -flto

NAME       := thesh
OUTDIR     := build

all: debug

debug:   $(OUTDIR)/debug/$(NAME)
release: $(OUTDIR)/release/$(NAME)

# `make install` destination: PREFIX defaults to ~/.local (user-local).
# DESTDIR is a staging prefix, empty by default; for the Haliade rootfs use
#   make install PREFIX=/usr DESTDIR=$(HOME)/haliade-root
DESTDIR ?=
PREFIX  ?= $(HOME)/.local

SRC := $(sort $(wildcard src/*.c))

DEBUG_OBJS   := $(patsubst src/%.c,$(OUTDIR)/debug/obj/%.o,$(SRC))
RELEASE_OBJS := $(patsubst src/%.c,$(OUTDIR)/release/obj/%.o,$(SRC))

$(OUTDIR)/debug/obj/%.o: src/%.c src/thesh.h | $(OUTDIR)/debug/obj
	$(CC) -std=$(CSTD) $(DEBUG_CFLAGS) -c -o $@ $<

$(OUTDIR)/release/obj/%.o: src/%.c src/thesh.h | $(OUTDIR)/release/obj
	$(CC) -std=$(CSTD) $(RELEASE_CFLAGS) -c -o $@ $<

$(OUTDIR)/debug/$(NAME): $(DEBUG_OBJS) | $(OUTDIR)/debug
	$(CC) $(DEBUG_CFLAGS) -o $@ $(DEBUG_OBJS) $(LDFLAGS) $(DEBUG_LDFLAGS)

$(OUTDIR)/release/$(NAME): $(RELEASE_OBJS) | $(OUTDIR)/release
	$(CC) $(RELEASE_CFLAGS) -o $@ $(RELEASE_OBJS) $(LDFLAGS) $(RELEASE_LDFLAGS)

$(OUTDIR)/debug/obj $(OUTDIR)/release/obj \
$(OUTDIR)/debug $(OUTDIR)/release:
	mkdir -p $@

install: release
	install -Dm755 $(OUTDIR)/release/$(NAME) $(DESTDIR)$(PREFIX)/bin/$(NAME)
	@echo "installed $(NAME) -> $(DESTDIR)$(PREFIX)/bin/$(NAME)"
	@echo "config: cp theshrc.sample ~/.theshrc   (system-wide: /etc/theshrc)"

clean:
	rm -rf $(OUTDIR)

.PHONY: all debug release clean install