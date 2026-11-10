#!/usr/bin/env bash
# Renders the README animations from the Remotion compositions and converts them to the files
# the README and the Pages site use:
#   assets/<name>-light.gif, assets/<name>-dark.gif   every scene below
#   docs/media/<name>-<theme>.mp4                     the scenes the site embeds as video
#   docs/flashcp.mp4                                  the dark showreel (no audio)
# Intermediate MP4s go to media/out/ (untracked).
#
#   ./render.sh                         everything
#   ./render.sh DriveMode ClusterSlack  only these scenes (both themes); the showreel is skipped
#   ./render.sh Showreel                only the showreel
set -euo pipefail

cd "$(dirname "$0")"
ROOT="$(cd .. && pwd)"
ASSETS="$ROOT/assets"
DOCS="$ROOT/docs"
OUT="out"
MAX_GIF_BYTES=3000000

# scene (composition id without the theme) -> file name
declare -A NAME=(
  [DriveMode]=drive-mode
  [ClusterSlack]=cluster-slack
  [SyncOnlyChanged]=sync-only-changed
  [BenchmarkChart-usb3]=benchmark-usb3
  [BenchmarkChart-ssd]=benchmark-ssd
  [BenchmarkChart-usb2]=benchmark-usb2
  [CopyVsBuild]=copy-vs-build
  [VolumeLayout]=volume-layout
  [AddNoErase]=add-no-erase
  [PullSweep]=pull-sweep
  [TwoDrives]=two-drives
)
ALL=(DriveMode ClusterSlack SyncOnlyChanged BenchmarkChart-usb3 BenchmarkChart-ssd BenchmarkChart-usb2
  CopyVsBuild VolumeLayout AddNoErase PullSweep TwoDrives)
# scenes the Pages site plays as video (sharper and smaller than a GIF)
SITE_VIDEO=" DriveMode ClusterSlack SyncOnlyChanged "

SCENES=()
REEL=1
if [ "$#" -gt 0 ]; then
  REEL=0
  for arg in "$@"; do
    if [ "$arg" = Showreel ]; then
      REEL=1
    elif [ -n "${NAME[$arg]:-}" ]; then
      SCENES+=("$arg")
    else
      echo "unknown scene: $arg (known: ${ALL[*]} Showreel)" >&2
      exit 2
    fi
  done
else
  SCENES=("${ALL[@]}")
fi

mkdir -p "$OUT" "$ASSETS" "$DOCS/media"

# GIF: two-pass palette. Falls back to fewer fps, narrower width, fewer colours until < 3 MB.
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
      echo "    WARNING: $(basename "$dst") still above 3 MB at the lowest settings" >&2
      return 0
    fi
  done
}

to_mp4() {
  ffmpeg -y -v error -i "$1" -an -c:v libx264 -crf 26 -pix_fmt yuv420p -movflags +faststart "$2"
}

for scene in ${SCENES[@]+"${SCENES[@]}"}; do
  for theme in light dark; do
    id="$scene-$theme"
    file="${NAME[$scene]}-$theme"
    echo "==> $id"
    npx remotion render src/index.ts "$id" "$OUT/$id.mp4" --overwrite --log=error
    to_gif "$OUT/$id.mp4" "$ASSETS/$file.gif"
    if [[ "$SITE_VIDEO" == *" $scene "* ]]; then
      to_mp4 "$OUT/$id.mp4" "$DOCS/media/$file.mp4"
    fi
  done
done

if [ "$REEL" = 1 ]; then
  echo "==> docs/flashcp.mp4"
  npx remotion render src/index.ts Showreel-dark "$OUT/Showreel-dark.mp4" --overwrite --log=error
  to_mp4 "$OUT/Showreel-dark.mp4" "$DOCS/flashcp.mp4"
fi

ls -la "$ASSETS" "$DOCS/media" "$DOCS/flashcp.mp4"
