#include "thesh.h"
#include <time.h>

/* ── Color presets ─────────────────────────────────────────────────── */
typedef struct {
    const char *name;
    const char *sgr;
    uint8_t r, g, b;        /* sRGB, used for opacity blending */
} ColorSpec;

static const ColorSpec colors[] = {
    { "black",        "30",   0,   0,   0 },
    { "red",          "31", 205,  49,  49 },
    { "green",        "32",  13, 188, 121 },
    { "yellow",       "33", 229, 229,  16 },
    { "blue",         "34",  36, 114, 200 },
    { "purple",       "35", 188,  63, 188 },   /* magenta */
    { "cyan",         "36",  17, 168, 205 },
    { "white",        "37", 229, 229, 229 },
    { "grey",         "90", 102, 102, 102 },   /* bright black */
    { "pink",         "95", 255, 105, 180 },   /* bright magenta */
    { "brightred",    "91", 255,  84,  84 },
    { "brightgreen",  "92",   5, 221, 139 },
    { "brightyellow", "93", 255, 255,  64 },
    { "brightblue",   "94",  90, 140, 255 },
    { "brightcyan",   "96", 102, 229, 255 },
    { "brightwhite",  "97", 255, 255, 255 },
    { "default",      "39",   0,   0,   0 },   /* terminal default fg */
    { "none",          "",   0,   0,   0 },
};
#define NCOLORS ((int)(sizeof colors / sizeof colors[0]))

static const ColorSpec *find_spec(const char *sgr)
{
    if (!sgr) return NULL;
    for (int i = 0; i < NCOLORS; i++)
        if (!strcmp(colors[i].sgr, sgr)) return &colors[i];
    return NULL;
}

Config Cfg;

static const char default_looks[] =
    "$RIGHTWALL $SPACER $USER $SPACER $SEPERATOR $SPACER "
    "$HOSTNAME $SPACER $LEFTWALL $SPACER $PATH $SPACER $CURSOR $SPACER";

const char *config_default_looks(void) { return default_looks; }

/* ── Defaults (no config file present, or fresh shell) ────────────── */
void config_init(void)
{
    Cfg.looks        = NULL;
    Cfg.rightwall    = xstrdup("[");
    Cfg.leftwall     = xstrdup("]");
    Cfg.sep          = xstrdup("|");
    Cfg.cursor       = xstrdup("$");
    Cfg.hybrid       = 1;      /* case-insensitive commands  */
    Cfg.guesser      = 1;      /* ghost suggestions          */
    Cfg.corrector    = 0;      /* passive                    */
    Cfg.autoreload   = 1;      /* re-source rc files on edit */
    Cfg.col_rightwall = NULL;
    Cfg.col_leftwall  = NULL;
    Cfg.col_sep       = NULL;
    Cfg.col_host      = NULL;
    Cfg.col_path      = NULL;
    Cfg.col_cursor    = NULL;
    Cfg.col_guess     = NULL;
    Cfg.col_user      = NULL;
    Cfg.textcolor     = NULL;
    Cfg.op_rightwall  = -1;
    Cfg.op_leftwall   = -1;
    Cfg.op_sep        = -1;
    Cfg.op_host       = -1;
    Cfg.op_user       = -1;
    Cfg.op_path       = -1;
    Cfg.op_cursor     = -1;
    Cfg.op_guess      = -1;
}

/* ── Helpers ──────────────────────────────────────────────────────── */
static int str_bool(const char *v)
{
    if (!v) return 0;
    if (!strcasecmp(v, "yes") || !strcasecmp(v, "on") ||
        !strcasecmp(v, "true") || !strcmp(v, "1")) return 1;
    if (!strcasecmp(v, "no") || !strcasecmp(v, "off") ||
        !strcasecmp(v, "false") || !strcmp(v, "0")) return 0;
    return 0;
}

static void set_str(char **slot, const char *v)
{
    free(*slot);
    *slot = v ? xstrdup(v) : NULL;
}

/* Resolve a color name to its SGR code string. "none"/unknown → no
 * color; unknown names warn once. */
static void set_color(char **slot, const char *v)
{
    free(*slot);
    *slot = NULL;
    if (!v || !*v) return;
    for (int i = 0; i < NCOLORS; i++) {
        if (!strcasecmp(colors[i].name, v)) {
            if (colors[i].sgr[0]) *slot = xstrdup(colors[i].sgr);
            return;
        }
    }
    dprintf(STDERR_FILENO, "%s: unknown color: '%s'\n", THESH_NAME, v);
}

/* Parse `100%` / `65` / `'65%'`. Stores 0-100, or -1 when the value is
 * empty/unparsable. Returns 1 when a value was supplied. */
static int parse_opacity(const char *v, int *out)
{
    *out = -1;
    if (!v || !*v) return 1;
    const char *s = v;
    if (*s == '"' || *s == '\'') s++;
    while (*s && isspace((unsigned char)*s)) s++;
    char *end = NULL;
    long n = strtol(s, &end, 10);
    if (end == s) return 0;
    if (*end == '%') end++;
    if (n < 0) n = 0;
    if (n > 100) n = 100;
    *out = (int)n;
    return 1;
}

/* Apply one config directive. Returns 1 if it was a known option
 * (consumed), 0 otherwise so callers can fall through to execution. */
int config_set_option(const char *key, const char *val)
{
    if (!strcmp(key, "looks") || !strcmp(key, "layout")) { set_str(&Cfg.looks, val); return 1; }
    if (!strcmp(key, "rightwallstyle")) { set_str(&Cfg.rightwall, val); return 1; }
    if (!strcmp(key, "leftwallstyle"))  { set_str(&Cfg.leftwall, val); return 1; }
    if (!strcmp(key, "seperatorstyle")) { set_str(&Cfg.sep, val); return 1; }
    if (!strcmp(key, "cursorstyle"))    { set_str(&Cfg.cursor, val); return 1; }
    if (!strcmp(key, "rightwall-color")) { set_color(&Cfg.col_rightwall, val); return 1; }
    if (!strcmp(key, "leftwall-color"))  { set_color(&Cfg.col_leftwall, val); return 1; }
    if (!strcmp(key, "seperator-color")) { set_color(&Cfg.col_sep, val); return 1; }
    if (!strcmp(key, "hostname-color"))  { set_color(&Cfg.col_host, val); return 1; }
    if (!strcmp(key, "path-color"))      { set_color(&Cfg.col_path, val); return 1; }
    if (!strcmp(key, "cursor-color"))    { set_color(&Cfg.col_cursor, val); return 1; }
    if (!strcmp(key, "guesser-color"))   { set_color(&Cfg.col_guess, val); return 1; }
    if (!strcmp(key, "user-color"))      { set_color(&Cfg.col_user, val); return 1; }
    if (!strcmp(key, "textcolor"))       { set_color(&Cfg.textcolor, val); return 1; }
    if (!strcmp(key, "rightwall-opacity")) { parse_opacity(val, &Cfg.op_rightwall); return 1; }
    if (!strcmp(key, "leftwall-opacity"))  { parse_opacity(val, &Cfg.op_leftwall); return 1; }
    if (!strcmp(key, "seperator-opacity")) { parse_opacity(val, &Cfg.op_sep); return 1; }
    if (!strcmp(key, "hostname-opacity"))  { parse_opacity(val, &Cfg.op_host); return 1; }
    if (!strcmp(key, "user-opacity"))      { parse_opacity(val, &Cfg.op_user); return 1; }
    if (!strcmp(key, "path-opacity"))      { parse_opacity(val, &Cfg.op_path); return 1; }
    if (!strcmp(key, "cursor-opacity"))    { parse_opacity(val, &Cfg.op_cursor); return 1; }
    if (!strcmp(key, "guesser-opacity"))   { parse_opacity(val, &Cfg.op_guess); return 1; }
    if (!strcmp(key, "typing")) {
        Cfg.hybrid = (val && !strcasecmp(val, "hybrid")) ? 1 : 0;
        return 1;
    }
    if (!strcmp(key, "guesser")) { Cfg.guesser = str_bool(val); return 1; }
    if (!strcmp(key, "corrector")) {
        if (!val)                        Cfg.corrector = 0;
        else if (!strcasecmp(val, "active"))   Cfg.corrector = 1;
        else if (!strcasecmp(val, "consent"))  Cfg.corrector = 2;
        else if (!strcasecmp(val, "inactive")) Cfg.corrector = 3;
        else Cfg.corrector = 0;                  /* passive */
        return 1;
    }
    if (!strcmp(key, "autoreload")) { Cfg.autoreload = str_bool(val); return 1; }
    return 0;
}

/* ── Known option names (rc / interactive interception) ───────────── */
static const char *known_keys[] = {
    "looks", "layout", "rightwallstyle", "leftwallstyle", "seperatorstyle",
    "cursorstyle", "rightwall-color", "leftwall-color", "seperator-color",
    "hostname-color", "path-color", "cursor-color", "guesser-color",
    "user-color", "textcolor",
    "rightwall-opacity", "leftwall-opacity", "seperator-opacity",
    "hostname-opacity", "user-opacity", "path-opacity", "cursor-opacity",
    "guesser-opacity",
    "typing", "guesser", "corrector", "autoreload",
};

static int config_is_key(const char *key)
{
    size_t n = sizeof known_keys / sizeof known_keys[0];
    for (size_t i = 0; i < n; i++)
        if (!strcmp(known_keys[i], key)) return 1;
    return 0;
}

/* Parse one line as a config directive. Accepts `key = value` and
 * `key : value`, with single/double-quoted or bare values. Returns 1 if
 * the line was a config directive (known key), else 0. */
int config_apply_line(const char *line)
{
    const char *p = line;
    while (*p && isspace((unsigned char)*p)) p++;
    if (!*p || *p == '#') return 0;

    if (bind_parse_line(line)) return 1;        /* user key binding */

    const char *k = p;
    while (*p && !isspace((unsigned char)*p) && *p != '=' && *p != ':') p++;
    size_t klen = (size_t)(p - k);
    if (klen == 0) return 0;

    char key[64];
    if (klen >= sizeof key) klen = sizeof key - 1;
    memcpy(key, k, klen);
    key[klen] = 0;

    while (*p && isspace((unsigned char)*p)) p++;
    if (*p != '=' && *p != ':') return 0;
    p++;
    while (*p && isspace((unsigned char)*p)) p++;

    if (!config_is_key(key)) return 0;

    size_t vlen = strlen(p);
    char *val = xmalloc(vlen + 1);
    size_t vi = 0;
    if (vlen >= 2 && (*p == '"' || *p == '\'') && p[vlen - 1] == *p) {
        memcpy(val, p + 1, vlen - 2);
        vi = vlen - 2;
    } else if (*p == '"' || *p == '\'') {       /* tolerate a missing close */
        memcpy(val, p + 1, vlen - 1);
        vi = vlen - 1;
    } else {
        memcpy(val, p, vlen + 1);
        vi = vlen;
    }
    val[vi] = 0;

    config_set_option(key, val);
    free(val);
    return 1;
}

/* ── Prompt rendering ─────────────────────────────────────────────── */
static void pcat(char **out, size_t *cap, size_t *len, const char *s)
{
    size_t sl = strlen(s);
    if (*len + sl + 1 > *cap) {
        while (*cap < *len + sl + 64) *cap *= 2;
        *out = xrealloc(*out, *cap);
    }
    memcpy(*out + *len, s, sl + 1);
    *len += sl;
}

static void pcat_part(char **out, size_t *cap, size_t *len,
                      const char *s, const char *sgr, int *colored)
{
    if (!s || !*s) return;
    if (sgr && sgr[0]) {
        char wrap[16];
        snprintf(wrap, sizeof wrap, "\033[%sm", sgr);
        pcat(out, cap, len, wrap);
        *colored = 1;
    }
    pcat(out, cap, len, s);
}

/* Render a looks template. Keywords: $USER $HOSTNAME $SEPERATOR
 * $RIGHTWALL $LEFTWALL $PATH (or $DIR/$PWD) $CURSOR $SPACER. Literal
 * whitespace between tokens is ignored — use $SPACER for explicit gaps;
 * any other literal text is kept as typed. Each element takes its own
 * color when set, otherwise `textcolor`; opacity blends toward the
 * terminal default (assumed dark background). */
static void pcat_lit(char **out, size_t *cap, size_t *len, char *lit, int *colored)
{
    char sgb[24] = "";
    if (Cfg.textcolor) color_sgr(Cfg.textcolor, 100, sgb, sizeof sgb);
    pcat_part(out, cap, len, lit, sgb[0] ? sgb : NULL, colored);
}

char *render_looks_str(const char *tpl, const char *user, const char *host,
                       const char *dir)
{
    size_t cap = 256, len = 0;
    char *out = xmalloc(cap);
    out[0] = 0;
    int colored = 0;

    char lit[LINE_MAX_CP];
    int li = 0;
    const char *p = tpl;

    while (*p) {
        if (*p == '$') {
            const char *q = p + 1;
            char tok[32];
            size_t ti = 0;
            while (*q && ti < sizeof tok - 1 &&
                   (isalnum((unsigned char)*q) || *q == '_'))
                tok[ti++] = *q++;
            tok[ti] = 0;
            p = q;

            if (ti == 0) {                  /* lone '$' */
                if (li < (int)sizeof lit - 1) lit[li++] = '$';
                continue;
            }

            while (li > 0 && isspace((unsigned char)lit[li - 1])) li--;
            if (li > 0) { lit[li] = 0; pcat_lit(&out, &cap, &len, lit, &colored); li = 0; }

            if (!strcmp(tok, "SPACER")) { pcat(&out, &cap, &len, " "); continue; }

            const char *txt = NULL, *csgr = NULL;
            int eop = 100;
            if      (!strcmp(tok, "USER"))      { txt = user; csgr = Cfg.col_user; eop = Cfg.op_user; }
            else if (!strcmp(tok, "HOSTNAME"))  { txt = host; csgr = Cfg.col_host; eop = Cfg.op_host; }
            else if (!strcmp(tok, "SEPERATOR")) { txt = Cfg.sep; csgr = Cfg.col_sep; eop = Cfg.op_sep; }
            else if (!strcmp(tok, "RIGHTWALL")) { txt = Cfg.rightwall; csgr = Cfg.col_rightwall; eop = Cfg.op_rightwall; }
            else if (!strcmp(tok, "LEFTWALL"))  { txt = Cfg.leftwall; csgr = Cfg.col_leftwall; eop = Cfg.op_leftwall; }
            else if (!strcmp(tok, "PATH") || !strcmp(tok, "DIR") ||
                     !strcmp(tok, "PWD"))       { txt = dir; csgr = Cfg.col_path; eop = Cfg.op_path; }
            else if (!strcmp(tok, "CURSOR"))    { txt = Cfg.cursor; csgr = Cfg.col_cursor; eop = Cfg.op_cursor; }
            else {                             /* unknown token → literal */
                if (li < (int)sizeof lit - 1) lit[li++] = '$';
                for (size_t i = 0; i < ti && li < (int)sizeof lit - 1; i++)
                    lit[li++] = tok[i];
                continue;
            }
            if (!csgr) csgr = Cfg.textcolor;    /* general text color */
            char sgb[24] = "";
            if (csgr) color_sgr(csgr, eop, sgb, sizeof sgb);
            pcat_part(&out, &cap, &len, txt, sgb[0] ? sgb : NULL, &colored);
        } else {
            if (li < (int)sizeof lit - 1) lit[li++] = *p;
            p++;
        }
    }

    while (li > 0 && isspace((unsigned char)lit[li - 1])) li--;
    if (li > 0) { lit[li] = 0; pcat_lit(&out, &cap, &len, lit, &colored); }
    if (colored) pcat(&out, &cap, &len, "\033[0m");
    return out;
}

/* ── SGR emission with opacity ────────────────────────────────────── */
/* Write the SGR number(s) for a stored color string (or its name) at
 * the given opacity (0-100, -1 = unset = full). `default`/unset emit
 * nothing below 100% opacity; below 100% the color is blended toward
 * black and emitted as a 256-color foreground code. */
static int cube_index(int r, int g, int b)
{
    int best = 16, bestd = INT_MAX;
    for (int ri = 0; ri < 6; ri++) {
        int rv = ri == 0 ? 0 : 95 + (ri - 1) * 40;
        for (int gi = 0; gi < 6; gi++) {
            int gv = gi == 0 ? 0 : 95 + (gi - 1) * 40;
            for (int bi = 0; bi < 6; bi++) {
                int bv = bi == 0 ? 0 : 95 + (bi - 1) * 40;
                int dr = r - rv, dg = g - gv, db = b - bv;
                int d = dr * dr + dg * dg + db * db;
                if (d < bestd) { bestd = d; best = 16 + 36 * ri + 6 * gi + bi; }
            }
        }
    }
    return best;
}

void color_sgr(const char *sgr, int opacity, char *buf, size_t sz)
{
    buf[0] = 0;
    if (!sgr || !sgr[0]) return;
    if (opacity < 0) opacity = 100;
    if (opacity > 100) opacity = 100;

    if (!strcmp(sgr, "39")) {               /* `default`: strictly default */
        if (opacity >= 100) snprintf(buf, sz, "39");
        return;
    }
    const ColorSpec *sp = find_spec(sgr);
    if (!sp) { snprintf(buf, sz, "%s", sgr); return; }
    if (opacity >= 100) { snprintf(buf, sz, "%s", sp->sgr); return; }

    int r = (int)sp->r * opacity / 100;
    int g = (int)sp->g * opacity / 100;
    int b = (int)sp->b * opacity / 100;
    snprintf(buf, sz, "38;5;%d", cube_index(r, g, b));
}

/* ── Preset export ────────────────────────────────────────────────── */
/* Write a value as `'...'` (or "..." when it contains a single quote),
 * i.e. in a form config_apply_line re-parses verbatim. */
static void fqval(FILE *f, const char *v)
{
    if (!strchr(v, '\'')) fprintf(f, "'%s'", v);
    else                  fprintf(f, "\"%s\"", v);
}

static const char *color_name_of(const char *sgr)
{
    if (!sgr) return NULL;
    for (int i = 0; i < NCOLORS; i++)
        if (colors[i].sgr[0] && !strcmp(colors[i].sgr, sgr))
            return colors[i].name;
    return NULL;
}

/* Serialize the effective config as a theshrc-style preset. Aliases and
 * key bindings are appended by the callers (exec.c / bind.c). */
void config_dump_current(FILE *f)
{
    struct { const char *key; char **slot; } cols[] = {
        { "rightwall-color", &Cfg.col_rightwall },
        { "leftwall-color",  &Cfg.col_leftwall },
        { "seperator-color", &Cfg.col_sep },
        { "hostname-color",  &Cfg.col_host },
        { "user-color",      &Cfg.col_user },
        { "path-color",      &Cfg.col_path },
        { "cursor-color",    &Cfg.col_cursor },
        { "guesser-color",   &Cfg.col_guess },
        { "textcolor",       &Cfg.textcolor },
    };
    struct { const char *key; int v; } ops[] = {
        { "rightwall-opacity", Cfg.op_rightwall },
        { "leftwall-opacity",  Cfg.op_leftwall },
        { "seperator-opacity", Cfg.op_sep },
        { "hostname-opacity",  Cfg.op_host },
        { "user-opacity",      Cfg.op_user },
        { "path-opacity",      Cfg.op_path },
        { "cursor-opacity",    Cfg.op_cursor },
        { "guesser-opacity",   Cfg.op_guess },
    };

    fprintf(f, "# thesh preset\n");
    if (Cfg.looks && Cfg.looks[0]) { fputs("layout = ", f); fqval(f, Cfg.looks); fputc('\n', f); }
    if (Cfg.rightwall && Cfg.rightwall[0]) { fputs("rightwallstyle = ", f); fqval(f, Cfg.rightwall); fputc('\n', f); }
    if (Cfg.leftwall && Cfg.leftwall[0])   { fputs("leftwallstyle = ", f); fqval(f, Cfg.leftwall); fputc('\n', f); }
    if (Cfg.sep && Cfg.sep[0])             { fputs("seperatorstyle = ", f); fqval(f, Cfg.sep); fputc('\n', f); }
    if (Cfg.cursor && Cfg.cursor[0])       { fputs("cursorstyle = ", f); fqval(f, Cfg.cursor); fputc('\n', f); }

    for (size_t i = 0; i < sizeof cols / sizeof cols[0]; i++) {
        char *sgr = *cols[i].slot;
        if (!sgr || !sgr[0]) continue;
        const char *nm = color_name_of(sgr);
        if (!nm) continue;
        fputs(cols[i].key, f);
        fputs(" = ", f);
        fqval(f, nm);
        fputc('\n', f);
    }
    for (size_t i = 0; i < sizeof ops / sizeof ops[0]; i++) {
        if (ops[i].v < 0) continue;
        fprintf(f, "%s = '%d%%'\n", ops[i].key, ops[i].v);
    }

    fprintf(f, "typing = '%s'\n", Cfg.hybrid ? "hybrid" : "lowercase");
    fprintf(f, "guesser = '%s'\n", Cfg.guesser ? "yes" : "no");
    static const char *cor[] = { "passive", "active", "consent", "inactive" };
    fprintf(f, "corrector = '%s'\n",
            (Cfg.corrector >= 0 && Cfg.corrector < 4) ? cor[Cfg.corrector] : "passive");
    fprintf(f, "autoreload = '%s'\n", Cfg.autoreload ? "yes" : "no");
}

/* ── $PS1 expansion (bash-style subset) ───────────────────────────── */
enum { PS1_T_HMS = 0, PS1_T_HM, PS1_T_12H, PS1_T_DATE };

static void ps1_time(char **out, size_t *cap, size_t *len, int which)
{
    time_t t = time(NULL);
    struct tm *tm = localtime(&t);
    if (!tm) return;
    char b[64];
    int n = 0;
    switch (which) {
    case PS1_T_HMS:  n = (int)strftime(b, sizeof b, "%H:%M:%S", tm); break;
    case PS1_T_HM:   n = (int)strftime(b, sizeof b, "%H:%M", tm);    break;
    case PS1_T_12H:  n = (int)strftime(b, sizeof b, "%I:%M %p", tm); break;
    case PS1_T_DATE: n = (int)strftime(b, sizeof b, "%a %b %d", tm); break;
    default: break;
    }
    if (n > 0 && n < (int)sizeof b) pcat(out, cap, len, b);
}

char *render_ps1(const char *ps1, const char *user, const char *host,
                 const char *dir)
{
    size_t cap = 256, len = 0;
    char *out = xmalloc(cap);
    out[0] = 0;

    for (const char *p = ps1; *p; p++) {
        if (p[0] == '\\' && p[1]) {
            char c = p[1];
            switch (c) {
            case 'u': pcat(&out, &cap, &len, user); break;
            case 'h': case 'H': pcat(&out, &cap, &len, host); break;
            case 'w': pcat(&out, &cap, &len, dir); break;
            case 'W': {
                const char *sl = strrchr(dir, '/');
                pcat(&out, &cap, &len, sl ? sl + 1 : dir);
                break;
            }
            case '$': pcat(&out, &cap, &len, getuid() == 0 ? "#" : "$"); break;
            case 's': pcat(&out, &cap, &len, THESH_NAME); break;
            case 'v': pcat(&out, &cap, &len, THESH_VERSION); break;
            case 'n': pcat(&out, &cap, &len, "\n"); break;
            case 'e': pcat(&out, &cap, &len, "\033"); break;
            case '\\': pcat(&out, &cap, &len, "\\"); break;
            case '[': case ']': break;               /* drop markers */
            case 't': ps1_time(&out, &cap, &len, PS1_T_HMS); break;
            case 'A': ps1_time(&out, &cap, &len, PS1_T_HM); break;
            case '@': ps1_time(&out, &cap, &len, PS1_T_12H); break;
            case 'd': ps1_time(&out, &cap, &len, PS1_T_DATE); break;
            case '!': case '#': {
                char b[16];
                snprintf(b, sizeof b, "%d", H.count + 1);
                pcat(&out, &cap, &len, b);
                break;
            }
            case 'j': pcat(&out, &cap, &len, "0"); break;
            default: {
                char two[3] = { '\\', c, 0 };
                pcat(&out, &cap, &len, two);
                break;
            }
            }
            p++;                                 /* skip the escaped char */
        } else {
            char t[2] = { *p, 0 };
            pcat(&out, &cap, &len, t);
        }
    }
    return out;
}