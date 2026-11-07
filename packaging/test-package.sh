#!/bin/sh
# Tests a built package without installing it (no root needed):
#   packaging/test-package.sh dist/hearth_X_amd64.deb
# Contents, metadata and binaries are checked, then an upgrade is simulated
# the way dpkg performs it (new files renamed over the old ones) while a
# server from the package is running.
set -u
deb=${1:?usage: test-package.sh DEB}
work=$(mktemp -d)
run=$(mktemp -d -p "${XDG_RUNTIME_DIR:-/tmp}" hpkg.XXXXXX)
server=""
cleanup() {
    [ -n "$server" ] && kill "$server" 2>/dev/null
    rm -rf "$work" "$run"
}
trap cleanup EXIT
fail=0
check() {
    printf '%-58s' "$1"
    shift
    if "$@" >"$work/out" 2>&1; then echo ok; else echo FAIL; sed 's/^/    /' "$work/out"; fail=1; fi
}

dpkg-deb -x "$deb" "$work/root" && dpkg-deb -e "$deb" "$work/ctl" || exit 1
bin=$work/root/usr/bin

check "binaries start" sh -c "'$bin/hearthd' --version && '$bin/hearth' version && '$bin/hearth-app' --version"
check "every shared library resolves" sh -c "! ldd '$bin/hearthd' '$bin/hearth' '$bin/hearth-app' | grep 'not found'"
check "Depends covers GTK, libadwaita and VTE" sh -c "grep '^Depends:' '$work/ctl/control' | grep libgtk-4-1 | grep libadwaita-1-0 | grep -q libvte-2.91-gtk4-0"
check "desktop entry is valid" desktop-file-validate "$work/root/usr/share/applications/dev.hearth.Hearth.desktop"
check "desktop entry icon is shipped" test -s "$work/root/usr/share/icons/hicolor/scalable/apps/dev.hearth.Hearth.svg"
sed "s|/usr/bin/hearthd|$bin/hearthd|" "$work/root/usr/lib/systemd/user/hearth.service" >"$work/hearth.service"
check "systemd user unit is valid" systemd-analyze --user verify "$work/hearth.service"
check "license notices are shipped" sh -c "test -s '$work/root/usr/share/doc/hearth/copyright' && zgrep -q '^alacritty_terminal' '$work/root/usr/share/doc/hearth/THIRD-PARTY-LICENSES.gz' && zgrep -q '^Lua 5.4' '$work/root/usr/share/doc/hearth/THIRD-PARTY-LICENSES.gz'"
check "no file is group- or world-writable" sh -c "! find '$work/root' -perm /022 -type f | grep ."

# A server from the package, isolated from the user's own.
export HEARTH_RUNTIME_DIR="$run" HEARTH_SOCKET="$run/s.sock" HEARTH_STATE_DIR="$work/state" HEARTH_CONFIG_DIR="$work/cfg"
"$bin/hearthd" >"$work/hearthd.log" 2>&1 &
server=$!
i=0
until [ -S "$run/s.sock" ] || [ $i -gt 100 ]; do sleep 0.05; i=$((i + 1)); done
h() { "$bin/hearth" "$@"; }
check "packaged server and CLI work together" sh -c "'$bin/hearth' new -n pkg -c '$work' -- sleep 600 && '$bin/hearth' ls | grep -q sleep"
pid_before=$(h --json info -t b2 | sed -n 's/.*"pid": *\([0-9]*\).*/\1/p' | head -1)

# Upgrade: dpkg writes each new file next to the old one and renames it over.
for b in hearthd hearth hearth-app; do
    cp "$bin/$b" "$bin/$b.dpkg-new" && mv "$bin/$b.dpkg-new" "$bin/$b"
done
check "running server survives the file replacement" kill -0 "$server"
check "programs keep running across the upgrade" sh -c "'$bin/hearth' --json info -t b2 | grep -q '\"pid\": *$pid_before'"
check "new CLI talks to the old server" sh -c "'$bin/hearth' server status | grep -q pid"

# Restarting onto the new version: the layout returns with new processes.
check "server stops on request" sh -c "'$bin/hearth' server stop --force"
wait "$server" 2>/dev/null
"$bin/hearthd" >>"$work/hearthd.log" 2>&1 &
server=$!
i=0
until [ -S "$run/s.sock" ] || [ $i -gt 100 ]; do sleep 0.05; i=$((i + 1)); done
check "layout is restored by the new server" sh -c "'$bin/hearth' ls | grep -q pkg"
check "restored terminal is a new process" sh -c "! '$bin/hearth' --json ls | grep -q '\"pid\": *$pid_before[,}]'"
h server stop --force >/dev/null 2>&1
wait "$server" 2>/dev/null
server=""

[ $fail -eq 0 ] && echo "all package checks passed" || echo "SOME PACKAGE CHECKS FAILED"
exit $fail
