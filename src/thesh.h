#ifndef THESH_H
#define THESH_H

#define _GNU_SOURCE

#include <stdio.h>
#include <stdlib.h>
#include <stdarg.h>
#include <stdint.h>
#include <string.h>
#include <strings.h>
#include <ctype.h>
#include <unistd.h>
#include <termios.h>
#include <signal.h>
#include <errno.h>
#include <fcntl.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <wchar.h>
#include <locale.h>
#include <poll.h>
#include <dirent.h>
#include <limits.h>
#include <pwd.h>

#define THESH_NAME    "thesh"
#define THESH_VERSION "0.2.0"
#define HIST_NAME     ".thesh_history"
#define HIST_MAX      500
#define ALIAS_MAX     128
#define LINE_MAX_CP   4096

extern int shell_status;

/* ---- util.c ---- */
void   *xmalloc(size_t n);
void   *xrealloc(void *p, size_t n);
char   *xstrdup(const char *s);
void    out(const char *s);
void    outn(const void *s, size_t n);
void    outf(const char *fmt, ...);
char   *get_home(void);

/* ---- utf8.c ---- */
int     utf8_decode(const char *s, int *len, uint32_t *cp);
int     utf8_encode(uint32_t cp, char *out);
int     cp_width(uint32_t cp);

/* ---- term.c ---- */
int     term_enter_raw(void);
void    term_exit_raw(void);
int     term_raw_active(void);

/* ---- history.c ---- */
typedef struct {
    char **items;
    int    count;
    int    cap;
    int    max;
    char  *path;
    int    dirty;
} History;

extern History H;

void    hist_setup(void);
void    hist_load(void);
void    hist_add(const char *line);
void    hist_save(void);
int     hist_find(const char *needle, int start, int backward);
int     hist_find_substr(const char *sub, int start, int backward);

/* ---- dict.c ---- */
void        dict_refresh(const char *pathstr);
int         dict_count(void);
const char *dict_get(int i);
int         dict_has(const char *name);
int         file_matches(const char *dir, const char *stem,
                         const char ***out, int *cap); /* caller frees *out */

/* ---- spell.c ---- */
int     edit_distance(const char *a, const char *b);
void    print_suggestion(const char *word);

/* ---- editor.c ---- */
char   *edit_line(const char *prompt, int *cancelled);

/* ---- exec.c ---- */
int     exec_line(const char *line);
int     run_file(const char *path);
int     is_builtin(const char *name);
const char *alias_lookup(const char *name);
void    alias_set(const char *name, const char *value);
void    alias_unset(const char *name);
void    print_aliases(void);
const char *const *builtin_names(void);
char   *build_prompt(void);

#endif