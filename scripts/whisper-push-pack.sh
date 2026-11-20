#!/usr/bin/env bash
# Push a Whisper pack to the Scribe app over adb, instead of downloading it on the phone.
#
# A pack is the encoder.bin + decoder.bin of a Qualcomm AI Hub qnn_context_binary release zip,
# plus the WHISPER marker that says both arrived. Model data needs no exec bit, so it rides on
# the app's own /sdcard directory, which adb can write and the app reads with no permission.
#
# Usage:
#   scripts/whisper-push-pack.sh [--tcp HOST:PORT] [--serial SERIAL] [--pkg PKG] <name> <dir-with-the-two-bins>
#   scripts/whisper-push-pack.sh whisper-large-v3-turbo ~/Documents/Ai/whisper/packs-src/whisper_large_v3_turbo/*/
#
# <name> is the pack's directory name on the phone; the catalog's are whisper-large-v3-turbo,
# whisper-small and whisper-base. Any other name shows up in Settings as a hand-installed pack.
set -euo pipefail

PKG="com.kingsofalchemy.scribe"
TCP=""
SERIAL=""
ARGS=()
while [ $# -gt 0 ]; do
  case "$1" in
    --tcp) TCP="$2"; shift 2 ;;
    --serial|-s) SERIAL="$2"; shift 2 ;;
    --pkg) PKG="$2"; shift 2 ;;
    -h|--help) sed -n '2,14p' "$0"; exit 0 ;;
    *) ARGS+=("$1"); shift ;;
  esac
done
[ "${#ARGS[@]}" -eq 2 ] || { echo "usage: whisper-push-pack.sh [--tcp HOST:PORT] <name> <dir>" >&2; exit 1; }
NAME="${ARGS[0]}"
SRC="${ARGS[1]%/}"
for f in encoder.bin decoder.bin; do
  [ -f "$SRC/$f" ] || { echo "missing $SRC/$f" >&2; exit 1; }
done

HERE="$(cd "$(dirname "$0")" && pwd)"
SDK="${ANDROID_HOME:-$HOME/Android/Sdk}"
ADB="$SDK/platform-tools/adb"; [ -x "$ADB" ] || ADB="adb"
# shellcheck source=adb-target.sh
source "$HERE/adb-target.sh"
adb_resolve_target

DEST="/sdcard/Android/data/$PKG/files/whisper/$NAME"
# The marker goes last: until it exists the app treats the directory as an unfinished install.
"${ADBS[@]}" shell "rm -f '$DEST/WHISPER'; mkdir -p '$DEST'"
for f in encoder.bin decoder.bin; do
  echo "push $f ($(du -h "$SRC/$f" | cut -f1)) -> $DEST/"
  "${ADBS[@]}" push "$SRC/$f" "$DEST/$f"
done
"${ADBS[@]}" shell "touch '$DEST/WHISPER'"
echo "pack on device: $DEST"
"${ADBS[@]}" shell ls -l "$DEST"
