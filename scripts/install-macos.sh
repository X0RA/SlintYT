#!/usr/bin/env bash
set -euo pipefail

# Override SLINTYT_REPO to install from a fork.
REPO="${SLINTYT_REPO:-X0RA/SlintYT}"
VERSION="${SLINTYT_VERSION:-latest}"
DMG_NAME="slintyt-macos-arm64.dmg"
APP_NAME="SlintYT.app"
APP_DEST="/Applications/${APP_NAME}"
if [ "$VERSION" = latest ]; then
    BASE_URL="https://github.com/${REPO}/releases/latest/download"
else
    BASE_URL="https://github.com/${REPO}/releases/download/${VERSION}"
fi
DMG_URL="${BASE_URL}/${DMG_NAME}"

if [ "$(uname -s)" != "Darwin" ]; then
    echo "install-macos.sh: this installer only runs on macOS." >&2
    exit 1
fi

if [ "$(uname -m)" != "arm64" ]; then
    echo "install-macos.sh: only Apple Silicon (arm64) builds are published." >&2
    echo "  detected arch: $(uname -m)" >&2
    exit 1
fi

WORK_DIR="$(mktemp -d -t slintyt-install.XXXXXX)"
MOUNT_POINT=""

cleanup() {
    if [ -n "${MOUNT_POINT}" ] && [ -d "${MOUNT_POINT}" ]; then
        hdiutil detach "${MOUNT_POINT}" -quiet >/dev/null 2>&1 || true
    fi
    rm -rf "${WORK_DIR}"
}
trap cleanup EXIT

DMG_PATH="${WORK_DIR}/${DMG_NAME}"
echo "Downloading ${DMG_NAME}..."
curl --fail --location --proto '=https' --tlsv1.2 --connect-timeout 15 --max-time 180 --retry 3 --retry-max-time 300 -o "${DMG_PATH}" "${DMG_URL}"
curl --fail --location --proto '=https' --tlsv1.2 --connect-timeout 15 --max-time 180 --retry 3 --retry-max-time 300 -o "${WORK_DIR}/SHA256SUMS" "${BASE_URL}/SHA256SUMS"
awk -v file="$DMG_NAME" '$2 == file { print }' "$WORK_DIR/SHA256SUMS" > "$WORK_DIR/checksum"
test -s "$WORK_DIR/checksum" || { echo 'Missing DMG checksum' >&2; exit 1; }
(cd "$WORK_DIR" && shasum -a 256 --check checksum)

echo "Mounting DMG..."
MOUNT_POINT="${WORK_DIR}/mount"
mkdir -p "$MOUNT_POINT"
hdiutil attach "$DMG_PATH" -mountpoint "$MOUNT_POINT" -nobrowse -readonly -quiet

if [ -z "${MOUNT_POINT}" ] || [ ! -d "${MOUNT_POINT}/${APP_NAME}" ]; then
    echo "install-macos.sh: failed to locate ${APP_NAME} inside the DMG." >&2
    exit 1
fi

# Quote one value for /bin/sh.
shell_quote() {
    printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"
}

# Copy the entire new bundle before moving the old one. Keep the backup until
# the final rename succeeds, including when the helper receives a signal.
replace_app() (
    set -euo pipefail
    local source="$1" destination="$2" parent stage="" committed=0
    parent="$(dirname "$destination")"
    local lock="$parent/.SlintYT-install-lock"
    mkdir "$lock" || { echo "Another install is active. If none is, remove $lock and try again." >&2; exit 1; }
    cleanup_install() {
        local status=$?
        trap - EXIT HUP INT TERM
        if [[ -n "$stage" && -d "$stage/previous.app" && "$committed" == 0 ]]; then
            if ! { rm -rf "$destination" && mv "$stage/previous.app" "$destination"; }; then
                echo "Rollback failed. Move $stage/previous.app back to $destination, then remove $lock." >&2
                exit 1
            fi
        fi
        # A leftover backup is harmless, but a leftover lock blocks every later install.
        if [[ -n "$stage" ]]; then rm -rf "$stage" || echo "Warning: could not remove $stage" >&2; fi
        rmdir "$lock" || echo "Warning: could not remove $lock. Remove it before installing again." >&2
        exit "$status"
    }
    trap cleanup_install EXIT
    trap 'exit 1' HUP INT TERM
    stage="$(mktemp -d "$parent/.SlintYT-update.XXXXXX")"
    ditto "$source" "$stage/new.app"
    [[ -x "$stage/new.app/Contents/MacOS/slintyt" ]] || { echo 'The staged app is missing its executable.' >&2; exit 1; }
    if [[ -e "$destination" ]]; then mv "$destination" "$stage/previous.app"; fi
    mv "$stage/new.app" "$destination"
    committed=1
)

# The same transaction runs with or without elevation. Pass paths as arguments.
REPLACE="/bin/bash -c $(shell_quote "set -euo pipefail; $(declare -f replace_app); replace_app \"\$1\" \"\$2\"") bash $(shell_quote "${MOUNT_POINT}/${APP_NAME}") $(shell_quote "$APP_DEST")"

# Until this point SlintYT's in-app updater may stop the installer, which is
# only downloading. It stops sending signals once it reads the "Installing to"
# line, so ignore them first: from here the app may be replaced.
if [ "${SLINTYT_ELEVATE:-}" = gui ]; then
    trap '' HUP INT TERM
fi
echo "Installing to ${APP_DEST}..."

if [ -w "/Applications" ] && { [ ! -e "$APP_DEST" ] || [ -w "$APP_DEST" ]; }; then
    /bin/sh -c "$REPLACE"
elif [ "${SLINTYT_ELEVATE:-}" = gui ]; then
    # SlintYT's in-app updater has no terminal for sudo, so use the standard
    # macOS administrator prompt. The command is passed as an argument, never
    # spliced into AppleScript source.
    echo "Administrator access is needed to write /Applications."
    if ! OUTPUT="$(osascript \
        -e 'on run argv' \
        -e 'do shell script (item 1 of argv) with administrator privileges' \
        -e 'end run' \
        "$REPLACE" 2>&1)"; then
        case "$OUTPUT" in
            *-128*) echo 'Permission was not granted.' >&2; exit 126 ;;
            *) echo "$OUTPUT" >&2; exit 1 ;;
        esac
    fi
else
    echo "Administrator access is needed to write /Applications; sudo will be used."
    sudo /bin/sh -c "$REPLACE"
fi

echo 'SlintYT downloads and updates its own yt-dlp and Deno in Application Support on first launch.'
echo 'The app is ad-hoc signed, not notarized. macOS may require approval in Privacy & Security.'

echo "Installed. Open with: open \"${APP_DEST}\""
