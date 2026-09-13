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

char *get_home(void)
{
    const char *h = getenv("HOME");
    if (h && *h) return xstrdup(h);
    struct passwd *pw = getpwuid(getuid());
    return xstrdup((pw && pw->pw_dir) ? pw->pw_dir : "/");
}