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

int main(int argc, char **argv)
{
    setlocale(LC_ALL, "");
    signal(SIGINT, SIG_IGN);       /* shell itself never dies on SIGINT;    */
    signal(SIGQUIT, SIG_IGN);      /* children get defaults in run_child(). */

    shell_status = 0;
    config_init();
    dict_refresh(getenv("PATH"));
    hist_setup();
    hist_load();
    atexit(thesh_on_exit);

    /* The C library spawns subprocesses as `sh -c -- command_string`, a
     * convention bash/dash both honour: a `--` immediately after -c is the
     * end-of-options marker, not the command. Swallow it so the command is
     * the argument that follows (argc==2 falls through to run_file). */
    if (argc >= 3 && strcmp(argv[1], "-c") == 0) {
        int ai = 2;
        if (argc >= 4 && strcmp(argv[2], "--") == 0) ai = 3;
        int st = exec_line(argv[ai]);
        hist_save();
        return st;
    }
    if (argc == 2) {
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