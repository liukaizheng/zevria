# Fixed POSIX source shared by probe and handoff. No profiles or evaluated data.
set -eu
unset BASH_ENV ENV ZEVRIA_INSTALL
zevria_inherited_path=${PATH-}
zevria_path() {
    PATH="$HOME/.zevria/bin:$HOME/.cargo/bin:$HOME/.local/bin:/usr/local/bin:/usr/bin:/bin:$zevria_inherited_path"
    if [ "$1" != "$HOME/.zevria/bin" ]; then PATH="$1:$PATH"; fi
    export PATH
}
zevria_locator_error() {
    echo 'Invalid or stale Linux ~/.zevria/install-root; rerun the matching Linux installer with the intended ZEVRIA_INSTALL, or remove the locator to restore default discovery. No alternate Linux installation was selected.' >&2
    exit 33
}
zevria_select() {
    zevria_path "$HOME/.zevria/bin"
    locator=$HOME/.zevria/install-root
    if [ -L "$HOME/.zevria" ] || { [ -e "$HOME/.zevria" ] && { [ ! -d "$HOME/.zevria" ] || [ ! -r "$HOME/.zevria" ] || [ ! -x "$HOME/.zevria" ]; }; }; then
        zevria_locator_error
    fi
    if [ -e "$locator" ] || [ -L "$locator" ]; then
        [ -f "$locator" ] && [ ! -L "$locator" ] && [ -r "$locator" ] || zevria_locator_error
        size=$(wc -c < "$locator") || zevria_locator_error
        [ "$size" -gt 1 ] && [ "$size" -le 4096 ] || zevria_locator_error
        # Bound the read itself as well as the size check (the file may change).
        root=$(head -c 4097 -- "$locator") || zevria_locator_error
        case "$root" in /*) ;; *) zevria_locator_error;; esac
        # Byte count enforces exactly one terminating newline, including rejection
        # of stripped NULs/newlines. A root is data, never a shell expression.
        actual=$(printf '%s\n' "$root" | wc -c)
        [ "$actual" -eq "$size" ] || zevria_locator_error
        if printf '%s' "$root" | LC_ALL=C grep -q -e '[[:cntrl:]:]' -e "$(printf '\302[\200-\237]')"; then zevria_locator_error; fi
        exe=${root%/}/bin/zevria
        [ -f "$exe" ] && [ -x "$exe" ] && [ ! -L "$exe" ] || zevria_locator_error
        recorded=1
    else
        exe=$(command -v zevria) || { echo 'Linux Zevria is missing; run the matching Linux installer inside WSL' >&2; exit 33; }
        recorded=0
    fi
    exe=$(readlink -f -- "$exe") || {
        [ "$recorded" = 0 ] || zevria_locator_error
        exit 33
    }
    [ -f "$exe" ] && [ -x "$exe" ] || {
        [ "$recorded" = 0 ] || zevria_locator_error
        echo 'Linux Zevria is not a regular executable' >&2; exit 34
    }
    magic=$(od -An -tx1 -N4 -- "$exe" | tr -d ' \n')
    if [ "$magic" != 7f454c46 ]; then
        [ "$recorded" = 0 ] || zevria_locator_error
        echo 'resolved Zevria is not a Linux ELF executable (possible Windows recursion)' >&2; exit 34
    fi
    # Readiness (including RTK) and handoff must use the same selected-bin order,
    # even when legacy discovery selected Cargo or followed an executable link.
    zevria_path "${exe%/*}"
}
