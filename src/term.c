#include "thesh.h"

static struct termios orig_tios;
static int raw_on = 0;

int term_enter_raw(void)
{
    if (raw_on) return 0;
    if (tcgetattr(STDIN_FILENO, &orig_tios) < 0) return -1;

    struct termios t = orig_tios;
    t.c_lflag &= (tcflag_t)~(ICANON | ECHO | ISIG | IEXTEN);
    t.c_iflag &= (tcflag_t)~(IXON | ICRNL);
    t.c_cc[VMIN] = 1;
    t.c_cc[VTIME] = 0;
    if (tcsetattr(STDIN_FILENO, TCSADRAIN, &t) < 0) return -1;
    raw_on = 1;
    return 0;
}

void term_exit_raw(void)
{
    if (!raw_on) return;
    tcsetattr(STDIN_FILENO, TCSADRAIN, &orig_tios);
    raw_on = 0;
}

int term_raw_active(void)
{
    return raw_on;
}