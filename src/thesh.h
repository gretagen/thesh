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
#include <time.h>

#define THESH_NAME    "thesh"
#define THESH_VERSION "0.5.1"
#define HIST_NAME     ".thesh_history"
#define HIST_MAX      500
#define ALIAS_MAX     128
#define LINE_MAX_CP   4096

extern int shell_status;
extern volatile sig_atomic_t thesh_winched;   /* SIGWINCH: redraw needed  */
extern volatile sig_atomic_t thesh_conted;    /* SIGCONT: re-assert state  */
void mux_notify_winch(void);                  /* wake the mux relayout    */
void thesh_on_cont(void);                     /* raw+mouse after resume    */

/* ---- util.c ---- */
void   *xmalloc(size_t n);
void   *xrealloc(void *p, size_t n);
char   *xstrdup(const char *s);
void    out(const char *s);
void    outn(const void *s, size_t n);
void    outf(const char *fmt, ...);
char   *get_home(void);
int     muxdbg_on(void);                    /* THESH_MUX_DEBUG=<path> log  */
void    muxdbg(const char *fmt, ...);
void    muxdbg_bytes(const char *b, size_t n);
void    tty_buf_write(const char *b, size_t n); /* batched tty output     */
void    tty_buf_flush(void);                /* push it to the real tty     */

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
    int    loaded;
} History;

extern History H;

void    hist_setup(void);
void    hist_load(void);
void    hist_add(const char *line);
void    hist_save(void);
void    hist_clear(void);
void    hist_set_limit(int n);
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
    K_F1, K_F2, K_F3, K_F4, K_F5, K_F6, K_F7, K_F8,
    K_F9, K_F10, K_F11, K_F12, K_F13, K_F14, K_F15, K_F16,
    K_F17, K_F18, K_F19, K_F20, K_F21, K_F22, K_F23, K_F24,
};
#define MOD_CTRL  1
#define MOD_ALT   2
#define MOD_SUPER 4
#define MOD_SHIFT 8

/* ---- bind.c (user key bindings from the config) ---- */
#define BIND_MAX 32
#define BIND_CMD_MAX 256
#define BIND_ASK_MAX 64

typedef enum { BIND_CLOSE, BIND_COPY, BIND_PASTE, BIND_EXEC, BIND_VERB } BindKind;

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
#define ANIM_NONE      0    /* typed-character animation styles        */
#define ANIM_MATRIX    1
#define ANIM_NEWCOMER  2
#define ANIM_PLACEMENT 3
#define ANIM_SPINNER   4

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
    char  *col_mux_active; /* multiplexer border: focused pane         */
    char  *col_mux_inactive;/* multiplexer border: unfocused panes     */
    int    multiplexer;     /* 1 = splits allowed                      */
    int    mux_window_limit;/* max panes; 0 = unlimited (inf)          */
    int    mux_style;       /* 0 = crosshair (tmux separators)         */
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
    int    history_limit;  /* max commands kept in history             */
    int    history_enabled;/* 1 = record + save command history        */
    int    animation;      /* typed-character animation (ANIM_*)       */
    int    movespeed;      /* cells per left/right press               */
    int    scrollspeed;    /* history entries per up/down press        */
    int    amplify;        /* held-arrow step multiplier (hold-to-2×)  */
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

/* ---- screen.c / mux.c: native terminal multiplexer ---- */
#define MUX_MAX_PANES  128
#define MUX_SCROLLBACK 1000
#define MUX_MIN_W      8              /* smallest pane that may be split */
#define MUX_MIN_H      6

/* cell attribute bits (SGR) */
#define CA_BOLD    0x01
#define CA_DIM     0x02
#define CA_ITALIC  0x04
#define CA_UL      0x08
#define CA_REV     0x10
#define CA_BLINK   0x20
#define CA_INVIS   0x40
#define CA_STRIKE  0x80

/* Cell colors: kind 0 = terminal default, 1 = basic SGR (idx holds the
 * original 30..97/40..107 number), 2 = 256-palette, 3 = 24-bit rgb. */
typedef struct { uint8_t kind, idx, r, g, b; } MuxColor;

typedef struct {
    uint32_t cp;                          /* 0 = blank space            */
    uint8_t  attrs;                       /* CA_* bits                  */
    uint8_t  pad;                         /* 1 = 2nd half of a wide glyph */
    MuxColor fg, bg;
} MuxCell;

typedef struct {
    int      w, h;                        /* interior grid (no border)  */
    MuxCell *cells;                       /* w*h                        */
    MuxCell *alt;                         /* alternate screen if held   */
    int      cx, cy, wrap;                /* cursor + deferred wrap     */
    int      autowrap;                    /* DECAWM (on by default)     */
    uint8_t  attrs; MuxColor fg, bg;      /* current pen                */
    int      top, bot;                    /* scroll region (0-based)    */
    int      scx, scy;                    /* saved cursor (DECSC)       */
    uint8_t  sattrs; MuxColor sfg, sbg; int ssaved;
    int      asx, asy;                    /* cursor saved by 1048/1049  */
    int      cur_vis;                     /* DECTCEM (1 = visible)      */
    int      mouse;                       /* requested mouse level:      */
                                          /* bit0-2: 0 off,1 std,2 btn, */
                                          /* 4 all; bit3: SGR (1006)    */
    MuxCell **sb;                         /* scrollback ring rows       */
    int      *sb_ws;                      /* that row's width           */
    int      sb_head, sb_count;           /* next slot / live count     */
    int      view;                        /* lines scrolled up (0=live) */
    uint8_t *dirty;                       /* one flag per row           */
    int      all_dirty;
    /* parser state — persists across feed() chunks */
    int      st;                          /* PT_* below                 */
    char     csib[40]; int csin;          /* raw CSI collected          */
    int      cp_p[16]; int cp_n;          /* parsed CSI params          */
    char     csi_priv;                    /* '?', '>', '<', '=' or 0    */
    uint8_t  u_buf[4]; int u_have, u_need;/* incremental utf-8          */
    uint32_t last_cp;                     /* last printed (for REP)     */
    /* writeback for child panes (DSR/DA replies) */
    void   (*reply)(void *ctx, const char *b, size_t n);
    void    *reply_ctx;
} MuxScreen;

typedef struct {
    MuxScreen scr;
    int      x, y, w, h;                  /* rect including its border  */
    int      master;                      /* pty master                 */
    pid_t    pid;                         /* child pid                  */
} MuxPane;

int  mux_active(void);                    /* this process hosts the mux  */
void mux_term_write(const char *b, size_t n); /* raw write to the real tty */
void mux_pump(void);                      /* pane ptys, repaint, park    */
int  mux_split(int vertical, int pct);    /* 1st call: fork-on-split     */
int  mux_close(void);
int  mux_focus_dir(int dir);              /* 0 left, 1 right, 2 up, 3 down */
int  mux_focus_cycle(int delta);
void mux_scroll(int delta);               /* >0 up, <0 down, 0 = live   */
int  mux_mouse(int button, int x, int y, int release); /* 1 = pass to TUI */
const char *mux_mouse_seq(int *len);      /* translated SGR for the pane */
void mux_forward(const char *b, size_t n);/* keys -> focused pane's pty */
void mux_forward_mouse(const char *b, size_t n); /* mouse -> clicked pane */
void mux_resize_check(void);              /* consume the SIGWINCH flag  */
void mux_shutdown(void);                  /* restore tty, kill panes    */
void mux_leave(void);                     /* leave mux + fresh shell    */
int  mux_kill_all(void);                  /* kill every pane            */
void mux_list(void);                      /* `panes` builtin output      */
void term_size(int *cols, int *rows);    /* tty size, 80x24 fallback     */

/* screen.c */
void mux_screen_init(MuxScreen *s, int w, int h);
void mux_screen_resize(MuxScreen *s, int w, int h);
void mux_screen_free(MuxScreen *s);
void mux_screen_feed(MuxScreen *s, const char *data, size_t n);
void mux_screen_repaint(MuxScreen *s, int x, int y, int w, int h);

/* ---- editor.c ---- */
char   *edit_line(const char *prompt, int *cancelled);
int     ed_read_key(uint32_t *cp, int *mods);
const char *ed_raw(int *len);   /* raw bytes of the last key read      */
void    ed_pushback(const char *b, int n); /* unread bytes for read_byte */
int     ed_mux_nav(int key, int mods);     /* ctrl+arrow → pane dir      */
int     ed_mouse_forward(void);            /* mouse event belongs to TUI  */
const Bind *ed_bind_lookup(int key, uint32_t cp, int mods);
                                          /* map key → bind (router)     */
void    ed_reset_anchor(void);            /* pane shell: drop old coords */
void    thesh_config_check(void);         /* rc auto-reload tick         */
int     prompt_line(char *buf, size_t sz, const char *prompt);

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