CC      ?= cc
CFLAGS  ?= -O2 -Wall -Wextra -std=c11
LDFLAGS ?=

SRC := $(sort $(wildcard src/*.c))
OBJ := $(SRC:.c=.o)

DESTDIR ?= $(HOME)/haliade-root

all: thesh

thesh: $(OBJ)
	$(CC) $(CFLAGS) -o $@ $(OBJ) $(LDFLAGS)

%.o: %.c src/thesh.h
	$(CC) $(CFLAGS) -c -o $@ $<

install: thesh
	install -Dm755 thesh $(DESTDIR)/usr/bin/thesh
	@test -f $(DESTDIR)/etc/theshrc || install -Dm644 theshrc.sample $(DESTDIR)/etc/theshrc
	@echo "installed thesh -> $(DESTDIR)/usr/bin/thesh"
	@echo "system rc    -> $(DESTDIR)/etc/theshrc"

clean:
	rm -f thesh $(OBJ)

.PHONY: all clean install