#!/bin/bash
# Re-records the terminal output corpus used by model_tests.rs.
# Every program runs in a throwaway HOME on generated files, so recordings
# hold no personal data. Size: 80x24, TERM=xterm-256color.
set -euo pipefail
out=$(cd "$(dirname "$0")" && pwd)
home=$(mktemp -d)
trap 'rm -rf "$home"' EXIT
export HOME=$home TERM=xterm-256color LANG=C.UTF-8 LC_ALL=C.UTF-8 PS1='$ '
export TZ=UTC GIT_AUTHOR_DATE='2026-01-01T00:00:00Z' GIT_COMMITTER_DATE='2026-01-01T00:00:00Z'
cd "$home"

# Sample files.
python3 - <<'PY'
lines = []
for i in range(200):
    lines.append(f"def function_{i}(x, y):\n    \"\"\"Docstring {i} with unicode: café 日本 👍\"\"\"\n    return x * {i} + y  # comment {i}\n")
open("sample.py", "w").write("\n".join(lines))
open("notes.txt", "w").write("\n".join(f"line {i}: the quick brown fox jumps over the lazy dog {'x' * (i % 70)}" for i in range(400)))
PY
git init -q repo && cd repo
git config user.name "Sample Author" && git config user.email sample@example.com
for i in $(seq 1 15); do echo "$i" >>f; git add f; git commit -qm "commit $i: change number $i"; [ $((i % 5)) -eq 0 ] && git checkout -qb "branch-$i" && git checkout -q -; done
cd "$home"

# rec NAME DELAY_INPUT... : runs a command in an 80x24 pty, typing the inputs.
rec() {
    local name=$1 cmd=$2
    shift 2
    {
        sleep 1
        for k in "$@"; do printf '%b' "$k"; sleep 0.4; done
        sleep 1
    } | script -q -E always -O "$out/$name.vt" -c "stty cols 80 rows 24; $cmd" >/dev/null
    sed -i '1{/^Script started/d}; ${/^Script done/d}' "$out/$name.vt"
}

rec vim "vim -u NONE -N -c 'syntax on' -c 'set number hlsearch' sample.py" \
    '/unicode\r' 'n' 'n' ':vsplit notes.txt\r' '\x17l' 'G' 'gg' ':help\r' '\x04' ':qa!\r'
rec less "less -R notes.txt" ' ' ' ' '/fox\r' 'n' 'G' 'g' 'q'
rec git-log "git -C repo log --graph --all --decorate --color=always | less -R" ' ' 'q'
rec nano "nano notes.txt" '\x16' '\x16' '\x17fox\r' '\x18'
rec whiptail "whiptail --title 'Pick one' --checklist 'Choose:' 15 50 5 a Apple ON b Banana OFF c Cherry OFF d 'Dragon fruit' OFF e Elderberry ON" '\x1b[B' ' ' '\t' '\r'
rec progress "python3 -c \"
import sys, time
for i in range(101):
    bar = '█' * (i // 2) + '░' * (50 - i // 2)
    sys.stdout.write(f'\\r\\x1b[32m{bar}\\x1b[0m {i:3d}%')
    sys.stdout.flush(); time.sleep(0.01)
print()
for i in range(30):
    print(f'\\x1b[1;3{i % 8}mstep {i}\\x1b[0m', 'emoji 🚀✨', 'wide 漢字テスト', '\\x1b]8;;https://example.com/{i}\\x1b\\\\\\\\link\\x1b]8;;\\x1b\\\\\\\\')
\""
ls -la "$out"/*.vt
