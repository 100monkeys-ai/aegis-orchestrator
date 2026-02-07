#!/usr/bin/env python3
from __future__ import annotations

from pathlib import Path

ROOT = Path(__file__).resolve().parent
HEADER = (
    "// Copyright (c) 2026 100monkeys.ai\n"
    "// SPDX-License-Identifier: AGPL-3.0\n\n"
)
SPDX_MARKER = "SPDX-License-Identifier: AGPL-3.0"
SKIP_DIRS = {"target", ".git", ".github", ".idea", ".vscode"}


def should_skip(path: Path) -> bool:
    return any(part in SKIP_DIRS for part in path.parts)


def add_header(file_path: Path) -> bool:
    content = file_path.read_text(encoding="utf-8")
    if SPDX_MARKER in content:
        return False

    file_path.write_text(HEADER + content, encoding="utf-8")
    return True


def main() -> int:
    updated = 0
    scanned = 0

    for file_path in ROOT.rglob("*.rs"):
        if should_skip(file_path):
            continue
        scanned += 1
        if add_header(file_path):
            updated += 1

    print(f"Scanned {scanned} Rust files. Updated {updated} files.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
