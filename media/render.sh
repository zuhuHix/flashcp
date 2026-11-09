#!/usr/bin/env bash
# Renders every README animation from the Remotion compositions and converts them to the
# files the README uses:
#   assets/*.gif   (copy-vs-build, add-no-erase, pull-sweep, two-drives, benchmark, volume-layout;
#                   light + dark)
#   docs/flashcp.mp4 (dark showreel: copy -> add -> pull -> two drives -> benchmark, no audio)
# Intermediate MP4s go to media/out/ (untracked).
set -euo pipefail

cd "$(dirname "$0")"
ROOT="$(cd .. && pwd)"
ASSETS="$ROOT/assets"
DOCS="$ROOT/docs"
OUT="out"
MAX_GIF_BYTES=4000000

mkdir -p "$OUT" "$ASSETS" "$DOCS"

# composition id -> gif file name
declare -A GIF_NAME=(
  [CopyVsBuild-light]=copy-vs-build-light.gif
  [CopyVsBuild-dark]=copy-vs-build-dark.gif
  [AddNoErase-light]=add-no-erase-light.gif
  [AddNoErase-dark]=add-no-erase-dark.gif
  [PullSweep-light]=pull-sweep-light.gif
  [PullSweep-dark]=pull-sweep-dark.gif
  [TwoDrives-light]=two-drives-light.gif
  [TwoDrives-dark]=two-drives-dark.gif
  [BenchmarkChart-light]=benchmark-light.gif
  [BenchmarkChart-dark]=benchmark-dark.gif
  [VolumeLayout-light]=volume-layout-light.gif
  [VolumeLayout-dark]=volume-layout-dark.gif
)
COMPS=(CopyVsBuild-light CopyVsBuild-dark AddNoErase-light AddNoErase-dark PullSweep-light PullSweep-dark TwoDrives-light TwoDrives-dark BenchmarkChart-light BenchmarkChart-dark VolumeLayout-light VolumeLayout-dark)

# 1. Render MP4s
for id in "${COMPS[@]}" Showreel-dark; do
  echo "==> rendering $id"
  npx remotion render src/index.ts "$id" "$OUT/$id.mp4" --overwrite
done

# 2. GIFs: two-pass palette. Fall back to fewer fps, narrower width, fewer colours until < 4 MB.
to_gif() {
  local src="$1" dst="$2"
  local fps=20 width=900 colors=128
  while :; do
    ffmpeg -y -v error -i "$src" -vf "fps=${fps},scale=${width}:-1:flags=lanczos,split[s0][s1];[s0]palettegen=max_colors=${colors}:stats_mode=diff[p];[s1][p]paletteuse=dither=bayer:bayer_scale=4:diff_mode=rectangle" "$dst"
    local bytes
    bytes=$(stat -c %s "$dst")
    echo "    $(basename "$dst"): ${bytes} bytes (fps=${fps}, width=${width}, colors=${colors})"
    if [ "$bytes" -lt "$MAX_GIF_BYTES" ]; then
      return 0
    fi
    if [ "$fps" -gt 15 ]; then fps=15
    elif [ "$width" -gt 800 ]; then width=800
    elif [ "$colors" -gt 64 ]; then colors=64
    else
      echo "    WARNING: $(basename "$dst") still above 4 MB at the lowest settings" >&2
      return 0
    fi
  done
}

for id in "${COMPS[@]}"; do
  echo "==> gif ${GIF_NAME[$id]}"
  to_gif "$OUT/$id.mp4" "$ASSETS/${GIF_NAME[$id]}"
done

# 3. Showreel -> docs/flashcp.mp4
echo "==> docs/flashcp.mp4"
ffmpeg -y -v error -i "$OUT/Showreel-dark.mp4" -an -c:v libx264 -crf 26 -pix_fmt yuv420p -movflags +faststart "$DOCS/flashcp.mp4"
ls -la "$ASSETS" "$DOCS/flashcp.mp4"
