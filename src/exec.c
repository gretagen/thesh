#include "thesh.h"

extern char **environ;

/* ── Aliases ──────────────────────────────────────────────────────── */
typedef struct { char *name, *value; } Alias;
static Alias aliases[ALIAS_MAX];
static int nalias = 0;

const char *alias_lookup(const char *name)
{
    for (int i = 0; i < nalias; i++)
        if (!strcmp(aliases[i].name, name)) return aliases[i].value;
    return NULL;
}

void alias_set(const char *name, const char *value)
{
    for (int i = 0; i < nalias; i++)
        if (!strcmp(aliases[i].name, name)) {
            free(aliases[i].value);
            aliases[i].value = xstrdup(value);
            return;
        }
    if (nalias < ALIAS_MAX) {
        aliases[nalias].name = xstrdup(name);
        aliases[nalias].value = xstrdup(value);
        nalias++;
    }
}

void alias_unset(const char *name)
{
    for (int i = 0; i < nalias; i++)
        if (!strcmp(aliases[i].name, name)) {
            free(aliases[i].name);
            free(aliases[i].value);
            memmove(&aliases[i], &aliases[i + 1],
                    sizeof(Alias) * (size_t)(nalias - i - 1));
            nalias--;
            return;
        }
}

void print_aliases(void)
{
    for (int i = 0; i < nalias; i++)
        outf("%s=%s\n", aliases[i].name, aliases[i].value);
}

static const char *builtin_list[] = {
    "cd", "pwd", "echo", "export", "unset", "alias", "unalias",
    "history", "source", "exit", "type", "set", NULL
};

const char *const *builtin_names(void) { return builtin_list; }

int is_builtin(const char *name)
{
    for (int i = 0; builtin_list[i]; i++)
        if (!strcmp(builtin_list[i], name)) return 1;
    return 0;
}

/* ── Tokenizer ────────────────────────────────────────────────────── */
typedef struct { char *b; size_t len, cap; } Word;

static void wadd(Word *w, char c)
{
    if (w->len + 2 > w->cap) {
        w->cap = w->cap ? w->cap * 2 : 32;
        w->b = xrealloc(w->b, w->cap);
    }
    w->b[w->len++] = c;
    w->b[w->len] = 0;
}

static void wadds(Word *w, const char *s)
{
    while (*s) wadd(w, *s++);
}

static const char *expand_var(Word *w, const char *p)
{
    p++;                                   /* skip $ */
    if (*p == '?') {
        char sb[16];
        snprintf(sb, sizeof sb, "%d", shell_status);
        wadds(w, sb);
        return p + 1;
    }
    if (*p == '{') {
        char name[256];
        int ni = 0;
        p++;
        while (*p && *p != '}' && ni < 255) name[ni++] = *p++;
        if (*p == '}') p++;
        name[ni] = 0;
        const char *v = getenv(name);
        if (v) wadds(w, v);
        return p;
    }
    char name[256];
    int ni = 0;
    while (*p && (isalnum((unsigned char)*p) || *p == '_') && ni < 255)
        name[ni++] = *p++;
    name[ni] = 0;
    const char *v = getenv(name);
    if (v) wadds(w, v);
    return p;
}

static char **tokenize(const char *line, int *argc)
{
    *argc = 0;
    char **args = NULL;
    int n = 0, cap = 0;
    Word w = {0};
    int started = 0;
    const char *p = line;

#define FLUSH() do {                                                       \
        if (started) {                                                     \
            if (n >= cap) { cap = cap ? cap * 2 : 8;                       \
                args = xrealloc(args, sizeof(char *) * (size_t)cap); }     \
            args[n++] = xstrdup(w.b);                                      \
        }                                                                  \
        w.len = 0; started = 0;                                            \
    } while (0)

    while (*p) {
        if (isspace((unsigned char)*p)) { FLUSH(); p++; continue; }
        if (*p == '#' && !started) break;  /* comment to end of line */
        started = 1;
        if (*p == '\'') {
            p++;
            while (*p && *p != '\'') wadd(&w, *p++);
            if (*p) p++;
            continue;
        }
        if (*p == '"') {
            p++;
            while (*p && *p != '"') {
                if (*p == '\\' && p[1] &&
                    (p[1] == '"' || p[1] == '$' || p[1] == '\\' || p[1] == '`')) {
                    p++;
                    wadd(&w, *p++);
                    continue;
                }
                if (*p == '$') { p = expand_var(&w, p); continue; }
                wadd(&w, *p++);
            }
            if (*p) p++;
            continue;
        }
        if (*p == '\\' && p[1]) { p++; wadd(&w, *p++); continue; }
        if (*p == '$') { p = expand_var(&w, p); continue; }
        if (*p == '~' && w.len == 0) {
            const char *h = getenv("HOME");
            if (h) wadds(&w, h);
            p++;
            continue;
        }
        wadd(&w, *p);
        p++;
    }
    FLUSH();
#undef FLUSH

    free(w.b);
    if (!args) args = xmalloc(sizeof(char *));
    args[n] = NULL;
    *argc = n;
    return args;
}

static void free_argv(char **args)
{
    for (int i = 0; args[i]; i++) free(args[i]);
    free(args);
}

/* ── Builtins ─────────────────────────────────────────────────────── */
static int path_of_command(const char *cmd, char *out, size_t outsz)
{
    const char *path = getenv("PATH");
    if (!path) return 0;
    char *dup = xstrdup(path), *tok, *save = NULL;
    for (tok = strtok_r(dup, ":", &save); tok; tok = strtok_r(NULL, ":", &save)) {
        char p[PATH_MAX];
        snprintf(p, sizeof p, "%s/%s", tok, cmd);
        if (access(p, X_OK) == 0) {
            snprintf(out, outsz, "%s", p);
            free(dup);
            return 1;
        }
    }
    free(dup);
    return 0;
}

static int run_builtin(char **args, int argc)
{
    const char *c = args[0];

    if (!strcmp(c, "cd")) {
        char tgt[PATH_MAX];
        if (argc > 1) {
            if (!strcmp(args[1], "-")) {
                const char *o = getenv("OLDPWD");
                if (!o) { dprintf(2, "%s: cd: no previous directory\n", THESH_NAME); shell_status = 1; return 1; }
                snprintf(tgt, sizeof tgt, "%s", o);
            } else if (args[1][0] == '~' && (args[1][1] == '/' || args[1][1] == 0)) {
                const char *hh = getenv("HOME");
                snprintf(tgt, sizeof tgt, "%s%s", hh ? hh : "/", args[1] + 1);
            } else {
                snprintf(tgt, sizeof tgt, "%s", args[1]);
            }
        } else {
            const char *hh = getenv("HOME");
            snprintf(tgt, sizeof tgt, "%s", hh ? hh : "/");
        }
        const char *oldpwd = getenv("PWD");
        if (chdir(tgt) < 0) {
            dprintf(2, "%s: cd: %s: %s\n", THESH_NAME, tgt, strerror(errno));
            shell_status = 1;
            return 1;
        }
        if (oldpwd) setenv("OLDPWD", oldpwd, 1);
        char cwd[PATH_MAX];
        if (getcwd(cwd, sizeof cwd)) setenv("PWD", cwd, 1);
        shell_status = 0;
        return 1;
    }

    if (!strcmp(c, "pwd")) {
        char cwd[PATH_MAX];
        if (getcwd(cwd, sizeof cwd)) outf("%s\n", cwd);
        shell_status = 0;
        return 1;
    }

    if (!strcmp(c, "echo")) {
        int nflag = (argc > 1 && !strcmp(args[1], "-n")) ? 1 : 0;
        int start = nflag ? 2 : 1;
        for (int i = start; i < argc; i++)
            outf("%s%s", i == start ? "" : " ", args[i]);
        if (!nflag) out("\n");
        shell_status = 0;
        return 1;
    }

    if (!strcmp(c, "export")) {
        if (argc == 1) {
            for (char **e = environ; *e; e++) outf("%s\n", *e);
        } else {
            for (int i = 1; i < argc; i++) {
                char *eq = strchr(args[i], '=');
                if (eq) {
                    *eq = 0;
                    setenv(args[i], eq + 1, 1);
                    *eq = '=';
                } else {
                    const char *v = getenv(args[i]);
                    if (v) outf("%s=%s\n", args[i], v);
                }
            }
        }
        shell_status = 0;
        return 1;
    }

    if (!strcmp(c, "unset")) {
        for (int i = 1; i < argc; i++) unsetenv(args[i]);
        shell_status = 0;
        return 1;
    }

    if (!strcmp(c, "alias")) {
        if (argc == 1) {
            print_aliases();
        } else {
            for (int i = 1; i < argc; i++) {
                char *eq = strchr(args[i], '=');
                if (eq) {
                    char name[128];
                    size_t nl = (size_t)(eq - args[i]);
                    if (nl >= sizeof name) nl = sizeof name - 1;
                    memcpy(name, args[i], nl);
                    name[nl] = 0;
                    char val[LINE_MAX_CP];
                    snprintf(val, sizeof val, "%s", eq + 1);
                    size_t vl = strlen(val);
                    for (int j = i + 1; j < argc; j++) {
                        if (vl && vl + 1 < sizeof val) val[vl++] = ' ';
                        strncpy(val + vl, args[j], sizeof val - vl - 1);
                        val[sizeof val - 1] = 0;
                        vl = strlen(val);
                    }
                    alias_set(name, val);
                    break;   /* remainder consumed into this one alias */
                } else {
                    const char *v = alias_lookup(args[i]);
                    if (v) outf("%s=%s\n", args[i], v);
                }
            }
        }
        shell_status = 0;
        return 1;
    }

    if (!strcmp(c, "unalias")) {
        if (argc > 1 && !strcmp(args[1], "-a")) {
            for (int i = 0; i < nalias; i++) {
                free(aliases[i].name);
                free(aliases[i].value);
            }
            nalias = 0;
        } else {
            for (int i = 1; i < argc; i++) alias_unset(args[i]);
        }
        shell_status = 0;
        return 1;
    }

    if (!strcmp(c, "history")) {
        for (int i = 0; i < H.count; i++)
            outf("%5d  %s\n", i + 1, H.items[i]);
        shell_status = 0;
        return 1;
    }

    if (!strcmp(c, "source") || !strcmp(c, ".")) {
        if (argc > 1) run_file(args[1]);
        shell_status = 0;
        return 1;
    }

    if (!strcmp(c, "exit")) {
        hist_save();
        exit(argc > 1 ? atoi(args[1]) : shell_status);
    }

    if (!strcmp(c, "type")) {
        for (int i = 1; i < argc; i++) {
            const char *a = alias_lookup(args[i]);
            if (a) outf("%s is aliased to `%s'\n", args[i], a);
            else if (is_builtin(args[i])) outf("%s is a shell builtin\n", args[i]);
            else {
                char p[PATH_MAX];
                if (path_of_command(args[i], p, sizeof p)) outf("%s is %s\n", args[i], p);
                else outf("%s: not found\n", args[i]);
            }
        }
        shell_status = 0;
        return 1;
    }

    if (!strcmp(c, "set")) {
        shell_status = 0;
        return 1;
    }

    return 0;
}

/* ── Execution ────────────────────────────────────────────────────── */
static void run_child(char **argv)
{
    term_exit_raw();
    fflush(stdout);
    pid_t pid = fork();
    if (pid < 0) {
        perror("fork");
        shell_status = 1;
        return;
    }
    if (pid == 0) {
        signal(SIGINT, SIG_DFL);
        signal(SIGQUIT, SIG_DFL);
        signal(SIGTSTP, SIG_DFL);
        signal(SIGPIPE, SIG_DFL);
        execvp(argv[0], argv);
        dprintf(2, "%s: %s: %s\n", THESH_NAME, argv[0], strerror(errno));
        _exit(127);
    }
    int st;
    while (waitpid(pid, &st, 0) < 0) {
        if (errno != EINTR) break;
    }
    if (WIFEXITED(st)) shell_status = WEXITSTATUS(st);
    else if (WIFSIGNALED(st)) shell_status = 128 + WTERMSIG(st);
    term_enter_raw();
}

static char **expand_aliases(char **argv, int *argc)
{
    int guard = 0;
    while (guard++ < 10) {
        const char *a = alias_lookup(argv[0]);
        if (!a) break;
        size_t need = strlen(a) + 1;
        for (int i = 1; i < *argc; i++) need += strlen(argv[i]) + 2;
        char *nl = xmalloc(need);
        strcpy(nl, a);
        for (int i = 1; i < *argc; i++) { strcat(nl, " "); strcat(nl, argv[i]); }
        free_argv(argv);
        argv = tokenize(nl, argc);
        free(nl);
        if (*argc == 0) break;
    }
    return argv;
}

/* Split a command line on unquoted `|` characters. Whitespace-only
 * segments are dropped. */
static int split_pipeline(const char *line, char ***out, int *outn)
{
    char **segs = NULL;
    int n = 0, cap = 0;
    char seg[LINE_MAX_CP];
    int si = 0;
    char q = 0;
    const char *p = line;

    while (*p) {
        char ch = *p;
        if (q == 0 && (ch == '\'' || ch == '"')) { q = ch; seg[si++] = ch; p++; continue; }
        if (q != 0 && ch == q) { q = 0; seg[si++] = ch; p++; continue; }
        if (q == 0 && ch == '\\' && p[1]) { seg[si++] = ch; seg[si++] = p[1]; p += 2; continue; }
        if (q == 0 && ch == '|') {
            seg[si] = 0;
            char *t = xstrdup(seg);
            char *s = t, *e = t + strlen(t);
            while (*s && isspace((unsigned char)*s)) s++;
            while (e > s && isspace((unsigned char)e[-1])) e--;
            *e = 0;
            if (*s) {
                if (n >= cap) { cap = cap ? cap * 2 : 4; segs = xrealloc(segs, sizeof(char *) * (size_t)cap); }
                segs[n++] = xstrdup(s);
            }
            free(t);
            si = 0;
            p++;
            continue;
        }
        seg[si++] = ch;
        p++;
    }

    seg[si] = 0;
    char *t = xstrdup(seg);
    char *s = t, *e = t + strlen(t);
    while (*s && isspace((unsigned char)*s)) s++;
    while (e > s && isspace((unsigned char)e[-1])) e--;
    *e = 0;
    if (*s) {
        if (n >= cap) { cap = cap ? cap * 2 : 4; segs = xrealloc(segs, sizeof(char *) * (size_t)cap); }
        segs[n++] = xstrdup(s);
    }
    free(t);

    if (!segs) segs = xmalloc(sizeof(char *));
    *out = segs;
    *outn = n;
    return n;
}

static void pipeline_child(const char *seg, int in_fd, int out_fd)
{
    signal(SIGINT, SIG_DFL);
    signal(SIGQUIT, SIG_DFL);
    signal(SIGTSTP, SIG_DFL);
    signal(SIGPIPE, SIG_DFL);

    if (in_fd >= 0) {
        if (dup2(in_fd, STDIN_FILENO) < 0) _exit(126);
        close(in_fd);
    }
    if (out_fd >= 0) {
        if (dup2(out_fd, STDOUT_FILENO) < 0) _exit(126);
        close(out_fd);
    }

    int argc = 0;
    char **argv = tokenize(seg, &argc);
    if (!argc) _exit(0);
    argv = expand_aliases(argv, &argc);
    if (!argc) { free_argv(argv); _exit(0); }

    dict_refresh(getenv("PATH"));

    if (run_builtin(argv, argc)) _exit(shell_status);

    if (strchr(argv[0], '/')) {
        execvp(argv[0], argv);
        dprintf(2, "%s: %s: %s\n", THESH_NAME, argv[0], strerror(errno));
        _exit(127);
    }
    if (!dict_has(argv[0])) {
        print_suggestion(argv[0]);
        _exit(127);
    }
    execvp(argv[0], argv);
    if (errno == ENOENT) print_suggestion(argv[0]);
    else dprintf(2, "%s: %s: %s\n", THESH_NAME, argv[0], strerror(errno));
    _exit(127);
}

static int run_pipeline(char **segs, int n)
{
    term_exit_raw();
    fflush(stdout);

    pid_t *pids = xmalloc(sizeof(pid_t) * (size_t)n);
    int prev = -1;
    int fd[2];

    for (int i = 0; i < n; i++) {
        if (i < n - 1 && pipe(fd) < 0) {
            perror("pipe");
            for (int j = 0; j < i; j++) waitpid(pids[j], NULL, 0);
            shell_status = 1;
            free(pids);
            term_enter_raw();
            return 1;
        }
        pid_t pid = fork();
        if (pid < 0) {
            perror("fork");
            if (i < n - 1) { close(fd[0]); close(fd[1]); }
            for (int j = 0; j < i; j++) waitpid(pids[j], NULL, 0);
            shell_status = 1;
            free(pids);
            term_enter_raw();
            return 1;
        }
        if (pid == 0) {
            pipeline_child(segs[i], prev, (i < n - 1) ? fd[1] : -1);
            _exit(127);                /* unreachable */
        }
        if (prev >= 0) close(prev);
        if (i < n - 1) { close(fd[1]); prev = fd[0]; }
        pids[i] = pid;
    }
    if (prev >= 0) close(prev);

    int st = 0;
    for (int i = 0; i < n; i++) {
        int ws;
        while (waitpid(pids[i], &ws, 0) < 0) {
            if (errno != EINTR) break;
        }
        if (i == n - 1) {
            if (WIFEXITED(ws)) st = WEXITSTATUS(ws);
            else if (WIFSIGNALED(ws)) st = 128 + WTERMSIG(ws);
        }
    }
    free(pids);
    term_enter_raw();
    shell_status = st;
    return st;
}

static int exec_single(const char *line)
{
    int argc;
    char **argv = tokenize(line, &argc);
    if (!argc) { free_argv(argv); return 0; }
    argv = expand_aliases(argv, &argc);
    if (!argc) { free_argv(argv); return 0; }

    dict_refresh(getenv("PATH"));

    if (run_builtin(argv, argc)) {
        int st = shell_status;
        free_argv(argv);
        return st;
    }

    if (strchr(argv[0], '/')) {
        if (access(argv[0], X_OK) != 0) {
            dprintf(2, "%s: %s: %s\n", THESH_NAME, argv[0], strerror(errno));
            free_argv(argv);
            shell_status = 127;
            return 127;
        }
        run_child(argv);
        free_argv(argv);
        return shell_status;
    }

    if (!dict_has(argv[0])) {
        print_suggestion(argv[0]);
        free_argv(argv);
        shell_status = 127;
        return 127;
    }

    run_child(argv);
    free_argv(argv);
    return shell_status;
}

int exec_line(const char *line)
{
    char **segs;
    int n;
    split_pipeline(line, &segs, &n);
    if (n == 0) { free(segs); return 0; }

    int st = (n == 1) ? exec_single(segs[0]) : run_pipeline(segs, n);

    for (int i = 0; i < n; i++) free(segs[i]);
    free(segs);
    return st;
}

int run_file(const char *path)
{
    FILE *f = fopen(path, "r");
    if (!f) return -1;
    char line[LINE_MAX_CP];
    while (fgets(line, sizeof line, f)) {
        size_t n = strlen(line);
        while (n && (line[n - 1] == '\n' || line[n - 1] == '\r')) line[--n] = 0;
        if (!line[0] || line[0] == '#') continue;
        exec_line(line);
    }
    fclose(f);
    return shell_status;
}