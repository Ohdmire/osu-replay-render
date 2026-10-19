#!/usr/bin/env bash
# Validate the GPU NV12 output against the BGRA reference:
#   - dimensions/pix_fmt
#   - no near-black frames
#   - PSNR / SSIM high enough (same scene, two input formats)
# Usage: scripts/check-nv12.sh <map.osu> <replay.osr> [size] [frames] [encoder]
set -euo pipefail

MAP=${1:?map.osu}
REPLAY=${2:?replay.osr}
SIZE=${3:-1920x1080}
FRAMES=${4:-60}
ENCODER=${5:-nvenc}
BIN=${BIN:-./target/release/osu_replay_render}
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

echo "== $SIZE / $FRAMES frames / $ENCODER =="
"$BIN" "$MAP" "$REPLAY" --out "$TMP/nv12.mp4" --size "$SIZE" --fps 60 \
  --start 30000 --limit "$FRAMES" --results off --encoder "$ENCODER" >/dev/null 2>&1
"$BIN" "$MAP" "$REPLAY" --out "$TMP/bgra.mp4" --size "$SIZE" --fps 60 \
  --start 30000 --limit "$FRAMES" --results off --encoder "$ENCODER" --no-gpu-yuv >/dev/null 2>&1

DIM=$(ffprobe -v error -select_streams v:0 -show_entries stream=width,height,pix_fmt -of csv=p=0 "$TMP/nv12.mp4")
echo "nv12 stream: $DIM"

PSNR=$(ffmpeg -hide_banner -i "$TMP/nv12.mp4" -i "$TMP/bgra.mp4" \
  -lavfi '[0:v][1:v]psnr' -f null - 2>&1 | grep -o 'average:[0-9.]*' | tail -1 | cut -d: -f2)
SSIM=$(ffmpeg -hide_banner -i "$TMP/nv12.mp4" -i "$TMP/bgra.mp4" \
  -lavfi '[0:v][1:v]ssim' -f null - 2>&1 | grep -o 'All:[0-9.]*' | tail -1 | cut -d: -f2)
BLACK=$(ffmpeg -hide_banner -v info -i "$TMP/nv12.mp4" \
  -vf 'blackframe=amount=98:threshold=32' -f null - 2>&1 | grep -c 'blackframe' || true)
YAVG=$(ffmpeg -hide_banner -v error -i "$TMP/nv12.mp4" \
  -vf 'signalstats,metadata=print:key=lavfi.signalstats.YAVG:file=-' -f null - 2>&1 \
  | grep -o 'YAVG=[0-9.]*' | head -1 | cut -d= -f2)

echo "PSNR=${PSNR} dB  SSIM=${SSIM}  YAVG=${YAVG}  near_black_frames=${BLACK}"

awk -v p="$PSNR" -v s="$SSIM" -v y="$YAVG" -v b="$BLACK" 'BEGIN {
  if (p < 30.0) { print "FAIL: PSNR < 30 dB"; exit 1 }
  if (s < 0.95) { print "FAIL: SSIM < 0.95"; exit 1 }
  if (y < 20.0) { print "FAIL: mean luma < 20 (black frame?)"; exit 1 }
  if (b > 0)   { print "FAIL: near-black frame detected"; exit 1 }
  print "PASS"
}'
