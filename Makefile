CC     ?= cc
CSTD   ?= c23
LDFLAGS ?=

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

DESTDIR ?= $(HOME)/haliade-root

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
	install -Dm755 $(OUTDIR)/release/$(NAME) $(DESTDIR)/usr/bin/$(NAME)
	@test -f $(DESTDIR)/etc/theshrc || install -Dm644 theshrc.sample $(DESTDIR)/etc/theshrc
	@echo "installed $(NAME) -> $(DESTDIR)/usr/bin/$(NAME)"
	@echo "system rc   -> $(DESTDIR)/etc/theshrc"

clean:
	rm -rf $(OUTDIR)

.PHONY: all debug release clean install