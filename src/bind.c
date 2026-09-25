#include "thesh.h"

/* ── User key bindings ──────────────────────────────────────────────
 * Config syntax:
 *     ALT  + T        = exec('top')
 *     CTRL + ALT + V  = paste
 *     CTRL + C        = close
 *     CTRL + M        = "exec('micro') ask(path?)"
 * Left side: one or more of CTRL/ALT joined with `+`, ending in a key
 * (single letter/digit/symbol, or a name such as ENTER/TAB/UP/...).
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
    if (ieq(t, "CTRL")) return MOD_CTRL;
    if (ieq(t, "ALT"))  return MOD_ALT;
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
    if (ieq(t, "HOME"))        return K_HOME;
    if (ieq(t, "END"))         return K_END;
    if (ieq(t, "PGUP") || ieq(t, "PAGEUP"))     return K_PGUP;
    if (ieq(t, "PGDN") || ieq(t, "PAGEDOWN"))   return K_PGDN;
    if (ieq(t, "ESC")  || ieq(t, "ESCAPE"))     return 27;
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

/* Parse one config line that looks like a key binding. Returns 1 if the
 * line was consumed as a binding (even when malformed), else 0. */
int bind_parse_line(const char *line)
{
    char buf[512];
    snprintf(buf, sizeof buf, "%s", line);
    char *t = svtrim(buf);
    if (!*t || *t == '#') return 0;

    /* First word must be a modifier. */
    {
        const char *sp = t;
        while (*sp && !isspace((unsigned char)*sp) && *sp != '+' && *sp != '=') sp++;
        size_t fl = (size_t)(sp - t);
        if (fl == 0 || fl >= 16) return 0;
        char first[16];
        memcpy(first, t, fl);
        first[fl] = 0;
        if (!ieq(first, "CTRL") && !ieq(first, "ALT")) return 0;
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
    if (nt < 2) return 0;

    int mods = 0;
    for (int i = 0; i < nt - 1; i++) {
        int m = mod_from(toks[i]);
        if (!m) {
            dprintf(STDERR_FILENO, "%s: bind: unknown modifier '%s' (use CTRL/ALT)\n",
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
    for (int i = 0; i < nbinds; i++)
        if (binds[i].mods == mods && binds[i].key == key)
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
    default:          return NULL;
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
    }
}

/* Serialize every binding back into config syntax (used by savepreset). */
void bind_dump(FILE *f)
{
    for (int i = 0; i < nbinds; i++) {
        const Bind *b = &binds[i];
        if (b->mods & MOD_CTRL) fputs("CTRL", f);
        if (b->mods & MOD_ALT) {
            if (b->mods & MOD_CTRL) fputs(" + ALT", f);
            else                    fputs("ALT", f);
        }
        fputs(" + ", f);
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