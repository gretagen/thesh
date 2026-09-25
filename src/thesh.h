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
#include <sys/stat.h>
#include <sys/wait.h>
#include <wchar.h>
#include <locale.h>
#include <poll.h>
#include <dirent.h>
#include <limits.h>
#include <pwd.h>

#define THESH_NAME    "thesh"
#define THESH_VERSION "0.3.1"
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
void    hist_clear(void);
int     hist_find(const char *needle, int start, int backward);
int     hist_find_substr(const char *sub, int start, int backward);

/* ---- dict.c ---- */
void        dict_refresh(const char *pathstr);
void        dict_force_refresh(const char *pathstr);
int         dict_count(void);
const char *dict_get(int i);
int         dict_has(const char *name);
int         file_matches(const char *dir, const char *stem,
                         const char ***out, int *cap); /* caller frees *out */

/* ---- spell.c ---- */
int     edit_distance(const char *a, const char *b);
const char *best_suggestion(const char *word);
void    print_suggestion(const char *word);

/* ---- key codes (editor + user bindings) ---- */
enum {
    K_NONE = 256,           /* raw codepoint returned in `cp` */
    K_EOF,
    K_TAB,
    K_ENTER,
    K_DEL,                  /* backspace */
    K_FDEL,                 /* forward delete key */
    K_UP, K_DOWN, K_LEFT, K_RIGHT,
    K_HOME, K_END, K_PGUP, K_PGDN,
    K_WLEFT, K_WRIGHT,      /* word / ctrl / alt movement */
    K_CTL_A, K_CTL_E, K_CTL_B, K_CTL_F,
    K_CTL_K, K_CTL_U, K_CTL_W, K_CTL_L, K_CTL_R,
    K_CTL_C, K_CTL_D,
};
#define MOD_CTRL 1
#define MOD_ALT  2

/* ---- bind.c (user key bindings from the config) ---- */
#define BIND_MAX 32
#define BIND_CMD_MAX 256
#define BIND_ASK_MAX 64

typedef enum { BIND_CLOSE, BIND_COPY, BIND_PASTE, BIND_EXEC } BindKind;

typedef struct {
    int      mods;          /* MOD_CTRL / MOD_ALT bitmask             */
    int      key;           /* char code or K_* for named keys        */
    BindKind kind;
    char     cmd[BIND_CMD_MAX];  /* exec target                      */
    char     ask[BIND_ASK_MAX];  /* ask() prompt label ("" = none)    */
} Bind;

int          bind_parse_line(const char *line);
const Bind  *bind_lookup(int mods, int key);
void         bind_dump(FILE *f);

/* ---- config.c ---- */
typedef struct {
    char  *looks;          /* prompt template; NULL → PS1/default      */
    char  *rightwall;      /* styles                                   */
    char  *leftwall;
    char  *sep;
    char  *cursor;         /* cursorstyle symbol                       */
    char  *col_rightwall;  /* color names, NULL = no color             */
    char  *col_leftwall;
    char  *col_sep;
    char  *col_host;
    char  *col_path;
    char  *col_cursor;
    char  *col_guess;      /* ghost suggestion color                   */
    char  *col_user;       /* user-color                               */
    char  *textcolor;      /* general prompt text color                */
    int    op_rightwall;   /* per-element opacity (0-100), -1 = unset  */
    int    op_leftwall;
    int    op_sep;
    int    op_host;
    int    op_user;
    int    op_path;
    int    op_cursor;
    int    op_guess;
    int    hybrid;         /* typing: 1 = hybrid (case-insensitive)    */
    int    guesser;        /* 1 = ghost suggestions on                 */
    int    corrector;      /* 0 passive, 1 active, 2 consent, 3 inactive */
    int    autoreload;     /* 1 = re-source rc files when they change  */
} Config;

extern Config Cfg;
void     config_init(void);
int      config_apply_line(const char *line);
const char *config_default_looks(void);
char    *render_looks_str(const char *tpl, const char *user,
                          const char *host, const char *dir);
char    *render_ps1(const char *ps1, const char *user,
                    const char *host, const char *dir);
void     color_sgr(const char *sgr, int opacity, char *buf, size_t sz);
void     config_dump_current(FILE *f);

/* ---- editor.c ---- */
char   *edit_line(const char *prompt, int *cancelled);

/* ---- exec.c ---- */
int     exec_line(const char *line);
int     run_file(const char *path);
int     run_file_silent(const char *path);
int     is_builtin(const char *name);
const char *alias_lookup(const char *name);
void    alias_set(const char *name, const char *value);
void    alias_unset(const char *name);
void    print_aliases(void);
void    dump_aliases(FILE *f);
const char *const *builtin_names(void);
char   *build_prompt(void);

/* ---- presets.c ---- */
int     cmd_presets(int argc, char **argv);
int     cmd_savepreset(int argc, char **argv);

#endif