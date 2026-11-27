#!/usr/bin/env bash
# Regenerates the committed installer PNGs from art.html with headless Chrome (run in Git Bash).
# Scales 100-300 mirror the Windows display scaling steps.
set -e
cd "$(dirname "$0")"
HERE=$(cygpath -m "$PWD")
BROWSER="${CHROME:-/c/Program Files/Google/Chrome/Application/chrome.exe}"

# render <element id> <width> <height> <percent> <png name>
render() {
  local zoom
  zoom=$(awk -v p="$4" 'BEGIN { print p / 100 }')
  "$BROWSER" --headless=new --disable-gpu --hide-scrollbars \
    --default-background-color=00000000 --allow-file-access-from-files \
    --virtual-time-budget=3000 --force-device-scale-factor="$zoom" \
    --window-size="$2,$3" --screenshot="$HERE/$5" "file:///$HERE/art.html#$1" >/dev/null 2>&1
}

for pct in 100 125 150 175 200 250 300; do
  render side 164 314 "$pct" "side-$pct.png"
  render small 55 55 "$pct" "small-$pct.png"
done

for pct in 100 150 200 300; do
  for button in support kofi topup; do
    for lang in en id; do
      render "$button-$lang" 440 58 "$pct" "$button-$lang-$pct.png"
    done
  done
done
