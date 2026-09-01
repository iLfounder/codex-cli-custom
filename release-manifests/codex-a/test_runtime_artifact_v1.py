#!/usr/bin/env python3

import hashlib
import json
import re
import subprocess
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parent
REPOSITORY_ROOT = ROOT.parents[1]
MANIFEST_PATH = ROOT / "runtime-artifact-v1.json"
SCHEMA_PATH = ROOT / "runtime-artifact-v1.schema.json"
CHECKSUM_PATH = ROOT / "SHA256SUMS"
FROZEN_FILE_SHA256 = {
    MANIFEST_PATH.name: "6bbc8606e737463eebb56960fb1bd302dc6b6f1dbd299f7d62f65d91f62a40f0",
    SCHEMA_PATH.name: "daa5dc24d96aafefe69b2785aea4666e4034b99f25b56ec235ef98d34a962e2f",
}


def load_json(path: Path):
    with path.open(encoding="utf-8") as source:
        return json.load(source)


def repository_blob_bytes(path: Path, revision=None):
    relative = path.relative_to(REPOSITORY_ROOT).as_posix()
    if revision is None:
        object_id = subprocess.run(
            [
                "git",
                "-C",
                str(REPOSITORY_ROOT),
                "hash-object",
                f"--path={relative}",
                str(path),
            ],
            check=True,
            capture_output=True,
            text=True,
        ).stdout.strip()
        object_name = object_id
    else:
        object_name = f"{revision}:{relative}"
    return subprocess.run(
        ["git", "-C", str(REPOSITORY_ROOT), "cat-file", "blob", object_name],
        check=True,
        capture_output=True,
    ).stdout


def resolve_ref(root, ref):
    if not ref.startswith("#/"):
        raise AssertionError(f"unsupported schema reference: {ref}")
    value = root
    for component in ref[2:].split("/"):
        value = value[component.replace("~1", "/").replace("~0", "~")]
    return value


def validate(instance, schema, root, path="$"):
    if "$ref" in schema:
        validate(instance, resolve_ref(root, schema["$ref"]), root, path)
        return

    if "oneOf" in schema:
        matches = 0
        for option in schema["oneOf"]:
            try:
                validate(instance, option, root, path)
                matches += 1
            except AssertionError:
                pass
        if matches != 1:
            raise AssertionError(f"{path}: expected exactly one schema match")
        return

    expected_type = schema.get("type")
    type_map = {
        "object": dict,
        "array": list,
        "string": str,
        "integer": int,
        "boolean": bool,
        "null": type(None),
    }
    if expected_type is not None:
        if expected_type == "integer":
            valid_type = isinstance(instance, int) and not isinstance(instance, bool)
        else:
            valid_type = isinstance(instance, type_map[expected_type])
        if not valid_type:
            raise AssertionError(f"{path}: expected {expected_type}")

    if "const" in schema and instance != schema["const"]:
        raise AssertionError(f"{path}: value does not match const")
    if "enum" in schema and instance not in schema["enum"]:
        raise AssertionError(f"{path}: value is not in enum")

    if isinstance(instance, dict):
        required = set(schema.get("required", []))
        missing = required - set(instance)
        if missing:
            raise AssertionError(f"{path}: missing {sorted(missing)}")
        properties = schema.get("properties", {})
        if schema.get("additionalProperties") is False:
            extra = set(instance) - set(properties)
            if extra:
                raise AssertionError(f"{path}: unexpected {sorted(extra)}")
        for key, value in instance.items():
            if key in properties:
                validate(value, properties[key], root, f"{path}.{key}")

    if isinstance(instance, list):
        if len(instance) < schema.get("minItems", 0):
            raise AssertionError(f"{path}: too few items")
        if schema.get("uniqueItems"):
            encoded = [json.dumps(item, sort_keys=True) for item in instance]
            if len(encoded) != len(set(encoded)):
                raise AssertionError(f"{path}: duplicate items")
        item_schema = schema.get("items")
        if item_schema is not None:
            for index, value in enumerate(instance):
                validate(value, item_schema, root, f"{path}[{index}]")

    if isinstance(instance, str):
        if len(instance) < schema.get("minLength", 0):
            raise AssertionError(f"{path}: string is too short")
        pattern = schema.get("pattern")
        if pattern is not None and re.fullmatch(pattern, instance) is None:
            raise AssertionError(f"{path}: string does not match {pattern}")

    if isinstance(instance, int) and not isinstance(instance, bool):
        if instance < schema.get("minimum", instance):
            raise AssertionError(f"{path}: integer is below minimum")


class RuntimeArtifactManifestTest(unittest.TestCase):
    def test_manifest_satisfies_schema_and_frozen_contract(self):
        manifest = load_json(MANIFEST_PATH)
        schema = load_json(SCHEMA_PATH)
        validate(manifest, schema, schema)

        self.assertEqual(
            {
                "sourceRevision": manifest["sourceRevision"],
                "artifact": {
                    "id": manifest["artifact"]["id"],
                    "digest": manifest["artifact"]["digest"],
                    "payloadChecksum": manifest["artifact"]["payloadChecksum"],
                },
                "trees": {
                    "patched": manifest["product"]["patchedTree"],
                    "compiled": manifest["product"]["compiledTree"],
                },
                "platform": manifest["platform"],
            },
            {
                "sourceRevision": "7c45bcdab1e5eb1ab38d3797d02af67c827b0b7b",
                "artifact": {
                    "id": 9745233008,
                    "digest": "sha256:e3041829b578e33769cafdbba11ddec36b4f9de7b0634d3b019d5794f2d738e2",
                    "payloadChecksum": {
                        "path": "SHA256SUMS",
                        "digest": "sha256:27a8aa9c294cdf416cf86b088481021e23c9c5963dbc03ec37a9812783799d3d",
                    },
                },
                "trees": {
                    "patched": {
                        "grammar": "git-tree-sha1",
                        "digest": "1f35e1ae8139df81dfdf4a8eccd4027a736c2754",
                    },
                    "compiled": {
                        "grammar": "git-tree-sha1",
                        "digest": "1f35e1ae8139df81dfdf4a8eccd4027a736c2754",
                    },
                },
                "platform": {
                    "os": "darwin",
                    "arch": "arm64",
                    "target": "aarch64-apple-darwin",
                },
            },
        )
        self.assertEqual(
            manifest["entrypoints"]["supervisor"],
            {
                "archivePath": "codex-package-aarch64-apple-darwin.tar.gz",
                "executablePath": "bin/codex",
                "mode": "0755",
                "argv": ["app-server", "daemon", "supervisor"],
            },
        )
        self.assertEqual(
            manifest["providerFreeCanary"],
            {
                "providerCallsAllowed": False,
                "contracts": [
                    "C1",
                    "C2",
                    "C3",
                    "C4",
                    "C5",
                    "C6",
                    "C7",
                    "C8",
                    "C9",
                    "C10",
                    "multiple-TUI",
                    "Relay-zero-child",
                ],
            },
        )

    def test_frozen_files_match_checksum_receipt(self):
        lines = CHECKSUM_PATH.read_text(encoding="utf-8").splitlines()
        entries = {}
        for line in lines:
            expected, name = line.split("  ", 1)
            self.assertNotIn(name, entries)
            entries[name] = expected
        for name, frozen_digest in FROZEN_FILE_SHA256.items():
            self.assertEqual(entries.get(name), frozen_digest)
            current = hashlib.sha256(repository_blob_bytes(ROOT / name)).hexdigest()
            committed = hashlib.sha256(
                repository_blob_bytes(ROOT / name, revision="HEAD")
            ).hexdigest()
            self.assertEqual(current, frozen_digest)
            self.assertEqual(committed, frozen_digest)


if __name__ == "__main__":
    unittest.main()
