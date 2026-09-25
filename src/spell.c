#include "thesh.h"

int edit_distance(const char *a, const char *b)
{
    size_t la = strlen(a), lb = strlen(b);
    if (la > 256 || lb > 256) return 999;

    size_t w = lb + 1;
    size_t *prev = xmalloc(sizeof(size_t) * w);
    size_t *cur  = xmalloc(sizeof(size_t) * w);
    for (size_t j = 0; j < w; j++) prev[j] = j;

    for (size_t i = 1; i <= la; i++) {
        cur[0] = i;
        for (size_t j = 1; j < w; j++) {
            size_t cost = a[i - 1] == b[j - 1] ? 0 : 1;
            size_t best = prev[j] + 1;          /* delete   */
            size_t ins  = cur[j - 1] + 1;       /* insert   */
            size_t sub  = prev[j - 1] + cost;   /* subs     */
            if (ins < best) best = ins;
            if (sub < best) best = sub;
            cur[j] = best;
        }
        size_t *t = prev; prev = cur; cur = t;
    }
    size_t r = prev[lb];
    free(prev);
    free(cur);
    return r < 4096 ? (int)r : 4096;
}

const char *best_suggestion(const char *word)
{
    if (!word || !*word) return NULL;

    int best_d = INT_MAX;
    const char *best = NULL;
    size_t wl = strlen(word);
    int maxd = (int)wl >= 8 ? 3 : 2;

#define TRY(c) do {                                                         \
        if (!strcmp((c), word)) break;                                      \
        if ((c)[0] != word[0]) break;                                       \
        int d = edit_distance(word, (c));                                   \
        if (d <= maxd && (d < best_d ||                                    \
            (d == best_d && (!best || strlen((c)) < strlen(best))))) {      \
            best_d = d; best = (c);                                         \
        }                                                                   \
    } while (0)

    for (int i = 0; builtin_names()[i]; i++) TRY(builtin_names()[i]);
    for (int i = 0; i < dict_count(); i++) TRY(dict_get(i));

#undef TRY

    return best;
}

void print_suggestion(const char *word)
{
    if (!word || !*word) {
        dprintf(STDERR_FILENO, "%s: no command given\n", THESH_NAME);
        return;
    }

    const char *best = best_suggestion(word);

    if (best)
        dprintf(STDERR_FILENO,
                "%s: no such command: '%s'. Did you mean '%s'?\n",
                THESH_NAME, word, best);
    else
        dprintf(STDERR_FILENO, "%s: no such command: '%s'\n", THESH_NAME, word);
}