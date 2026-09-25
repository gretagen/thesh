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

Syntax — the key is separated from the value with `=` (the canonical form);
the older `:` form is still accepted so existing configs keep working. The value
may be quoted with `'…'` or `"…"` or bare:

```sh
cursorstyle = '$'
seperatorstyle = '|'
looks = "$RIGHTWALL $SPACER $USER $SPACER $SEPERATOR $SPACER $HOSTNAME $SPACER $LEFTWALL $SPACER $PATH $SPACER $CURSOR"
```

`layout` is an alias for `looks` (same template, either spelling works).

### Prompt elements

| Key               | Default | Meaning                       |
|-------------------|---------|-------------------------------|
| `looks` / `layout`| —       | prompt template (see above)   |
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
| `user-color`       | `$USER`        |
| `guesser-color`    | ghost suggestion |
| `textcolor`        | general text color: literals in the template and any element without a specific color |

`textcolor` is the fallback for every element that has no specific `…-color`
set, and for literal (non-token) text in the `looks` template. Elements with
their own color always win over it.

Available colors: `black red green yellow blue purple cyan white grey pink`,
each with a `bright-` variant (e.g. `bright-blue`), plus `default` and `none`:

* `default` — no color change: the element renders strictly in the terminal's
  default foreground (and breaks out of any color from the preceding element).
* `none` — no color code at all (the element inherits whatever is active).

Example: `path-color = 'green'`.

### Opacity

Every colorable prompt element accepts an opacity percentage:

| Key                   | Applies to       |
|-----------------------|------------------|
| `rightwall-opacity`   | right wall       |
| `leftwall-opacity`    | left wall        |
| `seperator-opacity`   | separator        |
| `hostname-opacity`    | hostname         |
| `user-opacity`        | `$USER`          |
| `path-opacity`        | directory        |
| `cursor-opacity`      | cursor glyph     |
| `guesser-opacity`     | ghost suggestion |

Values are percentages written with or without `%`, quoted or bare
(`'100%'`, `65`, `'50%'`). At 100% the element uses its plain ANSI color. Below
100% the color is blended toward black (the assumed dark default background)
and emitted as a 256-color code — so `guesser-opacity = '65%'` renders a
faded ghost. Opacity only has an effect on a colored element (it needs
`…-color` to work with; `default` at <100% renders as no color).

Example:

```sh
hostname-color = 'red'
hostname-opacity = '50%'
guesser-color = 'grey'
guesser-opacity = '65%'
```

### Behavior options

| Key        | Default    | Values                                                    |
|------------|----------- |-----------------------------------------------------------|
| `typing`   | `hybrid`   | `hybrid` folds command names case-insensitively; `lowercase` is case-sensitive |
| `guesser`  | `yes`      | `yes`/`no` — ghost suggestions on or off                  |
| `corrector`| `passive`  | `passive` prints "Did you mean …?"; `active` auto-runs the fix; `consent` prompts `[y/N]`; `inactive` suppresses suggestions |
| `autoreload`| `yes`     | `yes`/`no` — re-source rc files automatically when they change on disk |
| `history`  | `yes`      | `yes`/`no` — record new commands and save them on exit |
| `historylimit` | `500`  | any number — max commands kept in history (oldest are trimmed; `0` disables) |

### Key bindings

Shortcuts are defined in the config with `+`-joined modifiers and a key on the
left, and an action on the right:

```sh
ALT + T        = exec('top')              # run a command
CTRL + ALT + V = paste                    # paste the line clipboard
CTRL + ALT + C = copy                     # copy the current line (also OSC 52)
CTRL + C       = close                    # exit the shell
ALT + M        = "exec('micro') ask(path?)"
CTRL + Z       = "exec('zeta')  ask(action?)"
SUPER + UP     = exec('echo super pressed')   # Windows/Super key + arrow
F11            = exec('toggle')               # plain function key, no modifier
ALT + F5       = exec('reload')
```

> **Enter is `CTRL + M` (and `CTRL + J`)** at the byte level, so those two
> keys can't be rebound — the submit key always wins. Every other key is
> fair game (including `CTRL + C`, if you really want to bind `close` to it).

* **Modifiers** — `CTRL`, `ALT`, `SUPER` (the Windows/Super key; `WIN`,
  `WINDOWS`, `META` are also accepted), joined with `+`. Keys are a single
  letter, digit or symbol (case-insensitive), or a name: `ENTER TAB SPACE
  BACKSPACE DELETE UP DOWN LEFT RIGHT HOME END PGUP PGDN ESC F1` through
  `F24`. A modifier is optional — `F11 = …` binds the plain function key.
  Terminals report Super/Meta combinations as CSI modifiers (9–16 or 33–40),
  which the shell decodes to `SUPER`, so `SUPER + UP` works with the arrow
  keys. Shift is folded into the matched modifier (`SUPER+Shift+UP` still
  triggers a `SUPER + UP` binding).
* **Actions**:
  * `exec('cmd')` — run `cmd` (useful for shortcuts to interactive tools).
  * `ask(label)` — after `exec(...)`: prompt `label : ` on its own line, then
    append the typed text to the command. `ALT + M = "exec('micro') ask(path?)"`
    prompts `path? : `, and answering `/etc/theshrc` runs `micro /etc/theshrc`.
    **ESC, Ctrl+C or Ctrl+D cancels the prompt** without running the command.
  * `copy` — copy the current line (shell clipboard + best-effort OSC 52 to the
    terminal's clipboard). `paste` — insert the last copied text at the cursor.
  * `close` — exit the shell.
* Bindings are looked up **before** the built-in key handling, so a bound key
  replaces that key's default action (e.g. binding `CTRL + C` overrides the
  cancel-line key). Unbound keys keep their defaults. Comments can be removed
  and the same binding redefined later — the last definition wins.

Example bindings and the `ask` flow are included in `theshrc.sample`.

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

## Presets

A preset is a named, plain theshrc file. Presets live in two places:

| Location | Directory                        |
|----------|----------------------------------|
| user     | `~/.config/thesh/presets/`       |
| system   | `/etc/thesh/presets`             |

### `presets` — list and apply

Running `presets` interactively prints a numbered list (user presets first;
a user preset shadows a system one of the same name) and lets you pick by
number or name:

```
select preset :

1 : kawaii
2 : default
```

After a pick it asks which rc file(s) to make the shell default for:

```
make this shell default for...?
1 : system
2 : user
3 : both
```

The chosen preset is copied over `/etc/theshrc` and/or `~/.theshrc` and the
existing shell re-sources it immediately — the new prompt, aliases and key
bindings are active on the very next prompt. `presets NAME` applies a preset
directly without the menu. Non-interactively (e.g. `thesh -c 'presets'`) the
command just lists the names; with an argument it applies to the user rc.

> **Every prompt in `presets` and `savepreset` (and the `ask(...)` bind
> prompt) can be cancelled with ESC, Ctrl+C or Ctrl+D** — the pick is
> discarded and nothing is written.

### `savepreset` — export your current config

Serializes the effective config — options, colors/opacity, aliases and key
bindings — as a preset file:

```
save preset at? :
1 : system /etc/thesh/presets
2 : user ~/.config/thesh/presets
3 : custom
```

Options 1 and 2 ask for a preset name; option 3 takes a full path (`path : `).
`savepreset NAME` supplies the name up front and skips the name prompt.
Preset files round-trip: a saved preset can be applied weeks later and restores
the exact prompt, aliases and bindings.

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
| `presets`          | List/apply preset configs (see above) |
| `savepreset`       | Save the current config as a preset  |
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

The size is configurable, and recording can be turned off entirely:

```sh
historylimit = 30      # keep only the last 30 commands (any value works)
history      = yes     # 'no' stops recording and saving (existing file is kept)
```

The limit is applied when history loads, so with `historylimit = 30` the shell
starts with only the newest 30 commands from the file, and every additional
command pushes the oldest one out. `history = no` also clears the in-session
history (up-arrow recalls nothing) without touching the saved file. Both keys
are included in `savepreset` exports.