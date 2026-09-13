# thesh

A lightweight, standalone POSIX shell for Haliade OS, written in C. No bash dependency — works with any system providing a C11 compiler and POSIX libc.

## Building

```sh
make
```

The binary is produced in the project root as `thesh`.

## Installation

```sh
make install          # installs to ~/haliade-root/usr/bin/thesh + /etc/theshrc
DESTDIR=/ sudo make install   # system-wide install
```

`DESTDIR` defaults to `$HOME/haliade-root`.

## Features

- **No bash dependency** — pure C11 + POSIX
- **Fish-style ghost suggestions** — the most recent matching history line appears dimmed as you type
- **Tab completion** — commands from `$PATH` and builtins; file/directory paths with `/` on directories
- **Prefix history search** — type a prefix, press ↑/↓ to walk only matching past commands
- **Reverse-i-search** — Ctrl-R for substring history search
- **Levenshtein typo detection** — misspelled commands trigger a "Did you mean …?" suggestion
- **Full tokenizer** — single/double quotes, backslash escapes, `$VAR`, `${VAR}`, `$?`, `~` expansion
- **Aliases** — `alias name=val` with multi-word values; stored in-process, not exported
- **RC files** — `/etc/theshrc` (system) then `~/.therc` (user) at startup
- **Non-interactive / pipe support** — reads stdin silently when not a TTY

## Prompt

```
[ user | hostname ] ~/current/dir $
```

No color codes, no escape sequences — clean and portable.

## Builtins

| Command       | Description                          |
|---------------|--------------------------------------|
| `cd [dir\|-]` | Change directory; `cd -` returns previous |
| `pwd`         | Print working directory              |
| `echo [-n]`   | Print args; `-n` suppresses newline  |
| `export`      | `export VAR=val` or print all vars   |
| `unset`       | Remove environment variables         |
| `alias`       | `alias name=val` or list all         |
| `unalias`     | Remove aliases; `unalias -a` for all|
| `history`     | Numbered command history             |
| `type`        | Identify command type (alias/builtin/path) |
| `source .`    | Execute lines from a file            |
| `exit [n]`    | Exit with status `n`                 |

## Configuration

At startup thesh loads `/etc/theshrc` (system-wide) then `~/.therc` (user). Example:

```sh
alias ll='ls -l'
alias la='ls -la'
alias h='history'
```

## Run modes

```sh
./thesh              # interactive REPL
./thesh -c 'ls -la' # execute a single command
./thesh script.sh    # run a file non-interactively
echo 'ls' | thesh   # read from stdin (no prompt)
```

## History

Command history is stored in `~/.thesh_history` (max 500 entries). Override the path with the `THESH_HISTFILE` environment variable.