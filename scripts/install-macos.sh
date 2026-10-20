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
curl --fail --location --proto '=https' --tlsv1.2 --retry 3 -o "${DMG_PATH}" "${DMG_URL}"
curl --fail --location --proto '=https' --tlsv1.2 --retry 3 -o "${WORK_DIR}/SHA256SUMS" "${BASE_URL}/SHA256SUMS"
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

SUDO=""
if [ ! -w "/Applications" ]; then
    SUDO="sudo"
    echo "/Applications is not writable; sudo will be used."
fi

if [ -e "${APP_DEST}" ]; then
    echo "Removing existing ${APP_DEST}..."
    ${SUDO} rm -rf "${APP_DEST}"
fi

echo "Installing to ${APP_DEST}..."
${SUDO} ditto "${MOUNT_POINT}/${APP_NAME}" "${APP_DEST}"

echo 'SlintYT downloads and updates its own yt-dlp and Deno in Application Support on first launch.'
echo 'The app is ad-hoc signed, not notarized. macOS may require approval in Privacy & Security.'

echo "Installed. Open with: open \"${APP_DEST}\""
