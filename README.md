# thesh

A lightweight, standalone POSIX shell for Haliade OS, written in C11/C23. No bash
dependency — works with any system providing a C compiler and POSIX libc. Tiny
single binary; no external libraries.

Current version: **0.5.0**

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
make install                                   # -> ~/.local/bin/thesh
sudo make install PREFIX=/usr                  # system-wide -> /usr/bin/thesh
make install PREFIX=/usr DESTDIR=$HOME/haliade-root   # populate the Haliade rootfs
```

`PREFIX` defaults to `$HOME/.local` — make sure `$PREFIX/bin` is on your
`$PATH`. `DESTDIR` is an empty staging prefix (for packaging). No config
file is written; the shell reads `/etc/theshrc` then `~/.theshrc`:

```sh
cp theshrc.sample ~/.theshrc
```

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
| `\s`   | shell name (`thesh`)             | `\v`   | version (`0.5.0`) |
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
| `multiplexer-active-color` | focused pane's separator (default blue) |
| `multiplexer-inactive-color` | other panes' separators (default fg) |
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
| `animation` | `none`   | `none`/`matrix`/`newcomer`/`placement`/`spinner` — how typed characters appear (see below) |
| `multiplexer` | `yes` | `yes`/`no` — allow splitting; setting `no` tears down an active mux |
| `multiplexer-window-limit` | `inf` | `inf` or a number — max panes in one terminal |
| `multiplexer-style` | `crosshair` | separator style (`crosshair` = tmux look) |
| `movespeed` | `1`      | letters the cursor advances per Left/Right press (`1..100`) |
| `scrollspeed` | `1`    | history entries traversed per Up/Down press (`1..100`) |
| `amplify`   | `2`      | hold-to-boost multiplier: while the same arrow is held (pressed again within 400 ms), each repeat advances `step × amplify`; `yes` = `2`, `no` = `1` |

Typed-character animations are purely cosmetic and **never block input**: the
letter is committed to the line the instant the key is pressed, so typing fast
simply cuts the animation short — nothing is lost and the command runs even
while a frame is still on screen.

| Style        | Effect                                                            |
|--------------|-------------------------------------------------------------------|
| `none`       | the character appears instantly (default)                         |
| `matrix`     | random letters scramble in place, then settle on what you typed   |
| `newcomer`   | the letter rides into position from the right                     |
| `placement`  | pieces snap in from every side, like legos                       |
| `spinner`    | a `/ - \ |` spinner twirls before the letter lands                |

```sh
animation = spinner
```

### Movement speed

Cursor movement runs at your terminal's key-repeat rate — every press of an
arrow counts, one for one. `movespeed` and `scrollspeed` set the base step
per press (letters for Left/Right, history entries for Up/Down), and
`amplify` adds a hold-to-boost: keep the same arrow held and each repeat
advances `step × amplify`, so holding Right sweeps at double speed while a
single tap stays precise.

```sh
movespeed = 1      # letters per left/right press
scrollspeed = 1    # history entries per up/down press
amplify = 2        # held arrows advance twice as fast (no = off)
```

### Multiplexer

thesh has a built-in tiling multiplexer — no server, no sockets. The
first split **forks**: your current shell keeps running as pane #1 on
its own pty (same typed line, aliases, cwd, history), while the parent
process becomes a pure router that never prompts again; each new pane
runs a fresh thesh on its own pty. Panes are separated the way tmux
does it — thin `│`/`─` lines with `┬` `┴` `├` `┤` `┼` at the
junctions, nothing drawn on the terminal edges — and the focused
pane's separators glow in the active colour.

| Command | Effect |
|---|---|
| `split-window-vertical [N]` | split along a vertical divider (side-by-side), `N` percent (default 50) |
| `split-window-horizontal [N]` | stacked split, `N` percent |
| `close-pane` | close the focused pane (tmux kill-pane); closing the last pane leaves the multiplexer |
| `kill-curent-window` | alias of `close-pane` (`kill-current-window` also accepted) |
| `kill-all-windows` | kill every pane — emptying the multiplexer hands you a fresh plain shell on the real terminal |
| `move-to-right/left/up/down-window` | move focus to that pane |
| `focus-left/right/up/down/next/prev` | move focus between panes |
| `panes` | list every pane's size and position, marking the focused one |

They work typed at the prompt **and** as bind actions — a bound split
acts on the focused pane, a typed one splits the pane you type in (so
running it inside a pane starts that pane's own nested multiplexer).
Mux verbs (`split-*`, `kill-*`, `move-*`, `focus-*`, `panes`) always
run in the router; **every other bind is forwarded to the focused pane
and runs there** — a shortcut like `CTRL + E = exec('micro …')`
pressed in a child pane opens the program *in that pane*, with its
cwd and aliases:

```sh
bind = ALT + V = split-window-vertical 50
bind = ALT + H = split-window-horizontal 50
bind = CTRL + Q = kill-curent-window
bind = CTRL + K = kill-all-windows
bind = ALT + R-ARROW = move-to-right-window
bind = ALT + L-ARROW = move-to-left-window
bind = ALT + U-ARROW = move-to-up-window
bind = ALT + D-ARROW = move-to-down-window
```

* **Navigation:** `Ctrl + arrows` moves between panes out of the box (or
  bind arrows as above — `L-ARROW`/`R-ARROW`/`U-ARROW`/`D-ARROW` are key
  names). With a single pane nothing changes: Ctrl+arrows keep
  word-jumping there, and Alt+arrows word-jump everywhere.
* **Settings:** `multiplexer = 'no'` disables splitting (and tears down
  an active mux live), `multiplexer-window-limit = 'inf'|N` caps the
  pane count, `multiplexer-style = 'crosshair'` is the separator look.
* **Colors:** `multiplexer-active-color` (default blue) paints the
  separators touching the pane you're typing in; `multiplexer-inactive-color`
  (default terminal fg) the rest — it's always obvious where your
  keystrokes go.
* **Scrollback:** every pane keeps the last 1000 lines; `PgUp`/`PgDn`
  scroll whichever pane is focused — selected or not — and they keep
  working while an editor or `htop` runs in it; any other key returns
  to live.
* **Mouse:** the pointer works on the pane it is over and **never
  changes focus** — you move between panes with the move binds only
  (`Ctrl`+arrows or your bound keys). The wheel scrolls whichever pane
  it is over; a click is forwarded to that pane's program (htop, an
  editor, …) **translated to pane-relative coordinates**, and only if
  that program asked for mouse tracking (`?1000/?1002/?1003` is
  tracked per pane, like tmux) — clicks on a divider, on scrollback,
  or when the program never requested mouse are ignored. Mouse
  reporting is enabled only while the mux runs, and is re-asserted
  after a program exits or the terminal resizes. Clicks and
  `Ctrl+arrows` are handled by the router even while a full-screen
  program runs; `PgUp`/`PgDn` stay with the mux either way.
* **Activation is a fork:** splitting turns the parent process into
  the router, and your shell becomes pane #1 with everything in memory
  intact (typed line, aliases, cwd, history); the pane starts fresh at
  the top of the screen, tmux-style. When the last pane dies the
  router hands the terminal back to a fresh plain shell.
* Pane children run with `$THESH_MUX=1` set, so rc files can guard
  auto-starts with `[ -z "$TMUX" ] && [ -z "$THESH_MUX" ] && …`.
* Full-screen apps work: each pane emulates its own terminal (cursor
  addressing, alternate screen, colors, scroll regions) and repaints
  only its rectangle; resizing the terminal re-lays every pane out and
  resizes the children. **Your binds keep working while a program
  runs** — split, kill, move focus without leaving htop first — and
  the caret re-lays out with the window (no keypress needed). A
  prompt following program output without a newline keeps that tail
  instead of overwriting it.

### Key bindings

Shortcuts are defined in the config with a `bind = ` prefix, `+`-joined
modifiers and a key in the middle, and an action on the right. The bare form
(without the prefix) is still accepted everywhere:

```sh
bind = ALT + T = exec('top')          # run a command
bind = CTRL + ALT + V = paste         # paste the line clipboard
bind = CTRL + ALT + C = copy          # copy the current line (also OSC 52)
bind = CTRL + C = close               # exit the shell
bind = ALT + M = "exec('micro') ask(path?)"
bind = CTRL + Z = "exec('zeta')  ask(action?)"
bind = SHIFT + LEFT = exec('select-mode')   # shift + arrow
bind = SHIFT + TAB = exec('other-window')   # shift + tab (CSI Z)
bind = SUPER + UP = exec('echo super pressed')  # Windows/Super key + arrow
bind = F11 = exec('toggle')                # plain function key, no modifier
bind = ALT + F5 = exec('reload')
```

> **Shared bytes:** `CTRL + M` / `CTRL + J` *are* Enter (`0x0D` / `0x0A`)
> and `CTRL + H` *is* Backspace (`0x08`) — no terminal can tell those
> combos apart from the plain key (and terminals disagree on which byte
> Backspace sends: `0x7f` directly, `^H` through tmux — thesh accepts
> both). Binds for these bytes therefore fire **only when the line is
> empty**, where Enter/Backspace do nothing anyway: while there's text on
> the line, submit and delete always win. So `bind = CTRL + M =
> exec('manual')` works from the idle prompt without ever eating your
> Enter key. Every other key is fair game (including `CTRL + C`, if you
> really want to bind `close` to it).

* **Modifiers** — `CTRL`, `ALT`, `SUPER` (the Windows/Super key; `WIN`,
  `WINDOWS`, `META` are also accepted) and `SHIFT`, joined with `+`. Keys are
  a single letter, digit or symbol (case-insensitive), or a name: `ENTER TAB
  SPACE BACKSPACE DELETE UP DOWN LEFT RIGHT HOME END PGUP PGDN ESC F1` through
  `F24` — arrows may also be spelled `L-ARROW`/`R-ARROW`/`U-ARROW`/`D-ARROW`.
  A modifier is optional — `bind = F11 = …` binds the plain function
  key. Terminals report Super/Meta combinations as CSI modifiers (9–16 or
  33–40), which the shell decodes to `SUPER`, so `SUPER + UP` works with the
  arrow keys.
* **Shift** — `SHIFT + …` binds fire for any key the terminal reports with an
  xterm modifier param: arrows, `HOME`/`END`, `PGUP`/`PGDN`, `F1`–`F24`, plus
  Tab via `CSI Z` (`SHIFT + TAB`). Combos work too (`CTRL + SHIFT + LEFT`).
  When no `SHIFT` bind exists for a key, the shifted press falls back to that
  key's plain bind — so `SUPER + UP` still fires on Super+Shift+Up. What
  *can't* carry a shift bind: plain letters/digits/symbols and `SHIFT + ENTER`
  (terminals send a capital or Enter as a plain byte with no shift bit — the
  shell warns and rejects `SHIFT + A` style lines).
* **Actions**:
  * `exec('cmd')` — run `cmd` (useful for shortcuts to interactive tools).
  * `ask(label)` — after `exec(...)`: prompt `label : ` on its own line, then
    append the typed text to the command. `bind = ALT + M = "exec('micro') ask(path?)"`
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