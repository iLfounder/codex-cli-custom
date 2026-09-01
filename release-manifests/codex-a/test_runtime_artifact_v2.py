#!/usr/bin/env python3

from __future__ import annotations

import argparse
import copy
import hashlib
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parent
REPOSITORY_ROOT = ROOT.parents[1]
sys.path.insert(0, str(ROOT))

import build_runtime_artifact_v2 as producer  # noqa: E402


FROZEN_RUNTIME_CONTRACT = {
    "contractId": "ananke-work/codex-app-server-runtime-semantics/v1",
    "capabilities": [
        "ananke_session_identity_v1",
        "ananke_session_control_v1",
        "ananke_thread_transition_v1",
    ],
    "sequenceSemantics": {
        "id": "nondecreasing-coalesced-v1",
        "ordering": "nondecreasing",
        "sameSequenceDistinctSnapshots": True,
        "sequenceJumpsAllowed": True,
        "readMayAdvanceWithoutNotification": True,
        "fullSnapshotNotifications": True,
        "resyncOn": [
            "instance_epoch_change",
            "sequence_regression",
            "notification_gap",
            "notification_overflow",
            "stale_cursor",
        ],
    },
    "sharedRuntime": {
        "id": "owner-shared-app-server-v1",
        "topology": "single_instance_single_socket",
        "executionAuthority": "sessionRuntime.account.current",
        "attachmentHomeMeaning": "mcp_lease_location",
        "processReplacementRotates": [
            "instanceId",
            "instanceGeneration",
            "instanceEpoch",
            "processGeneration",
        ],
    },
    "attachment": {
        "id": "loaded-thread-mcp-probe-v1",
        "methods": ["thread/loaded/list", "mcpServer/tool/call"],
        "privateProbeTool": "__autoattach_probe",
        "authority": "consumer_private_until_provider_owned",
        "coldCallPolicy": "bounded_pending_until_probe_terminal",
    },
    "mutationSemantics": {
        "connection": "persistent_subscriber",
        "autoRetry": "forbidden",
        "operation": {
            "nonterminal": ["accepted", "running"],
            "terminal": ["ready", "released", "failed"],
        },
        "transition": {
            "nonterminal": ["prepared"],
            "terminal": ["committed"],
        },
        "goalCas": {
            "readback": ["goalId", "revision"],
            "request": ["expectedGoalId", "expectedRevision"],
        },
    },
}

FROZEN_V1_SHA256 = {
    "release-manifests/codex-a/runtime-artifact-v1.json": (
        "6bbc8606e737463eebb56960fb1bd302dc6b6f1dbd299f7d62f65d91f62a40f0"
    ),
    "release-manifests/codex-a/runtime-artifact-v1.schema.json": (
        "daa5dc24d96aafefe69b2785aea4666e4034b99f25b56ec235ef98d34a962e2f"
    ),
}

REQUIRED_PATHS = (
    ("schema",),
    ("runtimeContract", "contractId"),
    ("runtimeContract", "capabilities"),
    ("runtimeContract", "sequenceSemantics"),
    ("runtimeContract", "sharedRuntime"),
    ("runtimeContract", "attachment"),
    ("runtimeContract", "mutationSemantics"),
    ("protocolSurface", "schemaPath"),
    ("protocolSurface", "schemaSha256"),
    ("protocolSurface", "sourceAnchorsPath"),
    ("protocolSurface", "sourceAnchorsSha256"),
    ("source", "upstreamCommit"),
    ("source", "sourceRevision"),
    ("source", "buildRevision"),
    ("source", "patchedTree"),
    ("source", "compiledTree"),
    ("source", "patchedTreeEqualsCompiledTree"),
    ("artifact", "target"),
    ("artifact", "appServerSha256"),
    ("artifact", "entrypoints", "appServer"),
    ("artifact", "entrypoints", "supervisor"),
    ("supervisor", "contractVersion"),
)


def valid_manifest_fixture() -> dict:
    return producer.manifest_document(
        runtime_contract=copy.deepcopy(FROZEN_RUNTIME_CONTRACT),
        schema_sha256="1" * 64,
        source_anchors_sha256="2" * 64,
        upstream_commit="3" * 40,
        source_revision="4" * 40,
        build_revision="5" * 40,
        patched_tree="6" * 40,
        compiled_tree="6" * 40,
        app_server_sha256="7" * 64,
    )


def delete_path(document: dict, path: tuple[str, ...]) -> None:
    parent = document
    for component in path[:-1]:
        parent = parent[component]
    del parent[path[-1]]


def git(*arguments: str, repository: Path = REPOSITORY_ROOT) -> bytes:
    return subprocess.run(
        ["git", "-C", str(repository), *arguments],
        check=True,
        capture_output=True,
    ).stdout


def current_repository_blob(relative: str) -> bytes:
    path = REPOSITORY_ROOT / relative
    object_id = git(
        "hash-object", f"--path={relative}", str(path)
    ).decode("ascii").strip()
    return git("cat-file", "blob", object_id)


class ProviderStaticFilesTest(unittest.TestCase):
    def test_frozen_runtime_contract_and_order(self):
        actual = producer.load_frozen_runtime_contract()
        self.assertEqual(actual, FROZEN_RUNTIME_CONTRACT)
        self.assertEqual(list(actual), list(FROZEN_RUNTIME_CONTRACT))
        producer.validate_manifest_document(valid_manifest_fixture())

    def test_every_contract_required_path_is_required(self):
        for path in REQUIRED_PATHS:
            with self.subTest(path=".".join(path)):
                manifest = valid_manifest_fixture()
                delete_path(manifest, path)
                with self.assertRaises(producer.ContractError):
                    producer.validate_manifest_document(manifest)

    def test_tree_equality_is_fail_closed(self):
        manifest = valid_manifest_fixture()
        manifest["source"]["compiledTree"]["digest"] = "8" * 40
        with self.assertRaises(producer.ContractError):
            producer.validate_manifest_document(manifest)

        manifest = valid_manifest_fixture()
        manifest["source"]["patchedTreeEqualsCompiledTree"] = False
        with self.assertRaises(producer.ContractError):
            producer.validate_manifest_document(manifest)

    def test_digest_grammars_are_strict(self):
        mutations = (
            ("schema prefix", lambda value: value["protocolSurface"].__setitem__(
                "schemaSha256", "1" * 64
            )),
            ("anchor lowercase", lambda value: value["protocolSurface"].__setitem__(
                "sourceAnchorsSha256", "sha256:" + "A" * 64
            )),
            ("tree grammar", lambda value: value["source"]["patchedTree"].__setitem__(
                "grammar", "git-tree-sha256"
            )),
            ("tree digest", lambda value: value["source"]["compiledTree"].__setitem__(
                "digest", "not-a-tree"
            )),
            ("app-server prefix", lambda value: value["artifact"].__setitem__(
                "appServerSha256", "sha256:" + "7" * 64
            )),
        )
        for name, mutate in mutations:
            with self.subTest(name=name):
                manifest = valid_manifest_fixture()
                mutate(manifest)
                with self.assertRaises(producer.ContractError):
                    producer.validate_manifest_document(manifest)

    def test_target_and_entrypoints_are_frozen(self):
        mutations = (
            lambda value: value["artifact"].__setitem__(
                "target", "x86_64-pc-windows-msvc"
            ),
            lambda value: value["artifact"]["entrypoints"]["appServer"].__setitem__(
                "executablePath", "bin/codex"
            ),
            lambda value: value["artifact"]["entrypoints"]["supervisor"].__setitem__(
                "argv", ["app-server", "daemon"]
            ),
        )
        for mutate in mutations:
            manifest = valid_manifest_fixture()
            mutate(manifest)
            with self.assertRaises(producer.ContractError):
                producer.validate_manifest_document(manifest)

    def test_supervisor_version_must_be_integer_one(self):
        manifest = valid_manifest_fixture()
        manifest["supervisor"]["contractVersion"] = "1"
        with self.assertRaises(producer.ContractError):
            producer.validate_manifest_document(manifest)

    def test_protocol_surface_rejects_private_probe_method(self):
        manifest = valid_manifest_fixture()
        manifest["protocolSurface"]["methods"] = [producer.PRIVATE_PROBE_TOOL]
        with self.assertRaises(producer.ContractError):
            producer.validate_manifest_document(manifest)

        valid_schema = json.dumps(
            {"methods": list(producer.ATTACHMENT_METHODS)}, separators=(",", ":")
        ).encode()
        producer.validate_protocol_schema_bytes(valid_schema)
        invalid_schema = json.dumps(
            {
                "methods": [
                    *producer.ATTACHMENT_METHODS,
                    producer.PRIVATE_PROBE_TOOL,
                ]
            },
            separators=(",", ":"),
        ).encode()
        with self.assertRaises(producer.ContractError):
            producer.validate_protocol_schema_bytes(invalid_schema)

    def test_v1_git_blob_bytes_are_unchanged(self):
        for relative, expected in FROZEN_V1_SHA256.items():
            with self.subTest(path=relative):
                current = current_repository_blob(relative)
                committed = git("cat-file", "blob", f"HEAD:{relative}")
                self.assertEqual(hashlib.sha256(current).hexdigest(), expected)
                self.assertEqual(hashlib.sha256(committed).hexdigest(), expected)


class ProviderGitFixtureTest(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.repository = self.root / "source"
        self.repository.mkdir()
        self._git("init", "--quiet")
        self._git("config", "user.name", "runtime-artifact-test")
        self._git("config", "user.email", "runtime-artifact-test@example.invalid")

        for index, relative in enumerate(producer.ANCHOR_PATHS):
            path = self.repository / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(f"anchor {index}: {relative}\n".encode())
        protocol_schema = self.repository / producer.PROTOCOL_SCHEMA_PATH
        protocol_schema.parent.mkdir(parents=True, exist_ok=True)
        self.protocol_schema_bytes = (
            json.dumps(
                {"methods": list(producer.ATTACHMENT_METHODS)},
                separators=(",", ":"),
            )
            + "\n"
        ).encode()
        protocol_schema.write_bytes(self.protocol_schema_bytes)
        self._git("add", ".")
        self._git("commit", "--quiet", "-m", "fixture source")
        self.head = self._git_text("rev-parse", "HEAD")
        self.tree = self._git_text("rev-parse", "HEAD^{tree}")

        self.series = self.root / "series.toml"
        self._write_series(self.head, self.tree)
        self.anchors = self.root / "runtime-artifact-v2.source-anchors.json"
        producer.write_source_anchors(self.repository, self.tree, self.anchors)
        self.executable = self.root / "bin" / "codex-app-server"
        self.executable.parent.mkdir()
        self.executable_bytes = b"test Mach-O app-server bytes\x00\x01\xff"
        self.executable.write_bytes(self.executable_bytes)

    def tearDown(self):
        self.temporary.cleanup()

    def _git(self, *arguments: str) -> bytes:
        return subprocess.run(
            ["git", "-C", str(self.repository), *arguments],
            check=True,
            capture_output=True,
        ).stdout

    def _git_text(self, *arguments: str) -> str:
        return self._git(*arguments).decode("ascii").strip()

    def _write_series(self, base_commit: str, final_tree: str) -> None:
        self.series.write_text(
            "schema_version = 1\n"
            'applies_to = "fixture"\n'
            f'base_commit = "{base_commit}"\n'
            f'final_tree = "{final_tree}"\n',
            encoding="utf-8",
            newline="\n",
        )

    def _build(self, *, output=None, evidence=None, **overrides):
        output = self.root / "runtime-artifact-v2.json" if output is None else output
        evidence = (
            self.root / "runtime-artifact-v2.protocol-schema.json"
            if evidence is None
            else evidence
        )
        arguments = {
            "source_repository": self.repository,
            "series_manifest": self.series,
            "upstream_commit": self.head,
            "source_revision": "a" * 40,
            "build_revision": "b" * 40,
            "patched_tree": self.tree,
            "compiled_tree": self.tree,
            "app_server_executable": self.executable,
            "expected_anchors": self.anchors,
            "output": output,
            "protocol_schema_output": evidence,
        }
        arguments.update(overrides)
        manifest = producer.build_runtime_artifact_manifest(**arguments)
        return manifest, output, evidence

    def test_anchor_serialization_is_canonical_and_byte_sorted(self):
        data = self.anchors.read_bytes()
        document = producer.load_json_bytes(data, source=str(self.anchors))
        self.assertEqual(data, producer.canonical_source_anchor_bytes(document))
        self.assertTrue(data.endswith(b"\n"))
        self.assertFalse(data.endswith(b"\n\n"))
        self.assertNotIn(b"\r", data)
        self.assertTrue(data.startswith(b'{"anchors":[{"blobOid":{"digest":'))
        self.assertEqual(
            [anchor["path"] for anchor in document["anchors"]],
            list(producer.ANCHOR_PATHS),
        )

    def test_manifest_uses_raw_schema_and_executable_bytes(self):
        manifest, output, evidence = self._build()
        producer.validate_manifest_document(manifest)
        self.assertEqual(evidence.read_bytes(), self.protocol_schema_bytes)
        self.assertEqual(
            manifest["protocolSurface"]["schemaSha256"],
            "sha256:" + hashlib.sha256(self.protocol_schema_bytes).hexdigest(),
        )
        self.assertEqual(
            manifest["protocolSurface"]["sourceAnchorsSha256"],
            "sha256:" + hashlib.sha256(self.anchors.read_bytes()).hexdigest(),
        )
        self.assertEqual(
            manifest["artifact"]["appServerSha256"],
            hashlib.sha256(self.executable_bytes).hexdigest(),
        )
        self.assertEqual(
            output.read_bytes(), producer.deterministic_manifest_bytes(manifest)
        )
        validated = producer.validate_real_manifest(
            source_repository=self.repository,
            manifest_path=output,
            app_server_executable=self.executable,
            expected_anchors=self.anchors,
        )
        self.assertEqual(validated, manifest)

    def test_anchor_drift_fails_before_outputs(self):
        document = producer.load_json_file(self.anchors)
        document["anchors"][0]["blobOid"]["digest"] = "0" * 40
        self.anchors.write_bytes(producer.canonical_source_anchor_bytes(document))
        output = self.root / "drift-manifest.json"
        evidence = self.root / "drift-schema.json"
        with self.assertRaises(producer.ContractError):
            self._build(output=output, evidence=evidence)
        self.assertFalse(output.exists())
        self.assertFalse(evidence.exists())

    def test_tree_mismatch_fails_before_outputs(self):
        output = self.root / "mismatch-manifest.json"
        evidence = self.root / "mismatch-schema.json"
        with self.assertRaises(producer.ContractError):
            self._build(
                output=output,
                evidence=evidence,
                compiled_tree="0" * 40,
            )
        self.assertFalse(output.exists())
        self.assertFalse(evidence.exists())

    def test_series_identity_is_enforced(self):
        self._write_series("1" * 40, self.tree)
        output = self.root / "series-manifest.json"
        evidence = self.root / "series-schema.json"
        with self.assertRaises(producer.ContractError):
            self._build(output=output, evidence=evidence)
        self.assertFalse(output.exists())
        self.assertFalse(evidence.exists())

    def test_patched_head_tree_is_enforced(self):
        extra = self.repository / "extra.txt"
        extra.write_text("drift\n", encoding="utf-8", newline="\n")
        self._git("add", "extra.txt")
        self._git("commit", "--quiet", "-m", "move patched head")
        output = self.root / "head-manifest.json"
        evidence = self.root / "head-schema.json"
        with self.assertRaises(producer.ContractError):
            self._build(output=output, evidence=evidence)
        self.assertFalse(output.exists())
        self.assertFalse(evidence.exists())


def _argument_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Validate frozen provider files or one real runtime artifact v2 manifest"
    )
    parser.add_argument("--source-root", type=Path)
    parser.add_argument("--manifest", type=Path)
    parser.add_argument("--app-server-executable", type=Path)
    return parser


def main(argv: list[str] | None = None) -> int:
    parser = _argument_parser()
    args, unittest_arguments = parser.parse_known_args(argv)
    real_arguments = (
        args.source_root,
        args.manifest,
        args.app_server_executable,
    )
    if any(value is not None for value in real_arguments):
        if not all(value is not None for value in real_arguments):
            parser.error(
                "--source-root, --manifest, and --app-server-executable are all required together"
            )
        if unittest_arguments:
            parser.error(f"unexpected arguments: {' '.join(unittest_arguments)}")
        producer.validate_real_manifest(
            source_repository=args.source_root,
            manifest_path=args.manifest,
            app_server_executable=args.app_server_executable,
        )
        print(f"validated {args.manifest}")
        return 0

    program = unittest.main(
        module=__name__,
        argv=[sys.argv[0], *unittest_arguments],
        exit=False,
    )
    return 0 if program.result.wasSuccessful() else 1


if __name__ == "__main__":
    raise SystemExit(main())
