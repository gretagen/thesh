#include "thesh.h"

/* ── User key bindings ──────────────────────────────────────────────
 * Config syntax (wm-style prefix form — canonical):
 *     bind = ALT  + T = exec('top')
 *     bind = CTRL + ALT + V = paste
 *     bind = SHIFT + LEFT = exec('select-mode')
 *     bind = F11 = exec('something')
 * The bare form (`ALT + T = exec('top')`) stays fully supported.
 * Left side: one or more of CTRL/ALT/SUPER/SHIFT joined with `+`, ending
 * in a key (single letter/digit/symbol, or a name such as ENTER/TAB/UP/
 * .../F1..F24). SUPER also matches WIN/WINDOWS/META in config lines.
 * SHIFT works for keys that carry an xterm modifier param (arrows,
 * Home/End, PgUp/PgDn, F-keys) plus Tab (CSI Z); plain letters and
 * Shift+Enter are indistinguishable from plain input in a terminal.
 * Right side: `close`, `copy`, `paste`, or `exec('cmd')` optionally
 * followed by `ask(label)` which prompts for one line appended to cmd. */

static Bind binds[BIND_MAX];
static int  nbinds = 0;

static int ieq(const char *a, const char *b)
{
    return strcasecmp(a, b) == 0;
}

static char *svtrim(char *s)
{
    while (*s && isspace((unsigned char)*s)) s++;
    char *e = s + strlen(s);
    while (e > s && isspace((unsigned char)e[-1])) e--;
    *e = 0;
    return s;
}

static int mod_from(const char *t)
{
    if (ieq(t, "CTRL"))  return MOD_CTRL;
    if (ieq(t, "ALT"))   return MOD_ALT;
    if (ieq(t, "SHIFT")) return MOD_SHIFT;
    if (ieq(t, "SUPER") || ieq(t, "WIN") || ieq(t, "WINDOWS") ||
        ieq(t, "META")) return MOD_SUPER;
    return 0;
}

/* Map a key token to its key code (ASCII char or K_*). 0 = invalid. */
static int key_from(const char *t)
{
    if (t[0] && !t[1]) {
        unsigned char c = (unsigned char)t[0];
        if (isalpha(c)) c = (unsigned char)tolower(c);
        return (int)c;
    }
    if (ieq(t, "ENTER"))       return K_ENTER;
    if (ieq(t, "TAB"))         return K_TAB;
    if (ieq(t, "SPACE"))       return ' ';
    if (ieq(t, "BACKSPACE") || ieq(t, "BACK")) return K_DEL;
    if (ieq(t, "DELETE")   || ieq(t, "DEL"))   return K_FDEL;
    if (ieq(t, "UP"))          return K_UP;
    if (ieq(t, "DOWN"))        return K_DOWN;
    if (ieq(t, "LEFT"))        return K_LEFT;
    if (ieq(t, "RIGHT"))       return K_RIGHT;
    /* R-ARROW style tokens (arrive as word-keys when modified) */
    if (ieq(t, "U-ARROW") || ieq(t, "UP-ARROW"))     return K_UP;
    if (ieq(t, "D-ARROW") || ieq(t, "DOWN-ARROW"))   return K_DOWN;
    if (ieq(t, "L-ARROW") || ieq(t, "LEFT-ARROW"))   return K_LEFT;
    if (ieq(t, "R-ARROW") || ieq(t, "RIGHT-ARROW"))  return K_RIGHT;
    if (ieq(t, "HOME"))        return K_HOME;
    if (ieq(t, "END"))         return K_END;
    if (ieq(t, "PGUP") || ieq(t, "PAGEUP"))     return K_PGUP;
    if (ieq(t, "PGDN") || ieq(t, "PAGEDOWN"))   return K_PGDN;
    if (ieq(t, "ESC")  || ieq(t, "ESCAPE"))     return 27;
    if (t[0] == 'F' && t[1] && isdigit((unsigned char)t[1])) {
        char *end = NULL;
        long n = strtol(t + 1, &end, 10);
        if (end && !*end && n >= 1 && n <= 24)
            return K_F1 + (int)n - 1;
    }
    return 0;
}

/* Parse the flattened `action` string into bind fields. */
static void parse_exec_action(char *a, Bind *b)
{
    const char *q = NULL;
    const char *p = a;
    while (*p && *p != '(') p++;
    if (*p == '(') {
        p++;
        if (*p == '"' || *p == '\'') {
            char cq = *p++;
            size_t i = 0;
            while (*p && *p != cq && i < BIND_CMD_MAX - 1) b->cmd[i++] = *p++;
            b->cmd[i] = 0;
        } else {
            size_t i = 0;
            while (*p && *p != ')' && i < BIND_CMD_MAX - 1) b->cmd[i++] = *p++;
            b->cmd[i] = 0;
        }
    }
    q = strstr(p, "ask");
    if (q && (q[3] == '(' || isspace((unsigned char)q[3]))) {
        const char *ap = q + 3;
        while (*ap && *ap != '(') ap++;
        if (*ap == '(') {
            ap++;
            if (*ap == '"' || *ap == '\'') {
                char cq = *ap++;
                size_t i = 0;
                while (*ap && *ap != cq && i < BIND_ASK_MAX - 1) b->ask[i++] = *ap++;
                b->ask[i] = 0;
            } else {
                size_t i = 0;
                while (*ap && *ap != ')' && i < BIND_ASK_MAX - 1) b->ask[i++] = *ap++;
                b->ask[i] = 0;
            }
        }
    }
}

/* Is `t` a bindable named key (for plain-key binds like `F11 = ...`)? */
static int is_key_token(const char *t)
{
    if (t[0] == 'F' && t[1] && isdigit((unsigned char)t[1])) {
        char *end = NULL;
        long n = strtol(t + 1, &end, 10);
        if (end && !*end && n >= 1 && n <= 24) return 1;
    }
    return ieq(t, "ENTER") || ieq(t, "TAB") || ieq(t, "SPACE") ||
           ieq(t, "BACKSPACE") || ieq(t, "BACK") ||
           ieq(t, "DELETE") || ieq(t, "DEL") ||
           ieq(t, "UP") || ieq(t, "DOWN") || ieq(t, "LEFT") || ieq(t, "RIGHT") ||
           ieq(t, "U-ARROW") || ieq(t, "UP-ARROW") ||
           ieq(t, "D-ARROW") || ieq(t, "DOWN-ARROW") ||
           ieq(t, "L-ARROW") || ieq(t, "LEFT-ARROW") ||
           ieq(t, "R-ARROW") || ieq(t, "RIGHT-ARROW") ||
           ieq(t, "HOME") || ieq(t, "END") ||
           ieq(t, "PGUP") || ieq(t, "PAGEUP") ||
           ieq(t, "PGDN") || ieq(t, "PAGEDOWN") ||
           ieq(t, "ESC") || ieq(t, "ESCAPE");
}

/* Is `a` a multiplexer verb action (`split-window-vertical 50`,
 * `close-pane`, `focus-next`, `panes`, …)? `split-window-vertial` (the
 * typo in a long-standing rc) is tolerated as an alias. */
static int is_verb_action(const char *a)
{
    char w[48];
    size_t i = 0;
    while (a[i] && !isspace((unsigned char)a[i]) && i < sizeof w - 1) {
        w[i] = a[i];
        i++;
    }
    w[i] = 0;
    const char *rest = a + i;
    while (*rest && isspace((unsigned char)*rest)) rest++;

    if (!strcmp(w, "split-window-vertical") ||
        !strcmp(w, "split-window-vertial") ||
        !strcmp(w, "split-window-horizontal")) {
        if (!*rest) return 1;
        char *end = NULL;
        long n = strtol(rest, &end, 10);
        if (end == rest) return 0;
        while (*end && isspace((unsigned char)*end)) end++;
        return *end == 0 && n >= 0 && n <= 100;
    }
    if (*rest) return 0;
    return !strcmp(w, "close-pane") || !strcmp(w, "panes") ||
           !strcmp(w, "kill-curent-window") ||
           !strcmp(w, "kill-current-window") ||
           !strcmp(w, "kill-all-windows") ||
           !strcmp(w, "move-to-right-window") ||
           !strcmp(w, "move-to-left-window") ||
           !strcmp(w, "move-to-up-window") ||
           !strcmp(w, "move-to-down-window") ||
           !strcmp(w, "focus-left")  || !strcmp(w, "focus-right") ||
           !strcmp(w, "focus-up")    || !strcmp(w, "focus-down") ||
           !strcmp(w, "focus-next")  || !strcmp(w, "focus-prev");
}

/* Parse one config line that looks like a key binding. Returns 1 if the
 * line was consumed as a binding (even when malformed), else 0. */
int bind_parse_line(const char *line)
{
    char buf[512];
    snprintf(buf, sizeof buf, "%s", line);
    char *t = svtrim(buf);
    if (!*t || *t == '#') return 0;

    /* wm-style prefix: `bind = ALT + C = action` (also `bind :` — the
     * legacy separator is accepted like everywhere else in the config).
     * The prefix is stripped here and the remainder runs through the
     * exact same grammar as the bare form, which stays supported. */
    if (strncasecmp(t, "bind", 4) == 0 &&
        (t[4] == 0 || isspace((unsigned char)t[4]) ||
         t[4] == '=' || t[4] == ':')) {
        char *p = t + 4;
        while (isspace((unsigned char)*p)) p++;
        if (*p != '=' && *p != ':') {
            dprintf(STDERR_FILENO, "%s: bind: expected '=' after 'bind'\n",
                    THESH_NAME);
            return 1;
        }
        t = svtrim(p + 1);
        if (!*t) {
            dprintf(STDERR_FILENO, "%s: bind: nothing to bind after 'bind = '\n",
                    THESH_NAME);
            return 1;
        }
    }

    /* First word must be a modifier (CTRL/ALT/SUPER/SHIFT/...) or a named
     * so plain `F11 = ...` / `UP = ...` binds parse, while ordinary
     * command/config lines fall through. */
    {
        const char *sp = t;
        while (*sp && !isspace((unsigned char)*sp) && *sp != '+' && *sp != '=') sp++;
        size_t fl = (size_t)(sp - t);
        if (fl == 0 || fl >= 16) return 0;
        char first[16];
        memcpy(first, t, fl);
        first[fl] = 0;
        if (!mod_from(first) && !is_key_token(first)) return 0;
    }

    char *eq = strchr(t, '=');
    if (!eq) return 0;
    *eq = 0;
    char *action = svtrim(eq + 1);
    if (!*action) return 0;

    char *toks[16];
    int nt = 0;
    char *save = NULL;
    for (char *tok = strtok_r(t, "+", &save); tok && nt < 16;
         tok = strtok_r(NULL, "+", &save))
        toks[nt++] = svtrim(tok);
    if (nt < 1) return 0;

    int mods = 0;
    for (int i = 0; i < nt - 1; i++) {
        int m = mod_from(toks[i]);
        if (!m) {
            dprintf(STDERR_FILENO, "%s: bind: unknown modifier '%s' (use CTRL/ALT/SUPER/SHIFT)\n",
                    THESH_NAME, toks[i]);
            return 1;
        }
        mods |= m;
    }
    int key = key_from(toks[nt - 1]);
    if (!key) {
        dprintf(STDERR_FILENO, "%s: bind: unknown key '%s'\n",
                THESH_NAME, toks[nt - 1]);
        return 1;
    }

    /* SHIFT + <single character> can never fire: the terminal sends a
     * capital letter/symbol as a plain byte with no shift distinction.
     * Reject it loudly instead of storing a dead binding. */
    if ((mods & MOD_SHIFT) && ((key >= 32 && key <= 126) || key == 27)) {
        dprintf(STDERR_FILENO, "%s: bind: SHIFT + plain keys never fire "
                "(terminals send letters/symbols without a shift bit)\n",
                THESH_NAME);
        return 1;
    }

    Bind b;
    memset(&b, 0, sizeof b);
    b.mods = mods;
    b.key = key;

    size_t al = strlen(action);
    if (al >= 2 && (action[0] == '"' || action[0] == '\'') &&
        action[al - 1] == action[0]) {
        memmove(action, action + 1, al - 2);
        action[al - 2] = 0;
    }

    if (ieq(action, "close")) {
        b.kind = BIND_CLOSE;
    } else if (ieq(action, "copy")) {
        b.kind = BIND_COPY;
    } else if (ieq(action, "paste")) {
        b.kind = BIND_PASTE;
    } else if (strncasecmp(action, "exec", 4) == 0 &&
               (action[4] == '(' || isspace((unsigned char)action[4]))) {
        b.kind = BIND_EXEC;
        parse_exec_action(action, &b);
        if (!b.cmd[0]) {
            dprintf(STDERR_FILENO, "%s: bind: exec action needs a command\n",
                    THESH_NAME);
            return 1;
        }
    } else if (is_verb_action(action)) {
        b.kind = BIND_VERB;
        snprintf(b.cmd, sizeof b.cmd, "%s", action);
    } else {
        dprintf(STDERR_FILENO, "%s: bind: unknown action '%s'\n",
                THESH_NAME, action);
        return 1;
    }

    for (int i = 0; i < nbinds; i++) {
        if (binds[i].mods == b.mods && binds[i].key == b.key) {
            binds[i] = b;                       /* last definition wins */
            return 1;
        }
    }
    if (nbinds < BIND_MAX) {
        binds[nbinds++] = b;
    } else {
        dprintf(STDERR_FILENO, "%s: bind: too many bindings (%d max)\n",
                THESH_NAME, BIND_MAX);
    }
    return 1;
}

const Bind *bind_lookup(int mods, int key)
{
    /* Ali/Ctrl+arrow presses arrive as the word-move keys (K_WLEFT /
     * K_WRIGHT), so a bind stored as (ALT, K_LEFT) must also match the
     * arrival (ALT, K_WLEFT). Exact candidates come first, then the
     * word-key alias, with and without Shift. */
    int keys[2];
    keys[0] = key;
    keys[1] = (key == K_WLEFT) ? K_LEFT :
              (key == K_WRIGHT) ? K_RIGHT : key;
    int nkeys = (keys[1] != keys[0]) ? 2 : 1;

    int modset[2];
    modset[0] = mods;
    modset[1] = mods & ~MOD_SHIFT;
    int nmodset = (modset[1] != mods) ? 2 : 1;

    for (int m = 0; m < nmodset; m++)
        for (int k = 0; k < nkeys; k++)
            for (int i = 0; i < nbinds; i++)
                if (binds[i].mods == modset[m] && binds[i].key == keys[k])
                    return &binds[i];
    return NULL;
}

/* ── Preset export ────────────────────────────────────────────────── */
static const char *key_token_name(int key)
{
    switch (key) {
    case K_ENTER:     return "ENTER";
    case K_TAB:       return "TAB";
    case ' ':         return "SPACE";
    case K_DEL:       return "BACKSPACE";
    case K_FDEL:      return "DELETE";
    case K_UP:        return "UP";
    case K_DOWN:      return "DOWN";
    case K_LEFT:      return "LEFT";
    case K_RIGHT:     return "RIGHT";
    case K_HOME:      return "HOME";
    case K_END:       return "END";
    case K_PGUP:      return "PAGEUP";
    case K_PGDN:      return "PAGEDOWN";
    case 27:          return "ESC";
    default: {
        if (key >= K_F1 && key <= K_F24) {
            static char fb[8];
            snprintf(fb, sizeof fb, "F%d", key - K_F1 + 1);
            return fb;
        }
        return NULL;
    }
    }
}

static void dump_bind_action(FILE *f, const Bind *b)
{
    switch (b->kind) {
    case BIND_CLOSE: fputs("close\n", f); break;
    case BIND_COPY:  fputs("copy\n", f); break;
    case BIND_PASTE: fputs("paste\n", f); break;
    case BIND_EXEC:
        fputs("exec(", f);
        if (!strchr(b->cmd, '\'')) fprintf(f, "'%s'", b->cmd);
        else                       fprintf(f, "\"%s\"", b->cmd);
        fputc(')', f);
        if (b->ask[0]) {
            fputs(" ask(", f);
            if (!strchr(b->ask, '\'')) fprintf(f, "'%s'", b->ask);
            else                       fprintf(f, "\"%s\"", b->ask);
            fputc(')', f);
        }
        fputc('\n', f);
        break;
    case BIND_VERB:
        fputs(b->cmd, f);
        fputc('\n', f);
        break;
    }
}

/* Serialize every binding back into config syntax (used by savepreset).
 * Canonical wm-style form: `bind = CTRL + SHIFT + LEFT = exec('…')`.
 * A modifier-less bind emits `bind = F11 = …` (no stray separator). */
void bind_dump(FILE *f)
{
    for (int i = 0; i < nbinds; i++) {
        const Bind *b = &binds[i];
        const char *sep = "";
        fputs("bind = ", f);
        if (b->mods & MOD_CTRL)  { fputs("CTRL", f);  sep = " + "; }
        if (b->mods & MOD_ALT)   { fprintf(f, "%sALT", sep);   sep = " + "; }
        if (b->mods & MOD_SUPER) { fprintf(f, "%sSUPER", sep); sep = " + "; }
        if (b->mods & MOD_SHIFT) { fprintf(f, "%sSHIFT", sep); sep = " + "; }
        fputs(sep, f);
        const char *nm = key_token_name(b->key);
        if (nm) {
            fputs(nm, f);
        } else if (b->key >= 0 && b->key < 256 && isgraph((unsigned char)b->key)) {
            fputc((unsigned char)toupper(b->key), f);
        } else {
            fputc('?', f);
        }
        fputs(" = ", f);
        dump_bind_action(f, b);
    }
}