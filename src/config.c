#include "thesh.h"
#include <time.h>

/* ── Color presets ─────────────────────────────────────────────────── */
typedef struct { const char *name; const char *sgr; } ColorSpec;

static const ColorSpec colors[] = {
    { "black",        "30" },
    { "red",          "31" },
    { "green",        "32" },
    { "yellow",       "33" },
    { "blue",         "34" },
    { "purple",       "35" },   /* magenta */
    { "cyan",         "36" },
    { "white",        "37" },
    { "grey",         "90" },   /* bright black */
    { "pink",         "95" },   /* bright magenta */
    { "brightred",    "91" },
    { "brightgreen",  "92" },
    { "brightyellow", "93" },
    { "brightblue",   "94" },
    { "brightcyan",   "96" },
    { "brightwhite",  "97" },
    { "default",      "39" },
    { "none",          "" },
};
#define NCOLORS ((int)(sizeof colors / sizeof colors[0]))

Config Cfg;

static const char default_looks[] =
    "$RIGHTWALL $SPACER $USER $SPACER $SEPERATOR $SPACER "
    "$HOSTNAME $SPACER $LEFTWALL $SPACER $PATH $SPACER $CURSOR";

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
    Cfg.col_rightwall = NULL;
    Cfg.col_leftwall  = NULL;
    Cfg.col_sep       = NULL;
    Cfg.col_host      = NULL;
    Cfg.col_path      = NULL;
    Cfg.col_cursor    = NULL;
    Cfg.col_guess     = NULL;
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

/* Apply one config directive. Returns 1 if it was a known option
 * (consumed), 0 otherwise so callers can fall through to execution. */
int config_set_option(const char *key, const char *val)
{
    if (!strcmp(key, "looks"))          { set_str(&Cfg.looks, val); return 1; }
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
    return 0;
}

/* ── Known option names (rc / interactive interception) ───────────── */
static const char *known_keys[] = {
    "looks", "rightwallstyle", "leftwallstyle", "seperatorstyle",
    "cursorstyle", "rightwall-color", "leftwall-color", "seperator-color",
    "hostname-color", "path-color", "cursor-color", "guesser-color",
    "typing", "guesser", "corrector",
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
 * any other literal text is kept as typed. */
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
            if (li > 0) { lit[li] = 0; pcat(&out, &cap, &len, lit); li = 0; }

            if (!strcmp(tok, "SPACER")) { pcat(&out, &cap, &len, " "); continue; }

            const char *txt = NULL, *sgr = NULL;
            if      (!strcmp(tok, "USER"))      { txt = user; }
            else if (!strcmp(tok, "HOSTNAME"))  { txt = host; sgr = Cfg.col_host; }
            else if (!strcmp(tok, "SEPERATOR")) { txt = Cfg.sep; sgr = Cfg.col_sep; }
            else if (!strcmp(tok, "RIGHTWALL")) { txt = Cfg.rightwall; sgr = Cfg.col_rightwall; }
            else if (!strcmp(tok, "LEFTWALL"))  { txt = Cfg.leftwall; sgr = Cfg.col_leftwall; }
            else if (!strcmp(tok, "PATH") || !strcmp(tok, "DIR") ||
                     !strcmp(tok, "PWD"))       { txt = dir; sgr = Cfg.col_path; }
            else if (!strcmp(tok, "CURSOR"))    { txt = Cfg.cursor; sgr = Cfg.col_cursor; }
            else {                             /* unknown token → literal */
                if (li < (int)sizeof lit - 1) lit[li++] = '$';
                for (size_t i = 0; i < ti && li < (int)sizeof lit - 1; i++)
                    lit[li++] = tok[i];
                continue;
            }
            pcat_part(&out, &cap, &len, txt, sgr, &colored);
        } else {
            if (li < (int)sizeof lit - 1) lit[li++] = *p;
            p++;
        }
    }

    while (li > 0 && isspace((unsigned char)lit[li - 1])) li--;
    if (li > 0) { lit[li] = 0; pcat(&out, &cap, &len, lit); }
    if (colored) pcat(&out, &cap, &len, "\033[0m");
    return out;
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