#include "thesh.h"

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

    char *out = xmalloc((size_t)(PATH_MAX + 256));
    snprintf(out, (size_t)(PATH_MAX + 256),
             "[ %s | %s ] %s $ ", user, host, dir);
    return out;
}

int main(int argc, char **argv)
{
    setlocale(LC_ALL, "");
    signal(SIGINT, SIG_IGN);       /* shell itself never dies on SIGINT;    */
    signal(SIGQUIT, SIG_IGN);      /* children get defaults in run_child(). */

    shell_status = 0;
    dict_refresh(getenv("PATH"));
    hist_setup();
    hist_load();
    atexit(thesh_on_exit);

    if (argc >= 3 && strcmp(argv[1], "-c") == 0) {
        int st = exec_line(argv[2]);
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
        snprintf(rc, sizeof rc, "%s/.therc", home);
        run_file(rc);
    }

    for (;;) {
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