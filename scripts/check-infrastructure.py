#!/usr/bin/env python3
"""Validate the repository descriptors and Cargo topology without fetching code."""
from pathlib import Path
import tomllib

root = Path(__file__).resolve().parent.parent

def load(relative):
    with (root / relative).open("rb") as source:
        return tomllib.load(source)

repo = load("repo.toml")["repo"]
workspace = load("Cargo.toml")["workspace"]
toolchain = load("rust-toolchain.toml")["toolchain"]
assert repo["name"] == "koma", "unexpected repository identity"
assert repo["repository"] == workspace["package"]["repository"], "repository URL drift"
assert repo["docs"].startswith("https://github.com/komawm/"), "docs must use canonical organization URLs"
assert toolchain["components"] == ["rustfmt", "clippy"], "quality components missing"
package = workspace["package"]
assert package["edition"] == "2024", "unexpected Rust edition"
assert toolchain["channel"].startswith(package["rust-version"] + "."), "MSRV/toolchain drift"
assert package["license"] == "GPL-3.0-or-later", "product license drift"
assert package["license-file"] == "LICENSE", "workspace license file drift"
assert (root / "LICENSE").read_text().startswith("GNU GENERAL PUBLIC LICENSE"), "GPL license text missing"
assert list(root.glob("LICENSE*")) == [root / "LICENSE"], "keep one root LICENSE"
for member in workspace["members"]:
    crate = load(Path(member) / "Cargo.toml")
    assert crate["package"]["name"] == Path(member).name, f"crate name drift: {member}"
    assert crate["package"]["version"] == {"workspace": True}, f"crate version drift: {member}"
    assert crate["package"]["license"] == {"workspace": True}, f"unexpected product license override: {member}"
    assert crate["package"]["license-file"] == {"workspace": True}, f"license file inheritance missing: {member}"
    assert not list((root / member).glob("LICENSE*")), f"use the shared root LICENSE: {member}"
    assert (root / member / "src").is_dir(), f"crate source missing: {member}"
for name, dependency in workspace["dependencies"].items():
    if name.startswith("koma-"):
        assert dependency["version"] == package["version"], f"internal version drift: {name}"
load(".carryctx/config.toml")
assert not (root / ".gitmodules").exists(), "Koma does not use Git submodules"
print(f"Infrastructure metadata valid: {len(workspace['members'])} Cargo crates")

