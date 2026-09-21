#include "thesh.h"

static char **cmds = NULL;
static int ncmds = 0, cmdcap = 0;
static char *path_cache = NULL;

static void add_cmd(const char *n)
{
    for (int i = 0; i < ncmds; i++)
        if (strcmp(cmds[i], n) == 0) return;
    if (ncmds >= cmdcap) {
        cmdcap = cmdcap ? cmdcap * 2 : 512;
        cmds = xrealloc(cmds, sizeof(char *) * (size_t)cmdcap);
    }
    cmds[ncmds++] = xstrdup(n);
}

void dict_refresh(const char *pathstr)
{
    if (path_cache && pathstr && strcmp(path_cache, pathstr) == 0) return;
    dict_force_refresh(pathstr);
}

void dict_force_refresh(const char *pathstr)
{
    free(path_cache);
    path_cache = pathstr ? xstrdup(pathstr) : NULL;

    for (int i = 0; i < ncmds; i++) free(cmds[i]);
    free(cmds);
    cmds = NULL;
    ncmds = 0;
    cmdcap = 0;

    if (!pathstr || !*pathstr) return;

    char *dup = xstrdup(pathstr), *tok, *save = NULL;
    for (tok = strtok_r(dup, ":", &save); tok; tok = strtok_r(NULL, ":", &save)) {
        DIR *d = opendir(tok);
        if (!d) continue;
        struct dirent *e;
        while ((e = readdir(d))) {
            if (!strcmp(e->d_name, ".") || !strcmp(e->d_name, "..")) continue;
            char p[PATH_MAX];
            snprintf(p, sizeof p, "%s/%s", tok, e->d_name);
            if (access(p, X_OK) == 0) add_cmd(e->d_name);
        }
        closedir(d);
    }
    free(dup);

    for (int i = 1; i < ncmds; i++) {
        char *t = cmds[i];
        int j = i - 1;
        while (j >= 0 && strcmp(cmds[j], t) > 0) {
            cmds[j + 1] = cmds[j];
            j--;
        }
        cmds[j + 1] = t;
    }
}

int dict_count(void) { return ncmds; }
const char *dict_get(int i) { return cmds[i]; }

int dict_has(const char *name)
{
    int lo = 0, hi = ncmds - 1;
    while (lo <= hi) {
        int m = (lo + hi) / 2;
        int c = strcmp(cmds[m], name);
        if (c == 0) return 1;
        if (c < 0) lo = m + 1;
        else hi = m - 1;
    }
    return 0;
}

static int name_cmp(const void *a, const void *b)
{
    return strcmp(*(const char *const *)a, *(const char *const *)b);
}

/* Collect entries in `dir` starting with `stem`. Returns them malloc'd and
 * sorted; caller frees the returned array with free() (not the strings —
 * they are owned here). N is the event count out-param. */
int file_matches(const char *dir, const char *stem, const char ***out, int *cap)
{
    DIR *d = opendir((dir && *dir) ? dir : ".");
    if (!d) { *out = NULL; *cap = 0; return 0; }

    size_t sl = strlen(stem);
    int n = 0, c = 0;
    char **v = NULL;
    struct dirent *e;
    while ((e = readdir(d))) {
        const char *nm = e->d_name;
        if (!strcmp(nm, ".") || !strcmp(nm, "..")) continue;
        if (nm[0] == '.' && (sl == 0 || stem[0] != '.')) continue;
        if (sl && strncmp(nm, stem, sl) != 0) continue;
        if (n >= c) {
            c = c ? c * 2 : 64;
            v = xrealloc(v, sizeof(char *) * (size_t)c);
        }
        char buf[PATH_MAX];
        int root = (dir && strcmp(dir, "/") == 0);
        if (root) snprintf(buf, sizeof buf, "/%s", nm);
        else if (dir && *dir) snprintf(buf, sizeof buf, "%s/%s", dir, nm);
        else snprintf(buf, sizeof buf, "%s", nm);
        v[n++] = xstrdup(buf);
    }
    closedir(d);

    qsort(v, (size_t)n, sizeof(char *), name_cmp);
    *out = (const char **)v;
    *cap = c;
    return n;
}