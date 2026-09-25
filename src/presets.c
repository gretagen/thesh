#include "thesh.h"

/* ── Presets ─────────────────────────────────────────────────────────
 * Presets are plain theshrc-style files with a name, found in
 *     user:   ~/.config/thesh/presets/
 *     system: /etc/thesh/presets/
 * `presets` lists them and, after a selection, asks which rc file(s) to
 * make the shell default for (system / user / both), then copies the
 * preset over those rc files and re-sources them so the change takes
 * effect immediately.  `savepreset` serializes the current config —
 * options, aliases and key bindings — out as a preset. */

#define PRESET_NAME_MAX 128
#define PRESET_MAX      128

typedef struct {
    char name[PRESET_NAME_MAX];
    char path[PATH_MAX];
} PresetEntry;

typedef struct {
    PresetEntry v[PRESET_MAX];
    int n;
} PresetList;

enum { SCOPE_SYSTEM = 1, SCOPE_USER, SCOPE_BOTH };   /* "make default for" */
enum { LOC_SYSTEM = 1,   LOC_USER,   LOC_CUSTOM };   /* savepreset target  */

/* ── Prompt helpers ────────────────────────────────────────────────── */
static void strip_crlf(char *s)
{
    size_t n = strlen(s);
    while (n && (s[n - 1] == '\n' || s[n - 1] == '\r')) s[--n] = 0;
}

/* Print `prompt`, then read one trimmed line from stdin.
 * ESC/Ctrl+C cancels: returns -1 with an empty buffer (aborts the
 * surrounding command). Uses the shared raw-mode prompt editor. */
static int read_line(char *buf, size_t sz, const char *prompt)
{
    if (prompt_line(buf, sz, prompt) < 0) {
        buf[0] = 0;
        return -1;
    }
    strip_crlf(buf);
    return 0;
}

/* ── Collecting the preset list ────────────────────────────────────── */
static void collect_dir(const char *dir, PresetList *pl)
{
    DIR *d = opendir(dir);
    if (!d) return;
    struct dirent *de;
    while ((de = readdir(d))) {
        const char *nm = de->d_name;
        size_t l = strlen(nm);
        if (!l || nm[0] == '.' || nm[l - 1] == '~') continue;   /* hidden/swap */
        if (l >= PRESET_NAME_MAX) continue;

        int dup = 0;                       /* user preset shadows system */
        for (int i = 0; i < pl->n; i++)
            if (!strcmp(pl->v[i].name, nm)) { dup = 1; break; }
        if (dup) continue;
        if (pl->n >= PRESET_MAX) break;

        char full[PATH_MAX];
        snprintf(full, sizeof full, "%s/%s", dir, nm);
        struct stat st;
        if (stat(full, &st) != 0 || !S_ISREG(st.st_mode)) continue;

        PresetEntry *e = &pl->v[pl->n++];
        memcpy(e->name, nm, l + 1);
        snprintf(e->path, sizeof e->path, "%s", full);
    }
    closedir(d);
}

static void preset_collect(PresetList *pl)
{
    memset(pl, 0, sizeof *pl);
    const char *home = getenv("HOME");
    if (home && *home) {
        char dir[PATH_MAX];
        snprintf(dir, sizeof dir, "%s/.config/thesh/presets", home);
        collect_dir(dir, pl);
    }
    collect_dir("/etc/thesh/presets", pl);
}

/* Accept a number (1..n) or an exact name. */
static int preset_pick(const PresetList *pl, const char *in)
{
    while (*in && isspace((unsigned char)*in)) in++;
    if (!*in) return -1;
    if (isdigit((unsigned char)*in)) {
        char *end = NULL;
        long n = strtol(in, &end, 10);
        if (end != in && !*end && n >= 1 && n <= pl->n) return (int)n - 1;
    }
    for (int i = 0; i < pl->n; i++)
        if (!strcasecmp(pl->v[i].name, in)) return i;
    return -1;
}

static int parse_scope(const char *in)
{
    while (*in && isspace((unsigned char)*in)) in++;
    if (!strcmp(in, "1") || !strcasecmp(in, "system")) return SCOPE_SYSTEM;
    if (!strcmp(in, "3") || !strcasecmp(in, "both") ||
        !strcasecmp(in, "all"))                        return SCOPE_BOTH;
    return SCOPE_USER;        /* 2 / user / empty / anything else */
}

/* ── Filesystem helpers ────────────────────────────────────────────── */
/* mkdir -p; safe to call when components already exist. */
static int mkdir_p(const char *path, mode_t mode)
{
    char tmp[PATH_MAX];
    snprintf(tmp, sizeof tmp, "%s", path);
    size_t len = strlen(tmp);
    while (len > 0 && tmp[len - 1] == '/') tmp[--len] = 0;
    if (len == 0) return 0;
    for (char *p = tmp + 1; *p; p++) {
        if (*p == '/') {
            *p = 0;
            if (mkdir(tmp, mode) < 0 && errno != EEXIST) { *p = '/'; return -1; }
            *p = '/';
        }
    }
    if (mkdir(tmp, mode) < 0 && errno != EEXIST) return -1;
    return 0;
}

static void mkdir_parent(const char *file)
{
    char tmp[PATH_MAX];
    snprintf(tmp, sizeof tmp, "%s", file);
    char *slash = strrchr(tmp, '/');
    if (!slash || slash == tmp) return;
    *slash = 0;
    mkdir_p(tmp, 0755);
}

static int copy_file(const char *src, const char *dst)
{
    FILE *in = fopen(src, "r");
    if (!in) return -1;
    FILE *out = fopen(dst, "w");
    if (!out) { fclose(in); return -1; }
    char buf[8192];
    size_t n;
    while ((n = fread(buf, 1, sizeof buf, in)) > 0)
        if (fwrite(buf, 1, n, out) != n) { fclose(in); fclose(out); return -1; }
    int bad = ferror(in);
    fclose(in);
    if (fclose(out) != 0) bad = 1;
    return bad ? -1 : 0;
}

/* ── Applying a preset ─────────────────────────────────────────────── */
/* Copy the preset over the requested rc file(s); re-source them so the
 * shell picks the change up right away. Returns 0 on full success. */
static int preset_apply(const char *src, int scope, int *wrote_system,
                        int *wrote_user)
{
    int rc = 0;
    *wrote_system = *wrote_user = 0;

    if (scope == SCOPE_SYSTEM || scope == SCOPE_BOTH) {
        if (copy_file(src, "/etc/theshrc") == 0) {
            *wrote_system = 1;
        } else {
            int e = errno;
            dprintf(STDERR_FILENO, "%s: presets: cannot write /etc/theshrc: %s\n",
                    THESH_NAME, strerror(e));
            rc = 1;
        }
    }
    if (scope == SCOPE_USER || scope == SCOPE_BOTH) {
        const char *home = getenv("HOME");
        char rcpath[PATH_MAX];
        if (home && *home) snprintf(rcpath, sizeof rcpath, "%s/.theshrc", home);
        else               snprintf(rcpath, sizeof rcpath, "/.theshrc");
        if (copy_file(src, rcpath) == 0) {
            *wrote_user = 1;
        } else {
            int e = errno;
            dprintf(STDERR_FILENO, "%s: presets: cannot write %s: %s\n",
                    THESH_NAME, rcpath, strerror(e));
            rc = 1;
        }
    }

    if (*wrote_system) run_file_silent("/etc/theshrc");
    if (*wrote_user) {
        const char *home = getenv("HOME");
        char rcpath[PATH_MAX];
        if (home && *home) snprintf(rcpath, sizeof rcpath, "%s/.theshrc", home);
        else               snprintf(rcpath, sizeof rcpath, "/.theshrc");
        run_file_silent(rcpath);
    }
    return rc;
}

static int preset_apply_flow(const PresetList *pl, int idx)
{
    int scope = SCOPE_USER;
    if (isatty(STDIN_FILENO)) {
        char buf[64];
        if (read_line(buf, sizeof buf,
                      "make this shell default for...?\n"
                      "1 : system\n"
                      "2 : user\n"
                      "3 : both\n"
                      "> ") < 0)
            return 0;                                   /* ESC: abort */
        scope = parse_scope(buf);
    }

    int ws = 0, wu = 0;
    int rc = preset_apply(pl->v[idx].path, scope, &ws, &wu);
    if (ws || wu) {
        outf("preset '%s' applied", pl->v[idx].name);
        if (ws && wu)      outf(" (system + user)\n");
        else if (ws)       outf(" (system)\n");
        else               outf(" (user)\n");
    }
    return rc;
}

/* ── Commands ──────────────────────────────────────────────────────── */
int cmd_presets(int argc, char **argv)
{
    PresetList pl;
    preset_collect(&pl);

    /* `presets NAME` applies directly (system scope is asked on a tty). */
    if (argc > 1) {
        int idx = preset_pick(&pl, argv[1]);
        if (idx < 0) {
            dprintf(STDERR_FILENO, "%s: presets: no preset '%s'\n",
                    THESH_NAME, argv[1]);
            return 1;
        }
        return preset_apply_flow(&pl, idx);
    }

    if (pl.n == 0) {
        outf("no presets found (~/.config/thesh/presets, /etc/thesh/presets)\n");
        return 0;
    }

    /* Non-interactive: just list available preset names. */
    if (!isatty(STDIN_FILENO)) {
        for (int i = 0; i < pl.n; i++) outf("%s\n", pl.v[i].name);
        return 0;
    }

    outf("select preset :\n\n");
    for (int i = 0; i < pl.n; i++)
        outf("%d : %s\n", i + 1, pl.v[i].name);

    char buf[LINE_MAX_CP];
    if (read_line(buf, sizeof buf, "\nselect preset : ") < 0) return 0;
    int idx = preset_pick(&pl, buf);
    if (idx < 0) {
        outf("no such preset\n");
        return 1;
    }
    return preset_apply_flow(&pl, idx);
}

int cmd_savepreset(int argc, char **argv)
{
    const char *name = (argc > 1) ? argv[1] : NULL;
    int loc = LOC_USER;

    if (isatty(STDIN_FILENO)) {
        char buf[128];
        if (read_line(buf, sizeof buf,
                      "save preset at? :\n"
                      "1 : system /etc/thesh/presets\n"
                      "2 : user ~/.config/thesh/presets\n"
                      "3 : custom\n"
                      "> ") < 0) return 0;
        if (!strcmp(buf, "1") || !strcasecmp(buf, "system")) loc = LOC_SYSTEM;
        else if (!strcmp(buf, "3") || !strcasecmp(buf, "custom")) loc = LOC_CUSTOM;
        else loc = LOC_USER;
    }

    char path[PATH_MAX];
    if (loc == LOC_CUSTOM) {
        char buf[PATH_MAX];
        if (read_line(buf, sizeof buf, "path : ") < 0) return 0;
        if (!buf[0]) {
            dprintf(STDERR_FILENO, "%s: savepreset: no path given\n", THESH_NAME);
            return 1;
        }
        snprintf(path, sizeof path, "%s", buf);
    } else {
        const char *dir;
        char dirbuf[PATH_MAX];
        if (loc == LOC_SYSTEM) {
            dir = "/etc/thesh/presets";
        } else {
            const char *home = getenv("HOME");
            if (home && *home)
                snprintf(dirbuf, sizeof dirbuf, "%s/.config/thesh/presets", home);
            else
                snprintf(dirbuf, sizeof dirbuf, "/.config/thesh/presets");
            dir = dirbuf;
        }

        char buf[PRESET_NAME_MAX];
        if (name) {
            snprintf(buf, sizeof buf, "%s", name);
        } else if (read_line(buf, sizeof buf, "preset name : ") < 0) {
            return 0;
        }
        if (!buf[0]) {
            dprintf(STDERR_FILENO, "%s: savepreset: no preset name given\n",
                    THESH_NAME);
            return 1;
        }
        if (strchr(buf, '/') || strstr(buf, "..")) {
            dprintf(STDERR_FILENO, "%s: savepreset: invalid preset name '%s'\n",
                    THESH_NAME, buf);
            return 1;
        }
        size_t dl = strlen(dir), bl = strlen(buf);
        if (dl + bl + 2 > sizeof path) {
            dprintf(STDERR_FILENO, "%s: savepreset: preset path too long\n",
                    THESH_NAME);
            return 1;
        }
        memcpy(path, dir, dl);
        path[dl] = '/';
        memcpy(path + dl + 1, buf, bl + 1);
    }

    mkdir_parent(path);

    FILE *f = fopen(path, "w");
    if (!f) {
        int e = errno;
        dprintf(STDERR_FILENO, "%s: savepreset: cannot write %s: %s\n",
                THESH_NAME, path, strerror(e));
        return 1;
    }
    config_dump_current(f);
    fputc('\n', f);
    dump_aliases(f);
    bind_dump(f);
    fclose(f);
    outf("preset saved to %s\n", path);
    return 0;
}