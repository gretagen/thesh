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
/* When the mux is up this also pumps pane output, so children repaint
 * in real time even while we sit blocked waiting for the next key. */
/* Bytes handed back by the mux's DSR cursor probe: they must be
 * consumed before touching stdin again. */
static char pushback[128];
static int  pushback_len = 0, pushback_pos = 0;

void ed_pushback(const char *b, int n)
{
    if (n <= 0) return;
    int room = (int)sizeof pushback - pushback_len;
    if (n > room) n = room;                /* drop overflow (rare) */
    if (n <= 0) return;
    memcpy(pushback + pushback_len, b, (size_t)n);
    pushback_len += n;
}

static void ed_check_resize(void);   /* defined with ed_draw below     */

static int read_byte(void)
{
    unsigned char c;
    for (;;) {
        ed_check_resize();                          /* resize / SIGCONT   */
        if (pushback_pos < pushback_len) {         /* DSR probe leftovers */
            c = (unsigned char)pushback[pushback_pos++];
            if (pushback_pos >= pushback_len) { pushback_len = pushback_pos = 0; }
            return c;
        }
        struct pollfd p = { STDIN_FILENO, POLLIN, 0 };
        int r = poll(&p, 1, mux_active() ? 16 : -1);
        if (r == 0) {                          /* idle tick: repaint    */
            mux_pump();
            continue;
        }
        if (r < 0) {
            if (errno == EINTR) continue;
            return -1;
        }
        ssize_t n = read(STDIN_FILENO, &c, 1);
        if (n == 1) return c;
        if (n < 0 && errno == EINTR) continue;
        return -1;                             /* EOF or error          */
    }
}

/* Decode an xterm modifier number (2=shift, 3/4=alt, 5/6=ctrl,
 * 7/8=alt+ctrl, 9..16=meta, 33..40=super) into MOD_* bits. Shift IS
 * reported now so `SHIFT + LEFT` binds fire; bind_lookup() falls back to
 * the unshifted bind when no SHIFT bind exists, so a `SUPER + UP` bind
 * still catches Super+Shift+Up. */
static int mod_bits(int mod)
{
    if (mod <= 1) return 0;
    int m = mod - 1;
    int bits = 0;
    if (m & 1) bits |= MOD_SHIFT;
    if (m & 2) bits |= MOD_ALT;
    if (m & 4) bits |= MOD_CTRL;
    if (m & 8) bits |= MOD_SUPER;    /* meta */
    if (m & 32) bits |= MOD_SUPER;   /* super */
    return bits;
}

static int mouse_fwd = 0;   /* last mouse event belongs to the TUI   */

int ed_mouse_forward(void)  /* consulted only for the event just read */
{
    return mouse_fwd;
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

    /* SGR mouse event (the mux enables 1000/1006 while active):
     * CSI < button ; col ; row M/m — handled centrally, never inserted.
     * mux_mouse tells us whether the event belongs to the focused
     * pane's program (click-through). A truncated or malformed <CSI
     * must be dropped, never fall through to the *cp = s[n-1] tail
     * below — that used to type stray characters when a long event
     * hit the old 16-byte seq buffer. */
    if (n > 2 && s[1] == '[' && s[2] == '<') {
        *cp = 0;
        if (n > 4 && (s[n - 1] == 'M' || s[n - 1] == 'm')) {
            int v[3] = { 0, 0, 0 }, vi = 0;
            for (int i = 3; i < n - 1 && vi < 3; i++) {
                if (s[i] >= '0' && s[i] <= '9')
                    v[vi] = v[vi] * 10 + (s[i] - '0');
                else if (s[i] == ';' && vi < 2)
                    vi++;
            }
            mouse_fwd = mux_mouse(v[0], v[1], v[2], s[n - 1] == 'm');
        } else {
            mouse_fwd = 0;                    /* truncated: drop cleanly */
        }
        return K_NONE;
    }

    /* a DSR cursor reply (`CSI row ; col R`) answers our own probe —
     * if it arrives late it must never become typed input */
    if (n > 4 && s[1] == '[' && s[n - 1] == 'R') {
        int semi = 0, ok = 1;
        for (int i = 2; i < n - 1; i++) {
            if (s[i] == ';') { if (++semi > 1) { ok = 0; break; } }
            else if (!isdigit((unsigned char)s[i])) { ok = 0; break; }
        }
        if (ok && semi == 1) { *cp = 0; return K_NONE; }
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
        case 'A': *mods |= mod_bits(mod); return K_UP;
        case 'B': *mods |= mod_bits(mod); return K_DOWN;
        case 'C': *mods |= mod_bits(mod); return (mod == 3 || mod == 5) ? K_WRIGHT : K_RIGHT;
        case 'D': *mods |= mod_bits(mod); return (mod == 3 || mod == 5) ? K_WLEFT : K_LEFT;
        case 'P': case 'Q': case 'R': case 'S':   /* F1-F4 (CSI or SS3) */
            *mods |= mod_bits(mod);
            return K_F1 + (fin - 'P');
        case 'H': *mods |= mod_bits(mod); return K_HOME;
        case 'F': *mods |= mod_bits(mod); return K_END;
        case 'M': *mods |= mod_bits(mod); return K_ENTER;
        case 'Z':                               /* CSI Z = Shift+Tab */
            *mods |= MOD_SHIFT | mod_bits(mod);
            return K_TAB;
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

/* A CSI (ESC [ …) sequence is complete once a final byte in 0x40..0x7E
 * (A-D, H/F, ~, …) lands; SS3 (ESC O x) at 3 bytes; Alt+X at 2 bytes.
 * A bare ESC (n==1) or ESC ESC … keeps waiting — that's what the caller's
 * 50 ms poll resolves (ESC vs Alt+X). Returning the moment the sequence
 * is complete is what keeps held-arrow repeats from being swallowed into
 * a single event and removes the fixed 50 ms tail every escape sequence
 * used to pay before the cursor moved. */
static int seq_complete(const char *s, int n)
{
    if (n < 2) return 0;
    if (s[1] == '[') {
        for (int i = 2; i < n; i++) {
            unsigned char c = (unsigned char)s[i];
            if (c >= 0x40 && c <= 0x7E) return 1;
        }
        return 0;
    }
    if (s[1] == 'O') return n >= 3;
    if (s[1] == 27)  return 0;             /* ESC ESC … — keep reading */
    return 1;                              /* Alt+X at 2 bytes         */
}

static char key_raw[32];                 /* raw bytes of last key read */
static int  key_rawlen = 0;

const char *ed_raw(int *len)
{
    if (len) *len = key_rawlen;
    return key_raw;
}

int ed_read_key(uint32_t *cp, int *mods)
{
    *mods = 0;
    int c = read_byte();
    if (c < 0) return K_EOF;
    key_raw[0] = (char)c;
    key_rawlen = 1;

    if (c == 27) {
        char seq[32];                        /* holds long SGR mouse events */
        int n = 0;
        seq[n++] = 27;
        while (n < (int)sizeof seq - 1) {
            /* A DSR reply can sit in the pushback (cursor_probe parks
             * interleaved bytes there). Its tail must be assembled from
             * the pushback — polling only stdin would time out mid-
             * sequence and type the reply (`[2;3R`) as literal text.
             * With only ESC so far, continue on a real sequence
             * introducer so a pushback of plain keystrokes still leaves
             * ESC bare; once started, drain it to seq_complete(). */
            if (pushback_pos < pushback_len) {
                if (n > 1) {                 /* sequence started: drain   */
                    int b = read_byte();
                    if (b < 0) break;
                    seq[n++] = (char)b;
                    if (seq_complete(seq, n)) break;   /* one event per call */
                    continue;
                }
                unsigned char nxt = (unsigned char)pushback[pushback_pos];
                if (nxt != '[' && nxt != 'O' && nxt != 27) break;
            }
            struct pollfd p = { STDIN_FILENO, POLLIN, 0 };
            if (poll(&p, 1, 50) <= 0) break;   /* bare ESC: silence decides */
            int b = read_byte();
            if (b < 0) break;
            seq[n++] = (char)b;
            if (seq_complete(seq, n)) break;   /* one event per call */
        }
        memcpy(key_raw, seq, (size_t)n);
        key_rawlen = n;
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
            if (key_rawlen < (int)sizeof key_raw) key_raw[key_rawlen++] = (char)b;
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
/* ── wrap-aware line drawing ─────────────────────────────────────────
 * ed_draw historically did "\r" + prompt + buffer, assuming the line
 * fits one row. In a narrow mux pane it wraps, and every redraw then
 * restarts on the wrapped row — the line drifts down one row per wrap
 * and leaves duplicates behind. Track the absolute anchor row (the mux
 * self-screen reports its cursor for free) and redraw by returning to
 * it, erasing the old span, and re-emitting. The plain terminal keeps
 * the historical single-row draw (anchors only exist under the mux). */
static int ed_anchor = -1;   /* self-pane row the line starts on        */
static int ed_anchor_col = 0;/* …and its column (prompt may start late) */
static int ed_off    = 0;    /* cursor's row offset below the anchor    */
static int ed_span   = 1;    /* rows the drawn line occupied            */
static int ed_w      = 80;   /* last seen pane geometry                 */
static int ed_h      = 24;
static int ed_drew   = 0;    /* a draw already happened for this line   */

/* Where is the cursor? The mux self pane answers from memory; a child
 * pane under a mux asks its emulator (in-process, instant); a plain
 * terminal keeps the legacy single-row draw — no round trip there. */
static int cursor_probe(int *row, int *col)
{
    static int probe_ok = 1;            /* sticky: never stall typing    */
    if (!getenv("THESH_MUX")) { probe_ok = 0; return -1; }
    if (!probe_ok) return -1;
    out("\033[6n");
    /* Read until the reply shows up — keystrokes forwarded by the mux
     * parent may arrive first; they are pushed back afterwards in
     * order. Give up only after real silence (misses), never on a
     * key-interleaved read: the reply is still in flight then. */
    static int misses = 0;
    char buf[256];
    int nr = 0;
    int rep = -1;                     /* offset of the reply's ESC      */
    for (int stage = 0; stage < 3 && rep < 0; stage++) {
        struct pollfd pfd = { STDIN_FILENO, POLLIN, 0 };
        if (poll(&pfd, 1, stage ? 75 : 100) <= 0) break;
        ssize_t r = read(STDIN_FILENO, buf + nr,
                         sizeof buf - 1 - (size_t)nr);
        if (r <= 0) break;
        nr += (int)r;
        for (int i = 0; i + 1 < nr; i++) {
            if (buf[i] != 0x1b || buf[i + 1] != '[') continue;
            int j = i + 2, r0 = 0, c0 = 0;
            while (j < nr && buf[j] >= '0' && buf[j] <= '9') { r0 = r0 * 10 + (buf[j] - '0'); j++; }
            if (j >= nr || buf[j] != ';') continue;
            j++;
            while (j < nr && buf[j] >= '0' && buf[j] <= '9') { c0 = c0 * 10 + (buf[j] - '0'); j++; }
            if (j < nr && buf[j] == 'R' && r0 > 0 && c0 > 0) {
                rep = i;
                *row = r0 - 1;
                *col = c0 - 1;
                muxdbg("probe: reply row=%d col=%d\n", *row, *col);
                /* keys before the reply keep their order; after it too */
                if (i > 0) ed_pushback(buf, i);
                if (j + 1 < nr) ed_pushback(buf + j + 1, nr - j - 1);
                return 0;
            }
        }
    }
    /* Never push back a partial reply prefix — those bytes answer our
     * own probe and would otherwise be typed into the line as text
     * (the `[4;1R` glitch). Only an ESC-anchored reply-shaped suffix
     * is dropped; real keys (and any sequence with a final byte that
     * isn't part of a reply) are kept. */
    int keep = nr;
    int last_esc = -1;
    for (int i = nr - 1; i >= 0; i--)
        if ((unsigned char)buf[i] == 0x1b) { last_esc = i; break; }
    if (last_esc >= 0) {
        int ok = 1;
        for (int i = last_esc + 1; i < nr; i++) {
            unsigned char c = (unsigned char)buf[i];
            if (!(c == '[' || c == ';' || (c >= '0' && c <= '9'))) { ok = 0; break; }
        }
        if (ok) keep = last_esc;        /* suffix is a (partial) reply   */
    }
    if (keep > 0) ed_pushback(buf, keep);
    if (nr == 0 && ++misses >= 3) probe_ok = 0;  /* true silence: fall back */
    return -1;
}

/* Visible width of the prompt in cells. ANSI escapes (SGR colors, OSC)
 * render as ZERO cells — the mux emulator consumes them without advancing
 * the cursor, so counting their bytes made every mux-mode cursor
 * position drift far right of the real line (the "floating block cursor"
 * bug: prompt_cells() returned 72 for a 33-cell prompt with colors). */
static int prompt_cells(void)
{
    int c = 0;
    if (!prompt) return 0;
    const char *p = prompt;
    while (*p) {
        if (*p == 0x1b) {                   /* escape: skip, render 0    */
            p++;
            if (*p == '[') {                /* CSI: params until final    */
                p++;
                while (*p && !(*p >= 0x40 && *p <= 0x7e)) p++;
                if (*p) p++;
            } else if (*p == ']') {         /* OSC: until BEL or ST       */
                p++;
                while (*p && *p != 0x07) {
                    if (p[0] == 0x1b && p[1] == '\\') { p += 2; break; }
                    p++;
                }
                if (*p == 0x07) p++;
            } else if (*p) {
                p++;                        /* ESC x (charset, DECSC …)   */
            }
            continue;
        }
        int n;
        uint32_t cp;
        utf8_decode(p, &n, &cp);
        c += cp_width(cp);
        p += n;
    }
    return c;
}

/* Position of cell `c` (counted from the line's start) on the anchor
 * row: the first row only has `cols - ed_anchor_col` cells left. */
static int ed_row_of(int c)
{
    int cap0 = ed_w - ed_anchor_col;
    if (cap0 < 1) cap0 = 1;
    if (c < cap0) return 0;
    return 1 + (c - cap0) / ed_w;
}

static int ed_col_of(int c)
{
    int cap0 = ed_w - ed_anchor_col;
    if (cap0 < 1) cap0 = 1;
    if (c < cap0) return ed_anchor_col + c;
    return (c - cap0) % ed_w;
}

static void ed_draw(void)
{
    int cols = 0, rows = 0, cx = 0, cy = 0;
    int have;

    /* Pane shells draw against their own pty: geometry is the pty size
     * and the cursor comes from one DSR probe per fresh line — the
     * parent router answers from the pane's emulator. Plain terminals
     * keep the legacy single-row draw. */
    if (getenv("THESH_MUX")) {
        term_size(&cols, &rows);        /* the pty IS the pane          */
        have = 1;
        if (ed_anchor < 0)              /* anchor needs the cursor:     */
            have = cursor_probe(&cy, &cx) == 0;   /* once per line      */
    } else {
        have = 0;                       /* plain terminal               */
    }

    if (have && cols > 0 && rows > 0) {
        int plen = prompt_cells();
        int cur_cell = plen + bcells(&e, 0, e.cur);
        int tcells = plen + bcells(&e, 0, e.len) + bcells(&gh, 0, gh.len);

        if (muxdbg_on())
            muxdbg("draw: pid=%d have=%d geom=%dx%d cx=%d cy=%d "
                   "anchor_in=%d acol=%d off=%d span=%d w=%d h=%d "
                   "cur_cell=%d tcells=%d gh=%d elen=%d ecur=%d\n",
                   (int)getpid(), have, cols, rows, cx, cy, ed_anchor,
                   ed_anchor_col, ed_off, ed_span, ed_w, ed_h, cur_cell,
                   tcells, gh.len, e.len, e.cur);

        if (ed_anchor >= 0 && (cols != ed_w || rows != ed_h)) {
            /* resized: keep the anchor and re-fit it — no cursor probe
             * needed, all positioning below is absolute */
            if (ed_anchor + ed_span > rows) {
                ed_anchor = rows - ed_span;
                if (ed_anchor < 0) ed_anchor = 0;
            }
        }
        if (ed_anchor == -1) {                    /* fresh line: the     */
            ed_anchor = cy;                       /* line starts AT the   */
            ed_anchor_col = cx;                   /* cursor (no col0 snap */
            ed_off = 0;                           /* over program output) */
            ed_span = 1;
            if (ed_anchor > rows - 1) ed_anchor = rows - 1;
            if (ed_anchor < 0) ed_anchor = 0;
        }
        ed_w = cols; ed_h = rows;
        int old_span = ed_span;
        if (muxdbg_on())
            muxdbg("draw: pid=%d anchor_out=%d acol=%d off=%d span=%d\n",
                   (int)getpid(), ed_anchor, ed_anchor_col, ed_off, ed_span);

        /* Absolute draw start: pane-local CUP to the anchor. The physical
         * cursor may be anywhere — probe miss, an animation frame that
         * wrapped, a plain-path excursion — anchoring by coordinates makes
         * every draw self-heal instead of drifting row by row. */
        outf("\033[%d;%dH", ed_anchor + 1, ed_anchor_col + 1);

        /* Draw: the new content overwrites every row it covers — no
         * pre-erase pass (blanking first flashed on every keystroke). */
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

        int last = tcells > 0 ? tcells - 1 : 0;
        int end_row = ed_row_of(last);
        int new_span = end_row + 1;

        /* the draw may have scrolled the pane (wrapped past the last
         * row): shift the anchor by exactly how much we drew over */
        int over = (ed_anchor + new_span - 1) - (rows - 1);
        if (over > 0) ed_anchor -= over;

        /* clear rows only when the line got SHORTER (leftovers below) —
         * absolute erase, no \n chains to go wrong */
        for (int rr = new_span; rr < old_span; rr++) {
            if (ed_anchor + rr > rows - 1) break;
            outf("\033[%d;1H\033[K", ed_anchor + rr + 1);
        }

        /* park the cursor on its cell — absolute pane-local CUP */
        int cur_row = ed_row_of(cur_cell);
        int cur_col = ed_col_of(cur_cell);
        outf("\033[%d;%dH", ed_anchor + cur_row + 1, cur_col + 1);
        if (muxdbg_on())
            muxdbg("draw: pid=%d cur_row=%d cur_col=%d expect_park=(%d,%d)\n",
                   (int)getpid(), cur_row, cur_col,
                   ed_anchor + cur_row, cur_col);

        ed_off = cur_row;
        ed_span = new_span;
        ed_drew = 1;
        return;
    }

    /* plain terminal (or mux not ready): the historical single-row draw */
    if (muxdbg_on()) muxdbg("draw: PLAIN path have=%d\n", have);
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
    /* a probe/plain excursion left the physical cursor mid-line: drop the
     * anchor so the next mux draw re-derives it from fresh geometry */
    if (ed_drew) ed_anchor = -1;
    ed_drew = 1;
}

/* Resize / resume: repaint the line at once instead of waiting for the
 * next keystroke (the caret used to sit at stale coordinates until then).
 * The anchor re-fit itself happens inside ed_draw's geometry check. */
static void ed_check_resize(void)
{
    thesh_on_cont();
    if (!thesh_winched) return;
    thesh_winched = 0;
    if (mux_active()) mux_pump();          /* router: relayout on winch  */
    if (!mux_active())                      /* pane shells / plain draw   */
        ed_draw();
}

/* Leave the input line: park at the end of its span first (absolute),
 * so Enter's newline lands below the whole wrapped line, then drop the
 * anchor. */
static void ed_leave_line(void)
{
    if (ed_anchor >= 0)
        outf("\033[%d;1H", ed_anchor + ed_span);
    ed_anchor = -1;
    ed_off = 0;
    ed_span = 1;
    ed_drew = 0;
}

static void rsearch_draw(void)
{
    const char *m = rmatch >= 0 ? H.items[rmatch] : "(no match)";
    if (ed_anchor >= 0) {
        /* status one row below the input line, then back to the cursor —
         * both by absolute pane-local CUP (no relative chain) */
        int cur_cell = prompt_cells() + bcells(&e, 0, e.cur);
        outf("\033[%d;1H\033[K", ed_anchor + ed_span + 1);
        outf("(reverse-i-search)`%.*s': %s", rqlen, rq, m);
        outf("\033[%d;%dH", ed_anchor + ed_off + 1, ed_col_of(cur_cell) + 1);
        return;
    }
    out("\n\033[K");
    outf("(reverse-i-search)`%.*s': %s", rqlen, rq, m);
}

static void clear_screen(void)
{
    out("\033[H\033[2J");
    ed_anchor = -1;                     /* next draw re-scans the cursor */
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

    /* absolute start: on the anchor row (mux), col 0 of the current row
     * otherwise — never inherit a cursor position from a prior frame */
    if (ed_anchor >= 0) outf("\033[%d;1H", ed_anchor + 1);
    else out("\r");
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
    if (ed_anchor >= 0 && (ed_anchor_col > 0 || ed_span > 1 ||
        prompt_cells() + bcells(&e, 0, e.len) + bcells(&gh, 0, gh.len) + ANIM_FRAMES + 2
            > ed_w - ed_anchor_col))
        return;         /* wrapped / mid-row / ghost-overflow line: the
                         * frame would wrap the pane and desync the row —
                         * fall back to the plain (absolute) redraw       */

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
            ed_leave_line();
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
        ed_leave_line();
        out("\r\n");
        return 1;
    case BIND_COPY: {
        char *t = btext(&e);
        clip_set(t);
        free(t);
        ed_leave_line();
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
            ed_leave_line();
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
        ed_leave_line();
        out("\r\n");
        exec_line(cmd);
        return 0;
    }
    case BIND_VERB:                            /* pane verbs are builtins */
        ed_leave_line();
        out("\r\n");
        exec_line(b->cmd);
        return 0;
    }
    return 0;
}

/* ── Movement speed & the hold-to-amplify boost ─────────────────────
 * movespeed   = cells per Left/Right press
 * scrollspeed = history entries per Up/Down press
 * amplify     = while the SAME arrow is held (its previous press was
 *               within AMPLIFY_HOLD_MS, i.e. auto-repeat is running),
 *               each repeat advances step × amplify — "hold to double". */
#define AMPLIFY_HOLD_MS 400

static struct timespec last_arrow[4];        /* indexed by key - K_UP */

static int arrow_step(int key)
{
    int idx = key - K_UP;                    /* K_UP K_DOWN K_LEFT K_RIGHT */
    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);

    int base = (key == K_UP || key == K_DOWN) ? Cfg.scrollspeed : Cfg.movespeed;

    long long dt_ms = -1;                    /* -1 = first press of this key */
    if (last_arrow[idx].tv_sec || last_arrow[idx].tv_nsec) {
        dt_ms = (now.tv_sec  - last_arrow[idx].tv_sec)  * 1000LL +
                (now.tv_nsec - last_arrow[idx].tv_nsec) / 1000000LL;
    }
    last_arrow[idx] = now;

    if (dt_ms >= 0 && dt_ms <= AMPLIFY_HOLD_MS) base *= Cfg.amplify;
    return base;
}

/* Returns 0 = no bind matched, 1 = a bind ran, 2 = bind wants EOF. */
static int try_bind(int key, uint32_t cp, int mods)
{
    /* Shared bytes (Enter/Ctrl+M/Ctrl+J, Backspace/Ctrl+H) only consult
     * binds on an empty line — with text the real action always wins. */
    int shared_byte = (key == K_NONE &&
                       (cp == 0x0d || cp == 0x0a || cp == 0x08));
    if (shared_byte && e.len > 0) return 0;

    int bkey = key;                         /* named keys pass through */
    if (key == K_NONE) {
        if (cp > 0 && cp < 0x1b) bkey = (int)cp + 0x60;   /* ctrl letter */
        else bkey = (int)cp;                                /* plain char */
    }
    const Bind *bd = bind_lookup(mods, bkey);
    if (!bd) return 0;
    return bind_run(bd) ? 2 : 1;
}

/* Map a decoded key to a configured bind (shared with the router: it
 * uses the same rc binds, minus the line-dependent shared-byte rule). */
const Bind *ed_bind_lookup(int key, uint32_t cp, int mods)
{
    int bkey = key;                         /* named keys pass through */
    if (key == K_NONE) {
        if (cp > 0 && cp < 0x1b) bkey = (int)cp + 0x60;   /* ctrl letter */
        else bkey = (int)cp;                                /* plain char */
    }
    return bind_lookup(mods, bkey);
}

/* A forked pane shell drops coordinates from the pre-fork screen: the
 * next draw re-derives its anchor from the pane's own cursor probe. */
void ed_reset_anchor(void)
{
    ed_anchor = -1;
    ed_off = 0;
    ed_span = 1;
    ed_drew = 0;
}

/* Ctrl+arrow → pane direction while multiplexed; -1 = not a nav key.
 * Without the mux these stay word-jumps, so nothing changes for
 * single-pane users. */
static int mux_nav(int key, int mods)
{
    if (!mux_active() || !(mods & MOD_CTRL)) return -1;
    switch (key) {
    case K_LEFT:  case K_WLEFT:  return 0;
    case K_RIGHT: case K_WRIGHT: return 1;
    case K_UP:                    return 2;
    case K_DOWN:                  return 3;
    default:                      return -1;
    }
}

int ed_mux_nav(int key, int mods)      /* public: the router loop       */
{
    return mux_nav(key, mods);
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
    ed_anchor = -1;                     /* fresh line: anchor = cursor    */
    ed_off = 0;
    ed_span = 1;
    ed_drew = 0;
    hist_reset();
    rqlen = 0;
    rmatch = -1;
    rsearch_on = 0;
    free(rbase); rbase = NULL;
    memset(last_arrow, 0, sizeof last_arrow);  /* first press of a new line
                                                  is always base speed      */

    for (;;) {
        /* every loop draws the line before blocking on a key */
        compute_suggest();
        ed_draw();
        if (rsearch_on) rsearch_draw();

        uint32_t cp = 0;
        int mods = 0;
        int key = ed_read_key(&cp, &mods);

        if (key == K_EOF) { eof = 1; goto done; }

        int control = (key == K_NONE && cp < 0x20);

        /* user bindings intercept before the default key actions */
        int tb = try_bind(key, cp, mods);
        if (tb == 2) { eof = 1; goto done; }
        if (tb == 1) continue;

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
        /* DEL (0x7f) and BS (0x08) are both Backspace — terminals and
         * tmux disagree on which byte the key sends, so accept either. */
        if (key == K_NONE && (cp == 0x7f || cp == 0x08)) key = K_DEL;

        switch (key) {
        case K_ENTER:
            if (rsearch_on) {
                if (rmatch >= 0) bset_str(&e, H.items[rmatch]);
                rsearch_on = 0;
                break;                       /* keep edited line */
            }
            ed_leave_line();
            out("\r\n");
            goto done;

        case K_CTL_C:
            if (rsearch_on) { rsearch_on = 0; break; }
            bclr(&e);
            ed_leave_line();
            out("\r\n");
            *cancelled = 1;
            goto done;

        case K_EOF:
            ed_leave_line();
            out("\r\n");
            eof = 1;
            goto done;

        case K_CTL_D:
            if (e.len == 0 && e.cur == 0) {
                ed_leave_line();
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

        case K_LEFT: {
            int step = arrow_step(K_LEFT);
            while (step-- > 0 && e.cur > 0) e.cur--;
            break;
        }
        case K_RIGHT: {
            int step = arrow_step(K_RIGHT);
            while (step-- > 0 && e.cur < e.len) e.cur++;
            break;
        }
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

        case K_UP: {
            int step = arrow_step(K_UP);
            while (step-- > 0) hist_up();
            break;
        }
        case K_DOWN: {
            int step = arrow_step(K_DOWN);
            while (step-- > 0) hist_down();
            break;
        }

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

        case K_PGUP:
            e.cur = 0;
            break;
        case K_PGDN:
            e.cur = e.len;
            break;

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