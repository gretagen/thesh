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
    "history", "source", "exit", "type", "set", "clearhistory", "hash", NULL
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
static int case_fold_command(char *cmd);

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

    if (!strcmp(c, "clearhistory")) {
        hist_clear();
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
            if (Cfg.hybrid) case_fold_command(args[i]);
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

    if (!strcmp(c, "hash")) {
        int force = 0;
        for (int i = 1; i < argc; i++) {
            if (!strcmp(args[i], "-r") || !strcmp(args[i], "--rehash")) force = 1;
        }
        if (force) dict_force_refresh(getenv("PATH"));
        shell_status = 0;
        return 1;
    }

    return 0;
}

/* ── Execution ────────────────────────────────────────────────────── */
typedef struct {
    int    fd;      /* target descriptor */
    int    mode;    /* 0 = read, 1 = truncate write, 2 = append write */
    int    dupfd;   /* >= 0: dup2(dupfd, fd); else open(path, ...) */
    int    shared;  /* 1: dup the previous redir's file description */
    char  *path;
} Redir;

static int parse_redir_tok(const char *tok, int *kind, int *mode, int *fd,
                           int *dupfd, const char **rest)
{
    *kind = 0; *mode = 0; *fd = 1; *dupfd = -1; *rest = NULL;
    size_t L = strlen(tok);

    if (isdigit((unsigned char)tok[0]) && L >= 2 && tok[1] == '>') {
        *fd = tok[0] - '0';
        if (L >= 4 && tok[2] == '&' && isdigit((unsigned char)tok[3])) {
            *kind = 5; *dupfd = tok[3] - '0'; return 1;         /* n>&m */
        }
        if (L >= 3 && tok[2] == '>') { *kind = 3; *mode = 2; *rest = tok + 3; return 1; }
        *kind = 3; *mode = 1; *rest = tok + 2; return 1;
    }
    if (L >= 2 && tok[0] == '&' && tok[1] == '>') {
        *kind = 4; *fd = 1;                  /* &> / &>> : both fd 1,2 */
        if (L >= 3 && tok[2] == '>') { *mode = 2; *rest = tok + 3; }
        else { *mode = 1; *rest = tok + 2; }
        return 1;
    }
    if (L >= 1 && tok[0] == '>') {
        *kind = 2; *fd = 1;                  /* > / >> */
        if (L >= 2 && tok[1] == '>') { *mode = 2; *rest = tok + 2; }
        else { *mode = 1; *rest = tok + 1; }
        return 1;
    }
    if (L >= 1 && tok[0] == '<') {
        *kind = 1; *fd = 0; *mode = 0; *rest = tok + 1;
        return 1;
    }
    return 0;
}

/* Filter redirection tokens out of argv, collect them as Redir actions.
 * *out gets a view of the non-redir args (NULL-terminated); the strings
 * are owned by argv. Returns number of redirs, or -1 on syntax error. */
static int collect_redirs(char **args, int argc, Redir *redr, int capr,
                          char ***out, int *outargc)
{
    char **na = NULL;
    int n = 0, cap = 0, nr = 0;

    for (int i = 0; i < argc; i++) {
        int kind, mode, fd, dupfd;
        const char *rest;
        if (!parse_redir_tok(args[i], &kind, &mode, &fd, &dupfd, &rest)) {
            if (n >= cap) {
                cap = cap ? cap * 2 : 8;
                na = xrealloc(na, sizeof(char *) * (size_t)cap);
            }
            na[n++] = args[i];
            continue;
        }
        if (kind == 5) {                       /* 2>&1 / 1>&2 */
            if (nr < capr) {
                redr[nr].fd = fd; redr[nr].mode = 0;
                redr[nr].dupfd = dupfd; redr[nr].path = NULL;
                nr++;
            }
            continue;
        }
        const char *path = (rest && *rest) ? rest : (i + 1 < argc ? args[++i] : NULL);
        if (!path) {
            dprintf(2, "%s: parse error: redirection with no file\n", THESH_NAME);
            if (na) free(na);
            return -1;
        }
        {
            int nred = (kind == 4) ? 2 : 1;
            for (int k = 0; k < nred && nr < capr; k++) {
                int tfd = (kind == 1) ? 0
                       : (kind == 3) ? fd
                       : (kind == 2) ? fd
                       : (k ? 2 : 1);
                redr[nr].fd = tfd;
                redr[nr].mode = mode;
                redr[nr].dupfd = -1;
                redr[nr].shared = (kind == 4) ? k : 0;
                redr[nr].path = xstrdup(path);
                nr++;
            }
        }
    }
    if (!na) na = xmalloc(sizeof(char *));
    na[n] = NULL;
    *out = na;
    *outargc = n;
    return nr;
}

/* Replace the command word in situ.  `clean` and `argv` view the same
 * token strings, so update both so later frees stay balanced and the
 * old word is freed exactly once.  Takes ownership of `word`. */
static void replace_cmd_word(char **clean, char **argv, char *word)
{
    free(*clean);
    *clean = word;
    *argv  = word;
}

static void free_redirs(Redir *redr, int n)
{
    for (int i = 0; i < n; i++) free(redr[i].path);
}

static int apply_redirs(const Redir *redr, int n)
{
    for (int i = 0; i < n; i++) {
        int nfd;
        if (redr[i].dupfd >= 0) {
            nfd = dup(redr[i].dupfd);
        } else if (redr[i].shared && i > 0) {
            nfd = dup(redr[i - 1].fd);
        } else {
            int fl = (redr[i].mode == 0)
                   ? O_RDONLY
                   : (redr[i].mode == 2) ? O_WRONLY | O_APPEND | O_CREAT
                                         : O_WRONLY | O_TRUNC | O_CREAT;
            nfd = open(redr[i].path, fl, 0644);
            if (nfd < 0) {
                dprintf(2, "%s: %s: %s\n", THESH_NAME, redr[i].path,
                        strerror(errno));
                return -1;
            }
        }
        if (nfd < 0 || dup2(nfd, redr[i].fd) < 0) {
            dprintf(2, "%s: dup/dup2: %s\n", THESH_NAME, strerror(errno));
            if (nfd >= 0) close(nfd);
            return -1;
        }
        if (redr[i].fd != nfd) close(nfd);
    }
    return 0;
}

static void run_child(char **argv, const Redir *redr, int nredir)
{
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
        if (nredir > 0 && apply_redirs(redr, nredir) < 0) _exit(1);
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

/* Lowercase ARGV[0] in place if the lowercase form resolves to a known
 * command (alias, builtin, or $PATH binary). Only folds DOWN — a command
 * whose real name contains capitals is left exactly as typed. Explicit
 * paths containing '/' are never folded. Returns 1 if folded. */
static int case_fold_command(char *cmd)
{
    if (!cmd || !cmd[0]) return 0;
    if (strchr(cmd, '/')) return 0;

    int has_upper = 0;
    for (const char *p = cmd; *p; p++) {
        if (isupper((unsigned char)*p)) { has_upper = 1; break; }
    }
    if (!has_upper) return 0;

    char lw[LINE_MAX_CP];
    size_t il = 0;
    for (; cmd[il]; il++) lw[il] = (char)tolower((unsigned char)cmd[il]);
    lw[il] = 0;
    if (strcmp(lw, cmd) == 0) return 0;

    if (is_builtin(lw) || alias_lookup(lw) || dict_has(lw)) {
        strcpy(cmd, lw);
        return 1;
    }
    return 0;
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
    if (Cfg.hybrid) case_fold_command(argv[0]);
    argv = expand_aliases(argv, &argc);
    if (!argc) { free_argv(argv); _exit(0); }
    if (Cfg.hybrid) case_fold_command(argv[0]);

    Redir redr[16];
    char **clean;
    int nredir = collect_redirs(argv, argc, redr, 16, &clean, &argc);
    if (nredir < 0) { free(clean); free_argv(argv); _exit(2); }

    dict_refresh(getenv("PATH"));

    if (nredir > 0 && apply_redirs(redr, nredir) < 0) {
        free_redirs(redr, nredir);
        free_argv(argv);
        _exit(1);
    }
    free_redirs(redr, nredir);

    if (run_builtin(clean, argc)) _exit(shell_status);

    if (strchr(clean[0], '/')) {
        execvp(clean[0], clean);
        dprintf(2, "%s: %s: %s\n", THESH_NAME, clean[0], strerror(errno));
        _exit(127);
    }
    if (!dict_has(clean[0])) {
        dict_force_refresh(getenv("PATH"));
        if (Cfg.hybrid) case_fold_command(clean[0]);
        if (!dict_has(clean[0])) {
            const char *best = (Cfg.corrector == 3) ? NULL
                                                    : best_suggestion(clean[0]);
            if (Cfg.corrector == 1 && best) {
                char *fixed = xstrdup(best);
                free(clean[0]);
                clean[0] = fixed;              /* auto-correct, run below */
            } else if (best) {
                print_suggestion(clean[0]);    /* passive (consent too)   */
                _exit(127);
            } else {
                dprintf(2, "%s: no such command: '%s'\n",
                        THESH_NAME, clean[0]);
                _exit(127);
            }
        }
    }
    execvp(clean[0], clean);
    if (errno == ENOENT) print_suggestion(clean[0]);
    else dprintf(2, "%s: %s: %s\n", THESH_NAME, clean[0], strerror(errno));
    _exit(127);
}

static int run_pipeline(char **segs, int n)
{
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
            return 1;
        }
        pid_t pid = fork();
        if (pid < 0) {
            perror("fork");
            if (i < n - 1) { close(fd[0]); close(fd[1]); }
            for (int j = 0; j < i; j++) waitpid(pids[j], NULL, 0);
            shell_status = 1;
            free(pids);
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
    shell_status = st;
    return st;
}

static int exec_single(const char *seg)
{
    int argc;
    char **argv = tokenize(seg, &argc);
    if (!argc) { free_argv(argv); return 0; }
    if (Cfg.hybrid) case_fold_command(argv[0]);
    argv = expand_aliases(argv, &argc);
    if (!argc) { free_argv(argv); return 0; }
    if (Cfg.hybrid) case_fold_command(argv[0]);

    Redir redr[16];
    char **clean;
    int nredir = collect_redirs(argv, argc, redr, 16, &clean, &argc);
    if (nredir < 0) {
        if (clean) free(clean);
        free_argv(argv);
        shell_status = 2;
        return 2;
    }

    dict_refresh(getenv("PATH"));

    int st;
    if (clean[0] && is_builtin(clean[0])) {
        if (nredir == 0) {
            run_builtin(clean, argc);
            st = shell_status;
        } else {
            int saved[3] = { -1, -1, -1 };
            saved[0] = dup(0);
            saved[1] = dup(1);
            saved[2] = dup(2);
            if (apply_redirs(redr, nredir) < 0) {
                for (int i = 0; i < 3; i++) {
                    if (saved[i] >= 0) { dup2(saved[i], i); close(saved[i]); }
                }
                free_redirs(redr, nredir);
                st = 1;
                shell_status = 1;
                goto out;
            }
            run_builtin(clean, argc);
            st = shell_status;
            for (int i = 0; i < 3; i++) {
                if (saved[i] >= 0) { dup2(saved[i], i); close(saved[i]); }
            }
            free_redirs(redr, nredir);
        }
    } else if (strchr(clean[0], '/')) {
        if (access(clean[0], X_OK) != 0) {
            dprintf(2, "%s: %s: %s\n", THESH_NAME, clean[0], strerror(errno));
            st = 127;
            shell_status = 127;
        } else {
            run_child(clean, redr, nredir);
            st = shell_status;
        }
        free_redirs(redr, nredir);
    } else if (!dict_has(clean[0])) {
        dict_force_refresh(getenv("PATH"));
        if (Cfg.hybrid) case_fold_command(clean[0]);
        if (!dict_has(clean[0])) {
            const char *best = (Cfg.corrector == 3) ? NULL
                                                    : best_suggestion(clean[0]);
            if (Cfg.corrector == 1 && best) {
                char *fixed = xstrdup(best);
                replace_cmd_word(clean, argv, fixed);   /* auto-correct */
                run_child(clean, redr, nredir);
                st = shell_status;
            } else if (Cfg.corrector == 2 && best) {
                dprintf(STDERR_FILENO,
                        "%s: '%s' not found. Use '%s'? [y/N] ",
                        THESH_NAME, clean[0], best);
                fflush(stderr);
                char ans[16];
                if (fgets(ans, sizeof ans, stdin) &&
                    (ans[0] == 'y' || ans[0] == 'Y')) {
                    char *fixed = xstrdup(best);
                    replace_cmd_word(clean, argv, fixed);
                    run_child(clean, redr, nredir);
                    st = shell_status;
                } else {
                    dprintf(STDERR_FILENO, "\n");
                    st = 127;
                    shell_status = 127;
                }
            } else if (best) {
                print_suggestion(clean[0]);
                st = 127;
                shell_status = 127;
            } else {
                dprintf(2, "%s: no such command: '%s'\n",
                        THESH_NAME, clean[0]);
                st = 127;
                shell_status = 127;
            }
        } else {
            run_child(clean, redr, nredir);
            st = shell_status;
        }
        free_redirs(redr, nredir);
    } else {
        run_child(clean, redr, nredir);
        st = shell_status;
        free_redirs(redr, nredir);
    }

out:
    free(clean);
    free_argv(argv);
    return st;
}

/* Split a command line on `;`, `&&`, `||` and background `&` at top
 * level.  ops[i] is the operator preceding units[i]: 0 (first), 1 (`&&`),
 * 2 (`||`), 3 (`;`), 4 (`&` async). */
static int split_ops(const char *line, char ***out, int **outops)
{
    char **units = NULL;
    int *ops = NULL;
    int n = 0, cap = 0;
    int op = 0;
    char seg[LINE_MAX_CP];
    int si = 0;
    char q = 0;
    const char *p = line;

#define PUSH() do {                                                          \
        int s = 0, e = si;                                                   \
        while (s < e && isspace((unsigned char)seg[s])) s++;                 \
        while (e > s && isspace((unsigned char)seg[e - 1])) e--;             \
        if (e > s) {                                                          \
            seg[e] = 0;                                                       \
            if (n >= cap) { cap = cap ? cap * 2 : 4;                          \
                units = xrealloc(units, sizeof(char *) * (size_t)cap);        \
                ops   = xrealloc(ops,   sizeof(int)    * (size_t)cap);        \
            }                                                                 \
            units[n] = xstrdup(seg);                                          \
            ops[n]   = op;                                                    \
            n++;                                                              \
        }                                                                     \
        si = 0;                                                               \
    } while (0)

    while (*p) {
        char ch = *p;
        if (q == 0 && (ch == '\'' || ch == '"')) { q = ch; seg[si++] = ch; p++; continue; }
        if (q != 0 && ch == q) { q = 0; seg[si++] = ch; p++; continue; }
        if (q == 0 && ch == '\\' && p[1]) { seg[si++] = ch; seg[si++] = p[1]; p += 2; continue; }
        if (q == 0) {
            if (p[0] == '&' && p[1] == '&') { PUSH(); op = 1; p += 2; continue; }
            if (p[0] == '|' && p[1] == '|') { PUSH(); op = 2; p += 2; continue; }
            if (p[0] == ';')                { PUSH(); op = 3; p += 1; continue; }
            if (p[0] == '&' && p[1] == '>') { seg[si++] = '&'; seg[si++] = '>'; p += 2; continue; }
            if (p[0] == '>' && p[1] == '&') { seg[si++] = '>'; seg[si++] = '&'; p += 2; continue; }
            if (p[0] == '&')                { PUSH(); op = 4; p += 1; continue; }
        }
        seg[si++] = ch;
        p++;
    }
    PUSH();
#undef PUSH

    if (!units) units = xmalloc(sizeof(char *));
    *out = units;
    *outops = ops;
    return n;
}

/* One command unit (`|`-pipeline).  Foreground brackets the terminal in
 * cooked mode; background children inherit whatever state is current. */
static int exec_unit(const char *unit, int bg)
{
    char **segs;
    int n;
    split_pipeline(unit, &segs, &n);
    if (n == 0) { free(segs); return 0; }

    if (!bg) term_exit_raw();
    fflush(stdout);

    int st = (n == 1) ? exec_single(segs[0]) : run_pipeline(segs, n);

    if (!bg) term_enter_raw();
    for (int i = 0; i < n; i++) free(segs[i]);
    free(segs);
    return st;
}

static int reap_bg(void)
{
    int st = 0;
    int ws;
    while (waitpid(-1, &ws, WNOHANG) > 0) {
        if (WIFSIGNALED(ws)) st = 128 + WTERMSIG(ws);
    }
    return st;
}

int exec_line(const char *line)
{
    /* Config directives (rc files or typed live) are consumed here. */
    if (config_apply_line(line)) return 0;

    char **units;
    int *ops;
    int n = split_ops(line, &units, &ops);
    if (n == 0) { free(ops); free(units); return 0; }

    static int bgjobs = 0;
    int st = 0;

    for (int i = 0; i < n; i++) {
        if (ops[i] == 1 && st != 0) continue;     /* && short-circuit */
        if (ops[i] == 2 && st == 0) continue;     /* || short-circuit */

        if (ops[i] == 4) {                        /* background */
            term_exit_raw();
            fflush(stdout);
            pid_t pid = fork();
            if (pid < 0) {
                perror("fork");
                st = 1;
            } else if (pid == 0) {
                signal(SIGINT, SIG_DFL);
                signal(SIGQUIT, SIG_DFL);
                signal(SIGTSTP, SIG_DFL);
                signal(SIGPIPE, SIG_DFL);
                int dn = open("/dev/null", O_RDONLY);
                if (dn >= 0) { dup2(dn, STDIN_FILENO); close(dn); }
                _exit(exec_unit(units[i], 1));
            } else {
                outf("[%d] %d\n", ++bgjobs, pid);
            }
            term_enter_raw();
            continue;
        }

        st = exec_unit(units[i], 0);
    }

    reap_bg();

    for (int i = 0; i < n; i++) free(units[i]);
    free(units);
    free(ops);
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