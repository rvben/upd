#!/usr/bin/env python3
"""Synchronize integration defaults with verified upd release artifacts."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import stat
import sys
import tempfile
from pathlib import Path
from typing import Any

TARGETS = (
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-gnu",
    "x86_64-unknown-linux-musl",
)
VERSION_RE = re.compile(r"^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$")
SHA_RE = re.compile(r"^[0-9a-f]{64}$")
COMMIT_RE = re.compile(r"^[0-9a-f]{40}$")

# Exact occurrence counts make replacement fail closed when a consumer changes
# shape. Add new distributed consumers here instead of scattering release-time
# edits through the workflow.
CONSUMERS: dict[str, dict[str, Any]] = {
    "ci/gitlab-dependency-update.yml": {
        "version": 2,
        "hashes": {target: 1 for target in TARGETS},
    },
    "docs/github-actions.md": {
        "version": 1,
        "hashes": {"x86_64-unknown-linux-gnu": 1},
    },
    "docs/gitlab.md": {
        "version": 2,
        "hashes": {"x86_64-unknown-linux-gnu": 1},
    },
}


class PinError(RuntimeError):
    """A release-pin invariant was violated."""


def load_manifest(path: Path) -> dict[str, Any]:
    try:
        manifest = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise PinError(f"cannot read {path}: {error}") from error
    validate_manifest(manifest)
    return manifest


def validate_manifest(manifest: dict[str, Any]) -> None:
    if manifest.get("schema") != 1:
        raise PinError("release-pin manifest schema must be 1")
    version = manifest.get("version")
    if not isinstance(version, str) or VERSION_RE.fullmatch(version) is None:
        raise PinError("manifest version must be an exact stable v-prefixed release")
    release_commit = manifest.get("release_commit")
    if not isinstance(release_commit, str) or COMMIT_RE.fullmatch(release_commit) is None:
        raise PinError("manifest release_commit must be a lowercase full commit SHA")
    assets = manifest.get("assets")
    if not isinstance(assets, dict) or set(assets) != set(TARGETS):
        raise PinError(f"manifest assets must contain exactly: {', '.join(TARGETS)}")
    for target in TARGETS:
        asset = assets[target]
        expected_name = f"upd-{version}-{target}.tar.gz"
        if not isinstance(asset, dict) or asset.get("name") != expected_name:
            raise PinError(f"manifest asset name for {target} must be {expected_name}")
        digest = asset.get("sha256")
        if not isinstance(digest, str) or SHA_RE.fullmatch(digest) is None:
            raise PinError(f"manifest checksum for {target} must be 64 lowercase hex characters")


def version_key(version: str) -> tuple[int, int, int]:
    match = VERSION_RE.fullmatch(version)
    if match is None:
        raise PinError(f"unsupported release version: {version}")
    return tuple(int(part) for part in match.groups())  # type: ignore[return-value]


def verify_consumers(root: Path, manifest: dict[str, Any]) -> None:
    version = manifest["version"]
    for relative, expectations in CONSUMERS.items():
        path = root / relative
        try:
            content = path.read_text(encoding="utf-8")
        except OSError as error:
            raise PinError(f"cannot read consumer {path}: {error}") from error
        actual_versions = content.count(version)
        if actual_versions != expectations["version"]:
            raise PinError(
                f"{relative}: expected {expectations['version']} occurrences of {version}, "
                f"found {actual_versions}"
            )
        for target, expected_count in expectations["hashes"].items():
            digest = manifest["assets"][target]["sha256"]
            actual_count = content.count(digest)
            if actual_count != expected_count:
                raise PinError(
                    f"{relative}: expected {expected_count} occurrences of the {target} "
                    f"checksum, found {actual_count}"
                )


def replace_exact(content: str, old: str, new: str, count: int, relative: str) -> str:
    actual = content.count(old)
    if actual != count:
        raise PinError(f"{relative}: expected {count} occurrences of {old}, found {actual}")
    return content.replace(old, new)


def atomic_write(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    mode = stat.S_IMODE(path.stat().st_mode) if path.exists() else 0o644
    descriptor, temporary = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8") as handle:
            handle.write(content)
        os.chmod(temporary, mode)
        os.replace(temporary, path)
    except BaseException:
        try:
            os.unlink(temporary)
        except FileNotFoundError:
            pass
        raise


def sync_consumers(root: Path, old: dict[str, Any], new: dict[str, Any]) -> bool:
    verify_consumers(root, old)
    old_version = old["version"]
    new_version = new["version"]
    if version_key(new_version) < version_key(old_version):
        raise PinError(f"refusing to downgrade release pins from {old_version} to {new_version}")
    if new_version == old_version and new != old:
        raise PinError(f"release {new_version} is immutable; its manifest cannot change")
    if new == old:
        return False

    rendered: dict[Path, str] = {}
    for relative, expectations in CONSUMERS.items():
        path = root / relative
        content = path.read_text(encoding="utf-8")
        content = replace_exact(
            content, old_version, new_version, expectations["version"], relative
        )
        for target, expected_count in expectations["hashes"].items():
            content = replace_exact(
                content,
                old["assets"][target]["sha256"],
                new["assets"][target]["sha256"],
                expected_count,
                relative,
            )
        rendered[path] = content

    for path, content in rendered.items():
        atomic_write(path, content)
    atomic_write(root / "release-pins.json", json.dumps(new, indent=2, sort_keys=True) + "\n")
    verify_consumers(root, new)
    return True


def unique_artifact(artifacts: Path, name: str) -> Path:
    matches = sorted(path for path in artifacts.rglob(name) if path.is_file())
    if len(matches) != 1:
        raise PinError(f"expected exactly one {name} below {artifacts}, found {len(matches)}")
    return matches[0]


def manifest_from_artifacts(artifacts: Path, version: str, release_commit: str) -> dict[str, Any]:
    if VERSION_RE.fullmatch(version) is None:
        raise PinError("--version must be an exact stable v-prefixed release")
    if COMMIT_RE.fullmatch(release_commit) is None:
        raise PinError("--release-commit must be a lowercase full commit SHA")
    assets: dict[str, dict[str, str]] = {}
    for target in TARGETS:
        name = f"upd-{version}-{target}.tar.gz"
        archive = unique_artifact(artifacts, name)
        sidecar = unique_artifact(artifacts, f"{name}.sha256")
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        words = sidecar.read_text(encoding="utf-8").split()
        sidecar_digest = words[0].lower() if words else ""
        if SHA_RE.fullmatch(sidecar_digest) is None:
            raise PinError(f"invalid checksum sidecar: {sidecar}")
        if digest != sidecar_digest:
            raise PinError(f"checksum sidecar does not match archive: {archive}")
        assets[target] = {"name": name, "sha256": digest}
    manifest = {
        "schema": 1,
        "version": version,
        "release_commit": release_commit,
        "assets": assets,
    }
    validate_manifest(manifest)
    return manifest


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--root", type=Path, default=Path(__file__).resolve().parents[1], help=argparse.SUPPRESS
    )
    parser.add_argument("--check", action="store_true", help="verify checked-in consumers")
    parser.add_argument("--artifacts", type=Path, help="directory containing release artifacts")
    parser.add_argument("--version", help="release tag, such as v0.6.4")
    parser.add_argument("--release-commit", help="full commit SHA referenced by the release tag")
    args = parser.parse_args()
    if args.check:
        if args.artifacts or args.version or args.release_commit:
            parser.error("--check cannot be combined with artifact synchronization options")
    elif not (args.artifacts and args.version and args.release_commit):
        parser.error("synchronization requires --artifacts, --version, and --release-commit")
    return args


def main() -> int:
    args = parse_args()
    root = args.root.resolve()
    try:
        current = load_manifest(root / "release-pins.json")
        if args.check:
            verify_consumers(root, current)
            print(f"release pins are consistent at {current['version']}")
            return 0
        proposed = manifest_from_artifacts(
            args.artifacts.resolve(), args.version, args.release_commit
        )
        changed = sync_consumers(root, current, proposed)
        print(
            f"release pins updated to {proposed['version']}"
            if changed
            else f"release pins already current at {proposed['version']}"
        )
        return 0
    except PinError as error:
        print(f"release pin error: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
