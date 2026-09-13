#include "thesh.h"

History H;

static void grow(void)
{
    if (H.count + 1 < H.cap) return;
    H.cap = H.cap ? H.cap * 2 : 64;
    H.items = xrealloc(H.items, sizeof(char *) * (size_t)H.cap);
}

void hist_setup(void)
{
    memset(&H, 0, sizeof H);
    H.max = HIST_MAX;
    const char *hm = getenv("THESH_HISTFILE");
    const char *home = getenv("HOME");
    char buf[PATH_MAX];
    if (hm && *hm) {
        H.path = xstrdup(hm);
    } else if (home && *home) {
        snprintf(buf, sizeof buf, "%s/%s", home, HIST_NAME);
        H.path = xstrdup(buf);
    } else {
        H.path = xstrdup("/tmp/.thesh_history");
    }
}

void hist_load(void)
{
    FILE *f = fopen(H.path, "r");
    if (!f) return;
    char line[LINE_MAX_CP];
    while (H.count < H.max && fgets(line, sizeof line, f)) {
        size_t n = strlen(line);
        while (n && (line[n - 1] == '\n' || line[n - 1] == '\r')) line[--n] = 0;
        if (!n) continue;
        grow();
        H.items[H.count++] = xstrdup(line);
    }
    fclose(f);
}

void hist_add(const char *line)
{
    if (!line || !*line) return;
    if (H.count && strcmp(H.items[H.count - 1], line) == 0) {
        H.dirty = 1;
        return;
    }
    grow();
    if (H.max > 0 && H.count >= H.max) {
        free(H.items[0]);
        memmove(H.items, H.items + 1, sizeof(char *) * (size_t)(H.count - 1));
        H.count--;
    }
    H.items[H.count++] = xstrdup(line);
    H.dirty = 1;
}

void hist_save(void)
{
    if (!H.dirty) return;
    FILE *f = fopen(H.path, "w");
    if (!f) return;
    int start = H.count > H.max ? H.count - H.max : 0;
    for (int i = start; i < H.count; i++) {
        fputs(H.items[i], f);
        fputc('\n', f);
    }
    fclose(f);
    H.dirty = 0;
}

int hist_find(const char *needle, int start, int backward)
{
    if (!needle) return -1;
    size_t nl = strlen(needle);
    if (backward) {
        for (int i = start; i >= 0; i--)
            if (strncmp(H.items[i], needle, nl) == 0) return i;
    } else {
        for (int i = start; i < H.count; i++)
            if (strncmp(H.items[i], needle, nl) == 0) return i;
    }
    return -1;
}

int hist_find_substr(const char *sub, int start, int backward)
{
    if (!sub || !*sub) return -1;
    if (backward) {
        for (int i = start; i >= 0; i--)
            if (strstr(H.items[i], sub)) return i;
    } else {
        for (int i = start; i < H.count; i++)
            if (strstr(H.items[i], sub)) return i;
    }
    return -1;
}