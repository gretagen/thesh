#!/bin/sh
# therc - Lightweight POSIX shell for Zerene OS
# No bash dependency. Works with /bin/sh.

# ── Config ──────────────────────────────────────────────────────────
THERC_VERSION="0.1.0"
THERC_HISTFILE="${HOME}/.therc_history"
THERC_HISTSIZE=500

# ── State ───────────────────────────────────────────────────────────
_therc_running=1
_therc_aliases=""
_therc_env=""

# ── Prompt ──────────────────────────────────────────────────────────
_therc_prompt() {
    _therc_user="$(id -un)"
    _therc_host="$(uname -n 2>/dev/null || echo unknown)"
    _therc_dir="$(pwd)"
    # Replace $HOME with ~
    case "$_therc_dir" in
        "$HOME"*) _therc_dir="~${_therc_dir#"$HOME"}" ;;
    esac
    # Color codes
    printf '\033[36m%s\033[0m | \033[33m%s\033[0m ] \033[32m%s\033[0m $ ' \
        "$_therc_user" "$_therc_host" "$_therc_dir"
}

# ── Builtins ────────────────────────────────────────────────────────
_therc_builtin_cd() {
    if [ -z "$1" ] || [ "$1" = "~" ]; then
        cd "$HOME" 2>/dev/null || return 1
    else
        cd "$1" 2>/dev/null || return 1
    fi
}

_therc_builtin_alias() {
    if [ -z "$1" ]; then
        echo "$_therc_aliases"
        return
    fi
    case "$1" in
        *=*)
            # Add/update alias
            _name="${1%%=*}"
            _value="${1#*=}"
            # Remove existing alias with same name
            _therc_aliases="$(echo "$_therc_aliases" | grep -v "^${_name}=")"
            _therc_aliases="${_therc_aliases}
${_name}=${_value}"
            ;;
        *)
            echo "$_therc_aliases" | grep "^${1}="
            ;;
    esac
}

_therc_builtin_export() {
    if [ -z "$1" ]; then
        env
        return
    fi
    case "$1" in
        *=*)
            _name="${1%%=*}"
            _value="${1#*=}"
            export "$_name"="$_value"
            ;;
        *)
            eval "export \"\$$_name\""
            ;;
    esac
}

_therc_builtin_exit() {
    _therc_running=0
}

_therc_builtin_unset() {
    [ -n "$1" ] && unset "$1"
}

# ── Resolve aliases ────────────────────────────────────────────────
_therc_resolve_alias() {
    local cmd="$1"
    local result
    result="$(echo "$_therc_aliases" | grep "^${cmd}=" | head -1)"
    if [ -n "$result" ]; then
        echo "${result#*=}"
    else
        echo "$cmd"
    fi
}

# ── Find command in PATH ────────────────────────────────────────────
_therc_find_cmd() {
    local cmd="$1"
    # Absolute or relative path
    case "$cmd" in
        /*|./*|../*)
            [ -x "$cmd" ] && echo "$cmd" && return 0
            return 1
            ;;
    esac
    # Search PATH
    local IFS=:
    for dir in $PATH; do
        if [ -x "${dir}/${cmd}" ]; then
            echo "${dir}/${cmd}"
            return 0
        fi
    done
    return 1
}

# ── Execute command line ───────────────────────────────────────────
_therc_exec() {
    local input="$1"
    [ -z "$input" ] && return

    # Skip comments
    case "$input" in
        \#*) return ;;
    esac

    # Split into words (basic)
    local cmd=""
    local args=""
    local _word=""
    local _in_quote=0
    local _quote_char=""
    local _i=0

    # Parse first word as command
    set -- $input
    cmd="$1"
    shift

    # Resolve alias
    cmd="$(_therc_resolve_alias "$cmd")"

    # Handle builtins
    case "$cmd" in
        cd)     _therc_builtin_cd "$@" ;;
        alias)  _therc_builtin_alias "$@" ;;
        export) _therc_builtin_export "$@" ;;
        exit)   _therc_builtin_exit ;;
        unset)  _therc_builtin_unset "$@" ;;
        unsetenv) unset "$@" ;;
        set)
            case "$1" in
                -o) ;;
                *) set -- ;;
            esac
            ;;
        history)
            if [ -f "$THERC_HISTFILE" ]; then
                nl -ba "$THERC_HISTFILE"
            fi
            ;;
        source)
            [ -f "$1" ] && . "$1"
            ;;
        *)
            # External command
            local full_cmd
            full_cmd="$(_therc_find_cmd "$cmd")"
            if [ -n "$full_cmd" ]; then
                "$full_cmd" "$@"
            else
                printf 'therc: %s: command not found\n' "$cmd" >&2
                return 127
            fi
            ;;
    esac
}

# ── History ─────────────────────────────────────────────────────────
_therc_hist_add() {
    local line="$1"
    [ -z "$line" ] && return
    echo "$line" >> "$THERC_HISTFILE"
    # Trim history
    if [ -f "$THERC_HISTFILE" ]; then
        local count
        count="$(wc -l < "$THERC_HISTFILE")"
        if [ "$count" -gt "$THERC_HISTSIZE" ]; then
            tail -n "$THERC_HISTSIZE" "$THERC_HISTFILE" > "${THERC_HISTFILE}.tmp"
            mv "${THERC_HISTFILE}.tmp" "$THERC_HISTFILE"
        fi
    fi
}

# ── Basic tab completion ────────────────────────────────────────────
_therc_complete() {
    local buf="$READLINE_LINE"
    local word="${buf##* }"

    # If empty word, complete from PATH commands
    if [ -z "$word" ]; then
        local cmds=""
        local IFS=:
        for dir in $PATH; do
            [ -d "$dir" ] || continue
            for f in "$dir"/*; do
                [ -x "$f" ] || continue
                local name="${f##*/}"
                cmds="${cmds}${name} "
            done
        done 2>/dev/null
        printf '\n%s\n' "$cmds"
        return
    fi

    # Path/directory completion
    local dir=""
    local base="$word"
    case "$word" in
        */*)
            dir="${word%/*}/"
            base="${word##*/}"
            ;;
    esac

    local matches=""
    local IFS='
'
    for f in ${dir}${base}*; do
        [ -e "$f" ] || continue
        local name="${f##*/}"
        if [ -d "$f" ]; then
            matches="${matches}${dir}${name}/ "
        else
            matches="${matches}${dir}${name} "
        fi
    done 2>/dev/null

    local count
    count="$(echo "$matches" | wc -w)"

    if [ "$count" -eq 1 ]; then
        READLINE_LINE="${dir}${matches%% *}"
        READLINE_POINT="${#READLINE_LINE}"
    elif [ "$count" -gt 1 ]; then
        printf '\n%s\n' "$matches"
    fi
}

# ── Signal handling ─────────────────────────────────────────────────
_therc_cleanup() {
    _therc_running=0
    printf '\n'
}

trap _therc_cleanup INT TERM
trap '' HUP

# ── Main REPL ───────────────────────────────────────────────────────
_therc_main() {
    local input=""

    # Set up completion
    bind -x '"\C-i": _therc_complete' 2>/dev/null

    # History file
    touch "$THERC_HISTFILE" 2>/dev/null

    while [ "$_therc_running" -eq 1 ]; do
        _therc_prompt
        read -r input || { _therc_running=0; break; }

        # Skip empty
        [ -z "$input" ] && continue

        # History
        _therc_hist_add "$input"

        # Execute
        _therc_exec "$input"
    done

    printf 'therc: exiting\n'
}

# ── Entry point ─────────────────────────────────────────────────────
_therc_main "$@"
