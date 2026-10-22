#!/usr/bin/env python3
"""Refresh explicitly named app snapshots from the canonical Mastertech theme crate."""
import argparse
import hashlib
from pathlib import Path
import shutil

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("destinations", nargs="+", type=Path)
args = parser.parse_args()
source = Path(__file__).resolve().parents[1] / "crates/egui-mobile-theme"
files = [Path("Cargo.toml"), Path("README.md"), Path("src/lib.rs")]
for destination in args.destinations:
    for obsolete in ["src/config.rs", "src/mastertech_color_scheme.json"]:
        (destination / obsolete).unlink(missing_ok=True)
    for relative in files:
        target = destination / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source / relative, target)
    digest = hashlib.sha256(b"".join((destination / f).read_bytes() for f in files)).hexdigest()
    print(f"{destination}: {digest}")
