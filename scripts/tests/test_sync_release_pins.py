from __future__ import annotations

import hashlib
import importlib.util
import json
import shutil
import tempfile
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "sync_release_pins", REPO / "scripts/sync-release-pins.py"
)
assert SPEC is not None and SPEC.loader is not None
PINS = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PINS)


class ReleasePinSynchronizerTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        for relative in ["release-pins.json", *PINS.CONSUMERS]:
            source = REPO / relative
            destination = self.root / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(source, destination)
        self.artifacts = self.root / "artifacts"
        self.artifacts.mkdir()

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def create_artifacts(self, version: str) -> None:
        for target in PINS.TARGETS:
            directory = self.artifacts / f"release-{target}"
            directory.mkdir(parents=True)
            name = f"upd-{version}-{target}.tar.gz"
            archive = directory / name
            archive.write_bytes(f"archive bytes for {version} {target}\n".encode())
            digest = hashlib.sha256(archive.read_bytes()).hexdigest()
            (directory / f"{name}.sha256").write_text(
                f"{digest}  {name}\n", encoding="utf-8"
            )

    def test_verified_artifacts_update_every_consumer_idempotently(self) -> None:
        self.create_artifacts("v0.6.4")
        old = PINS.load_manifest(self.root / "release-pins.json")
        new = PINS.manifest_from_artifacts(
            self.artifacts, "v0.6.4", "1" * 40
        )

        self.assertTrue(PINS.sync_consumers(self.root, old, new))
        PINS.verify_consumers(self.root, new)
        self.assertFalse(PINS.sync_consumers(self.root, new, new))
        self.assertEqual(
            json.loads((self.root / "release-pins.json").read_text()), new
        )

    def test_mismatched_sidecar_is_rejected_before_files_change(self) -> None:
        self.create_artifacts("v0.6.4")
        sidecar = next(self.artifacts.rglob("*.sha256"))
        sidecar.write_text(f"{'0' * 64}\n", encoding="utf-8")
        before = (self.root / "release-pins.json").read_text(encoding="utf-8")

        with self.assertRaisesRegex(PINS.PinError, "does not match archive"):
            PINS.manifest_from_artifacts(self.artifacts, "v0.6.4", "1" * 40)

        self.assertEqual(
            (self.root / "release-pins.json").read_text(encoding="utf-8"), before
        )

    def test_downgrade_is_rejected(self) -> None:
        current = PINS.load_manifest(self.root / "release-pins.json")
        older = json.loads(json.dumps(current))
        older["version"] = "v0.6.2"
        for target, asset in older["assets"].items():
            asset["name"] = f"upd-v0.6.2-{target}.tar.gz"

        with self.assertRaisesRegex(PINS.PinError, "refusing to downgrade"):
            PINS.sync_consumers(self.root, current, older)


if __name__ == "__main__":
    unittest.main()
