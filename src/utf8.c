#include "thesh.h"

int utf8_decode(const char *s, int *len, uint32_t *cp)
{
    unsigned char c = (unsigned char)s[0];
    if (c < 0x80) {
        *cp = c; *len = 1;
        return 1;
    }
    int n;
    uint32_t code;
    if ((c & 0xE0) == 0xC0)                { n = 2; code = c & 0x1F; }
    else if ((c & 0xF0) == 0xE0)           { n = 3; code = c & 0x0F; }
    else if ((c & 0xF8) == 0xF0)           { n = 4; code = c & 0x07; }
    else                                   { *cp = c; *len = 1; return 0; }

    for (int i = 1; i < n; i++) {
        unsigned char b = (unsigned char)s[i];
        if ((b & 0xC0) != 0x80)             { *cp = c; *len = 1; return 0; }
        code = (code << 6) | (b & 0x3F);
    }
    *cp = code; *len = n;
    return 1;
}

int utf8_encode(uint32_t cp, char *out)
{
    if (cp < 0x80) {
        out[0] = (char)cp;
        return 1;
    }
    if (cp < 0x800) {
        out[0] = (char)(0xC0 | (cp >> 6));
        out[1] = (char)(0x80 | (cp & 0x3F));
        return 2;
    }
    if (cp < 0x10000) {
        out[0] = (char)(0xE0 | (cp >> 12));
        out[1] = (char)(0x80 | ((cp >> 6) & 0x3F));
        out[2] = (char)(0x80 | (cp & 0x3F));
        return 3;
    }
    out[0] = (char)(0xF0 | (cp >> 18));
    out[1] = (char)(0x80 | ((cp >> 12) & 0x3F));
    out[2] = (char)(0x80 | ((cp >> 6) & 0x3F));
    out[3] = (char)(0x80 | (cp & 0x3F));
    return 4;
}

int cp_width(uint32_t cp)
{
    int w = wcwidth((wchar_t)cp);
    return w >= 0 ? w : 1;
}