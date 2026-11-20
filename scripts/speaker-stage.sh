#!/usr/bin/env bash
# Stage the speaker-embedding pack for APK bundling.
# Copies model.bin from a pack made by scripts/speaker-export/export.py into
# <app-dir>/assets/speaker/, the folder cargo-apk2 bundles when
# `[package.metadata.android] assets = "assets"` is set. The app unpacks it on first run
# (examples/scribe-android/src/platform.rs, `speaker_pack`).
#
# The binary is compiled for one chip by a proprietary toolchain and is not committed; the
# destination folder gets a .gitignore that excludes everything. An app built without it
# runs, with telling speakers apart switched off.
#
# Usage:
#   SPEAKER_PACK=~/Documents/Ai/speaker/pack scripts/speaker-stage.sh <app-dir>
set -euo pipefail

APP="${1:?usage: speaker-stage.sh <app-dir>  (the crate whose APK bundles the pack)}"
PACK="${SPEAKER_PACK:-$HOME/Documents/Ai/speaker/pack}"
[ -f "$APP/Cargo.toml" ] || { echo "no Cargo.toml in $APP" >&2; exit 1; }
[ -f "$PACK/SPEAKER" ] && [ -f "$PACK/model.bin" ] || { echo "no speaker pack at $PACK — run scripts/speaker-export/export.py" >&2; exit 1; }
DEST="$(cd "$APP" && pwd)/assets"

mkdir -p "$DEST/speaker"
printf '*\n!.gitignore\n' > "$DEST/.gitignore"
install -m 0644 "$PACK/model.bin" "$DEST/speaker/model.bin"
echo "staged $(du -h "$DEST/speaker/model.bin" | cut -f1) -> $DEST/speaker/model.bin"
