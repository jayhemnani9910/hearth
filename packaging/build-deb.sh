#!/bin/sh
# Builds dist/hearth_<version>_<arch>.deb from a release build.
# Usage: packaging/build-deb.sh [VERSION]   (default: the workspace version)
set -eu
cd "$(dirname "$0")/.."

version=${1:-$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)}
arch=$(dpkg --print-architecture)
maintainer=${DEB_MAINTAINER:-"$(git config user.name) <$(git config user.email)>"}
cargo build --release --locked

root=dist/hearth_${version}_${arch}
rm -rf "$root"
install -Dm755 target/release/hearthd "$root/usr/bin/hearthd"
install -Dm755 target/release/hearth "$root/usr/bin/hearth"
install -Dm755 target/release/hearth-app "$root/usr/bin/hearth-app"
install -Dm644 packaging/hearth.service "$root/usr/lib/systemd/user/hearth.service"
install -Dm644 packaging/dev.hearth.Hearth.desktop "$root/usr/share/applications/dev.hearth.Hearth.desktop"
install -Dm644 packaging/dev.hearth.Hearth.svg "$root/usr/share/icons/hicolor/scalable/apps/dev.hearth.Hearth.svg"
doc="$root/usr/share/doc/hearth"
install -Dm644 README.md "$doc/README.md"
install -Dm644 docs/PROTOCOL.md "$doc/PROTOCOL.md"
cat > "$doc/copyright" <<COPY
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: hearth

Files: *
Copyright: Hearth contributors
License: MIT
$(sed 's/^$/./; s/^/ /' LICENSE)

Files: usr/bin/*
Comment: The binaries also contain third-party code; see THIRD-PARTY-LICENSES.
COPY
python3 packaging/third_party.py "$doc/THIRD-PARTY-LICENSES"
gzip -9n "$doc/THIRD-PARTY-LICENSES"
chmod 644 "$doc/copyright" "$doc/THIRD-PARTY-LICENSES.gz"

# Library dependencies, from the binaries themselves.
mkdir -p "$root/debian"
printf 'Source: hearth\n\nPackage: hearth\nArchitecture: any\n' > "$root/debian/control"
deps=$(cd "$root" && dpkg-shlibdeps -O usr/bin/hearthd usr/bin/hearth usr/bin/hearth-app 2>/dev/null | sed -n 's/^shlibs:Depends=//p')
rm -rf "$root/debian"
[ -n "$deps" ] || { echo "dpkg-shlibdeps found no dependencies" >&2; exit 1; }

size=$(du -sk --exclude=DEBIAN "$root" | cut -f1)
mkdir -p "$root/DEBIAN"
cat > "$root/DEBIAN/control" <<CTRL
Package: hearth
Version: $version
Architecture: $arch
Maintainer: $maintainer
Installed-Size: $size
Depends: $deps
Recommends: openssh-client
Suggests: tailscale
Section: utils
Priority: optional
Description: terminal workspace whose programs outlive the window
 Hearth runs a per-user server that owns your shells. The GTK desktop app
 and the hearth command-line tool attach to it locally, over SSH or over a
 tailnet; closing a window or losing the network does not stop programs.
CTRL
cat > "$root/DEBIAN/postinst" <<'POST'
#!/bin/sh
set -e
if [ "$1" = configure ] && [ -n "$2" ]; then
    echo "hearth: running servers keep the old version until restarted:"
    echo "        systemctl --user restart hearth   (programs restart; layout is kept)"
fi
POST
chmod 755 "$root/DEBIAN/postinst"
dpkg-deb --root-owner-group --build "$root" "dist/hearth_${version}_${arch}.deb"
rm -rf "$root"
echo "dist/hearth_${version}_${arch}.deb"
