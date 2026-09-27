#!/usr/bin/env bash
# Standalone installer; Bash 3.2+. Sourcing defines helpers without installing.
# Tests replace transport helpers, never production URLs or verification policy.

zevria_die() { printf 'zevria installer: %s\n' "$*" >&2; exit 1; }
zevria_warn() { printf 'zevria installer: %s\n' "$*" >&2; }
zevria_help() {
    cat <<'HELP'
Usage: install.sh [VERSION] [--no-path-update] [--help]
Install latest stable, or a SemVer version (optional v prefix).
ZEVRIA_INSTALL must be absolute; default: $HOME/.zevria, executable: bin/zevria.
--no-path-update skips shell changes, not Linux install-root metadata.
Installs only Zevria, not RTK, Git Bash, WSL, Rust, or provider configuration.
HELP
}
zevria_version() {
    local value=${1#v} part pre
    local pattern='^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?(\+[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$'
    [[ $value =~ $pattern ]] || return 1
    pre=${value%%+*}
    if [[ $pre == *-* ]]; then
        pre=${pre#*-}
        while :; do
            part=${pre%%.*}
            if [[ $part =~ ^[0-9]+$ && $part == 0?* ]]; then return 1; fi
            [[ $pre == *.* ]] || break
            pre=${pre#*.}
        done
    fi
    printf '%s' "$value"
}
zevria_path() {
    local unicode_controls=$'\302[\200-\237]'
    [[ $1 == /* && $1 != *:* && ! $1 =~ [[:cntrl:]] && ! $1 =~ $unicode_controls ]] || return 1
}
zevria_quote() {
    local rest=$1
    printf "'"
    while [[ $rest == *\'* ]]; do
        printf '%s' "${rest%%\'*}" "'\\''"
        rest=${rest#*\'}
    done
    printf "%s'" "$rest"
}
zevria_target() {
    local os arch
    os=$(uname -s) || zevria_die 'Cannot detect operating system.'
    arch=$(uname -m) || zevria_die 'Cannot detect CPU.'
    case "$os" in
        Linux)
            [[ $arch == x86_64 ]] || zevria_die 'Linux requires x86_64 GNU; use a source build on other CPUs.'
            case "$(getconf GNU_LIBC_VERSION 2>/dev/null || :)" in
                glibc\ *) ;;
                *) zevria_die 'Linux requires glibc (GNU), not musl. Use a compatible distribution or source build.' ;;
            esac
            printf x86_64-unknown-linux-gnu ;;
        Darwin)
            if [[ $arch == arm64 ]] || { [[ $arch == x86_64 ]] && {
                [[ $(sysctl -n sysctl.proc_translated 2>/dev/null) == 1 ]] ||
                [[ $(sysctl -n hw.optional.arm64 2>/dev/null) == 1 ]]
            }; }; then
                printf aarch64-apple-darwin
            else zevria_die 'Intel macOS is not packaged; use a source build. Apple Silicon is required.'; fi ;;
        MINGW*|MSYS*|CYGWIN*) zevria_die 'On Windows, run install.ps1 in PowerShell, not install.sh in Git Bash/MSYS/Cygwin.' ;;
        *) zevria_die "Unsupported platform $os/$arch; use a source build." ;;
    esac
}
zevria_curl() {
    curl -q --fail --silent --show-error --location --proto '=https' --proto-redir '=https' \
        --connect-timeout 15 --max-time 180 --retry 2 --retry-max-time 400 --max-redirs 5 "$@"
}
zevria_latest() {
    zevria_curl --head --output /dev/null --write-out '%{url_effective}' \
        https://github.com/liukaizheng/zevria/releases/latest
}
zevria_download() { zevria_curl --max-filesize 536870912 --output "$2" "$1"; }
zevria_checksum() {
    local line digest name count=0 expected='' pattern='^([0-9a-fA-F]{64}) [ *]([^ /\\]+)$'
    [[ $(wc -c < "$1") -le 65536 ]] || zevria_die 'SHA256SUMS is too large.'
    while IFS= read -r line || [[ -n $line ]]; do
        [[ $line =~ $pattern ]] || zevria_die 'Malformed SHA256SUMS entry.'
        digest=${BASH_REMATCH[1]}; name=${BASH_REMATCH[2]}
        if [[ $name == "$2" ]]; then expected=$digest; count=$((count + 1)); fi
    done < "$1"
    [[ $count == 1 ]] || zevria_die 'SHA256SUMS must contain exactly one entry for the selected archive.'
    if command -v sha256sum >/dev/null 2>&1; then
        digest=$(sha256sum < "$3") || zevria_die 'Cannot hash archive.'
    else digest=$(shasum -a 256 < "$3") || zevria_die 'Cannot hash archive.'; fi
    digest=${digest%% *}
    [[ $(printf '%s' "$digest" | tr 'A-F' 'a-f') == "$(printf '%s' "$expected" | tr 'A-F' 'a-f')" ]] || zevria_die 'Archive checksum mismatch.'
}
zevria_extract() {
    # tar understands workflow-generated PAX headers; check its logical inventory,
    # not raw header count. Both GNU tar and bsdtar identify links by a non-'-' type.
    tar -tzf "$1" > "$2/members" || zevria_die 'Corrupt tar archive.'
    printf 'zevria\n' > "$2/expected"
    cmp -s "$2/members" "$2/expected" || zevria_die 'Archive must contain only root-level zevria.'
    tar -tvzf "$1" > "$2/types" || zevria_die 'Cannot inspect tar types.'
    awk 'substr($0,1,1) != "-" {exit 1} END {if (NR != 1) exit 1}' "$2/types" || zevria_die 'Archive member is not one regular file.'
    mkdir "$2/extracted" || zevria_die 'Cannot create extraction staging.'
    # Bound expanded file writes too, not just compressed download size.
    (ulimit -S -f 524288 && tar -xzf "$1" -C "$2/extracted") || zevria_die 'Cannot extract archive (corrupt or oversized file).'
    [[ $(wc -c < "$2/extracted/zevria") -le 536870912 ]] || zevria_die 'Expanded executable is too large.'
    [[ -f $2/extracted/zevria && ! -L $2/extracted/zevria ]] || zevria_die 'Unsafe extracted executable.'
    chmod 755 "$2/extracted/zevria" || zevria_die 'Cannot mark executable.'
}
zevria_check_directory() {
    [[ ! -L $1 && ( ! -e $1 || -d $1 ) ]] || zevria_die "Refusing linked or non-directory destination: $1"
}
zevria_directory() {
    zevria_check_directory "$1"
    mkdir -p "$1" || zevria_die "Cannot create directory: $1"
}
zevria_file_destination() {
    [[ ! -L $1 && ( ! -e $1 || -f $1 ) ]] || zevria_die "Refusing linked or nonregular destination: $1"
}
zevria_rollback() {
    local failed=0
    if [[ $binary_committed == 1 ]]; then
        if [[ -f $bin_stage/previous ]]; then mv -f "$bin_stage/previous" "$bin/zevria" || failed=1
        else rm -f "$bin/zevria" || failed=1; fi
    elif [[ -f $bin_stage/previous ]]; then mv -f "$bin_stage/previous" "$bin/zevria" || failed=1; fi
    if [[ -n $locator_stage && -f $locator_stage/previous ]]; then
        mv -f "$locator_stage/previous" "$locator" || failed=1
    elif [[ $locator_committed == 1 ]]; then rm -f "$locator" || failed=1; fi
    if [[ $failed == 1 ]]; then
        keep_backups=1
        zevria_warn "Recovery failed. Preserve and restore backups in $bin_stage and $locator_stage before rerunning."
    fi
}
zevria_cleanup() {
    local status=$?
    trap - EXIT HUP INT TERM
    if [[ $transaction == 1 ]]; then zevria_rollback; fi
    # Only unique directories created by this invocation are ever removed.
    [[ -z $stage ]] || rm -rf "$stage"
    if [[ $keep_backups == 0 ]]; then
        [[ -z $bin_stage ]] || rm -rf "$bin_stage"
        [[ -z $locator_stage ]] || rm -rf "$locator_stage"
    fi
    exit "$status"
}
zevria_commit() {
    zevria_directory "$root"
    zevria_directory "$bin"
    zevria_file_destination "$bin/zevria"
    bin_stage=$(mktemp -d "$bin/.zevria-install.XXXXXX") || zevria_die 'Cannot stage on destination filesystem.'
    cp "$stage/extracted/zevria" "$bin_stage/new" || zevria_die 'Cannot stage executable.'
    chmod 755 "$bin_stage/new" || zevria_die 'Cannot set executable permissions.'
    if [[ $target == x86_64-unknown-linux-gnu ]]; then
        zevria_directory "$home/.zevria"
        locator=$home/.zevria/install-root
        zevria_file_destination "$locator"
        locator_stage=$(mktemp -d "$home/.zevria/.install-root.XXXXXX") || zevria_die 'Cannot stage Linux discovery metadata.'
        printf '%s\n' "$root" > "$locator_stage/new" || zevria_die 'Cannot write Linux discovery metadata.'
    fi
    transaction=1
    if [[ -e $bin/zevria ]]; then mv "$bin/zevria" "$bin_stage/previous" || zevria_die 'Cannot retain previous executable.'; fi
    binary_committed=1
    mv "$bin_stage/new" "$bin/zevria" || zevria_die 'Executable replacement failed; restoring previous installation.'
    if [[ -n $locator_stage ]]; then
        if [[ -e $locator ]]; then mv "$locator" "$locator_stage/previous" || zevria_die 'Cannot retain previous locator; rolling back.'; fi
        locator_committed=1
        mv "$locator_stage/new" "$locator" || zevria_die 'Cannot publish Linux locator; rolling back.'
    fi
    transaction=0
}
zevria_shell_block() {
    local kind=$1 quoted
    printf '%s\n' '# >>> zevria installer >>>'
    if [[ $kind == fish ]]; then
        quoted=${bin//\\/\\\\}; quoted=${quoted//\'/\\\'}
        printf "begin\n    set -l zevria_bin '%s'\n" "$quoted"
        cat <<'FISH'
    set -l zevria_rest
    for entry in $PATH
        if test "$entry" != "$zevria_bin"
            set -a zevria_rest "$entry"
        end
    end
    set -gx PATH "$zevria_bin" $zevria_rest
end
FISH
    else
        printf '_zevria_bin=%s\n' "$(zevria_quote "$bin")"
        cat <<'POSIX'
_zevria_rest=${PATH-}; _zevria_path=
while :; do
    case $_zevria_rest in
        *:*) _zevria_entry=${_zevria_rest%%:*}; _zevria_rest=${_zevria_rest#*:}; _zevria_last= ;;
        *) _zevria_entry=$_zevria_rest; _zevria_last=1 ;;
    esac
    if [ "$_zevria_entry" != "$_zevria_bin" ]; then _zevria_path="$_zevria_path:$_zevria_entry"; fi
    [ -z "$_zevria_last" ] || break
done
export PATH="$_zevria_bin$_zevria_path"
unset _zevria_bin _zevria_rest _zevria_path _zevria_entry _zevria_last
POSIX
    fi
    printf '%s\n' '# <<< zevria installer <<<'
}
zevria_profile() (
    # Resolve existing symlinks to their intended file. Never replace the link.
    local file=$1 kind=$2 dest=$1 link parent tmp input count=0
    while [[ -L $dest ]]; do
        count=$((count + 1)); [[ $count -le 40 && -e $dest ]] || return 1
        link=$(readlink "$dest") || return 1
        case $link in /*) dest=$link;; *) dest=$(dirname "$dest")/$link;; esac
    done
    [[ ! -e $dest || ( -f $dest && -w $dest ) ]] || return 1
    parent=$(dirname "$dest")
    mkdir -p "$parent" || return 1
    tmp=$(mktemp "$parent/.zevria-profile.XXXXXX") || return 1
    trap 'rm -f "$tmp" "$tmp.block"' EXIT
    input=/dev/null
    if [[ -e $dest ]]; then cp -p "$dest" "$tmp" || return 1; input=$dest; fi
    zevria_shell_block "$kind" > "$tmp.block" || return 1
    # Replace in place, preserving block position and every unrelated line.
    # ENVIRON avoids awk -v interpreting backslashes in literal path data.
    ZEVRIA_PROFILE_BLOCK=$tmp.block awk '
      function block(  line) {
        while ((getline line < ENVIRON["ZEVRIA_PROFILE_BLOCK"]) > 0) print line
        close(ENVIRON["ZEVRIA_PROFILE_BLOCK"])
      }
      $0 == "# >>> zevria installer >>>" {if (inside || seen++) {bad=1; exit 1}; inside=1; block(); next}
      $0 == "# <<< zevria installer <<<" {if (!inside) {bad=1; exit 1}; inside=0; next}
      !inside {print}
      END {if (inside || bad) exit 1; if (!seen) {print ""; block()}}
    ' "$input" > "$tmp" || return 1
    mv -f "$tmp" "$dest" || return 1
    printf 'Updated %s; refresh with: source %s\n' "$file" "$(zevria_quote "$file")"
)
zevria_setup_path() {
    local shell=${SHELL:-} file login directory
    shell=${shell##*/}
    case $shell in
        bash)
            login=$home/.bash_profile
            for file in "$home/.bash_profile" "$home/.bash_login" "$home/.profile"; do
                if [[ -e $file || -L $file ]]; then login=$file; break; fi
            done
            for file in "$home/.bashrc" "$login"; do
                zevria_profile "$file" bash || zevria_warn "Could not safely update $file; use the manual PATH command below."
            done ;;
        zsh)
            directory=${ZDOTDIR:-$home}
            if zevria_path "$directory"; then
                zevria_profile "$directory/.zshrc" zsh || zevria_warn 'Could not safely update .zshrc; use manual PATH setup.'
            else zevria_warn 'ZDOTDIR is not a safe absolute path; use manual PATH setup.'; fi ;;
        fish)
            directory=${XDG_CONFIG_HOME:-$home/.config}
            if zevria_path "$directory"; then
                zevria_profile "$directory/fish/config.fish" fish || zevria_warn 'Could not safely update config.fish; use manual PATH setup.'
            else zevria_warn 'XDG_CONFIG_HOME is not a safe absolute path; use manual PATH setup.'; fi ;;
        *) zevria_warn 'Unknown shell; no profiles changed. Use manual PATH setup.' ;;
    esac
}
zevria_main() (
    set -eu
    set -o pipefail
    export LC_ALL=C
    unset TAR_OPTIONS GZIP
    umask 077
    # Subshell-scoped, not function-local: Bash unwinds locals before EXIT traps.
    version=latest version_seen=0 no_path=0 help=0
    target='' home='' root='' bin=''
    stage='' bin_stage='' locator_stage='' locator='' transaction=0 binary_committed=0 locator_committed=0 keep_backups=0
    for arg in "$@"; do
        case $arg in
            --help) [[ $help == 0 ]] || zevria_die 'Duplicate --help.'; help=1 ;;
            --no-path-update) [[ $no_path == 0 ]] || zevria_die 'Duplicate --no-path-update.'; no_path=1 ;;
            -*) zevria_die "Unknown argument: $arg" ;;
            *) [[ $version_seen == 0 ]] || zevria_die 'Only one version may be specified.'
               version_seen=1
               if [[ $arg != latest ]]; then version=$(zevria_version "$arg") || zevria_die 'Expected a SemVer version, optionally prefixed with v, or latest.'; fi ;;
        esac
    done
    if [[ $help == 1 ]]; then zevria_help; return; fi
    command -v uname >/dev/null 2>&1 || zevria_die 'Required utility missing: uname'
    target=$(zevria_target)
    home=${HOME:-}
    zevria_path "$home" || zevria_die 'Set HOME to a safe absolute user home (no colon or control characters).'
    root=${ZEVRIA_INSTALL-$home/.zevria}
    zevria_path "$root" || zevria_die 'ZEVRIA_INSTALL must be absolute, with no colon or control characters.'
    while [[ $root != / && $root == */ ]]; do root=${root%/}; done
    # Normalize dot segments and existing parent symlinks without evaluating text.
    # The selected root itself and bin must not be links (checked at commit).
    [[ $root != */../* && $root != */.. && $root != */./* && $root != */. ]] || zevria_die 'Use a normalized installation root without . or .. components.'
    [[ ${#root} -le 4095 ]] || zevria_die 'Installation root is too long for bounded Linux discovery metadata.'
    bin=${root%/}/bin
    for arg in curl tar mktemp chmod cp mv rm mkdir dirname readlink awk cmp cat wc tr; do
        command -v "$arg" >/dev/null 2>&1 || zevria_die "Required utility missing: $arg"
    done
    command -v sha256sum >/dev/null 2>&1 || command -v shasum >/dev/null 2>&1 || zevria_die 'Install sha256sum or shasum first.'
    zevria_check_directory "$root"
    zevria_check_directory "$bin"
    zevria_file_destination "$bin/zevria"
    if [[ $target == x86_64-unknown-linux-gnu ]]; then
        zevria_check_directory "$home/.zevria"
        zevria_file_destination "$home/.zevria/install-root"
    fi
    if [[ $version == latest ]]; then
        url=$(zevria_latest) || zevria_die 'Cannot resolve latest stable release (network failure or no published release).'
        case $url in https://github.com/liukaizheng/zevria/releases/tag/v*) tag=${url#https://github.com/liukaizheng/zevria/releases/tag/};; *) zevria_die 'Unexpected latest-release redirect identity.';; esac
        tag=${tag//%2B/+}; tag=${tag//%2b/+}
        version=$(zevria_version "$tag") || zevria_die 'Latest-release redirect has an invalid tag.'
        [[ ${version%%+*} != *-* ]] || zevria_die 'Latest-release redirect did not select a stable release.'
    fi
    tag=v$version
    archive=zevria-$tag-$target.tar.gz
    trap zevria_cleanup EXIT
    trap 'exit 130' INT
    trap 'exit 143' HUP TERM
    stage=$(mktemp -d "${TMPDIR:-/tmp}/zevria-install.XXXXXX") || zevria_die 'Cannot create private staging.'
    url=https://github.com/liukaizheng/zevria/releases/download/$tag
    zevria_download "$url/$archive" "$stage/archive" || zevria_die "Cannot download $archive; release/asset unavailable or network interrupted."
    zevria_download "$url/SHA256SUMS" "$stage/SHA256SUMS" || zevria_die 'Cannot download release SHA256SUMS.'
    zevria_checksum "$stage/SHA256SUMS" "$archive" "$stage/archive"
    zevria_extract "$stage/archive" "$stage"
    "$stage/extracted/zevria" --version > "$stage/version" || zevria_die 'Staged executable --version failed.'
    printf 'zevria %s\n' "$version" > "$stage/expected-version"
    cmp -s "$stage/version" "$stage/expected-version" || zevria_die 'Staged executable version does not exactly match the selected release.'
    zevria_commit
    if [[ $no_path == 0 ]]; then zevria_setup_path; fi
    printf '\nInstalled Zevria %s at %s\n' "$version" "$bin/zevria"
    printf 'Run directly: %s --help\n' "$(zevria_quote "$bin/zevria")"
    if [[ $no_path == 0 ]]; then
        printf 'A piped/child installer cannot change its parent shell. Open a new shell or refresh the profile above.\n'
        if [[ ${SHELL:-} == */fish ]]; then
            printf 'Manual fish setup (paste in the current shell):\n'
            zevria_shell_block fish
        else
            # shellcheck disable=SC2016 # Print a literal expansion for the user's shell.
            printf 'Manual Bash/zsh setup: export PATH=%s:"$PATH"\n' "$(zevria_quote "$bin")"
        fi
    else printf 'PATH was not changed (--no-path-update).\n'; fi
    command -v rtk >/dev/null 2>&1 || zevria_warn 'RTK is required for command tools; install it separately from rtk-ai/rtk (not the unrelated crates.io package).'
    printf 'Provider/model configuration is separate first-run setup. No credentials or configuration were changed.\n'
)

if [[ ${BASH_SOURCE[0]:-} == "$0" || -z ${BASH_SOURCE[0]:-} ]]; then zevria_main "$@"; fi
