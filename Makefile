BINARY       ?= slintyt
PREFIX       ?= /usr/local
BINDIR       ?= $(PREFIX)/bin
DATADIR      ?= $(PREFIX)/share
APPDIR       ?= $(DATADIR)/applications
ICONDIR      ?= $(DATADIR)/icons/hicolor/256x256/apps
SVGICONDIR   ?= $(DATADIR)/icons/hicolor/scalable/apps
LICENSEDIR   ?= $(DATADIR)/licenses/$(BINARY)
CARGO        ?= cargo
REPO_ROOT    := $(dir $(abspath $(lastword $(MAKEFILE_LIST))))
DESKTOP_FILE := $(REPO_ROOT)scripts/installer/$(BINARY).desktop
ICON_FILE    := $(REPO_ROOT)ui/assets/appicon.png
SVG_ICON_FILE := $(REPO_ROOT)ui/assets/appicon.svg
LICENSE_FILE := $(REPO_ROOT)LICENSE
INSTALL_BINARY ?= $(REPO_ROOT)target/release/$(BINARY)

# Detect fast linker flags
ifeq ($(shell command -v clang >/dev/null 2>&1 && command -v mold >/dev/null 2>&1 && echo 1),1)
  LINKER_FLAGS := -C linker=clang -C link-arg=-fuse-ld=mold
else ifeq ($(shell command -v clang >/dev/null 2>&1 && command -v lld >/dev/null 2>&1 && echo 1),1)
  LINKER_FLAGS := -C linker=clang -C link-arg=-fuse-ld=lld
else
  LINKER_FLAGS :=
endif

DEV_RUSTFLAGS     := $(strip $(RUSTFLAGS) $(LINKER_FLAGS))
RELEASE_RUSTFLAGS := $(strip $(RUSTFLAGS) $(LINKER_FLAGS))

.PHONY: dev build release run install uninstall clean

dev:
	cd "$(REPO_ROOT)" && CARGO_INCREMENTAL=1 RUSTFLAGS="$(DEV_RUSTFLAGS)" $(CARGO) run --profile dev-fast --locked

build: release

release:
	cd "$(REPO_ROOT)" && RUSTFLAGS="$(RELEASE_RUSTFLAGS)" $(CARGO) build --release --locked

run: release
	cd "$(REPO_ROOT)" && exec ./target/release/$(BINARY)

install:
	@test -x "$(INSTALL_BINARY)" || { echo "Build first with make build, or set INSTALL_BINARY to an existing development binary." >&2; exit 1; }
	@test -f "$(DESKTOP_FILE)" && test -f "$(ICON_FILE)" && test -f "$(SVG_ICON_FILE)"
	install -Dm755 "$(INSTALL_BINARY)" "$(DESTDIR)$(BINDIR)/$(BINARY)"
	if [ -f "$(LICENSE_FILE)" ]; then install -Dm644 "$(LICENSE_FILE)" "$(DESTDIR)$(LICENSEDIR)/LICENSE"; fi
	install -Dm644 "$(DESKTOP_FILE)" "$(DESTDIR)$(APPDIR)/$(BINARY).desktop"
	install -Dm644 "$(ICON_FILE)" "$(DESTDIR)$(ICONDIR)/$(BINARY).png"
	install -Dm644 "$(SVG_ICON_FILE)" "$(DESTDIR)$(SVGICONDIR)/$(BINARY).svg"
	if [ -z "$(DESTDIR)" ] && [ -d "$(APPDIR)" ] && command -v update-desktop-database >/dev/null 2>&1; then update-desktop-database "$(APPDIR)"; fi
	if [ -z "$(DESTDIR)" ] && [ -d "$(DATADIR)/icons/hicolor" ] && command -v gtk-update-icon-cache >/dev/null 2>&1; then gtk-update-icon-cache -q -t "$(DATADIR)/icons/hicolor"; fi

uninstall:
	rm -f "$(DESTDIR)$(BINDIR)/$(BINARY)"
	rm -f "$(DESTDIR)$(LICENSEDIR)/LICENSE"
	rm -f "$(DESTDIR)$(APPDIR)/$(BINARY).desktop"
	rm -f "$(DESTDIR)$(ICONDIR)/$(BINARY).png"
	rm -f "$(DESTDIR)$(SVGICONDIR)/$(BINARY).svg"
	rmdir "$(DESTDIR)$(LICENSEDIR)" 2>/dev/null || true
	if [ -z "$(DESTDIR)" ] && [ -d "$(APPDIR)" ] && command -v update-desktop-database >/dev/null 2>&1; then update-desktop-database "$(APPDIR)"; fi
	if [ -z "$(DESTDIR)" ] && [ -d "$(DATADIR)/icons/hicolor" ] && command -v gtk-update-icon-cache >/dev/null 2>&1; then gtk-update-icon-cache -q -t "$(DATADIR)/icons/hicolor"; fi

clean:
	cd "$(REPO_ROOT)" && $(CARGO) clean
