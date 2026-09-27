#!/usr/bin/env bash
# Offline fixture transport only. Source AFTER the production installer.
zevria_latest() {
    printf 'latest\n' >> "$FIXTURE_CALLS"
    printf '%s' "${FIXTURE_LATEST:-https://github.com/liukaizheng/zevria/releases/tag/v$FIXTURE_VERSION}"
}
zevria_download() {
    printf '%s\n' "$1" >> "$FIXTURE_CALLS"
    case $1 in
        "https://github.com/liukaizheng/zevria/releases/download/v$FIXTURE_VERSION/$FIXTURE_ASSET") cp "$FIXTURE_ARCHIVE" "$2" ;;
        "https://github.com/liukaizheng/zevria/releases/download/v$FIXTURE_VERSION/SHA256SUMS") cp "$FIXTURE_MANIFEST" "$2" ;;
        *) printf 'Unexpected fixture URL: %s\n' "$1" >&2; return 1 ;;
    esac
}
