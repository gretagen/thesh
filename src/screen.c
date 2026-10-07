#include "thesh.h"

/* ── MuxScreen: a pane's emulated terminal ───────────────────────────
 * Interprets the VT100/xterm subset real programs actually use — cursor
 * addressing, erasing, scrolling, SGR colors, alternate screen — but
 * everything is pane-relative and clamped: a child can never paint
 * outside its rectangle. Repaint pushes the pane's cell grid to the
 * real terminal with absolute cursor moves; borders are drawn around
 * the rect in the configured active/inactive colors. */

/* parser states */
enum { PT_NORM, PT_ESC, PT_CSI, PT_OSC, PT_SKIP, PT_CS };

static MuxColor cdef(void) { MuxColor c = {0, 0, 0, 0, 0}; return c; }

static void fix_row_pairs(MuxCell *row, int w);   /* defined near put_cp */

static void tw(const char *fmt, ...)
{
    char buf[1024];
    va_list ap;
    va_start(ap, fmt);
    int n = vsnprintf(buf, sizeof buf, fmt, ap);
    va_end(ap);
    if (n > 0) {
        size_t len = (size_t)n < sizeof buf ? (size_t)n : sizeof buf - 1;
        tty_buf_write(buf, len);            /* batched: flushed per pump  */
    }
}

static void mark_dirty(MuxScreen *s, int r)
{
    if (r >= 0 && r < s->h) s->dirty[r] = 1;
}

static void mark_all(MuxScreen *s)
{
    s->all_dirty = 1;
    memset(s->dirty, 1, (size_t)s->h);
}

static MuxCell *grid_alloc(int w, int h)
{
    MuxCell *g = xmalloc(sizeof(MuxCell) * (size_t)w * (size_t)h);
    for (int i = 0; i < w * h; i++) memset(&g[i], 0, sizeof g[i]);
    return g;
}

/* ── lifecycle ─────────────────────────────────────────────────────── */

void mux_screen_init(MuxScreen *s, int w, int h)
{
    memset(s, 0, sizeof *s);
    if (w < 1) w = 1;
    if (h < 1) h = 1;
    s->w = w; s->h = h;
    s->cells = grid_alloc(w, h);
    s->fg = cdef(); s->bg = cdef();
    s->top = 0; s->bot = h - 1;
    s->autowrap = 1;
    s->cur_vis = 1;
    s->dirty = xmalloc((size_t)h);
    mark_all(s);
    s->st = PT_NORM;
}

static MuxCell *grid_shrink_copy(MuxCell *old, int ow, int oh, int nw, int nh)
{
    MuxCell *g = grid_alloc(nw, nh);
    int cr = ow < nw ? ow : nw;
    int chr = oh < nh ? oh : nh;
    for (int r = 0; r < chr; r++)
        memcpy(g + (size_t)r * (size_t)nw, old + (size_t)r * (size_t)ow,
               sizeof(MuxCell) * (size_t)cr);
    free(old);
    return g;
}

void mux_screen_resize(MuxScreen *s, int w, int h)
{
    if (w < 1) w = 1;
    if (h < 1) h = 1;
    if (w == s->w && h == s->h) return;
    int ow = s->w, oh = s->h;
    s->cells = grid_shrink_copy(s->cells, s->w, s->h, w, h);
    if (s->alt) s->alt = grid_shrink_copy(s->alt, s->w, s->h, w, h);
    s->w = w; s->h = h;
    /* a wide pair may have lost its second half at the new right edge */
    int ncopy = ow < w ? ow : w;
    int rmax = (oh < h ? oh : h);
    for (int r = 0; r < rmax; r++)
        fix_row_pairs(s->cells + (size_t)r * (size_t)w, ncopy);
    if (s->alt)
        for (int r = 0; r < rmax; r++)
            fix_row_pairs(s->alt + (size_t)r * (size_t)w, ncopy);
    if (s->cx >= w) s->cx = w - 1;
    if (s->cy >= h) s->cy = h - 1;
    s->top = 0; s->bot = h - 1;              /* region resets on resize */
    s->wrap = 0;
    free(s->dirty);
    s->dirty = xmalloc((size_t)h);
    mark_all(s);
}

void mux_screen_free(MuxScreen *s)
{
    free(s->cells);
    free(s->alt);
    free(s->dirty);
    if (s->sb) {
        for (int i = 0; i < MUX_SCROLLBACK; i++) free(s->sb[i]);
        free(s->sb);
        free(s->sb_ws);
    }
    memset(s, 0, sizeof *s);
}

/* ── scrollback ring ───────────────────────────────────────────────── */

static void sb_push(MuxScreen *s, const MuxCell *row)
{
    if (!s->sb) {
        s->sb = xmalloc(sizeof(MuxCell *) * MUX_SCROLLBACK);
        s->sb_ws = xmalloc(sizeof(int) * MUX_SCROLLBACK);
        memset(s->sb, 0, sizeof(MuxCell *) * MUX_SCROLLBACK);
        memset(s->sb_ws, 0, sizeof(int) * MUX_SCROLLBACK);
    }
    if (s->sb_count == MUX_SCROLLBACK) {
        free(s->sb[s->sb_head]);             /* evict the oldest row    */
        s->sb[s->sb_head] = NULL;
    } else {
        s->sb_count++;
    }
    MuxCell *copy = xmalloc(sizeof(MuxCell) * (size_t)s->w);
    memcpy(copy, row, sizeof(MuxCell) * (size_t)s->w);
    s->sb[s->sb_head] = copy;
    s->sb_ws[s->sb_head] = s->w;
    s->sb_head = (s->sb_head + 1) % MUX_SCROLLBACK;
}

/* oldest ring index = (head - count + CAP) % CAP */
static const MuxCell *sb_row(MuxScreen *s, int idx, int *width)
{
    int i = (s->sb_head - s->sb_count + idx + MUX_SCROLLBACK) % MUX_SCROLLBACK;
    if (idx < 0 || idx >= s->sb_count || !s->sb[i]) return NULL;
    if (width) *width = s->sb_ws[i];
    return s->sb[i];
}

/* ── scrolling within the pane ─────────────────────────────────────── */

static void region_shift_up(MuxScreen *s, int n)
{
    for (int i = 0; i < n; i++) {
        if (s->top == 0 && s->bot == s->h - 1)
            sb_push(s, s->cells + (size_t)s->top * (size_t)s->w);
        memmove(s->cells + (size_t)s->top * (size_t)s->w,
                s->cells + (size_t)(s->top + 1) * (size_t)s->w,
                sizeof(MuxCell) * (size_t)(s->bot - s->top) * (size_t)s->w);
        MuxCell *last = s->cells + (size_t)s->bot * (size_t)s->w;
        for (int c = 0; c < s->w; c++) memset(&last[c], 0, sizeof last[c]);
    }
    for (int r = s->top; r <= s->bot; r++) mark_dirty(s, r);
}

static void region_shift_down(MuxScreen *s, int n)
{
    for (int i = 0; i < n; i++) {
        memmove(s->cells + (size_t)(s->top + 1) * (size_t)s->w,
                s->cells + (size_t)s->top * (size_t)s->w,
                sizeof(MuxCell) * (size_t)(s->bot - s->top) * (size_t)s->w);
        MuxCell *first = s->cells + (size_t)s->top * (size_t)s->w;
        for (int c = 0; c < s->w; c++) memset(&first[c], 0, sizeof first[c]);
    }
    for (int r = s->top; r <= s->bot; r++) mark_dirty(s, r);
}

static void do_lf(MuxScreen *s)
{
    if (s->cy >= s->bot) region_shift_up(s, 1);
    else if (s->cy < s->h - 1) s->cy++;
}

/* erase keeps the current background only (bce-ish) */
static MuxColor erase_color(MuxScreen *s) { return s->bg; }

static void clear_cells(MuxScreen *s, int r0, int c0, int r1, int c1)
{
    if (r0 < 0) r0 = 0;
    if (c0 < 0) c0 = 0;
    if (r1 > s->h - 1) r1 = s->h - 1;
    if (c1 > s->w - 1) c1 = s->w - 1;
    MuxColor bg = erase_color(s);
    for (int r = r0; r <= r1; r++) {
        /* never split a wide pair at the rectangle's edges: take the
         * glyph whose pad would be erased, and the pad whose glyph
         * would be erased */
        MuxCell *row = s->cells + (size_t)r * (size_t)s->w;
        int a = c0, b = c1;
        if (a > 0 && row[a].pad) a--;
        if (b < s->w - 1 && row[b].cp && cp_width(row[b].cp) == 2) b++;
        for (int c = a; c <= b; c++) {
            MuxCell *cell = &row[c];
            cell->cp = 0; cell->pad = 0;
            cell->attrs = 0; cell->fg = cdef(); cell->bg = bg;
        }
        mark_dirty(s, r);
    }
}

/* Drop half of any split wide pair on a row: a 2-column glyph without
 * its padding cell (or a padding cell without its glyph) would paint a
 * stray column. Called after cell-shifting edits and grid resizes. */
static void fix_row_pairs(MuxCell *row, int w)
{
    for (int c = 0; c < w; c++) {
        int cw = row[c].cp ? cp_width(row[c].cp) : 0;
        if (cw == 2) {
            if (c + 1 >= w || !row[c + 1].pad)
                memset(&row[c], 0, sizeof row[c]);    /* orphan glyph   */
            else
                c++;                                  /* skip its pad   */
        } else if (row[c].pad &&
                   !(c > 0 && row[c - 1].cp &&
                     cp_width(row[c - 1].cp) == 2)) {
            memset(&row[c], 0, sizeof row[c]);        /* orphan pad     */
        }
    }
}

/* ── printing ──────────────────────────────────────────────────────── */

/* The grid is column-exact: a wide glyph owns its cell plus a padding
 * cell (tmux GRID_FLAG_PADDING), so cell coordinates always equal real
 * terminal columns — otherwise the parked cursor and the painted text
 * drift apart by one per wide character. Zero-width combining marks are
 * dropped (one codepoint per cell; dropping keeps the grid identical to
 * what is actually painted). */
static void put_cp(MuxScreen *s, uint32_t cp)
{
    int cw = cp_width(cp);
    if (cw <= 0) return;

    if (s->wrap) {
        s->wrap = 0;
        if (s->autowrap) { s->cx = 0; do_lf(s); }
    }
    if (cw == 2 && s->cx == s->w - 1) {
        /* a wide glyph cannot straddle the pane edge: wrap eagerly */
        if (s->autowrap) { s->cx = 0; do_lf(s); }
        else return;                          /* no room: drop it        */
    }
    if (s->cx >= s->w) s->cx = s->w - 1;
    if (s->cy >= s->h) s->cy = s->h - 1;
    MuxCell *cell = &s->cells[(size_t)s->cy * (size_t)s->w + (size_t)s->cx];
    cell->cp = cp;
    cell->attrs = s->attrs;
    cell->fg = s->fg;
    cell->bg = s->bg;
    cell->pad = 0;
    if (cw == 2 && s->cx + 1 < s->w) {
        MuxCell *pad = cell + 1;
        memset(pad, 0, sizeof *pad);
        pad->pad = 1;
    }
    mark_dirty(s, s->cy);
    s->last_cp = cp;
    s->cx += cw;
    if (s->cx >= s->w) {
        s->cx = s->w - 1;
        if (s->autowrap) s->wrap = 1;
    }
}

/* ── alternate screen ──────────────────────────────────────────────── */

static void to_alt(MuxScreen *s, int save_cursor)
{
    if (save_cursor) { s->asx = s->cx; s->asy = s->cy; }
    if (!s->alt) {
        s->alt = s->cells;
        s->cells = grid_alloc(s->w, s->h);
        mark_all(s);
    }
    s->cx = 0; s->cy = 0; s->wrap = 0;
}

static void from_alt(MuxScreen *s, int restore_cursor)
{
    if (!s->alt) return;
    free(s->cells);
    s->cells = s->alt;
    s->alt = NULL;
    if (restore_cursor) {
        s->cx = s->asx < s->w ? s->asx : s->w - 1;
        s->cy = s->asy < s->h ? s->asy : s->h - 1;
    }
    s->wrap = 0;
    mark_all(s);
}

/* ── SGR ───────────────────────────────────────────────────────────── */

static void sgr(MuxScreen *s, const int *p, int n)
{
    if (n == 0) { s->attrs = 0; s->fg = cdef(); s->bg = cdef(); return; }
    for (int i = 0; i < n; i++) {
        int v = p[i];
        switch (v) {
        case 0:  s->attrs = 0; s->fg = cdef(); s->bg = cdef(); break;
        case 1:  s->attrs |= CA_BOLD; break;
        case 2:  s->attrs |= CA_DIM; break;
        case 3:  s->attrs |= CA_ITALIC; break;
        case 4:  s->attrs |= CA_UL; break;
        case 5:  s->attrs |= CA_BLINK; break;
        case 7:  s->attrs |= CA_REV; break;
        case 8:  s->attrs |= CA_INVIS; break;
        case 9:  s->attrs |= CA_STRIKE; break;
        case 21: case 22: s->attrs &= (uint8_t)~(CA_BOLD | CA_DIM); break;
        case 23: s->attrs &= (uint8_t)~CA_ITALIC; break;
        case 24: s->attrs &= (uint8_t)~CA_UL; break;
        case 25: s->attrs &= (uint8_t)~CA_BLINK; break;
        case 27: s->attrs &= (uint8_t)~CA_REV; break;
        case 28: s->attrs &= (uint8_t)~CA_INVIS; break;
        case 29: s->attrs &= (uint8_t)~CA_STRIKE; break;
        case 38: case 48: {
            int is_fg = (v == 38);
            MuxColor c = cdef();
            if (i + 1 < n && p[i + 1] == 5 && i + 2 < n) {
                c.kind = 2; c.idx = (uint8_t)p[i + 2];
                i += 2;
            } else if (i + 1 < n && p[i + 1] == 2 && i + 4 < n) {
                c.kind = 3;
                c.r = (uint8_t)p[i + 2]; c.g = (uint8_t)p[i + 3]; c.b = (uint8_t)p[i + 4];
                i += 4;
            } else { i = n; break; }
            if (is_fg) s->fg = c; else s->bg = c;
            break;
        }
        case 39: s->fg = cdef(); break;
        case 49: s->bg = cdef(); break;
        default:
            if (v >= 30 && v <= 37) { s->fg.kind = 1; s->fg.idx = (uint8_t)v; s->fg.r = s->fg.g = s->fg.b = 0; }
            else if (v >= 40 && v <= 47) { s->bg.kind = 1; s->bg.idx = (uint8_t)v; s->bg.r = s->bg.g = s->bg.b = 0; }
            else if (v >= 90 && v <= 97) { s->fg.kind = 1; s->fg.idx = (uint8_t)v; s->fg.r = s->fg.g = s->fg.b = 0; }
            else if (v >= 100 && v <= 107) { s->bg.kind = 1; s->bg.idx = (uint8_t)v; s->bg.r = s->bg.g = s->bg.b = 0; }
            break;
        }
    }
}

/* ── CSI dispatch ──────────────────────────────────────────────────── */

static int cparam(const MuxScreen *s, int i, int defv)
{
    if (i >= s->cp_n || s->cp_p[i] < 0) return defv;
    return s->cp_p[i];
}

/* ── pane modes: mouse the child program asked for (tmux input.c) ──── */
static void csi_mode_common(MuxScreen *s, int set, int p)
{
    switch (p) {
    case 1000: case 1001:                 /* normal tracking            */
        s->mouse = set ? (s->mouse & ~7) | 1 : s->mouse & ~7; break;
    case 1002:                            /* button-event (drag)        */
        s->mouse = set ? (s->mouse & ~7) | 2 : s->mouse & ~7; break;
    case 1003:                            /* any-event (all motion)     */
        s->mouse = set ? (s->mouse & ~7) | 4 : s->mouse & ~7; break;
    case 1006:                            /* SGR encoding               */
        if (set) s->mouse |= 8; else s->mouse &= ~8; break;
    default: break;
    }
}

static void csi_dispatch(MuxScreen *s)
{
    const int *p = s->cp_p;
    int n = s->cp_n;
    int v = n > 0 && p[0] > 0 ? p[0] : 1;
    char fin = s->csin ? (char)s->csib[s->csin - 1] : 0;

    switch (fin) {
    case 'A': {                                  /* CUU                  */
        int lim = (s->cy >= s->top && s->cy <= s->bot) ? s->top : 0;
        s->cy -= v; if (s->cy < lim) s->cy = lim; s->wrap = 0; break;
    }
    case 'B': {                                  /* CUD                  */
        int lim = (s->cy >= s->top && s->cy <= s->bot) ? s->bot : s->h - 1;
        s->cy += v; if (s->cy > lim) s->cy = lim; s->wrap = 0; break;
    }
    case 'C': s->cx += v; if (s->cx > s->w - 1) s->cx = s->w - 1; s->wrap = 0; break;
    case 'D': s->cx -= v; if (s->cx < 0) s->cx = 0; s->wrap = 0; break;
    case 'E': s->cx = 0; s->cy += v; if (s->cy > s->h - 1) s->cy = s->h - 1; s->wrap = 0; break;
    case 'F': s->cx = 0; s->cy -= v; if (s->cy < 0) s->cy = 0; s->wrap = 0; break;
    case 'G': s->cx = v - 1; if (s->cx > s->w - 1) s->cx = s->w - 1; if (s->cx < 0) s->cx = 0; s->wrap = 0; break;
    case 'd': s->cy = v - 1; if (s->cy > s->h - 1) s->cy = s->h - 1; if (s->cy < 0) s->cy = 0; s->wrap = 0; break;
    case 'H': case 'f':
        s->cy = cparam(s, 0, 1) - 1;
        s->cx = cparam(s, 1, 1) - 1;
        if (s->cy > s->h - 1) s->cy = s->h - 1;
        if (s->cy < 0) s->cy = 0;
        if (s->cx > s->w - 1) s->cx = s->w - 1;
        if (s->cx < 0) s->cx = 0;
        s->wrap = 0;
        break;
    case 'S': region_shift_up(s, v); break;
    case 'T': region_shift_down(s, v); break;
    case 'L': {                                  /* IL: insert lines      */
        int lim = s->bot;
        if (s->cy > lim) break;
        for (int i = 0; i < v; i++) {
            memmove(s->cells + (size_t)(s->cy + 1) * (size_t)s->w,
                    s->cells + (size_t)s->cy * (size_t)s->w,
                    sizeof(MuxCell) * (size_t)(lim - s->cy) * (size_t)s->w);
            MuxCell *first = s->cells + (size_t)s->cy * (size_t)s->w;
            for (int c = 0; c < s->w; c++) memset(&first[c], 0, sizeof first[c]);
        }
        for (int r = s->cy; r <= lim; r++) mark_dirty(s, r);
        break;
    }
    case 'M': {                                  /* DL: delete lines      */
        int lim = s->bot;
        if (s->cy > lim) break;
        for (int i = 0; i < v; i++) {
            memmove(s->cells + (size_t)s->cy * (size_t)s->w,
                    s->cells + (size_t)(s->cy + 1) * (size_t)s->w,
                    sizeof(MuxCell) * (size_t)(lim - s->cy) * (size_t)s->w);
            MuxCell *last = s->cells + (size_t)lim * (size_t)s->w;
            for (int c = 0; c < s->w; c++) memset(&last[c], 0, sizeof last[c]);
        }
        for (int r = s->cy; r <= lim; r++) mark_dirty(s, r);
        break;
    }
    case '@': {                                  /* ICH: insert blanks    */
        MuxColor bg = erase_color(s);
        MuxCell *row = s->cells + (size_t)s->cy * (size_t)s->w;
        if (v > s->w - s->cx) v = s->w - s->cx;
        for (int i = s->w - 1; i >= s->cx + v; i--) row[i] = row[i - v];
        for (int i = 0; i < v; i++) {
            MuxCell *c = &row[s->cx + i];
            c->cp = 0; c->pad = 0; c->attrs = 0; c->fg = cdef(); c->bg = bg;
        }
        fix_row_pairs(row, s->w);                /* shifting may split  */
        mark_dirty(s, s->cy);
        break;
    }
    case 'P': {                                  /* DCH: delete chars     */
        MuxCell *row = s->cells + (size_t)s->cy * (size_t)s->w;
        if (v > s->w - s->cx) v = s->w - s->cx;
        for (int i = s->cx; i < s->w - v; i++) row[i] = row[i + v];
        MuxColor bg = erase_color(s);
        for (int i = s->w - v; i < s->w; i++) {
            MuxCell *c = &row[i];
            c->cp = 0; c->pad = 0; c->attrs = 0; c->fg = cdef(); c->bg = bg;
        }
        fix_row_pairs(row, s->w);                /* shifting may split  */
        mark_dirty(s, s->cy);
        break;
    }
    case 'X': clear_cells(s, s->cy, s->cx, s->cy, s->cx + v - 1); break;
    case 'J':
        switch (n > 0 ? p[0] : 0) {
        case 0: clear_cells(s, s->cy, s->cx, s->cy, s->w - 1);
                clear_cells(s, s->cy + 1, 0, s->h - 1, s->w - 1); break;
        case 1: clear_cells(s, s->cy, 0, s->cy, s->cx);
                clear_cells(s, 0, 0, s->cy - 1, s->w - 1); break;
        default: clear_cells(s, 0, 0, s->h - 1, s->w - 1); break;
        }
        break;
    case 'K':
        switch (n > 0 ? p[0] : 0) {
        case 0: clear_cells(s, s->cy, s->cx, s->cy, s->w - 1); break;
        case 1: clear_cells(s, s->cy, 0, s->cy, s->cx); break;
        default: clear_cells(s, s->cy, 0, s->cy, s->w - 1); break;
        }
        break;
    case 'r': {                                  /* DECSTBM              */
        int t = cparam(s, 0, 1) - 1;
        int b = cparam(s, 1, s->h) - 1;
        if (t < 0) t = 0;
        if (b > s->h - 1) b = s->h - 1;
        if (t < b) { s->top = t; s->bot = b; }
        s->cx = 0; s->cy = 0; s->wrap = 0;
        break;
    }
    case 's':
        if (s->csin == 1 || n == 0) {           /* save cursor (no args) */
            s->scx = s->cx; s->scy = s->cy;
            s->sattrs = s->attrs; s->sfg = s->fg; s->sbg = s->bg;
            s->ssaved = 1;
        }
        break;
    case 'u':
        if (s->ssaved) {
            s->cx = s->scx; s->cy = s->scy;
            s->attrs = s->sattrs; s->fg = s->sfg; s->bg = s->sbg;
            s->wrap = 0;
        }
        break;
    case 'm': sgr(s, p, n); break;
    case 'n':
        if (n > 0 && p[0] == 6 && s->reply) {
            char rb[32];
            snprintf(rb, sizeof rb, "\x1b[%d;%dR", s->cy + 1, s->cx + 1);
            s->reply(s->reply_ctx, rb, strlen(rb));
        }
        break;
    case 'c':
        if (s->reply && s->csi_priv == 0)
            s->reply(s->reply_ctx, "\x1b[?6c", 5);
        break;
    case 'h': case 'l': {
        int set = (fin == 'h');
        if (s->csi_priv != '?') break;
        for (int i = 0; i < n; i++) {
            switch (p[i]) {
            case 7:  s->autowrap = set; break;
            case 25: s->cur_vis = set; break;
            case 47: case 1047:
                if (set) to_alt(s, 0); else from_alt(s, 0);
                break;
            case 1048:
                if (set) { s->asx = s->cx; s->asy = s->cy; }
                else if (s->asx < s->w && s->asy < s->h) { s->cx = s->asx; s->cy = s->asy; s->wrap = 0; }
                break;
            case 1049:
                if (set) to_alt(s, 1); else from_alt(s, 1);
                break;
            case 1000: case 1001: case 1002: case 1003: case 1006:
                csi_mode_common(s, set, p[i]);
                break;
            default: break;
            }
        }
        break;
    }
    default: break;                              /* unsupported: ignore  */
    }
}

/* ── the parser ────────────────────────────────────────────────────── */

static void csi_reset(MuxScreen *s)
{
    s->csin = 0;
    s->cp_n = 0;
    s->csi_priv = 0;
    for (int i = 0; i < 16; i++) s->cp_p[i] = -1;
}

static void csi_feed_char(MuxScreen *s, unsigned char c)
{
    if (c >= 0x40 && c <= 0x7e) {              /* final byte            */
        if (s->csin < (int)sizeof s->csib - 1) s->csib[s->csin++] = (char)c;
        /* parse params from csib[0..csin-2] (everything before final) */
        int i = 0;
        if (s->csin > 1 && (s->csib[0] == '?' || s->csib[0] == '>' ||
                            s->csib[0] == '<' || s->csib[0] == '=')) {
            s->csi_priv = s->csib[0];
            i = 1;
        }
        int cur = -1, sep = 0;
        for (; i < s->csin - 1; i++) {
            unsigned char ch = (unsigned char)s->csib[i];
            if (ch >= '0' && ch <= '9') {
                if (cur < 0) cur = 0;
                cur = cur * 10 + (ch - '0');
                if (cur > 65535) cur = 65535;
            } else if (ch == ';' || ch == ':') {
                if (s->cp_n < 16) s->cp_p[s->cp_n++] = cur;
                cur = -1;
                sep = 1;
            }
            /* intermediates (0x20-0x2f): ignored */
        }
        /* a parameterless CSI (ESC[m, ESC[K, ESC[H …) must yield cp_n == 0
         * so defaults apply — appending a phantom -1 made ESC[K erase the
         * WHOLE line instead of to-end-of-line, wiping every TUI frame */
        if (cur >= 0 || sep || s->cp_n > 0) {
            if (s->cp_n < 16) s->cp_p[s->cp_n++] = cur;
        }
        csi_dispatch(s);
        s->st = PT_NORM;
        return;
    }
    if (c == 0x1b) { csi_reset(s); s->st = PT_ESC; return; }
    if (s->csin < (int)sizeof s->csib - 1) s->csib[s->csin++] = (char)c;
}

static void feed_byte(MuxScreen *s, unsigned char c)
{
    switch (s->st) {
    case PT_NORM:
        if (c == 0x1b) { s->st = PT_ESC; return; }
        if (c == '\r') { s->cx = 0; s->wrap = 0; return; }
        if (c == '\n' || c == 0x0b) { do_lf(s); s->wrap = 0; return; }
        if (c == 0x08) { if (s->cx > 0) s->cx--; s->wrap = 0; return; }
        if (c == '\t') {
            s->cx = (s->cx / 8 + 1) * 8;
            if (s->cx > s->w - 1) s->cx = s->w - 1;
            s->wrap = 0;
            return;
        }
        if (c == 0x07 || c == 0x0e || c == 0x0f) return;
        if (c >= 0x20 && c != 0x7f) { put_cp(s, c); return; }
        return;

    case PT_ESC:
        s->st = PT_NORM;
        switch (c) {
        case '[': csi_reset(s); s->st = PT_CSI; return;
        case ']': s->st = PT_OSC; muxdbg("  [st OSC]\n"); return;
        case 'P': case '^': case '_': case 'X':
            s->st = PT_SKIP; muxdbg("  [st SKIP]\n"); return;
        case '7':
            s->scx = s->cx; s->scy = s->cy;
            s->sattrs = s->attrs; s->sfg = s->fg; s->sbg = s->bg;
            s->ssaved = 1;
            return;
        case '8':
            if (s->ssaved) {
                s->cx = s->scx; s->cy = s->scy;
                s->attrs = s->sattrs; s->fg = s->sfg; s->bg = s->sbg;
                s->wrap = 0;
            }
            return;
        case 'M':                                   /* RI                  */
            if (s->cy <= s->top) region_shift_down(s, 1);
            else if (s->cy > 0) s->cy--;
            s->wrap = 0;
            return;
        case 'D': do_lf(s); s->wrap = 0; return;    /* IND                 */
        case 'E': s->cx = 0; do_lf(s); s->wrap = 0; return;  /* NEL       */
        case '(': case ')': case '*': case '+': s->st = PT_CS; return;
        case '=': case '>': return;                  /* keypad modes: ignore */
        case 'c': {                                  /* RIS: reset the grid,
                                                      * keep scrollback      */
            free(s->alt); s->alt = NULL;
            free(s->cells);
            s->cells = grid_alloc(s->w, s->h);
            s->cx = 0; s->cy = 0; s->wrap = 0; s->autowrap = 1;
            s->attrs = 0; s->fg = cdef(); s->bg = cdef();
            s->top = 0; s->bot = s->h - 1;
            s->cur_vis = 1; s->ssaved = 0;
            s->mouse = 0;                    /* RIS drops mouse modes     */
            mark_all(s);
            return;
        }
        default: return;
        }

    case PT_CSI:
        csi_feed_char(s, c);
        return;

    case PT_OSC:
        if (c == 0x07) { s->st = PT_NORM; muxdbg("  [st NORM via BEL]\n"); return; }
        if (c == 0x1b) { s->st = PT_SKIP; return; }  /* expect ST backslash */
        return;

    case PT_SKIP:
        if (c == '\\') { s->st = PT_NORM; muxdbg("  [st NORM via ST]\n"); }
        else if (c == 0x1b) s->st = PT_ESC;          /* or a fresh escape   */
        else if (c == 0x07 && 0) s->st = PT_NORM;
        return;

    case PT_CS:                                       /* charset designator  */
        s->st = PT_NORM;                              /* consume one byte    */
        return;
    }
}

void mux_screen_feed(MuxScreen *s, const char *data, size_t n)
{
    const unsigned char *p = (const unsigned char *)data;
    size_t i = 0;
    int st_in = s->st;
    if (muxdbg_on() && n) {
        muxdbg("feed %p n=%zu st_in=%d cx=%d cy=%d\n",
               (void *)s, n, st_in, s->cx, s->cy);
        muxdbg_bytes(data, n);
    }
    while (i < n) {
        /* finish a multi-byte utf-8 sequence stashed from a prior chunk */
        if (s->u_need > 0) {
            s->u_buf[s->u_have++] = p[i++];
            if (s->u_have >= s->u_need) {
                int len = 0;
                uint32_t cp = 0;
                if (utf8_decode((const char *)s->u_buf, &len, &cp)) put_cp(s, cp);
                s->u_have = s->u_need = 0;
            }
            continue;
        }
        unsigned char c = p[i];
        if (c >= 0x80 && s->st == PT_NORM) {         /* lead byte           */
            int need = (c & 0xf8) == 0xf0 ? 3 :
                       (c & 0xf0) == 0xe0 ? 2 :
                       (c & 0xe0) == 0xc0 ? 1 : 0;
            if (need > 0) {
                s->u_buf[0] = c;
                s->u_have = 1;
                s->u_need = 1 + need;
                i++;
                continue;
            }
            put_cp(s, c);                             /* invalid: raw byte  */
            i++;
            continue;
        }
        feed_byte(s, c);
        i++;
    }
    if (muxdbg_on() && n)
        muxdbg("  -> st_out=%d cx=%d cy=%d wrap=%d alt=%d\n",
               s->st, s->cx, s->cy, s->wrap, s->alt != NULL);
}

/* ── repaint ───────────────────────────────────────────────────────── */

static void emit_sgr(char *buf, size_t sz, uint8_t attrs, MuxColor fg, MuxColor bg)
{
    char a[48], f[40], b[40];
    a[0] = 0;
    size_t ao = 0;
    ao += (size_t)snprintf(a + ao, sizeof a - ao, ";0");   /* reset first */
    if (attrs & CA_BOLD)   ao += (size_t)snprintf(a + ao, sizeof a - ao, ";1");
    if (attrs & CA_DIM)    ao += (size_t)snprintf(a + ao, sizeof a - ao, ";2");
    if (attrs & CA_ITALIC) ao += (size_t)snprintf(a + ao, sizeof a - ao, ";3");
    if (attrs & CA_UL)     ao += (size_t)snprintf(a + ao, sizeof a - ao, ";4");
    if (attrs & CA_BLINK)  ao += (size_t)snprintf(a + ao, sizeof a - ao, ";5");
    if (attrs & CA_REV)    ao += (size_t)snprintf(a + ao, sizeof a - ao, ";7");
    if (attrs & CA_INVIS)  ao += (size_t)snprintf(a + ao, sizeof a - ao, ";8");
    if (attrs & CA_STRIKE) ao += (size_t)snprintf(a + ao, sizeof a - ao, ";9");

    switch (fg.kind) {
    case 1: snprintf(f, sizeof f, ";%u", fg.idx); break;
    case 2: snprintf(f, sizeof f, ";38;5;%u", fg.idx); break;
    case 3: snprintf(f, sizeof f, ";38;2;%u;%u;%u", fg.r, fg.g, fg.b); break;
    default: snprintf(f, sizeof f, ";39"); break;
    }
    switch (bg.kind) {
    case 1: snprintf(b, sizeof b, ";%u", bg.idx); break;
    case 2: snprintf(b, sizeof b, ";48;5;%u", bg.idx); break;
    case 3: snprintf(b, sizeof b, ";48;2;%u;%u;%u", bg.r, bg.g, bg.b); break;
    default: snprintf(b, sizeof b, ";49"); break;
    }
    snprintf(buf, sz, "\033[%s%s%sm", a + 1, f, b);   /* m closes the SGR */
}

static int cell_is_blank(const MuxCell *c)
{
    return c->cp == 0 && c->attrs == 0 && c->fg.kind == 0 && c->bg.kind == 0;
}

/* repaint one display row: `abs_r`/`abs_c` are 1-based terminal coords */
static void repaint_row(MuxScreen *s, const MuxCell *row, int roww,
                        int abs_r, int abs_c)
{
    int w = s->w;
    int last = -1;
    int n = roww < w ? roww : w;
    for (int c = 0; c < n; c++)
        if (!cell_is_blank(&row[c])) last = c;

    tw("\033[%d;%dH", abs_r, abs_c);
    if (last < 0) {                                 /* empty line        */
        tw("\033[0m\033[%dX", w);
        return;
    }
    char sgrbuf[96];
    uint32_t cur_attrs = 0xffffffffu;
    MuxColor cf = cdef(), cb = cdef();
    int out_len = 0;
    for (int c = 0; c <= last; c++) {
        const MuxCell *cell = &row[c];
        if (cell->pad) continue;             /* painted with its glyph  */
        if (cell->attrs != cur_attrs ||
            cell->fg.kind != cf.kind || cell->fg.idx != cf.idx ||
            cell->fg.r != cf.r || cell->fg.g != cf.g || cell->fg.b != cf.b ||
            cell->bg.kind != cb.kind || cell->bg.idx != cb.idx ||
            cell->bg.r != cb.r || cell->bg.g != cb.g || cell->bg.b != cb.b) {
            emit_sgr(sgrbuf, sizeof sgrbuf, cell->attrs, cell->fg, cell->bg);
            tw("%s", sgrbuf);
            cur_attrs = cell->attrs; cf = cell->fg; cb = cell->bg;
        }
        if (cell->cp) {
            char tmp[8];
            int l = utf8_encode(cell->cp, tmp);
            /* MUST use the batched writer: a direct write here would
             * overtake the buffered CUP/SGR and paint the glyph at the
             * terminal's old cursor position (scrambled pane output) */
            tty_buf_write(tmp, (size_t)l);
            out_len += cp_width(cell->cp);
        } else {
            tw(" ");
            out_len++;
        }
    }
    tw("\033[0m");
    /* Erase the tail INSIDE the pane only — ESC[K (erase to end of line)
     * would run past the pane's right edge and wipe the separator column
     * (vertical dividers share rows with panes) and the neighbouring
     * pane's content. */
    if (out_len < w) tw("\033[%dX", w - out_len);
}

void mux_screen_repaint(MuxScreen *s, int x, int y, int w, int h)
{
    if (w < 1 || h < 1) return;
    int rows = s->h < h ? s->h : h;              /* never paint past rect */
    for (int r = 0; r < rows; r++) {
        if (!s->all_dirty && s->view == 0 && !s->dirty[r]) continue;
        const MuxCell *row;
        int roww = s->w;
        if (s->view == 0) {
            row = s->cells + (size_t)r * (size_t)s->w;
        } else {
            int total = s->sb_count + s->h;
            int top_v = total - s->h - s->view;
            if (top_v < 0) top_v = 0;
            int v = top_v + r;
            if (v < s->sb_count) {
                row = sb_row(s, v, &roww);
                if (!row) continue;
            } else {
                int lr = v - s->sb_count;
                if (lr >= s->h) continue;
                row = s->cells + (size_t)lr * (size_t)s->w;
            }
        }
        repaint_row(s, row, roww, y + 1 + r, x + 1);
    }

    if (s->view == 0) {
        memset(s->dirty, 0, (size_t)s->h);
        s->all_dirty = 0;
    }
}
