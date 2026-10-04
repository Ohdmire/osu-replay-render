#!/usr/bin/env bash
# Render one stable results PNG per skin. Run this after changing the
# results screen (src/results.rs, skin ranking lookup).
#
#   scripts/check-stable-results.sh
#   OSU_SKINS=/path/to/Skins scripts/check-stable-results.sh
#   scripts/check-stable-results.sh "Extra Skin" /abs/skin/dir
#
# Output: testset/results-check/<slug>.png  (testset/ is gitignored)
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

map="${MAP:-$root/testset/THE ORAL CIGARETTES - ReI (Nevo) [Rain].osu}"
replay="${REPLAY:-$root/testset/ATRI1024 playing THE ORAL CIGARETTES - ReI (Nevo) [Rain] (2026-08-08_11-56).osr}"
skins_root="${OSU_SKINS:-$HOME/.local/share/osu-wine/osu!/Skins}"
out="$root/testset/results-check"
bin="$root/target/debug/osu_replay_render"

default_skins=(
    "FGSky+Shigetora"
    "-.BTMC.Freedom.Dive"
    "Rafis +HDDT"
    "yui-alpha"
    "- [アロナ] Arona -"
    "Wintherest - v21022023"
)

if [[ ! -f "$map" || ! -f "$replay" ]]; then
    echo "missing map or replay:" >&2
    echo "  $map" >&2
    echo "  $replay" >&2
    exit 1
fi

echo "building osu_replay_render"
cargo build --offline --bin osu_replay_render

mkdir -p "$out"
ok=0
fail=0

slug() {
    # Keep the name readable and filesystem-safe.
    printf '%s' "$1" | tr ' /:\\' '----' | tr -cd '[:alnum:]._+\-' | cut -c1-80
}

render_one() {
    local name="$1"
    local skin="$2"
    local file="$out/$(slug "$name").png"
    local frames
    frames="$(mktemp -d)"
    echo "render $name"
    if "$bin" "$map" "$replay" \
        --skin "$skin" \
        --results-style stable \
        --results-only --results 1 --fps 1 \
        --png-dir "$frames" \
        --pp off
    then
        cp -f "$frames"/frame_000000.png "$file"
        echo "  -> $file"
        ok=$((ok + 1))
    else
        echo "  FAILED $name" >&2
        fail=$((fail + 1))
    fi
    rm -rf "$frames"
}

# No user skin: argon fallback, so a missing ranking-panel still draws.
frames="$(mktemp -d)"
echo "render argon (no skin)"
if "$bin" "$map" "$replay" \
    --results-style stable \
    --results-only --results 1 --fps 1 \
    --png-dir "$frames" \
    --pp off
then
    cp -f "$frames"/frame_000000.png "$out/argon.png"
    echo "  -> $out/argon.png"
    ok=$((ok + 1))
else
    echo "  FAILED argon" >&2
    fail=$((fail + 1))
fi
rm -rf "$frames"

if [[ $# -gt 0 ]]; then
    for arg in "$@"; do
        if [[ -d "$arg" ]]; then
            render_one "$(basename "$arg")" "$arg"
        elif [[ -d "$skins_root/$arg" ]]; then
            render_one "$arg" "$skins_root/$arg"
        else
            echo "skip missing skin: $arg" >&2
            fail=$((fail + 1))
        fi
    done
else
    for name in "${default_skins[@]}"; do
        if [[ -d "$skins_root/$name" ]]; then
            render_one "$name" "$skins_root/$name"
        else
            echo "skip missing skin: $name" >&2
        fi
    done
fi

echo "done: $ok ok, $fail failed, dir $out"
[[ "$ok" -gt 0 && "$fail" -eq 0 ]]
