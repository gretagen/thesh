#include "thesh.h"

/* ── rc file auto-reload ────────────────────────────────────────────────
 * The interactive shell re-sources /etc/theshrc and the user rc whenever
 * the on-disk file changes (mtime/size), so config edits take effect on
 * the next prompt without restarting the shell.  Gated by Cfg.autoreload. */
typedef struct {
    char          path[PATH_MAX];
    struct timespec mtime;
    off_t         size;
    int           exists;
} RcWatch;

static RcWatch rc_watch[4];
static int     rc_watch_n = 0;

static void rc_watch_add(const char *path)
{
    if (rc_watch_n >= (int)(sizeof rc_watch / sizeof rc_watch[0])) return;
    RcWatch *w = &rc_watch[rc_watch_n++];
    snprintf(w->path, sizeof w->path, "%s", path);
    struct stat st;
    if (stat(w->path, &st) == 0) {
        w->mtime  = st.st_mtim;
        w->size   = st.st_size;
        w->exists = 1;
    } else {
        w->mtime.tv_sec = 0; w->mtime.tv_nsec = 0;
        w->size = 0; w->exists = 0;
    }
}

/* Resolve the user rc path with the same priority as startup:
 * ~/.theshrc first, then the legacy ~/.therc. */
static void maybe_reload_config(void)
{
    if (!Cfg.autoreload) return;

    struct stat st;
    for (int i = 0; i < rc_watch_n; i++) {
        RcWatch *w = &rc_watch[i];
        if (!w->path[0]) continue;
        int ok = (stat(w->path, &st) == 0);
        int changed = 0;
        if (!ok) {
            changed = w->exists;          /* file removed */
            w->exists = 0;
        } else {
            changed = !w->exists ||
                      st.st_mtim.tv_sec != w->mtime.tv_sec ||
                      st.st_mtim.tv_nsec != w->mtime.tv_nsec ||
                      st.st_size != w->size;
            w->exists = 1;
            w->mtime = st.st_mtim;
            w->size  = st.st_size;
        }
        if (changed && ok) run_file_silent(w->path);
    }
}

static void thesh_on_exit(void)
{
    term_exit_raw();
    mux_shutdown();
}

/* rc auto-reload tick, shared with the mux router loop */
void thesh_config_check(void) { maybe_reload_config(); }

/* ── /etc/environment ───────────────────────────────────────────────────
 * pam_env-style KEY=value lines, loaded before anything else so declared
 * variables (declaration's `environment` section writes this file) reach
 * every execution path — interactive, -c, and scripts — and so the rc files
 * loaded below can still override them. Blank lines and # comments are
 * skipped, one layer of matching quotes is stripped, and keys are validated
 * before setenv so a malformed line can only be ignored, never injected.
 * THESH_ENV_FILE overrides the path (test hook). */
static void load_environment(void)
{
    const char *path = getenv("THESH_ENV_FILE");
    if (!path || !*path) path = "/etc/environment";

    FILE *f = fopen(path, "r");
    if (!f) return;

    char line[4096];
    while (fgets(line, sizeof line, f)) {
        /* Oversized line: drop it whole rather than misparse the tail. */
        if (!strchr(line, '\n') && !feof(f)) {
            int c;
            while ((c = fgetc(f)) != EOF && c != '\n') { }
            continue;
        }

        char *p = line;
        while (isspace((unsigned char)*p)) p++;
        if (*p == '\0' || *p == '#') continue;

        char *eq = strchr(p, '=');
        if (!eq || eq == p) continue;
        char *val = eq + 1;

        /* Trim trailing whitespace/newline from the value. */
        size_t vlen = strlen(val);
        while (vlen > 0 && isspace((unsigned char)val[vlen - 1]))
            val[--vlen] = '\0';

        /* Strip one layer of matching quotes. */
        if (vlen >= 2 && ((val[0] == '"' && val[vlen - 1] == '"')
                       || (val[0] == '\'' && val[vlen - 1] == '\''))) {
            val[vlen - 1] = '\0';
            val++;
        }

        /* KEY: ends at '=', trailing spaces trimmed, [A-Za-z_][A-Za-z0-9_]*. */
        char *key = p;
        while (eq > key && isspace((unsigned char)eq[-1])) eq--;
        *eq = '\0';
        if (*key == '\0') continue;
        char ok = 1;
        for (const char *k = key; *k; k++) {
            unsigned char c = (unsigned char)*k;
            if (!(isalpha(c) || c == '_' || (k > key && isdigit(c)))) { ok = 0; break; }
        }
        if (!ok) continue;

        setenv(key, val, 1);
    }
    fclose(f);
}

char *build_prompt(void)
{
    char user[64] = "?", host[128] = "?", dir[PATH_MAX];

    struct passwd *pw = getpwuid(getuid());
    if (pw && pw->pw_name) snprintf(user, sizeof user, "%s", pw->pw_name);
    if (gethostname(host, sizeof host) < 0) strcpy(host, "?");

    if (!getcwd(dir, sizeof dir)) strcpy(dir, "/");

    const char *home = getenv("HOME");
    if (home && home[0] && strcmp(home, "/") != 0) {
        size_t hl = strlen(home);
        if (strncmp(dir, home, hl) == 0) {
            if (dir[hl] == 0) strcpy(dir, "~");
            else if (dir[hl] == '/') {
                char tmp[PATH_MAX];
                snprintf(tmp, sizeof tmp, "~%s", dir + hl);
                strcpy(dir, tmp);
            }
        }
    }

    if (Cfg.looks && Cfg.looks[0])
        return render_looks_str(Cfg.looks, user, host, dir);

    const char *ps1 = getenv("PS1");
    if (ps1 && ps1[0])
        return render_ps1(ps1, user, host, dir);

    return render_looks_str(config_default_looks(), user, host, dir);
}

volatile sig_atomic_t thesh_winched = 0;
volatile sig_atomic_t thesh_conted = 0;

static void on_winch(int sig)
{
    (void)sig;
    thesh_winched = 1;      /* redraw the edit line          */
    mux_notify_winch();     /* and let the mux re-layout     */
}
static void on_cont(int sig)  { (void)sig; thesh_conted = 1; }

int main(int argc, char **argv)
{
    setlocale(LC_ALL, "");
    signal(SIGINT, SIG_IGN);       /* shell itself never dies on SIGINT;    */
    signal(SIGQUIT, SIG_IGN);      /* children get defaults in run_child(). */
    signal(SIGWINCH, on_winch);    /* redraw the line the size changed      */
    signal(SIGCONT, on_cont);      /* a VT switch may have reset our state  */

    shell_status = 0;
    config_init();
    load_environment();
    dict_refresh(getenv("PATH"));
    hist_setup();
    atexit(thesh_on_exit);

    /* The C library spawns subprocesses as `sh -c -- command_string`, a
     * convention bash/dash both honour: a `--` immediately after -c is the
     * end-of-options marker, not the command. Swallow it so the command is
     * the argument that follows (argc==2 falls through to run_file). */
    if (argc >= 3 && strcmp(argv[1], "-c") == 0) {
        int ai = 2;
        if (argc >= 4 && strcmp(argv[2], "--") == 0) ai = 3;
        hist_load();
        int st = exec_line(argv[ai]);
        hist_save();
        return st;
    }

    /* Version flags for CLI probes (`neofetch`-style tools, fastfetch once
     * it knows thesh): printed before any rc loading, like the -c path. */
    if (argc == 2 && (!strcmp(argv[1], "--version") ||
                      !strcmp(argv[1], "-v")      ||
                      !strcmp(argv[1], "-V"))) {
        printf("%s %s\n", THESH_NAME, THESH_VERSION);
        return 0;
    }

    if (argc == 2) {
        hist_load();
        int st = run_file(argv[1]);
        hist_save();
        return st >= 0 ? st : 1;
    }

    run_file("/etc/theshrc");
    const char *home = getenv("HOME");
    if (home && *home) {
        char rc[PATH_MAX];
        snprintf(rc, sizeof rc, "%s/.theshrc", home);
        if (access(rc, R_OK) != 0)
            snprintf(rc, sizeof rc, "%s/.therc", home);   /* legacy */
        run_file(rc);
    }

    /* Watch the rc files we just loaded so edits reload on the next prompt. */
    rc_watch_n = 0;
    rc_watch_add("/etc/theshrc");
    if (home && *home) {
        char rc[PATH_MAX];
        snprintf(rc, sizeof rc, "%s/.theshrc", home);
        if (access(rc, R_OK) != 0)
            snprintf(rc, sizeof rc, "%s/.therc", home);   /* legacy */
        rc_watch_add(rc);   /* added even if absent — creation is detected */
    }

    /* Load history after the rc files so `history`/`historylimit` from the
     * config already apply (limit trims, disabled means nothing loads). */
    hist_load();

    for (;;) {
        maybe_reload_config();
        char *prompt = build_prompt();
        int cancelled = 0;
        char *line = edit_line(prompt, &cancelled);
        free(prompt);
        if (!line) break;                    /* EOF */
        if (cancelled) { free(line); continue; }
        if (line[0]) {
            hist_add(line);
            exec_line(line);
        }
        free(line);
    }

    hist_save();
    return shell_status;
}