#include "thesh.h"

int shell_status = 0;

void *xmalloc(size_t n)
{
    void *p = malloc(n ? n : 1);
    if (!p) { perror("malloc"); exit(1); }
    return p;
}

void *xrealloc(void *p, size_t n)
{
    void *q = realloc(p, n ? n : 1);
    if (!q) { perror("realloc"); exit(1); }
    return q;
}

char *xstrdup(const char *s)
{
    if (!s) s = "";
    char *d = strdup(s);
    if (!d) { perror("strdup"); exit(1); }
    return d;
}

void out(const char *s)
{
    outn(s, strlen(s));
}

void outn(const void *s, size_t n)
{
    const char *p = s;
    while (n) {
        ssize_t r = write(STDOUT_FILENO, p, n);
        if (r < 0) {
            if (errno == EINTR) continue;
            return;
        }
        p += r; n -= (size_t)r;
    }
}

void outf(const char *fmt, ...)
{
    char buf[4096];
    va_list ap;
    va_start(ap, fmt);
    int n = vsnprintf(buf, sizeof buf, fmt, ap);
    va_end(ap);
    if (n < 0) return;
    if ((size_t)n >= sizeof buf) n = (int)sizeof buf - 1;
    outn(buf, (size_t)n);
}

/* Batched terminal output: heavy repaint paths (cells, separators)
 * append here and mux_pump flushes once — one write() per pump instead
 * of one per cell/SGR run, which is what made click-to-focus feel laggy
 * on a terminal full of colored panes. */
static char tty_buf[16384];
static size_t tty_len = 0;

void tty_buf_flush(void)
{
    if (!tty_len) return;
    size_t n = tty_len;
    tty_len = 0;                       /* clear first: mux_term_write's
                                        * own flush call re-enters here */
    mux_term_write(tty_buf, n);
}

void tty_buf_write(const char *b, size_t n)
{
    if (n >= sizeof tty_buf) {              /* too big: flush + direct   */
        tty_buf_flush();
        mux_term_write(b, n);
        return;
    }
    if (tty_len + n > sizeof tty_buf) tty_buf_flush();
    memcpy(tty_buf + tty_len, b, n);
    tty_len += n;
}

/* Optional debug log: set THESH_MUX_DEBUG=<path> to capture draw/park/feed
 * traces used to diagnose cursor misalignment. Best effort, append-only. */
static int dbg_fd = -2;                     /* -2 unchecked, -1 off */

int muxdbg_on(void)
{
    if (dbg_fd == -2) {
        const char *p = getenv("THESH_MUX_DEBUG");
        if (!p || !*p) dbg_fd = -1;
        else {
            /* one shared file for router + every pane shell: O_APPEND
             * writes are ordered, which the cursor-consistency analyzer
             * relies on */
            dbg_fd = open(p, O_WRONLY | O_CREAT | O_APPEND | O_CLOEXEC, 0644);
            if (dbg_fd < 0) dbg_fd = -1;
        }
    }
    return dbg_fd >= 0;
}

void muxdbg(const char *fmt, ...)
{
    if (!muxdbg_on()) return;
    char buf[1024];
    va_list ap;
    va_start(ap, fmt);
    int n = vsnprintf(buf, sizeof buf, fmt, ap);
    va_end(ap);
    if (n > 0)
        write(dbg_fd, buf, (size_t)n < sizeof buf ? (size_t)n : sizeof buf - 1);
}

/* Escape raw terminal bytes for the debug log (capped). */
void muxdbg_bytes(const char *b, size_t n)
{
    if (!muxdbg_on()) return;
    char esc[400];
    size_t e = 0;
    size_t shown = n < 160 ? n : 160;
    for (size_t i = 0; i < shown && e + 6 < sizeof esc; i++) {
        unsigned char c = (unsigned char)b[i];
        if (c == 0x1b)      e += (size_t)snprintf(esc + e, sizeof esc - e, "\\e");
        else if (c == '\r') e += (size_t)snprintf(esc + e, sizeof esc - e, "\\r");
        else if (c == '\n') e += (size_t)snprintf(esc + e, sizeof esc - e, "\\n");
        else if (c == '\t') e += (size_t)snprintf(esc + e, sizeof esc - e, "\\t");
        else if (c < 0x20 || c == 0x7f)
            e += (size_t)snprintf(esc + e, sizeof esc - e, "\\x%02x", c);
        else esc[e++] = (char)c;
    }
    if (n > shown) e += (size_t)snprintf(esc + e, sizeof esc - e, "...(+%zu)", n - shown);
    esc[e] = 0;
    muxdbg("  bytes: %s\n", esc);
}

char *get_home(void)
{
    const char *h = getenv("HOME");
    if (h && *h) return xstrdup(h);
    struct passwd *pw = getpwuid(getuid());
    return xstrdup((pw && pw->pw_dir) ? pw->pw_dir : "/");
}