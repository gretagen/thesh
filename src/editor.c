#include "thesh.h"
#include <stdbool.h>
#include <sys/stat.h>

/* ── Editing buffer (array of codepoints) ─────────────────────────── */
typedef struct {
    uint32_t *v;
    int len, cap, cur;
} Buf;

static void binit(Buf *b)
{
    memset(b, 0, sizeof *b);
    b->cap = 64;
    b->v = xmalloc(sizeof(uint32_t) * (size_t)b->cap);
}

static void bgrow(Buf *b, int need)
{
    if (b->len + need <= b->cap) return;
    while (b->cap < b->len + need) b->cap *= 2;
    b->v = xrealloc(b->v, sizeof(uint32_t) * (size_t)b->cap);
}

static void bins(Buf *b, int pos, uint32_t cp)
{
    if (b->len >= LINE_MAX_CP) return;
    bgrow(b, 1);
    memmove(b->v + pos + 1, b->v + pos, sizeof(uint32_t) * (size_t)(b->len - pos));
    b->v[pos] = cp;
    b->len++;
}

static void bdel(Buf *b, int pos)
{
    if (pos < 0 || pos >= b->len) return;
    memmove(b->v + pos, b->v + pos + 1, sizeof(uint32_t) * (size_t)(b->len - pos - 1));
    b->len--;
}

static void bclr(Buf *b)
{
    b->len = 0;
    b->cur = 0;
}

static void bset_str(Buf *b, const char *str)
{
    bclr(b);
    while (*str) {
        int n;
        uint32_t cp;
        utf8_decode(str, &n, &cp);
        if (b->len < LINE_MAX_CP) b->v[b->len++] = cp;
        str += n;
    }
    b->cur = b->len;
}

static int bcells(const Buf *b, int from, int to)
{
    int c = 0;
    for (int i = from; i < to; i++) c += cp_width(b->v[i]);
    return c;
}

static void bdump(const Buf *b, int from, int to)
{
    char tmp[8];
    for (int i = from; i < to; i++) {
        int l = utf8_encode(b->v[i], tmp);
        outn(tmp, (size_t)l);
    }
}

static char *btext(const Buf *b)
{
    char *s = xmalloc(sizeof(char) * (size_t)(b->len * 4 + 8));
    int n = 0;
    char tmp[8];
    for (int i = 0; i < b->len; i++) {
        int l = utf8_encode(b->v[i], tmp);
        memcpy(s + n, tmp, (size_t)l);
        n += l;
    }
    s[n] = 0;
    return s;
}

static int is_word(uint32_t c)
{
    return isalnum((int)c) || c == '_';
}

/* ── Key reading ──────────────────────────────────────────────────── */
static int read_byte(void)
{
    unsigned char c;
    while (read(STDIN_FILENO, &c, 1) != 1) {
        if (errno != EINTR) return -1;
    }
    return c;
}

/* Decode an xterm modifier number (2=shift, 3/4=alt, 5/6=ctrl,
 * 7/8=alt+ctrl, 9..16=meta, 33..40=super) into MOD_* bits.
 * Shift is deliberately ignored so SUPER+Shift+Up still hits a
 * `SUPER + UP` binding. */
static int mod_bits(int mod)
{
    if (mod <= 1) return 0;
    int m = mod - 1;
    int bits = 0;
    if (m & 2) bits |= MOD_ALT;
    if (m & 4) bits |= MOD_CTRL;
    if (m & 8) bits |= MOD_SUPER;    /* meta */
    if (m & 32) bits |= MOD_SUPER;   /* super */
    return bits;
}

static int parse_seq(const char *s, int n, uint32_t *cp, int *mods)
{
    if (n == 1) { *cp = 27; return K_NONE; }                      /* bare ESC */

    if (n == 2 && s[0] == 27) {                                   /* Alt+X */
        if (s[1] == 'b') { *mods |= MOD_ALT; return K_WLEFT; }
        if (s[1] == 'f') { *mods |= MOD_ALT; return K_WRIGHT; }
        if ((unsigned char)s[1] < 0x20) {                         /* Ctrl+Alt+X */
            *mods |= MOD_CTRL | MOD_ALT;
            *cp = (unsigned char)s[1];
            return K_NONE;
        }
        if ((unsigned char)s[1] == 0x7f) { *mods |= MOD_ALT; *cp = 0x7f; return K_NONE; }
        *mods |= MOD_ALT;
        *cp = (unsigned char)s[1];
        return K_NONE;
    }

    if (n >= 2 && (s[1] == '[' || s[1] == 'O')) {                 /* CSI */
        int num = 0, mod = 1;
        int i = 2;
        while (i < n && isdigit((unsigned char)s[i])) {
            num = num * 10 + (s[i] - '0');
            i++;
        }
        if (num == 0) num = 1;               /* parameter absent → 1 */
        if (i < n && s[i] == ';') i++;
        if (i < n && isdigit((unsigned char)s[i])) {
            int j = i;
            while (j < n && isdigit((unsigned char)s[j])) j++;
            mod = 0;
            for (int k = i; k < j; k++) mod = mod * 10 + (s[k] - '0');
            i = j;
        }
        char fin = i < n ? s[i] : 0;
        if (n > 2 && s[1] == 'O') fin = s[n - 1];

        switch (fin) {
        case 'A': *mods |= mod_bits(mod); return (mod == 3) ? K_WLEFT : K_UP;
        case 'B': *mods |= mod_bits(mod); return (mod == 3) ? K_WRIGHT : K_DOWN;
        case 'C': *mods |= mod_bits(mod); return (mod == 3 || mod == 5) ? K_WRIGHT : K_RIGHT;
        case 'D': *mods |= mod_bits(mod); return (mod == 3 || mod == 5) ? K_WLEFT : K_LEFT;
        case 'P': case 'Q': case 'R': case 'S':   /* F1-F4 (CSI or SS3) */
            *mods |= mod_bits(mod);
            return K_F1 + (fin - 'P');
        case 'H': return K_HOME;
        case 'F': return K_END;
        case 'M': return K_ENTER;
        case '~': {
            *mods |= mod_bits(mod);
            switch (num) {
            case 1: case 7: return K_HOME;
            case 4: case 8: return K_END;
            case 3: return K_FDEL;
            case 5: return K_PGUP;
            case 6: return K_PGDN;
            case 11:  return K_F1;
            case 12:  return K_F2;
            case 13:  return K_F3;
            case 14:  return K_F4;
            case 15:  return K_F5;
            case 17:  return K_F6;
            case 18:  return K_F7;
            case 19:  return K_F8;
            case 20:  return K_F9;
            case 21:  return K_F10;
            case 23:  return K_F11;
            case 24:  return K_F12;
            case 25:  return K_F13;
            case 26:  return K_F14;
            case 28:  return K_F15;
            case 29:  return K_F16;
            case 31:  return K_F17;
            case 32:  return K_F18;
            case 33:  return K_F19;
            case 34:  return K_F20;
            case 42:  return K_F21;
            case 43:  return K_F22;
            case 44:  return K_F23;
            case 45:  return K_F24;
            }
            break;
        }
        }
    }
    *cp = (unsigned char)s[n - 1];
    return K_NONE;
}

int ed_read_key(uint32_t *cp, int *mods)
{
    *mods = 0;
    int c = read_byte();
    if (c < 0) return K_EOF;

    if (c == 27) {
        char seq[16];
        int n = 0;
        seq[n++] = 27;
        struct pollfd p = { STDIN_FILENO, POLLIN, 0 };
        while (n < (int)sizeof seq - 1 && poll(&p, 1, 50) > 0) {
            int b = read_byte();
            if (b < 0) break;
            seq[n++] = (char)b;
        }
        return parse_seq(seq, n, cp, mods);
    }

    if (c == EOF) return K_EOF;

    if (c < 0x20) *mods = MOD_CTRL;       /* raw control byte */

    /* Plain (possibly UTF-8) character. */
    char raw[4];
    raw[0] = (char)c;
    int need = 0;
    if ((c & 0xE0) == 0xC0) need = 1;
    else if ((c & 0xF0) == 0xE0) need = 2;
    else if ((c & 0xF8) == 0xF0) need = 3;

    if (need) {
        for (int i = 1; i <= need; i++) {
            struct pollfd p = { STDIN_FILENO, POLLIN, 0 };
            if (poll(&p, 1, 10) <= 0) break;
            int b = read_byte();
            if (b < 0) break;
            raw[i] = (char)b;
        }
    }

    int len;
    uint32_t decoded;
    if (utf8_decode(raw, &len, &decoded)) {
        *cp = decoded;
        return K_NONE;
    }
    *cp = (uint32_t)(unsigned char)c;
    return K_NONE;
}

/* ── Tiny prompt line editor ─────────────────────────────────────────
 * Used by the preset/config prompts and bind ask() prompts. Reads one
 * line in raw mode (temporarily entering it if the caller is in cooked
 * mode). ESC, Ctrl+C, or Ctrl+D cancels: returns -1 with an empty buf.
 * Enter submits: returns 0. Arrows and other escape sequences are
 * consumed and ignored; backspace and Ctrl+U edit the text. */
int prompt_line(char *buf, size_t sz, const char *prompt)
{
    buf[0] = 0;
    int was_raw = term_raw_active();

    if (term_enter_raw() < 0) {
        /* not interactive — plain fgets fallback */
        if (prompt) { out(prompt); fflush(stdout); }
        if (!fgets(buf, (int)sz, stdin)) { buf[0] = 0; return -1; }
        size_t n = strlen(buf);
        while (n && (buf[n - 1] == '\n' || buf[n - 1] == '\r')) buf[--n] = 0;
        return buf[0] ? 0 : -1;
    }

    if (prompt) { out(prompt); fflush(stdout); }
    size_t len = 0;
    int r = -1;

    for (;;) {
        uint32_t cp = 0;
        int mods = 0;
        int key = ed_read_key(&cp, &mods);
        if (key == K_EOF) break;

        if (key == K_NONE && cp < 0x20) {
            switch (cp) {
            case 0x0d: case 0x0a:                       /* Enter */
                r = 0;
                goto done;
            case 0x1b:                                  /* ESC: cancel */
            case 0x03: case 0x04:                       /* ^C / ^D */
                goto done;
            case 0x08:                                  /* ^H backspace */
                if (len > 0) {
                    len--;
                    buf[len] = 0;
                    out("\b \b");
                }
                continue;
            case 0x15:                                  /* ^U: clear */
                while (len > 0) { len--; buf[len] = 0; out("\b \b"); }
                continue;
            default:
                continue;
            }
        }

        if (key == K_NONE && mods == 0) {
            if (cp == 0x7f) {                           /* DEL = backspace */
                if (len > 0) {
                    len--;
                    buf[len] = 0;
                    out("\b \b");
                }
                continue;
            }
            if (cp >= 0x20) {
                if (len + 4 < sz) {
                    char tmp[8];
                    int l = utf8_encode(cp, tmp);
                    memcpy(buf + len, tmp, (size_t)l);
                    len += (size_t)l;
                    buf[len] = 0;
                    outn(tmp, (size_t)l);
                }
                continue;
            }
            continue;
        }
        /* mods set or named key (arrows etc.): consume and ignore */
    }

done:
    out("\r\n");
    if (!was_raw) term_exit_raw();
    return r;
}

/* ── Candidates ───────────────────────────────────────────────────── */
typedef struct {
    char **v;
    int n, cap;
} Cand;

static void cand_add(Cand *c, const char *s)
{
    for (int i = 0; i < c->n; i++)
        if (strcmp(c->v[i], s) == 0) return;
    if (c->n >= c->cap) {
        c->cap = c->cap ? c->cap * 2 : 64;
        c->v = xrealloc(c->v, sizeof(char *) * (size_t)c->cap);
    }
    c->v[c->n++] = xstrdup(s);
}

static void cand_free(Cand *c)
{
    for (int i = 0; i < c->n; i++) free(c->v[i]);
    free(c->v);
    c->v = NULL;
    c->n = c->cap = 0;
}

#define TOKMAX 512

static void dir_stem_split(const char *tok, char *dir, size_t dirsz, char *stem, size_t stemsz)
{
    const char *sl = strrchr(tok, '/');
    if (sl) {
        size_t dl = (size_t)(sl - tok);
        snprintf(dir, dirsz, "%.*s", (int)dl, tok);
        if (!dir[0]) strcpy(dir, "/");
        snprintf(stem, stemsz, "%s", sl + 1);
    } else {
        strcpy(dir, ".");
        snprintf(stem, stemsz, "%s", tok);
    }
}

static bool is_dir_path(const char *p)
{
    struct stat st;
    return stat(p, &st) == 0 && S_ISDIR(st.st_mode);
}

static void collect_candidates(const char *tok, int is_first, Cand *c)
{
    if (!tok[0]) return;
    size_t tl = strlen(tok);

    if (is_first) {
        for (int i = 0; builtin_names()[i]; i++)
            if (strncmp(builtin_names()[i], tok, tl) == 0) cand_add(c, builtin_names()[i]);
        for (int i = 0; i < dict_count(); i++) {
            const char *d = dict_get(i);
            if (strncmp(d, tok, tl) == 0) cand_add(c, d);
        }
        if (strchr(tok, '/') || tok[0] == '.') {
            char dir[TOKMAX], stem[TOKMAX];
            dir_stem_split(tok, dir, sizeof dir, stem, sizeof stem);
            const char **v; int cap, n;
            n = file_matches(dir, stem, &v, &cap);
            for (int i = 0; i < n; i++) cand_add(c, v[i]);
            for (int i = 0; i < n; i++) free((void *)v[i]);
            free(v);
        }
        return;
    }

    /* argument position: complete filenames (dir + stem) */
    char dir[TOKMAX], stem[TOKMAX];
    dir_stem_split(tok, dir, sizeof dir, stem, sizeof stem);
    const char **v; int cap, n;
    n = file_matches(dir, stem, &v, &cap);
    for (int i = 0; i < n; i++) {
        cand_add(c, v[i]);
        free((void *)v[i]);
    }
    free(v);
}

static void token_parts(const char *text, char **pre, char **tok)
{
    const char *sp = strrchr(text, ' ');
    if (sp) {
        *pre = xstrdup(text);
        (*pre)[sp - text + 1] = 0;      /* keep the trailing space in pre */
        *tok = xstrdup(sp + 1);
    } else {
        *pre = xstrdup("");
        *tok = xstrdup(text);
    }
}

/* ── Editor state ─────────────────────────────────────────────────── */
static Buf e;
static Buf gh;                       /* ghost suggestion */
static const char *prompt;

static char *histq = NULL;           /* query used for history browsing */
static int browse = -1;
static char *bound = NULL;           /* un-browsed line to restore on Down */

static char rq[512];                 /* reverse-search query */
static int rqlen = 0;
static int rmatch = -1;
static int rsearch_on = 0;
static char *rbase = NULL;           /* line before search started */

static void hist_reset(void)
{
    free(histq); histq = NULL;
    free(bound); bound = NULL;
    browse = -1;
}

/* ── Fish-style ghost suggestion ──────────────────────────────────── */
static void compute_suggest(void)
{
    gh.len = 0;
    if (!Cfg.guesser) return;
    if (e.cur != e.len) return;

    char *text = btext(&e);
    size_t tl = strlen(text);
    if (!tl) { free(text); return; }

    /* most recent history entry starting with the whole line */
    int idx = hist_find(text, H.count - 1, 1);
    if (idx >= 0) {
        const char *rest = H.items[idx] + tl;
        if (rest[0]) bset_str(&gh, rest);
        free(text);
        return;
    }

    /* suggestion for the current token */
    char *pre, *tok;
    token_parts(text, &pre, &tok);

    if (tok[0]) {
        Cand c = {0};
        collect_candidates(tok, pre[0] ? false : true, &c);
        if (c.n == 1) {
            size_t ts = strlen(tok);
            if (strncmp(c.v[0], tok, ts) == 0) bset_str(&gh, c.v[0] + ts);
            else bset_str(&gh, c.v[0]);
        }
        cand_free(&c);
    }
    free(pre);
    free(tok);
    free(text);
}

/* ── Rendering ────────────────────────────────────────────────────── */
static void ed_draw(void)
{
    out("\r");
    if (prompt) out(prompt);
    bdump(&e, 0, e.cur);
    bdump(&e, e.cur, e.len);
    if (gh.len) {
        if (Cfg.col_guess) {
            char sgb[24] = "";
            color_sgr(Cfg.col_guess, Cfg.op_guess, sgb, sizeof sgb);
            char wrap[32] = "";
            if (sgb[0]) snprintf(wrap, sizeof wrap, "\033[%sm", sgb);
            out(wrap);
            bdump(&gh, 0, gh.len);
            out("\033[0m");
        } else {
            out("\033[2m");
            bdump(&gh, 0, gh.len);
            out("\033[22m");
        }
    }
    out("\033[K");
    int back = bcells(&e, e.cur, e.len) + bcells(&gh, 0, gh.len);
    if (back > 0) outf("\033[%dD", back);
}

static void rsearch_draw(void)
{
    const char *m = rmatch >= 0 ? H.items[rmatch] : "(no match)";
    out("\n\033[K");
    outf("(reverse-i-search)`%.*s': %s", rqlen, rq, m);
}

static void clear_screen(void)
{
    out("\033[H\033[2J");
}

/* ── Typed-character animations ──────────────────────────────────────
 * Pure cosmetics: the character is already committed to the buffer when
 * ed_animate() runs, so an in-flight animation can never block input or
 * lose a keystroke. Every frame polls stdin; the moment a new key queues
 * up the animation bails and the main loop takes over immediately.     */
#define ANIM_FRAME_MS 40
#define ANIM_FRAMES   10

static uint32_t rand_anim_glyph(void)
{
    static const char pool[] =
        "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789!#?";
    return (uint32_t)pool[rand() % (int)(sizeof pool - 1)];
}

/* Pace one frame. Returns 1 if a keystroke is already waiting. */
static int anim_pump(void)
{
    struct pollfd pfd = { .fd = STDIN_FILENO, .events = POLLIN };
    return poll(&pfd, 1, ANIM_FRAME_MS) > 0;
}

/* Render the line with the character at buffer index `idx` shown as
 * `glyph` (0 = blank) shifted `offs` cells from its natural position:
 *   0   in place (the final state)
 *   >0  riding in from the right
 *   <0  overlaid just left of its cell (pieces "from every side")      */
static void anim_draw(int idx, int offs, uint32_t glyph)
{
    int w = cp_width(e.v[idx]);                 /* natural char width */
    char tmp[8] = "";
    int gl = glyph ? utf8_encode(glyph, tmp) : 0;
    int gw = gl ? cp_width(glyph) : 0;
    int printed;                                /* cells after the prefix */

    out("\r");
    if (prompt) out(prompt);
    bdump(&e, 0, idx);

    if (offs < 0) {
        for (int i = 0; i < w; i++) out(" ");       /* blank the natural cell */
        outf("\033[%dD", w - offs);                 /* jump left over it       */
        if (gl) outn(tmp, (size_t)gl);              /* overlay the glyph       */
        outf("\033[%dC", w - offs - gw);            /* resume at the suffix    */
        printed = w;
    } else if (offs == 0) {
        if (gl) outn(tmp, (size_t)gl);
        else for (int i = 0; i < w; i++) out(" ");
        for (int i = gw; i < w; i++) out(" ");
        printed = gw > w ? gw : w;
    } else {
        for (int i = 0; i < offs; i++) out(" ");
        if (gl) outn(tmp, (size_t)gl);
        for (int i = offs + gw; i < w; i++) out(" ");
        printed = offs + gw > w ? offs + gw : w;
    }

    bdump(&e, idx + 1, e.len);
    if (gh.len) {
        if (Cfg.col_guess) {
            char sgb[24] = "";
            color_sgr(Cfg.col_guess, Cfg.op_guess, sgb, sizeof sgb);
            char wrap[32] = "";
            if (sgb[0]) snprintf(wrap, sizeof wrap, "\033[%sm", sgb);
            out(wrap);
            bdump(&gh, 0, gh.len);
            out("\033[0m");
        } else {
            out("\033[2m");
            bdump(&gh, 0, gh.len);
            out("\033[22m");
        }
    }
    out("\033[K");
    int back = printed + bcells(&e, idx + 1, e.len) + bcells(&gh, 0, gh.len) - w;
    if (back > 0) outf("\033[%dD", back);
}

static void ed_animate(int idx)
{
    if (Cfg.animation == ANIM_NONE) return;
    if (!term_raw_active())       return;

    uint32_t real = e.v[idx];
    compute_suggest();            /* fresh ghost behind the frames */

    switch (Cfg.animation) {
    case ANIM_MATRIX:             /* scrambled letters → the real char */
        for (int f = 0; f < ANIM_FRAMES; f++) {
            anim_draw(idx, 0, rand_anim_glyph());
            if (anim_pump()) return;
        }
        break;
    case ANIM_SPINNER: {          /* / - \ | spinner → the real char */
        static const uint32_t fr[] = { '/', '-', '\\', '|' };
        for (int f = 0; f < ANIM_FRAMES; f++) {
            anim_draw(idx, 0, fr[f % 4]);
            if (anim_pump()) return;
        }
        break;
    }
    case ANIM_NEWCOMER:           /* the letter rides in from the right */
        for (int f = ANIM_FRAMES; f >= 1; f--) {
            anim_draw(idx, f, real);
            if (anim_pump()) return;
        }
        break;
    case ANIM_PLACEMENT:          /* pieces snap in from every side */
        for (int f = ANIM_FRAMES; f >= 1; f--) {
            anim_draw(idx, rand() % 7 - 3, rand_anim_glyph());
            if (anim_pump()) return;
        }
        break;
    default:
        return;
    }
    anim_draw(idx, 0, real);      /* settle on the real character */
}

/* ── History navigation ───────────────────────────────────────────── */
static void hist_up(void)
{
    if (H.count == 0) return;
    if (!histq) {
        histq = btext(&e);
        bound = btext(&e);
    }
    int start = (browse >= 0 ? browse - 1 : H.count - 1);
    int idx = hist_find(histq, start, 1);
    if (idx >= 0) {
        browse = idx;
        bset_str(&e, H.items[idx]);
    }
}

static void hist_down(void)
{
    if (browse < 0 || !histq) return;
    int idx = hist_find(histq, browse + 1, 0);
    if (idx >= 0) {
        browse = idx;
        bset_str(&e, H.items[idx]);
    } else {
        browse = -1;
        if (bound) bset_str(&e, bound);
        else bclr(&e);
    }
}

/* ── Tab completion (fish-style) ──────────────────────────────────── */
static void tab_complete(void)
{
    char *text = btext(&e);
    char *pre, *tok;
    token_parts(text, &pre, &tok);

    bool first = (pre[0] == 0);
    Cand c = {0};
    collect_candidates(tok, first, &c);

    if (c.n == 1) {
        char nl[4096];
        snprintf(nl, sizeof nl, "%s%s", pre, c.v[0]);
        size_t l = strlen(nl);
        if (l > 0 && nl[l - 1] != '/' && is_dir_path(nl)) {
            nl[l] = '/';
            nl[l + 1] = 0;
        }
        bset_str(&e, nl);
    } else if (c.n > 1) {
        size_t tl = strlen(tok);
        char cp[PATH_MAX];
        snprintf(cp, sizeof cp, "%s", c.v[0]);
        for (int i = 1; i < c.n; i++) {
            size_t j = 0;
            while (cp[j] && c.v[i][j] && cp[j] == c.v[i][j]) j++;
            cp[j] = 0;
        }
        size_t cpl = strlen(cp);
        if (cpl > tl) {
            char nl[4096];
            snprintf(nl, sizeof nl, "%s%s", pre, cp);
            bset_str(&e, nl);
        } else {
            /* list the matches; the next keystroke redraws the line */
            out("\r\n");
            for (int i = 0; i < c.n; i++) outf("%s%s", i ? "  " : "", c.v[i]);
            out("\r\n");
        }
    }

    cand_free(&c);
    free(pre);
    free(tok);
    free(text);
}

/* ── Non-interactive fallback ─────────────────────────────────────── */
static char *edit_line_fallback(void)
{
    char buf[LINE_MAX_CP];
    if (!fgets(buf, sizeof buf, stdin)) return NULL;
    size_t n = strlen(buf);
    while (n && (buf[n - 1] == '\n' || buf[n - 1] == '\r')) buf[--n] = 0;
    return xstrdup(buf);
}

/* ── User key bindings (copy / paste / exec / close) ──────────────── */
static const char b64tab[] =
    "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

static char *b64encode(const unsigned char *in, size_t n)
{
    size_t olen = (n + 2) / 3 * 4 + 1;
    char *o = xmalloc(olen);
    size_t i = 0, j = 0;
    while (i + 2 < n) {
        uint32_t v = ((uint32_t)in[i] << 16) | ((uint32_t)in[i + 1] << 8) | in[i + 2];
        o[j++] = b64tab[(v >> 18) & 63];
        o[j++] = b64tab[(v >> 12) & 63];
        o[j++] = b64tab[(v >> 6) & 63];
        o[j++] = b64tab[v & 63];
        i += 3;
    }
    if (i < n) {
        uint32_t v = (uint32_t)in[i] << 16;
        size_t rem = n - i;
        if (rem == 2) v |= (uint32_t)in[i + 1] << 8;
        o[j++] = b64tab[(v >> 18) & 63];
        o[j++] = b64tab[(v >> 12) & 63];
        o[j++] = rem == 2 ? b64tab[(v >> 6) & 63] : '=';
        o[j++] = '=';
    }
    o[j] = 0;
    return o;
}

static char *clip = NULL;                 /* internal clipboard */

static void clip_set(const char *s)
{
    free(clip);
    clip = xstrdup(s);
    char *b = b64encode((const unsigned char *)s, strlen(s));
    out("\x1b]52;c;");                    /* OSC 52, best effort */
    out(b);
    out("\x1b\\");
    free(b);
}

static void clip_paste(void)
{
    const char *p = clip;
    while (p && *p) {
        int n;
        uint32_t c;
        utf8_decode(p, &n, &c);
        if (c >= 0x20 && c != 0x7f) {
            bins(&e, e.cur, c);
            e.cur++;
        }
        p += n;
    }
}

/* Run a bound action. Returns 1 when the shell should exit. */
static int bind_run(const Bind *b)
{
    switch (b->kind) {
    case BIND_CLOSE:
        out("\r\n");
        return 1;
    case BIND_COPY: {
        char *t = btext(&e);
        clip_set(t);
        free(t);
        out("\r\n");
        outf("%s: copied\r\n", THESH_NAME);
        return 0;
    }
    case BIND_PASTE:
        clip_paste();
        return 0;
    case BIND_EXEC: {
        char cmd[BIND_CMD_MAX * 2 + 8];
        if (b->ask[0]) {
            out("\r\n");                          /* leave the typing line */
            char pbuf[BIND_ASK_MAX + 4];
            snprintf(pbuf, sizeof pbuf, "%s : ", b->ask);
            char input[BIND_CMD_MAX];
            if (prompt_line(input, sizeof input, pbuf) == 0) {
                snprintf(cmd, sizeof cmd, "%s %s", b->cmd, input);
                exec_line(cmd);
            }
            return 0;
        }
        snprintf(cmd, sizeof cmd, "%s", b->cmd);
        out("\r\n");
        exec_line(cmd);
        return 0;
    }
    }
    return 0;
}

/* ── The main line editor ─────────────────────────────────────────── */
char *edit_line(const char *prompt_txt, int *cancelled)
{
    *cancelled = 0;
    if (!isatty(STDIN_FILENO)) return edit_line_fallback();

    int eof = 0;
    if (term_enter_raw() < 0) {
        dprintf(STDERR_FILENO, "%s: cannot set raw mode: %s\n",
                THESH_NAME, strerror(errno));
        return NULL;
    }

    prompt = prompt_txt;
    binit(&e);
    binit(&gh);
    hist_reset();
    rqlen = 0;
    rmatch = -1;
    rsearch_on = 0;
    free(rbase); rbase = NULL;

    for (;;) {
        compute_suggest();
        ed_draw();
        if (rsearch_on) rsearch_draw();

        uint32_t cp = 0;
        int mods = 0;
        int key = ed_read_key(&cp, &mods);
        int control = (key == K_NONE && cp < 0x20);

        /* user bindings intercept before the default key actions */
        if (key == K_NONE && (cp == 0x0d || cp == 0x0a)) {
            /* Enter bytes — these are CR/LF, never shadowed by a binding */
        } else {
            int bkey = key;                         /* named keys pass through */
            if (key == K_NONE) {
                if (cp > 0 && cp < 0x1b) bkey = (int)cp + 0x60;   /* ctrl letter */
                else bkey = (int)cp;                                /* plain char */
            }
            const Bind *bd = bind_lookup(mods, bkey);
            if (bd) {
                if (bind_run(bd)) { eof = 1; goto done; }
                continue;
            }
        }

        /* map plain control keys */
        if (key == K_NONE && control) {
            switch (cp) {
            case 0x01: key = K_CTL_A; break;
            case 0x02: key = K_CTL_B; break;
            case 0x03: key = K_CTL_C; break;
            case 0x04: key = K_CTL_D; break;
            case 0x05: key = K_CTL_E; break;
            case 0x06: key = K_CTL_F; break;
            case 0x09: key = K_TAB; break;
            case 0x0b: key = K_CTL_K; break;
            case 0x0c: key = K_CTL_L; break;
            case 0x0a: case 0x0d: key = K_ENTER; break;
            case 0x12: key = K_CTL_R; break;
            case 0x15: key = K_CTL_U; break;
            case 0x17: key = K_CTL_W; break;
            default:   key = K_NONE; break;
            }
        }
        if (key == K_NONE && cp == 0x7f) key = K_DEL;

        switch (key) {
        case K_ENTER:
            if (rsearch_on) {
                if (rmatch >= 0) bset_str(&e, H.items[rmatch]);
                rsearch_on = 0;
                break;                       /* keep edited line */
            }
            out("\r\n");
            goto done;

        case K_CTL_C:
            if (rsearch_on) { rsearch_on = 0; break; }
            bclr(&e);
            out("\r\n");
            *cancelled = 1;
            goto done;

        case K_EOF:
            out("\r\n");
            eof = 1;
            goto done;

        case K_CTL_D:
            if (e.len == 0 && e.cur == 0) {
                out("\r\n");
                eof = 1;
                goto done;                   /* EOF */
            }
            bdel(&e, e.cur);                 /* forward delete */
            break;

        case K_DEL:
            if (e.cur > 0) { e.cur--; bdel(&e, e.cur); }
            goto edited;

        case K_FDEL:
            bdel(&e, e.cur);
            goto edited;

        case K_LEFT:   if (e.cur > 0) e.cur--; break;
        case K_RIGHT:  if (e.cur < e.len) e.cur++; break;
        case K_HOME:   e.cur = 0; break;
        case K_END:    e.cur = e.len; break;

        case K_WLEFT: {
            int i = e.cur;
            while (i > 0 && !is_word(e.v[i - 1])) i--;
            while (i > 0 && is_word(e.v[i - 1])) i--;
            e.cur = i;
            break;
        }
        case K_WRIGHT: {
            int i = e.cur;
            while (i < e.len && is_word(e.v[i])) i++;
            while (i < e.len && !is_word(e.v[i])) i++;
            e.cur = i;
            break;
        }

        case K_UP:    hist_up(); break;
        case K_DOWN:  hist_down(); break;

        case K_CTL_K:
            while (e.cur < e.len) bdel(&e, e.cur);
            goto edited;
        case K_CTL_U:
            while (e.cur > 0) { e.cur--; bdel(&e, e.cur); }
            goto edited;
        case K_CTL_W: {
            int i = e.cur;
            while (i > 0 && !is_word(e.v[i - 1])) i--;
            while (i > 0 && is_word(e.v[i - 1])) i--;
            while (e.cur > i) { e.cur--; bdel(&e, e.cur); }
            goto edited;
        }

        case K_CTL_A: e.cur = 0; break;
        case K_CTL_E: e.cur = e.len; break;
        case K_CTL_B: if (e.cur > 0) e.cur--; break;
        case K_CTL_F: if (e.cur < e.len) e.cur++; break;

        case K_CTL_L:
            clear_screen();
            break;

        case K_CTL_R:
            if (!rsearch_on) {
                rsearch_on = 1;
                rqlen = 0;
                rq[0] = 0;
                rmatch = -1;
                free(rbase);
                rbase = btext(&e);
            } else {
                /* earlier match */
                if (rq[0]) {
                    int base = rmatch >= 0 ? rmatch - 1 : H.count - 1;
                    rmatch = hist_find_substr(rq, base, 1);
                }
            }
            break;

        case K_TAB:
            if (!rsearch_on) tab_complete();
            goto edited;

        case K_PGUP: e.cur = 0; break;
        case K_PGDN: e.cur = e.len; break;

        default:
            if (key == K_NONE && !control) {
                /* printable char */
                if (rsearch_on) {
                    if (rqlen + 4 < (int)sizeof rq) {
                        char tmp[8];
                        int l = utf8_encode(cp, tmp);
                        memcpy(rq + rqlen, tmp, (size_t)l);
                        rqlen += l;
                        rq[rqlen] = 0;
                        rmatch = hist_find_substr(rq, H.count - 1, 1);
                    }
                } else {
                    bins(&e, e.cur, cp);
                    e.cur++;
                    ed_animate(e.cur - 1);
                    goto edited;
                }
            }
            break;
        }
        continue;
    edited:
        hist_reset();
        continue;
    }

done:
    hist_reset();
    if (eof) return NULL;
    if (*cancelled) return xstrdup("");
    char *r = btext(&e);
    return r;
}