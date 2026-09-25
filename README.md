# thesh

A lightweight, standalone POSIX shell for Haliade OS, written in C11/C23. No bash
dependency — works with any system providing a C compiler and POSIX libc. Tiny
single binary; no external libraries.

Current version: **0.3.0**

## Building

```sh
make              # debug build (default): -g3 -O0 + Address/Undefined sanitizers
make release      # optimized: -g0 -O3 + LTO
make clean
make -j           # parallel is enabled by default (nproc)
```

Compiles warning-clean under `-Wall -Wextra -Wpedantic -Wshadow -Wconversion
-Wformat=2 -Wundef`. Override the compiler or standard if needed:

```sh
make CC=clang CSTD=c11
```

Binaries:

| Build    | Path                     |
|----------|--------------------------|
| debug    | `build/debug/thesh`      |
| release  | `build/release/thesh`    |

## Installation

```sh
make install                          # installs to ~/haliade-root/usr/bin/thesh
DESTDIR=/ make install                # system-wide install
```

`DESTDIR` defaults to `$HOME/haliade-root`. `make install` also copies
`theshrc.sample` to `$DESTDIR/etc/theshrc` if one isn't already there.

## Features

- **No bash dependency** — pure C + POSIX, one small binary
- **Configurable prompt** — `looks` template, `$PS1` (bash escapes), or the default
- **Colors** — 14 named colors for every prompt element and the ghost suggestion
- **Fish-style ghost suggestions** — the most recent matching history line appears dimmed (or in `guesser-color`) as you type
- **Tab completion** — commands from `$PATH` and builtins; file/directory paths with `/` on directories
- **Prefix history search** — type a prefix, press ↑/↓ to walk only matching past commands
- **Reverse-i-search** — Ctrl-R for substring history search
- **Levenshtein typo detection** — misspelled commands trigger a "Did you mean …?" suggestion or auto-correct
- **Smart command casing** — `hybrid` typing (default) folds command names (`LS`, `PACMAN -S` work); `lowercase` is case-sensitive; arguments always stay as typed
- **Full tokenizer** — single/double quotes, backslash escapes, `$VAR`, `${VAR}`, `$?`, `~` expansion
- **Aliases** — `alias name=val` with multi-word values; stored in-process, not exported
- **Pipelines** — `cmd1 | cmd2 | cmd3`, each stage in its own process, exit status from the last command
- **RC files** — `/etc/theshrc`, then `~/.theshrc`, then `~/.therc` (legacy fallback only)
- **Non-interactive / pipe support** — reads stdin silently when not a TTY

## Prompt

The default prompt is:

```
[ user | hostname ] ~/current/dir $
```

The prompt is fully configurable. Precedence:

1. `looks` in the config file (highest)
2. `$PS1` environment variable (bash-style escapes)
3. the built-in default above

### `looks` templates

`looks` is a string of tokens and literal text. Whitespace between tokens is
ignored; non-space literal text (like `@`) is kept. Tokens:

| Token         | Expands to                          |
|---------------|-------------------------------------|
| `$USER`       | current user                        |
| `$HOSTNAME`   | hostname (uses `hostname-color`)    |
| `$SEPERATOR`  | separator glyph (see below)         |
| `$RIGHTWALL`  | right wall glyph                    |
| `$LEFTWALL`   | left wall glyph                     |
| `$PATH`       | current directory, `~` collapsed    |
| `$DIR`, `$PWD`| aliases of `$PATH`                  |
| `$CURSOR`     | cursor glyph (`$` by default)       |
| `$SPACER`     | exactly one literal space           |

Examples:

```sh
looks = "$USER@$HOSTNAME:$PATH $CURSOR"      # user@host:dir $
looks = "$RIGHTWALL $SPACER $USER $SPACER $SEPERATOR $SPACER $HOSTNAME $SPACER $LEFTWALL $SPACER $PATH $SPACER $CURSOR"
```

A lone `$` or an unknown `$TOKEN` renders literally, so `fee $10 + $TAX` shows `fee $10 + $TAX`.

### `$PS1` fallback

When `looks` isn't set, `$PS1` is expanded. Supported escapes:

| Escape | Meaning                          | Escape | Meaning        |
|--------|----------------------------------|--------|----------------|
| `\u`   | user                             | `\w`   | cwd (`~` collapsed) |
| `\h`   | hostname (short)                 | `\W`   | basename of cwd |
| `\H`   | hostname (long)                  | `\$`   | `#` if root, else `$` |
| `\s`   | shell name (`thesh`)             | `\v`   | version (`0.3.0`) |
| `\t`   | time `HH:MM:SS`                  | `\A`   | time `HH:MM`    |
| `\@`   | time `HH:MM AM/PM`               | `\d`   | date `Day Mon DD` |
| `\n`   | newline                          | `\e`   | escape          |
| `\\`   | literal backslash                | `\[`/`\]` | non-printing markers |
| `\!`/`\#` | next history number           | `\j`   | job count (always 0) |

Example: `PS1="\u@\h:\w \$ "`

## Configuration

Config lives in rc files, loaded in order:

1. `/etc/theshrc` (system-wide)
2. `~/.theshrc` (user)
3. `~/.therc` (legacy fallback — only used when `~/.theshrc` doesn't exist)

Config lines also work **live** when typed into the shell (they're consumed
before command execution). A copy with examples ships as `theshrc.sample`.

Syntax — the key may be separated from the value by `=` or `:` and the value may
be quoted with `'…'` or `"…"` or bare:

```sh
cursorstyle = '$'
seperatorstyle : '|'
looks = "$RIGHTWALL $SPACER $USER $SPACER $SEPERATOR $SPACER $HOSTNAME $SPACER $LEFTWALL $SPACER $PATH $SPACER $CURSOR"
```

### Prompt elements

| Key               | Default | Meaning                       |
|-------------------|---------|-------------------------------|
| `looks`           | —       | prompt template (see above)   |
| `rightwallstyle`  | `[`     | right wall glyph              |
| `leftwallstyle`   | `]`     | left wall glyph               |
| `seperatorstyle`  | `\|`    | separator glyph (sic: `SEPERATOR`) |
| `cursorstyle`     | `$`     | cursor glyph                  |

### Colors

| Key                | Element        |
|--------------------|----------------|
| `rightwall-color`  | right wall     |
| `leftwall-color`   | left wall      |
| `seperator-color`  | separator      |
| `hostname-color`   | hostname       |
| `path-color`       | directory      |
| `cursor-color`     | cursor glyph   |
| `guesser-color`    | ghost suggestion |

`guesser-color` recolors the dim ghost text; without it, ghosts render dim.

Available colors: `black red green yellow blue purple cyan white grey pink`,
each with a `bright-` variant (e.g. `bright-blue`), plus `default` (terminal
default) and `none` (no color). Example: `path-color = 'green'`.

### Behavior options

| Key        | Default    | Values                                                    |
|------------|----------- |-----------------------------------------------------------|
| `typing`   | `hybrid`   | `hybrid` folds command names case-insensitively; `lowercase` is case-sensitive |
| `guesser`  | `yes`      | `yes`/`no` — ghost suggestions on or off                  |
| `corrector`| `passive`  | `passive` prints "Did you mean …?"; `active` auto-runs the fix; `consent` prompts `[y/N]`; `inactive` suppresses suggestions |
| `autoreload`| `yes`     | `yes`/`no` — re-source rc files automatically when they change on disk |

### Auto-reload

The interactive shell watches the rc files it loaded at startup (`/etc/theshrc`,
then the user rc). When one of them changes on disk — mtime or size — the shell
re-sources it quietly and the new settings take effect on the **next prompt**,
with no restart needed:

```sh
# open a second terminal and edit ~/.theshrc
cursorstyle = '%'      # ← saved from the editor

# back in the shell, press Enter: the next prompt uses the new style
```

Config `echo` lines are suppressed during an auto-reload (only warnings on
`stderr` show), so editing the file never replays rc output into the terminal.
Running `source ~/.theshrc` by hand still prints normally. Set `autoreload = no`
in an rc file to disable watching.

## Builtins

| Command            | Description                          |
|--------------------|--------------------------------------|
| `cd [dir\|-]`      | Change directory; `cd -` returns previous |
| `pwd`              | Print working directory              |
| `echo [-n]`        | Print args; `-n` suppresses newline  |
| `export`           | `export VAR=val` or print all vars   |
| `unset`            | Remove environment variables         |
| `alias`            | `alias name=val` or list all         |
| `unalias`          | Remove aliases; `unalias -a` for all |
| `history`          | Numbered command history             |
| `type`             | Identify command type (alias/builtin/path) |
| `set`              | Print shell variables/options        |
| `clearhistory`     | Clear and save the history           |
| `hash`             | Manage the command lookup cache      |
| `source`           | Execute lines from a file            |
| `exit [n]`         | Exit with status `n`                 |

## Run modes

```sh
build/release/thesh            # interactive REPL
thesh -c 'ls -la'              # execute a single command
thesh script.sh                # run a file non-interactively
echo 'ls' | thesh              # read from stdin (no prompt)
```

## History

Command history is stored in `~/.thesh_history` (max 500 entries). Override the
path with the `THESH_HISTFILE` environment variable.