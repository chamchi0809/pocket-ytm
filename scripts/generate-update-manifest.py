#!/usr/bin/env python3
"""Generate the canonical, signed-update manifest consumed by Pocket Music."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
from urllib.parse import quote


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repository", required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--asset", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()

    asset = args.asset.resolve(strict=True)
    digest = hashlib.sha256()
    with asset.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)

    tag = f"v{args.version}"
    asset_url = (
        f"https://github.com/{args.repository}/releases/download/"
        f"{quote(tag, safe='')}/{quote(asset.name, safe='')}"
    )
    manifest = {
        "schemaVersion": 1,
        "version": args.version,
        "repository": args.repository,
        "platforms": {
            "macos-universal": {
                "url": asset_url,
                "sha256": digest.hexdigest(),
                "size": asset.stat().st_size,
            }
        },
    }
    args.output.write_text(
        json.dumps(manifest, ensure_ascii=False, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )


if __name__ == "__main__":
    main()
