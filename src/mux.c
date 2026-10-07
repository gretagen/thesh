#define _GNU_SOURCE
#include "thesh.h"
#include <pty.h>
#include <sys/ioctl.h>

/* ── Native terminal multiplexer (tmux process model) ─────────────────
 * Two roles in one binary:
 *   - plain shell (active == 0): a normal thesh; the first split forks —
 *     the child keeps being this shell on a fresh pty (a pane), the
 *     parent becomes the router and never prompts again;
 *   - router (active == 1): a pure router. A binary tree of rectangles;
 *     the focused leaf owns the keyboard, the pane under the pointer
 *     owns the mouse. Keys are routed: mux VERB binds run here,
 *     everything else goes raw to the focused pane, where that shell's
 *     own bind table handles it. Every pane's pty feeds a MuxScreen
 *     repainted into its rect. When the last pane dies the router
 *     execs a fresh plain shell on the real terminal. */

typedef struct Node {
    struct Node *ch[2];
    int vertical;                        /* 1 = side-by-side (│ divider) */
    int pct;
    MuxPane *pane;                       /* leaf only                     */
    int x, y, w, h;
} Node;

static int      active = 0;              /* 1 = this process is the router */
static Node    *root = NULL;
static MuxPane *panes[MUX_MAX_PANES];
static int      npanes = 0;
static MuxPane *focus = NULL;
static volatile sig_atomic_t winch_flag = 0;
static char     self_path[PATH_MAX];
static int      mouse_fwd_press = 0;      /* pass-through click is down   */
static MuxPane *click_pane = NULL;        /* pane that received the press */
static int      mouse_sent = -1;          /* mouse level on the real tty  */
static char     mouse_seq[40];            /* translated SGR event         */
static int      mouse_seq_len = 0;
static int      park_force = 1;           /* re-park even at same spot    */
static int      seps_dirty = 0;           /* borders recolor: separators
                                             only, no pane cell repaint   */
static int      parked_row = -1, parked_col = -1, parked_vis = -1;

void mux_notify_winch(void) { winch_flag = 1; }

int mux_active(void) { return active; }

static void mux_router_loop(void);       /* defined after mux_split     */

void mux_term_write(const char *b, size_t n)
{
    /* any pending batched repaint must reach the terminal FIRST — a
     * direct write overtaking the buffer would scramble pane output */
    tty_buf_flush();
    while (n) {
        ssize_t r = write(STDOUT_FILENO, b, n);
        if (r < 0) {
            if (errno == EINTR) continue;
            return;
        }
        b += r; n -= (size_t)r;
    }
}

/* ── tree helpers ──────────────────────────────────────────────────── */

static Node *node_new(MuxPane *p)
{
    Node *n = xmalloc(sizeof *n);
    memset(n, 0, sizeof *n);
    n->pane = p;
    return n;
}

static Node *node_find_pane(MuxPane *p)
{
    Node *st[MUX_MAX_PANES + 4];
    int sp = 0;
    if (root) st[sp++] = root;
    while (sp) {
        Node *n = st[--sp];
        if (n->pane == p) return n;
        if (n->ch[0]) { st[sp++] = n->ch[0]; st[sp++] = n->ch[1]; }
    }
    return NULL;
}

static Node *node_parent(Node *target)
{
    if (!root || root == target) return NULL;
    Node *st[MUX_MAX_PANES + 4];
    int sp = 0;
    st[sp++] = root;
    while (sp) {
        Node *n = st[--sp];
        if (n->ch[0] == target || n->ch[1] == target) return n;
        if (n->ch[0]) { st[sp++] = n->ch[0]; st[sp++] = n->ch[1]; }
    }
    return NULL;
}

static void collect_leaves(Node *n, Node **out, int *cnt)
{
    if (!n) return;
    if (!n->ch[0]) {
        if (*cnt < MUX_MAX_PANES) out[(*cnt)++] = n;
        return;
    }
    collect_leaves(n->ch[0], out, cnt);
    collect_leaves(n->ch[1], out, cnt);
}

static Node *first_leaf(Node *n)
{
    while (n && n->ch[0]) n = n->ch[0];
    return n;
}

/* replace `old` with `sub` and free `old` */
static void replace_node(Node *old, Node *sub)
{
    Node *par = node_parent(old);
    if (!par) root = sub;
    else if (par->ch[0] == old) par->ch[0] = sub;
    else par->ch[1] = sub;
    free(old);
}

static void free_tree(Node *n)
{
    if (!n) return;
    free_tree(n->ch[0]);
    free_tree(n->ch[1]);
    free(n);
}

static void mark_all_panes(void)
{
    for (int i = 0; i < npanes; i++) {
        MuxScreen *s = &panes[i]->scr;
        s->all_dirty = 1;
        if (s->dirty) memset(s->dirty, 1, (size_t)s->h);
    }
}

/* ── layout ────────────────────────────────────────────────────────── */

static void pane_set_rect(MuxPane *p, int x, int y, int w, int h)
{
    p->x = x; p->y = y; p->w = w; p->h = h;
    int iw = w > 0 ? w : 1;                 /* the rect IS the interior  */
    int ih = h > 0 ? h : 1;
    if (p->scr.w != iw || p->scr.h != ih)
        muxdbg("rect: pane %p -> %d,%d %dx%d (scr %d,%d -> %d,%d; cx,cy=%d,%d)\n",
               (void *)p, x, y, iw, ih, p->scr.w, p->scr.h, iw, ih,
               p->scr.cx, p->scr.cy);
    if (!p->scr.cells) mux_screen_init(&p->scr, iw, ih);
    else               mux_screen_resize(&p->scr, iw, ih);
    if (p->master >= 0) {
        struct winsize ws;
        memset(&ws, 0, sizeof ws);
        ws.ws_col = (unsigned short)iw;
        ws.ws_row = (unsigned short)ih;
        ioctl(p->master, TIOCSWINSZ, &ws);
    }
}

static void layout_rec(Node *n, int x, int y, int w, int h)
{
    n->x = x; n->y = y; n->w = w; n->h = h;
    if (!n->ch[0]) {
        pane_set_rect(n->pane, x, y, w, h);
        return;
    }
    /* children + the 1-cell divider between them = the region (tmux
     * layout_split: `saved_size - 1 - new_size`); dividers belong to the
     * node and are drawn by the separator pass, not to any pane. */
    if (n->vertical) {
        int L = (int)((long long)w * n->pct / 100);
        if (L < 1) L = 1;
        if (w >= 3 && L > w - 2) L = w - 2;  /* both sides >= 1 cell */
        layout_rec(n->ch[0], x, y, L, h);
        layout_rec(n->ch[1], x + L + 1, y, w - L - 1, h);
    } else {
        int T = (int)((long long)h * n->pct / 100);
        if (T < 1) T = 1;
        if (h >= 3 && T > h - 2) T = h - 2;
        layout_rec(n->ch[0], x, y, w, T);
        layout_rec(n->ch[1], x, y + T + 1, w, h - T - 1);
    }
}

void term_size(int *cols, int *rows)
{
    struct winsize ws;
    memset(&ws, 0, sizeof ws);
    /* stdout is the right fd in every role: the real terminal for a
     * plain shell / the router, this pane's pty for a pane shell */
    ioctl(STDOUT_FILENO, TIOCGWINSZ, &ws);
    if (!ws.ws_col) ws.ws_col = 80;
    if (!ws.ws_row) ws.ws_row = 24;
    *cols = ws.ws_col;
    *rows = ws.ws_row;
}

/* ── pane reply (DSR/DA from children) ─────────────────────────────── */

static void pane_reply(void *ctx, const char *b, size_t n)
{
    int fd = (int)(intptr_t)ctx;
    while (n) {
        ssize_t r = write(fd, b, n);
        if (r < 0) {
            if (errno == EINTR) continue;
            return;
        }
        b += r; n -= (size_t)r;
    }
}

/* ── activation: fork-on-split (tmux process model) ─────────────────── */

/* First split from a plain shell: the child keeps being this exact
 * shell — same line, aliases, cwd, history — on a fresh pty (its own
 * pane); the parent becomes the pure router and never prompts again.
 * Returns 1 in the parent, 0 in the child (the caller must return
 * straight back into the REPL), -1 on error. */
static int fork_on_split(void)
{
    if (!self_path[0]) {
        ssize_t n = readlink("/proc/self/exe", self_path, sizeof self_path - 1);
        if (n > 0) self_path[n] = 0;
        else snprintf(self_path, sizeof self_path, "%s", THESH_NAME);
    }

    int cols, rows;
    term_size(&cols, &rows);
    struct winsize ws;
    memset(&ws, 0, sizeof ws);
    ws.ws_col = (unsigned short)(cols > 0 ? cols : 80);
    ws.ws_row = (unsigned short)(rows > 0 ? rows : 24);

    int master = -1, slave = -1;
    if (openpty(&master, &slave, NULL, NULL, &ws) < 0) {
        dprintf(STDERR_FILENO, "%s: mux: openpty: %s\n",
                THESH_NAME, strerror(errno));
        return -1;
    }

    pid_t pid = fork();
    if (pid < 0) {
        dprintf(STDERR_FILENO, "%s: mux: fork: %s\n",
                THESH_NAME, strerror(errno));
        close(master);
        close(slave);
        return -1;
    }

    if (pid == 0) {
        /* child: continue as the interactive shell, now pane-backed */
        close(master);
        setsid();
        if (ioctl(slave, TIOCSCTTY, 0) < 0)
            muxdbg("fork-on-split: TIOCSCTTY: %s\n", strerror(errno));
        dup2(slave, STDIN_FILENO);
        dup2(slave, STDOUT_FILENO);
        dup2(slave, STDERR_FILENO);
        if (slave > STDERR_FILENO) close(slave);
        setenv("THESH_MUX", "1", 1);
        term_enter_raw();               /* the pane pty starts cooked   */
        ed_reset_anchor();              /* old screen coords are void   */
        muxdbg("fork-on-split: child shell on pane pty (pid=%d)\n",
               (int)getpid());
        return 0;
    }

    close(slave);

    /* The router owns the real terminal from here. Activation usually
     * happens inside exec_unit's term_exit_raw() bracket (split is a
     * command!) and the matching term_enter_raw() sits below this
     * router loop — unreachable — so re-enter raw mode ourselves.
     * Idempotent: a full set if the bracket left it cooked. */
    term_enter_raw();

    MuxPane *p = xmalloc(sizeof *p);
    memset(p, 0, sizeof *p);
    p->master = master;
    p->pid = pid;
    mux_screen_init(&p->scr, ws.ws_col, ws.ws_row);
    p->scr.reply = pane_reply;
    p->scr.reply_ctx = (void *)(intptr_t)master;
    muxdbg("pane-map %p pid=%d (forked shell)\n", (void *)&p->scr,
           (int)pid);

    root = node_new(p);
    root->x = 0; root->y = 0; root->w = cols; root->h = rows;
    npanes = 0;
    panes[npanes++] = p;
    focus = p;

    active = 1;
    muxdbg("router: fork-on-split, original shell = pane pid=%d\n",
           (int)pid);
    return 1;
}

/* ── children ──────────────────────────────────────────────────────── */

static MuxPane *pane_spawn(int w, int h)
{
    MuxPane *p = xmalloc(sizeof *p);
    memset(p, 0, sizeof *p);
    p->master = -1; p->pid = -1;

    struct winsize ws;
    memset(&ws, 0, sizeof ws);
    ws.ws_col = (unsigned short)(w > 0 ? w : 1);
    ws.ws_row = (unsigned short)(h > 0 ? h : 1);

    pid_t pid = forkpty(&p->master, NULL, NULL, &ws);
    if (pid < 0) { free(p); return NULL; }
    /* answer DSR/DA inside the child shell — it needs its cursor for
     * wrap-aware line drawing, just like a real terminal would */
    mux_screen_init(&p->scr, ws.ws_col, ws.ws_row);
    p->scr.reply = pane_reply;
    p->scr.reply_ctx = (void *)(intptr_t)p->master;
    if (pid > 0)
        muxdbg("pane-map %p pid=%d (spawned)\n", (void *)&p->scr,
               (int)pid);
    if (pid == 0) {
        signal(SIGINT, SIG_DFL);
        signal(SIGQUIT, SIG_DFL);
        signal(SIGTSTP, SIG_DFL);
        signal(SIGPIPE, SIG_DFL);
        signal(SIGWINCH, SIG_DFL);
        setenv("THESH_MUX", "1", 1);
        /* forkpty should have given us the pane pty as our controlling
         * terminal; without it apps that open /dev/tty (micro, tcell)
         * die with ENXIO. Verify the exact operation they perform and
         * retry the acquire once if it's missing. */
        {
            int t = open("/dev/tty", O_RDWR);
            if (t < 0) {
                ioctl(STDIN_FILENO, TIOCSCTTY, 0);
                t = open("/dev/tty", O_RDWR);
                if (t < 0)
                    muxdbg("pane_spawn: no /dev/tty (ctty missing): %s\n",
                           strerror(errno));
            }
            if (t >= 0) close(t);
        }
        execl(self_path, self_path, (char *)NULL);
        _exit(127);
    }
    p->pid = pid;
    return p;
}

static void pane_free(MuxPane *p)
{
    if (!p) return;
    if (p->master >= 0) close(p->master);
    if (p->pid > 0) {
        kill(p->pid, SIGHUP);
        for (int i = 0; i < 50; i++) {
            if (waitpid(p->pid, NULL, WNOHANG) == p->pid) goto done;
            struct timespec ts = { 0, 10 * 1000 * 1000 };
            nanosleep(&ts, NULL);
        }
        kill(p->pid, SIGKILL);
        waitpid(p->pid, NULL, 0);
    }
done:
    mux_screen_free(&p->scr);
    free(p);
}

static void pane_drop(MuxPane *p)
{
    for (int i = 0; i < npanes; i++) {
        if (panes[i] == p) {
            memmove(&panes[i], &panes[i + 1],
                    sizeof(MuxPane *) * (size_t)(npanes - i - 1));
            npanes--;
            return;
        }
    }
}

/* ── split / close ─────────────────────────────────────────────────── */

int mux_split(int vertical, int pct)
{
    if (!Cfg.multiplexer) {
        dprintf(STDERR_FILENO, "%s: mux: disabled in config\n", THESH_NAME);
        return -1;
    }
    int activated = 0;
    if (!active) {
        int r = fork_on_split();
        if (r < 0) return -1;
        if (r == 0) return 0;       /* child: pane shell, nothing to split */
        activated = 1;              /* parent: router, shell pane = #1      */
    }
    int lim = Cfg.mux_window_limit ? Cfg.mux_window_limit : MUX_MAX_PANES;
    if (lim > MUX_MAX_PANES) lim = MUX_MAX_PANES;
    if (npanes >= lim) {
        dprintf(STDERR_FILENO, "%s: mux: too many panes (%d)\n",
                THESH_NAME, lim);
        return -1;
    }
    if (pct < 10) pct = 10;
    if (pct > 90) pct = 90;

    Node *f = node_find_pane(focus);
    if (!f) return -1;
    /* both sides + the divider cell must fit */
    if (vertical && f->w < 2 * MUX_MIN_W + 1) {
        dprintf(STDERR_FILENO, "%s: mux: pane too small to split\n", THESH_NAME);
        return -1;
    }
    if (!vertical && f->h < 2 * MUX_MIN_H + 1) {
        dprintf(STDERR_FILENO, "%s: mux: pane too small to split\n", THESH_NAME);
        return -1;
    }

    int nw = vertical ? f->w * pct / 100 : f->w;
    int nh = vertical ? f->h : f->h * pct / 100;
    MuxPane *np = pane_spawn(nw, nh);
    if (!np) {
        dprintf(STDERR_FILENO, "%s: mux: fork failed: %s\n",
                THESH_NAME, strerror(errno));
        return -1;
    }
    panes[npanes++] = np;

    Node *leaf = node_new(np);
    Node *par = node_new(NULL);
    par->vertical = vertical;
    par->pct = pct;
    par->ch[0] = f;
    par->ch[1] = leaf;
    /* put `par` where `f` stood — `f` stays alive as par->ch[0] */
    Node *fp = node_parent(f);
    if (!fp) root = par;
    else if (fp->ch[0] == f) fp->ch[0] = par;
    else fp->ch[1] = par;

    int cols, rows;
    term_size(&cols, &rows);
    layout_rec(root, 0, 0, cols, rows);
    mark_all_panes();
    if (activated)
        mux_router_loop();          /* never returns — the shell owns the
                                     * terminal no more; on the last pane
                                     * dying it execs a fresh one */
    return 0;
}

/* close one pane: splice its parent out of the tree, fix focus */
static int close_pane(MuxPane *victim)
{
    if (!victim) return -1;
    Node *n = node_find_pane(victim);
    if (!n) return -1;
    Node *par = node_parent(n);
    if (!par) {
        /* the only pane left: kill it — the router loop's top sees
         * npanes == 0 and execs a fresh shell (mux_leave) */
        pane_drop(victim);
        focus = NULL;
        pane_free(victim);
        muxdbg("close: last pane gone, router will leave\n");
        return 0;
    }
    int was_focus = (focus == victim);
    Node *sib = (par->ch[0] == n) ? par->ch[1] : par->ch[0];
    Node *nl = first_leaf(sib);
    replace_node(par, sib);
    pane_drop(victim);
    if (was_focus) focus = nl ? nl->pane : NULL;
    pane_free(victim);
    if (!focus && root) {
        Node *lf = first_leaf(root);
        focus = lf ? lf->pane : NULL;
    }
    int cols, rows;
    term_size(&cols, &rows);
    layout_rec(root, 0, 0, cols, rows);
    mark_all_panes();
    return 0;
}

/* drain a child's pty fully (non-blocking) so its last output shows */
static void drain_dead_pane(MuxPane *p)
{
    if (p->master < 0) return;
    int fl = fcntl(p->master, F_GETFL);
    fcntl(p->master, F_SETFL, fl | O_NONBLOCK);
    char buf[8192];
    ssize_t r;
    while ((r = read(p->master, buf, sizeof buf)) > 0)
        mux_screen_feed(&p->scr, buf, (size_t)r);
    fcntl(p->master, F_SETFL, fl);
}

int mux_close(void)
{
    if (!active) {
        dprintf(STDERR_FILENO, "%s: mux: not multiplexed\n", THESH_NAME);
        return -1;
    }
    /* tmux kill-pane: the focused pane dies; the last pane's death makes
     * the router leave the multiplexer (router loop top checks npanes) */
    return close_pane(focus);
}

/* kill every pane — `kill-all-windows` is always an exit-ward step */
int mux_kill_all(void)
{
    if (!active) {
        dprintf(STDERR_FILENO, "%s: mux: not multiplexed\n", THESH_NAME);
        return -1;
    }
    while (npanes > 0) {
        MuxPane *v = panes[0];
        if (close_pane(v) != 0) break;
    }
    return 0;
}

/* ── focus ─────────────────────────────────────────────────────────── */

static void focus_set(MuxPane *p)
{
    if (!p || p == focus) return;
    focus = p;
    mouse_fwd_press = 0;                  /* a new pane needs a new press */
    click_pane = NULL;
    /* Only the separator colours change with focus — pane cell content
     * never does. Repainting every cell of every pane here (the old
     * mark_all_panes) was a big part of click lag on colored TUIs. */
    seps_dirty = 1;
    muxdbg("focus -> pane %p at %d,%d %dx%d\n",
           (void *)p, p->x, p->y, p->w, p->h);
}

int mux_focus_cycle(int delta)
{
    if (!active) return -1;
    Node *lv[MUX_MAX_PANES];
    int cnt = 0;
    collect_leaves(root, lv, &cnt);
    if (cnt < 2) return 0;
    int idx = 0;
    for (int i = 0; i < cnt; i++)
        if (lv[i]->pane == focus) idx = i;
    idx = (idx + delta) % cnt;
    if (idx < 0) idx += cnt;
    focus_set(lv[idx]->pane);
    return 0;
}

int mux_focus_dir(int dir)
{
    if (!active || !focus) return -1;
    MuxPane *f = focus;
    Node *lv[MUX_MAX_PANES];
    int cnt = 0;
    collect_leaves(root, lv, &cnt);
    int best = -1, best_dist = INT_MAX;
    for (int i = 0; i < cnt; i++) {
        MuxPane *q = lv[i]->pane;
        if (q == f) continue;
        int overlap;
        if (dir <= 1)                                   /* left / right */
            overlap = q->y < f->y + f->h && q->y + q->h > f->y;
        else                                             /* up / down    */
            overlap = q->x < f->x + f->w && q->x + q->w > f->x;
        if (!overlap) continue;
        int dist = -1;
        switch (dir) {
        case 0: if (q->x + q->w <= f->x) dist = f->x - (q->x + q->w); break;
        case 1: if (q->x >= f->x + f->w) dist = q->x - (f->x + f->w); break;
        case 2: if (q->y + q->h <= f->y) dist = f->y - (q->y + q->h); break;
        case 3: if (q->y >= f->y + f->h) dist = q->y - (f->y + f->h); break;
        }
        if (dist >= 0 && dist < best_dist) { best_dist = dist; best = i; }
    }
    if (best < 0) return 0;                           /* edge: no-op    */
    focus_set(lv[best]->pane);
    return 0;
}

/* ── scrollback view ───────────────────────────────────────────────── */

static void scroll_pane(MuxPane *p, int delta)
{
    if (!p) return;
    MuxScreen *s = &p->scr;
    if (delta == 0) {
        if (s->view) { s->view = 0; s->all_dirty = 1; }
        return;
    }
    s->view += delta;
    if (s->view > s->sb_count) s->view = s->sb_count;
    if (s->view < 0) s->view = 0;
    s->all_dirty = 1;
}

void mux_scroll(int delta)
{
    if (active && focus) scroll_pane(focus, delta);
}

/* ── input forwarding ──────────────────────────────────────────────── */

void mux_forward(const char *b, size_t n)
{
    if (!active || !focus || focus->master < 0) return;
    while (n) {
        ssize_t r = write(focus->master, b, n);
        if (r < 0) {
            if (errno == EINTR) continue;
            return;
        }
        b += r; n -= (size_t)r;
    }
}

/* ── resize ────────────────────────────────────────────────────────── */

static void do_resize(void)
{
    if (!root) return;
    int cols, rows;
    term_size(&cols, &rows);
    if (root->w == cols && root->h == rows) return;
    layout_rec(root, 0, 0, cols, rows);
    mark_all_panes();
    park_force = 1;                       /* the terminal clamped too     */
    mouse_sent = -1;                      /* re-assert mouse on next pump */
}

void mux_resize_check(void)
{
    if (active && winch_flag) {
        winch_flag = 0;
        do_resize();
    }
}

/* SIGCONT (back from a VT switch / job control): the terminal may have
 * dropped raw mode, mouse reporting and our cursor — re-assert all. */
void thesh_on_cont(void)
{
    if (!thesh_conted) return;
    thesh_conted = 0;
    term_enter_raw();                     /* idempotent                  */
    if (active) {
        mouse_sent = -1;                  /* terminal dropped our modes  */
        park_force = 1;                   /* cursor state is unknown now */
    }
    thesh_winched = 1;                    /* re-layout + redraw the line */
    mux_notify_winch();
}

/* ── tmux-style separators (crosshair) ──────────────────────────────
 * Panes are pure interiors; the 1-cell lines between them belong to the
 * tree. Every separator cell is classified by its four connecting sides
 * (tmux CELL_*: plain lines, corners, tees, cross) and coloured by
 * adjacency: a segment touching the focused pane gets the active colour.
 * Nothing is drawn on the terminal's outer edges. */

typedef struct { int x, y; } SepPos;

static void tf(const char *fmt, ...)
{
    char buf[640];
    va_list ap;
    va_start(ap, fmt);
    int n = vsnprintf(buf, sizeof buf, fmt, ap);
    va_end(ap);
    if (n > 0) {
        size_t len = (size_t)n < sizeof buf ? (size_t)n : sizeof buf - 1;
        tty_buf_write(buf, len);            /* batched: flushed per pump  */
    }
}

static void sep_add(SepPos **out, int *cnt, int *cap, int x, int y)
{
    if (*cnt >= *cap) {
        *cap = *cap ? *cap * 2 : 256;
        *out = xrealloc(*out, sizeof(SepPos) * (size_t)*cap);
    }
    (*out)[(*cnt)++] = (SepPos){ x, y };
}

static void sep_collect(Node *n, SepPos **out, int *cnt, int *cap)
{
    if (!n || !n->ch[0]) return;
    if (n->vertical) {
        int cx = n->ch[0]->x + n->ch[0]->w;       /* divider column  */
        for (int r = n->y; r < n->y + n->h; r++)
            sep_add(out, cnt, cap, cx, r);
    } else {
        int cy = n->ch[0]->y + n->ch[0]->h;       /* divider row     */
        for (int c = n->x; c < n->x + n->w; c++)
            sep_add(out, cnt, cap, c, cy);
    }
    sep_collect(n->ch[0], out, cnt, cap);
    sep_collect(n->ch[1], out, cnt, cap);
}

/* connectivity bits: 1 up, 2 down, 4 left, 8 right */
static uint8_t sep_bits(const uint8_t *map, int cols, int rows, int x, int y)
{
    uint8_t b = 0;
    if (y > 0      && map[((y - 1) * cols) + x]) b |= 1;
    if (y + 1 < rows && map[((y + 1) * cols) + x]) b |= 2;
    if (x > 0      && map[(y * cols) + (x - 1)]) b |= 4;
    if (x + 1 < cols && map[(y * cols) + (x + 1)]) b |= 8;
    return b;
}

static const char *sep_glyph(uint8_t b)
{
    switch (b) {
    case 1:  case 2:  case 1 | 2:      return "\xe2\x94\x82"; /* │ */
    case 4:  case 8:  case 4 | 8:      return "\xe2\x94\x80"; /* ─ */
    case 2 | 8:                        return "\xe2\x94\x8c"; /* ┌ */
    case 2 | 4:                        return "\xe2\x94\x90"; /* ┐ */
    case 1 | 8:                        return "\xe2\x94\x94"; /* └ */
    case 1 | 4:                        return "\xe2\x94\x98"; /* ┘ */
    case 1 | 2 | 8:                    return "\xe2\x94\x9c"; /* ├ */
    case 1 | 2 | 4:                    return "\xe2\x94\xa4"; /* ┤ */
    case 4 | 8 | 2:                    return "\xe2\x94\xac"; /* ┬ */
    case 4 | 8 | 1:                    return "\xe2\x94\xb4"; /* ┴ */
    case 1 | 2 | 4 | 8:                return "\xe2\x94\xbc"; /* ┼ */
    default:                           return NULL;
    }
}

static MuxPane *pane_at(int x, int y)
{
    for (int i = 0; i < npanes; i++) {
        MuxPane *p = panes[i];
        if (x >= p->x && x < p->x + p->w && y >= p->y && y < p->y + p->h)
            return p;
    }
    /* clicked on a separator (or just outside): snap to the nearest
     * pane — in stacked layouts the divider row is a prime click target */
    MuxPane *best = NULL;
    int best_d = INT_MAX;
    for (int i = 0; i < npanes; i++) {
        MuxPane *p = panes[i];
        int dx = 0, dy = 0;
        if (x < p->x) dx = p->x - x;
        else if (x >= p->x + p->w) dx = x - (p->x + p->w) + 1;
        if (y < p->y) dy = p->y - y;
        else if (y >= p->y + p->h) dy = y - (p->y + p->h) + 1;
        int d = dx * dx + dy * dy;
        if (d < best_d) { best_d = d; best = p; }
    }
    return best;
}

/* Exact pane hit: NULL on a divider — button events must never snap a
 * click into a neighbouring pane's program (the wheel still snaps). */
static MuxPane *pane_at_exact(int x, int y)
{
    for (int i = 0; i < npanes; i++) {
        MuxPane *p = panes[i];
        if (x >= p->x && x < p->x + p->w && y >= p->y && y < p->y + p->h)
            return p;
    }
    return NULL;
}

static int sep_hot(int x, int y)          /* neighbour pane is focused? */
{
    if (!focus) return 0;
    return pane_at(x - 1, y) == focus || pane_at(x + 1, y) == focus ||
           pane_at(x, y - 1) == focus || pane_at(x, y + 1) == focus;
}

static const char *sep_color(int hot)
{
    const char *c = hot ? Cfg.col_mux_active : Cfg.col_mux_inactive;
    if (!c || !c[0]) c = hot ? "34" : "39";      /* tmux: blue / default */
    return c;
}

static void draw_separators(void)
{
    if (!root || npanes == 0) return;
    int cols, rows;
    term_size(&cols, &rows);

    SepPos *cells = NULL;
    int cnt = 0, cap = 0;
    sep_collect(root, &cells, &cnt, &cap);
    if (cnt == 0) { free(cells); return; }

    uint8_t *map = xmalloc((size_t)cols * (size_t)rows);
    memset(map, 0, (size_t)cols * (size_t)rows);
    for (int i = 0; i < cnt; i++) {
        SepPos c = cells[i];
        if (c.x >= 0 && c.x < cols && c.y >= 0 && c.y < rows)
            map[((size_t)c.y * (size_t)cols) + (size_t)c.x] = 1;
    }

    int i = 0;
    while (i < cnt) {
        SepPos c0 = cells[i];
        if (c0.x < 0 || c0.x >= cols || c0.y < 0 || c0.y >= rows) { i++; continue; }
        const char *g = sep_glyph(sep_bits(map, cols, rows, c0.x, c0.y));
        const char *col = sep_color(sep_hot(c0.x, c0.y));
        if (!g) { i++; continue; }

        int dx = 0, dy = 0;                /* run direction within segment */
        if (i + 1 < cnt) {
            if (cells[i + 1].x == c0.x && cells[i + 1].y == c0.y + 1) dy = 1;
            else if (cells[i + 1].y == c0.y && cells[i + 1].x == c0.x + 1) dx = 1;
        }
        int j = i + 1;
        while (dx || dy) {
            if (j >= cnt) break;
            SepPos c = cells[j], pv = cells[j - 1];
            if (c.x != pv.x + dx || c.y != pv.y + dy) break;
            const char *g2 = sep_glyph(sep_bits(map, cols, rows, c.x, c.y));
            if (!g2 || strcmp(g2, g) != 0) break;
            if (strcmp(sep_color(sep_hot(c.x, c.y)), col) != 0) break;
            j++;
        }

        tf("\033[0m");
        /* Per-cell positioning: a vertical run must step DOWN each row —
         * streaming the glyphs after one CUP would paint them sideways
         * across a single row. */
        for (int k = i; k < j; k++)
            tf("\x1b[%d;%dH\033[%sm%s", cells[k].y + 1, cells[k].x + 1, col, g);
        tf("\033[0m");
        i = j;
    }
    free(map);
    free(cells);
}

/* ── mouse: wheel scrollback + click-through (tmux model) ─────────────
 * Clicks never change focus — the user moves between panes with the
 * move binds only. A press goes to the program in the pane under the
 * pointer, translated to pane-relative coordinates (tmux cmd_mouse_at)
 * and only if that program asked for mouse tracking (per-pane
 * ?1000/?1002/?1003, tmux input.c). The wheel scrolls whichever pane
 * it is over. Button events on a divider or on scrollback are
 * swallowed. */

static MuxPane *pane_alive(MuxPane *p)
{
    if (!p) return NULL;
    for (int i = 0; i < npanes; i++)
        if (panes[i] == p) return p;
    return NULL;
}

/* SGR event with pane-relative coordinates: x - pane->x + 1, clamped
 * to the pane (tmux cmd_mouse_at + input_key_get_mouse). */
static void build_mouse_seq(MuxPane *p, int px, int py, int button,
                            int release)
{
    int lx = px - p->x + 1;
    int ly = py - p->y + 1;
    if (lx < 1) lx = 1;
    if (lx > p->w) lx = p->w;
    if (ly < 1) ly = 1;
    if (ly > p->h) ly = p->h;
    int n = snprintf(mouse_seq, sizeof mouse_seq, "\033[<%d;%d;%d%c",
                     button, lx, ly, release ? 'm' : 'M');
    mouse_seq_len = (n > 0 && n < (int)sizeof mouse_seq) ? n : 0;
}

const char *mux_mouse_seq(int *len)
{
    if (len) *len = mouse_seq_len;
    return mouse_seq;
}

int mux_mouse(int button, int x, int y, int release)
{
    if (!active) return 0;
    int px = x - 1, py = y - 1;
    if (button == 64 || button == 65) {                 /* wheel up/down */
        MuxPane *p = pane_at(px, py);
        if (!p) p = focus;
        scroll_pane(p, button == 64 ? 3 : -3);
        return 0;
    }
    if (release) {                                      /* SGR 'm'       */
        int f = (button == 0) ? mouse_fwd_press : 0;
        mouse_fwd_press = 0;
        MuxPane *p = pane_alive(click_pane);
        if (!f || !p) { click_pane = NULL; return 0; }
        build_mouse_seq(p, px, py, button, 1);
        muxdbg("mouse: release -> pane %p seq=%s\n", (void *)p, mouse_seq);
        return 1;                     /* pair with a pass-through press */
    }
    if (button == 0 || button == 32) {  /* left press or held drag      */
        MuxPane *p;
        if (button == 32) {               /* drag continues the press    */
            p = pane_alive(click_pane);
            if (!mouse_fwd_press || !p) return 0;
            if ((p->scr.mouse & 7) < 2) return 0;   /* needs 1002        */
        } else {
            p = pane_at_exact(px, py);
            if (!p || p->scr.view > 0) {           /* divider/scrollback */
                mouse_fwd_press = 0;
                click_pane = NULL;
                return 0;
            }
            if ((p->scr.mouse & 7) == 0) return 0; /* never asked mouse  */
            click_pane = p;
            mouse_fwd_press = 1;
        }
        build_mouse_seq(p, px, py, button, 0);
        muxdbg("mouse: %s %d,%d -> pane %p seq=%s\n",
               button == 32 ? "drag" : "press", px, py,
               (void *)p, mouse_seq);
        return 1;                     /* to that pane's program          */
    }
    return 0;                          /* other buttons: swallow         */
}

/* Forward a mouse event to the pane that received the press (the wheel
 * and click targets never follow the keyboard focus). Drops the event
 * if that pane is no longer in the tree. */
void mux_forward_mouse(const char *b, size_t n)
{
    MuxPane *p = click_pane ? click_pane : focus;
    if (!p || p->master < 0) return;
    int alive = 0;
    for (int i = 0; i < npanes; i++)
        if (panes[i] == p) { alive = 1; break; }
    if (!alive) { muxdbg("forward_mouse: pane gone\n"); return; }
    muxdbg("forward_mouse: %d bytes -> pane %p\n", (int)n, (void *)p);
    while (n) {
        ssize_t r = write(p->master, b, n);
        if (r < 0) {
            if (errno == EINTR) continue;
            return;
        }
        b += r; n -= (size_t)r;
    }
}

/* ── pump: drain all output sources, reap, repaint ─────────────────── */

/* Union of every pane's requested mouse level, applied clear-then-set
 * like tmux tty.c: the real terminal always reports 1000+1006 to the
 * router (clicks/wheel are ours), plus 1002/1003 when a pane's program
 * asked for them. */
static void mouse_mode_apply(void)
{
    int lv = 1;
    for (int i = 0; i < npanes; i++) {
        int m = panes[i]->scr.mouse & 7;
        if (m > lv) lv = m;
    }
    if (lv == mouse_sent) return;
    static const char off[] =
        "\033[?1006l\033[?1000l\033[?1002l\033[?1003l";
    static const char on_std[]  = "\033[?1006h\033[?1000h";
    static const char on_btn[]  = "\033[?1006h\033[?1000h\033[?1002h";
    static const char on_all[]  =
        "\033[?1006h\033[?1000h\033[?1002h\033[?1003h";
    mux_term_write(off, sizeof off - 1);
    if (lv >= 4)      mux_term_write(on_all, sizeof on_all - 1);
    else if (lv >= 2) mux_term_write(on_btn, sizeof on_btn - 1);
    else              mux_term_write(on_std, sizeof on_std - 1);
    mouse_sent = lv;
    muxdbg("mouse-mode: level %d on the real terminal\n", lv);
}

void mux_pump(void)
{
    if (!active) return;
    thesh_on_cont();
    mux_resize_check();

    /* pane ptys: read what's ready, detect dead children */
    MuxPane *dead[MUX_MAX_PANES];
    int ndead = 0;
    for (int i = 0; i < npanes; i++) {
        MuxPane *p = panes[i];
        if (p->master < 0) continue;
        struct pollfd pf = { .fd = p->master, .events = POLLIN };
        if (poll(&pf, 1, 0) > 0 && (pf.revents & (POLLIN | POLLHUP | POLLERR))) {
            char buf[8192];
            ssize_t r = read(p->master, buf, sizeof buf);
            if (r > 0) {
                mux_screen_feed(&p->scr, buf, (size_t)r);
            } else if (r == 0 || (r < 0 && errno != EAGAIN && errno != EINTR)) {
                dead[ndead++] = p;
                continue;
            }
        }
        if (p->pid > 0 && waitpid(p->pid, NULL, WNOHANG) == p->pid) {
            dead[ndead++] = p;
        }
    }
    for (int i = 0; i < ndead; i++) {
        MuxPane *p = dead[i];
        int dup = 0;
        for (int j = 0; j < i; j++) if (dead[j] == p) dup = 1;
        if (dup) continue;
        drain_dead_pane(p);
        Node *n = node_find_pane(p);
        if (n && n != root) close_pane(p);
    }

    /* repaint panes; separators only when layout/focus changed */
    int layout_dirty = 0;
    for (int i = 0; i < npanes; i++)
        if (panes[i]->scr.all_dirty) { layout_dirty = 1; break; }

    int painted = 0;
    for (int i = 0; i < npanes; i++) {
        MuxPane *p = panes[i];
        MuxScreen *s = &p->scr;
        int dirty = s->all_dirty || s->view > 0;
        if (!dirty) {
            for (int r = 0; r < s->h; r++)
                if (s->dirty[r]) { dirty = 1; break; }
        }
        if (muxdbg_on())
            muxdbg("repaint? pane=%p at %d,%d %dx%d all=%d dirty=%d\n",
                   (void *)p, p->x, p->y, p->w, p->h, s->all_dirty, dirty);
        if (dirty) {
            mux_screen_repaint(s, p->x, p->y, p->w, p->h);
            painted = 1;
        }
    }
    if (layout_dirty || seps_dirty) {
        draw_separators();
        painted = 1;
        seps_dirty = 0;
    }
    tty_buf_flush();               /* one write() per pump, before the park */
    mouse_mode_apply();            /* union of pane mouse modes on the tty  */

    /* Park the real cursor on the focused pane's cursor — but only
     * when something changed. Repeating the same position + ?25h
     * ~60x a second (every idle pump) made some terminals flicker. */
    int want_vis = (focus && focus->scr.view == 0 && focus->scr.cur_vis);
    int r = 0, c = 0;
    if (want_vis) {
        r = focus->scr.cy + 1 + focus->y;
        c = focus->scr.cx + 1 + focus->x;
    }
    if (parked_row != r || parked_col != c || parked_vis != want_vis ||
        painted || park_force) {
        char cb[48];
        if (want_vis)
            snprintf(cb, sizeof cb, "\033[%d;%dH\033[?25h", r, c);
        else
            snprintf(cb, sizeof cb, "\033[?25l");
        mux_term_write(cb, strlen(cb));
        muxdbg("park: pid=%d scr=(%d,%d) pane@(%d,%d) -> (%d,%d) "
               "vis=%d painted=%d\n",
               focus ? (int)focus->pid : -1,
               focus ? focus->scr.cx : -1, focus ? focus->scr.cy : -1,
               focus ? focus->x : -1, focus ? focus->y : -1,
               r, c, want_vis, painted);
        parked_row = r; parked_col = c; parked_vis = want_vis;
        park_force = 0;
    }
}

/* ── teardown / leaving ─────────────────────────────────────────────── */

/* Restore the terminal and kill every pane (atexit path). Does NOT
 * clear the screen — a crash trace must stay readable. */
void mux_shutdown(void)
{
    if (!active) return;
    for (int i = 0; i < npanes; i++)
        pane_free(panes[i]);
    npanes = 0;
    free_tree(root);
    root = NULL;
    focus = NULL;

    {
        static const char off[] =
            "\033[?1006l\033[?1000l\033[?1002l\033[?1003l";
        mux_term_write(off, sizeof off - 1);
    }
    mux_term_write("\033[?25h", 6);
    mouse_sent = -1;
    active = 0;
    mouse_fwd_press = 0;
    click_pane = NULL;
    parked_row = parked_col = parked_vis = -1;
}

/* Leave the multiplexer: shut down, then hand the real terminal a
 * fresh plain shell (cwd/env inherited). Called when the last pane
 * dies or `multiplexer = no` tears the mux down. Never returns. */
void mux_leave(void)
{
    if (!active) return;
    mux_shutdown();
    mux_term_write("\033[H\033[2J", 7);   /* panes are gone: clean slate */
    term_exit_raw();
    unsetenv("THESH_MUX");
    if (!self_path[0]) {
        ssize_t n = readlink("/proc/self/exe", self_path,
                             sizeof self_path - 1);
        if (n > 0) self_path[n] = 0;
    }
    muxdbg("leave: exec a fresh plain shell (%s)\n", self_path);
    if (self_path[0]) {
        char *argv[] = { self_path, NULL };
        execv(self_path, argv);
    }
    _exit(127);                           /* no exe: just end           */
}

/* ── the router: single key path (tmux server_client_handle_key) ──────
 * Bindings first — mux VERB binds run here; every other bound key and
 * every unbound key goes raw to the focused pane, where that shell's
 * own identical bind table handles it (so `exec('micro')` runs in the
 * pane you are standing in). Mouse events go to the pane under the
 * pointer, translated to its coordinates. Never returns: the last
 * pane's death execs a fresh shell. */

static void mux_router_loop(void)
{
    muxdbg("router: loop start, %d pane(s)\n", npanes);
    for (;;) {
        thesh_config_check();             /* rc auto-reload              */
        if (npanes <= 0) mux_leave();     /* never returns               */
        mux_pump();                       /* ptys -> repaint -> park     */

        uint32_t cp = 0;
        int mods = 0;
        int key = ed_read_key(&cp, &mods);
        if (key == K_EOF) {               /* terminal gone               */
            mux_shutdown();
            exit(shell_status);
        }

        if (key == K_NONE && cp == 0) {   /* mouse event or DSR reply    */
            int rl = 0;
            const char *raw = ed_raw(&rl);
            int is_dsr = (rl >= 3 && raw[0] == 0x1b && raw[1] == '[' &&
                          raw[rl - 1] == 'R');
            if (is_dsr) continue;         /* nothing here probes the tty */
            if (ed_mouse_forward()) {     /* to the pane under pointer   */
                int n = 0;
                const char *seq = mux_mouse_seq(&n);
                mux_forward_mouse(seq, (size_t)n);
            }
            continue;
        }

        /* mux VERB binds act on the tree; any other bound key and all
         * unbound keys belong to the focused pane (tmux forward_key) */
        const Bind *bd = ed_bind_lookup(key, cp, mods);
        if (bd) {
            if (bd->kind == BIND_VERB) {
                muxdbg("router: key=%d cp=%u mods=%x VERB '%s'\n",
                       key, cp, mods, bd->cmd);
                exec_line(bd->cmd);
                continue;
            }
            int rl = 0;
            const char *raw = ed_raw(&rl);
            muxdbg("router: key=%d cp=%u mods=%x bound->pane (%d bytes)\n",
                   key, cp, mods, rl);
            if (rl > 0) mux_forward(raw, (size_t)rl);
            continue;
        }

        int nd = ed_mux_nav(key, mods);   /* Ctrl+arrows: move focus     */
        if (nd >= 0) { mux_focus_dir(nd); continue; }
        if (key == K_PGUP) { mux_scroll(20); continue; }
        if (key == K_PGDN) { mux_scroll(-20); continue; }

        mux_scroll(0);                    /* any key returns to live     */
        int rl = 0;
        const char *raw = ed_raw(&rl);
        muxdbg("router: key=%d cp=%u mods=%x ->pane (%d bytes)\n",
               key, cp, mods, rl);
        if (rl > 0) mux_forward(raw, (size_t)rl);
    }
}

/* ── `panes` listing ───────────────────────────────────────────────── */

void mux_list(void)
{
    if (!active) {
        dprintf(STDERR_FILENO, "%s: mux: not multiplexed\n", THESH_NAME);
        return;
    }
    for (int i = 0; i < npanes; i++) {
        MuxPane *p = panes[i];
        outf("pane %d: %dx%d at %d,%d%s\n",
             i + 1, p->w, p->h, p->x, p->y,
             p == focus ? " [focused]" : "");
    }
}
