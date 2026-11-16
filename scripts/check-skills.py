#!/usr/bin/env python3
"""Validate the bundled agent skill catalog (`skills/*/SKILL.md`).

Checks, all stdlib only:

  1. every `skills/<name>/` contains a `SKILL.md`;
  2. the frontmatter block opens and closes with `---` and declares
     `name`, `description`, `license`, and a `metadata` mapping;
  3. `name` matches its directory and uses lowercase kebab-case;
  4. `description` is non-empty and within the 1024-character limit;
  5. `license: MIT` is backed by a self-contained MIT `LICENSE` in the skill
     directory, because the installer copies only that folder;
  6. every relative Markdown link in the skill resolves on disk.

Koma ships one installable skill under `skills/koma-desktop/`; the repository
root `LICENSE` stays GPL-3.0-or-later, so the MIT text must live inside the
skill directory where the installer can copy it.

Exit status: 0 on success, 1 on any failure.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SKILLS = ROOT / "skills"
NAME_PATTERN = re.compile(r"^[a-z0-9]+(?:-[a-z0-9]+)*$")
LINK_PATTERN = re.compile(r"\[[^\]]*\]\(([^)\s]+)\)")
METADATA_KEY = re.compile(r"^([A-Za-z0-9_.-]+):\s*(.*)$")

failures: list[str] = []


def fail(message: str) -> None:
    failures.append(message)


def read_frontmatter(path: Path) -> dict[str, object]:
    """Return a shallow mapping of top-level frontmatter keys."""
    lines = path.read_text(encoding="utf-8").splitlines()
    if not lines or lines[0].strip() != "---":
        fail(f"{path.relative_to(ROOT)}: missing opening frontmatter '---'")
        return {}
    try:
        end = lines[1:].index("---") + 1
    except ValueError:
        fail(f"{path.relative_to(ROOT)}: missing closing frontmatter '---'")
        return {}

    keys: dict[str, object] = {}
    current: str | None = None
    nested: dict[str, str] = {}
    block: list[str] | None = None
    for line in lines[1:end]:
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        if line.startswith((" ", "\t")):
            if current == "metadata":
                match = METADATA_KEY.match(line.strip())
                if match:
                    nested[match.group(1)] = match.group(2).strip().strip("\"'")
            elif block is not None:
                block.append(line.strip())
            continue
        match = METADATA_KEY.match(line)
        if not match:
            continue
        if current is not None and block is not None:
            keys[current] = " ".join(block)
        current = match.group(1)
        value = match.group(2).strip()
        block = [] if value in (">", "|", ">-", "|-") else None
        if current == "metadata" and not value:
            keys["metadata"] = nested
            continue
        keys[current] = value
    if current is not None and block is not None:
        keys[current] = " ".join(block)
    if "metadata" not in keys:
        keys["metadata"] = nested
    return keys


def check_skill(skill_dir: Path) -> None:
    name = skill_dir.name
    skill_md = skill_dir / "SKILL.md"
    rel = skill_md.relative_to(ROOT)
    if not skill_md.is_file():
        fail(f"{skill_dir.relative_to(ROOT)}: missing SKILL.md")
        return

    front = read_frontmatter(skill_md)
    for required in ("name", "description", "license", "metadata"):
        if required not in front:
            fail(f"{rel}: frontmatter missing `{required}`")

    declared = str(front.get("name", "")).strip().strip("\"'")
    if declared != name:
        fail(f"{rel}: frontmatter name `{declared}` does not match directory `{name}`")
    if declared and not NAME_PATTERN.match(declared):
        fail(f"{rel}: name `{declared}` is not lowercase kebab-case")

    description = str(front.get("description", "")).strip()
    if not description or description in (">", "|"):
        fail(f"{rel}: description is empty")
    elif len(description) > 1024:
        fail(f"{rel}: description exceeds 1024 characters ({len(description)})")

    license_value = str(front.get("license", "")).strip().strip("\"'")
    if license_value and license_value.upper() != "MIT":
        fail(f"{rel}: skill license must be MIT, found `{license_value}`")
    skill_license = skill_dir / "LICENSE"
    if not skill_license.is_file():
        fail(f"{rel}: missing per-skill LICENSE (the root GPL LICENSE is not copied)")
    elif not skill_license.read_text(encoding="utf-8").startswith("MIT License"):
        fail(f"{skill_license.relative_to(ROOT)}: expected MIT license text")

    metadata = front.get("metadata")
    if not isinstance(metadata, dict) or not metadata:
        fail(f"{rel}: metadata must be a non-empty mapping")


def check_links(path: Path) -> None:
    text = path.read_text(encoding="utf-8")
    for target in LINK_PATTERN.findall(text):
        if target.startswith(("http://", "https://", "mailto:", "#")):
            continue
        resolved = (path.parent / target.split("#", 1)[0]).resolve()
        if not resolved.exists():
            fail(f"{path.relative_to(ROOT)}: broken relative link `{target}`")


def main() -> int:
    if not SKILLS.is_dir():
        fail("skills/ directory is missing")
    else:
        skill_dirs = sorted(p for p in SKILLS.iterdir() if p.is_dir())
        if not skill_dirs:
            fail("no skills/<name>/ directories found")
        for skill_dir in skill_dirs:
            check_skill(skill_dir)
        for markdown in sorted(SKILLS.rglob("*.md")):
            check_links(markdown)
        stray = SKILLS / "SKILL.md"
        if stray.exists():
            fail("skills/SKILL.md is not discoverable; use skills/<name>/SKILL.md")

    if failures:
        for message in failures:
            print(f"::error::{message}")
        return 1
    print(f"skills/: {len([p for p in SKILLS.iterdir() if p.is_dir()])} skill(s) valid")
    return 0


if __name__ == "__main__":
    sys.exit(main())
