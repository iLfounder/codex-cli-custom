#!/usr/bin/env python3

"""Build and validate the frozen Codex A runtime artifact v2 evidence."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
import tempfile
import tomllib
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parent
RUNTIME_CONTRACT_PATH = ROOT / "runtime-semantics-v1.json"
MANIFEST_SCHEMA_PATH = ROOT / "runtime-artifact-v2.schema.json"
CANONICAL_ANCHORS_PATH = ROOT / "runtime-artifact-v2.source-anchors.json"

MANIFEST_SCHEMA_ID = "codex-a.runtime-artifact/v2"
SOURCE_ANCHORS_SCHEMA_ID = "codex-a.runtime-source-anchors/v1"
RUNTIME_CONTRACT_ID = "ananke-work/codex-app-server-runtime-semantics/v1"
PROTOCOL_SCHEMA_PATH = (
    "codex-rs/app-server-protocol/schema/json/"
    "codex_app_server_protocol.v2.schemas.json"
)
SOURCE_ANCHORS_REPOSITORY_PATH = (
    "release-manifests/codex-a/runtime-artifact-v2.source-anchors.json"
)
TARGET = "aarch64-apple-darwin"
SUPERVISOR_CONTRACT_VERSION = 1
PRIVATE_PROBE_TOOL = "__autoattach_probe"
ATTACHMENT_METHODS = ("thread/loaded/list", "mcpServer/tool/call")

ANCHOR_PATHS = (
    "codex-rs/app-server-daemon/src/supervisor.rs",
    "codex-rs/app-server-protocol/src/protocol/common.rs",
    "codex-rs/app-server-protocol/src/protocol/v2/mcp.rs",
    "codex-rs/app-server-protocol/src/protocol/v2/session_runtime.rs",
    "codex-rs/app-server-protocol/src/protocol/v2/thread.rs",
    "codex-rs/app-server-transport/src/supervisor.rs",
    "codex-rs/app-server/src/session_runtime/mod.rs",
    "codex-rs/app-server/src/session_runtime/operations.rs",
    "codex-rs/app-server/src/session_runtime/pagination.rs",
    "codex-rs/app-server/src/session_runtime/snapshot.rs",
)

GIT_SHA_RE = re.compile(r"^[0-9a-f]{40}$")
RAW_SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
PLACEHOLDER_RE = re.compile(r"<[^>]+>")

APP_SERVER_ENTRYPOINT = {
    "archivePath": "codex-app-server-package-aarch64-apple-darwin.tar.gz",
    "executablePath": "bin/codex-app-server",
    "mode": "0755",
    "argv": [],
}
SUPERVISOR_ENTRYPOINT = {
    "archivePath": "codex-package-aarch64-apple-darwin.tar.gz",
    "executablePath": "bin/codex",
    "mode": "0755",
    "argv": ["app-server", "daemon", "supervisor"],
}


class ContractError(ValueError):
    """Raised when provider evidence does not satisfy the frozen contract."""


def _reject_duplicate_keys(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    value: dict[str, Any] = {}
    for key, item in pairs:
        if key in value:
            raise ContractError(f"duplicate JSON key: {key}")
        value[key] = item
    return value


def load_json_bytes(data: bytes, *, source: str) -> Any:
    try:
        return json.loads(data.decode("utf-8"), object_pairs_hook=_reject_duplicate_keys)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ContractError(f"{source}: invalid UTF-8 JSON: {error}") from error


def load_json_file(path: Path) -> Any:
    try:
        data = path.read_bytes()
    except OSError as error:
        raise ContractError(f"cannot read {path}: {error}") from error
    return load_json_bytes(data, source=str(path))


def _json_identity(value: Any) -> str:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


def _json_equal(left: Any, right: Any) -> bool:
    return _json_identity(left) == _json_identity(right)


def _resolve_ref(root: dict[str, Any], reference: str) -> dict[str, Any]:
    if not reference.startswith("#/"):
        raise ContractError(f"unsupported JSON Schema reference: {reference}")
    value: Any = root
    for component in reference[2:].split("/"):
        key = component.replace("~1", "/").replace("~0", "~")
        try:
            value = value[key]
        except (KeyError, TypeError) as error:
            raise ContractError(f"unresolved JSON Schema reference: {reference}") from error
    if not isinstance(value, dict):
        raise ContractError(f"JSON Schema reference is not an object: {reference}")
    return value


def validate_json_schema_instance(
    instance: Any,
    schema: dict[str, Any],
    *,
    root_schema: dict[str, Any] | None = None,
    path: str = "$",
) -> None:
    """Validate the JSON Schema keywords used by the frozen provider schema."""

    root = schema if root_schema is None else root_schema
    if "$ref" in schema:
        validate_json_schema_instance(
            instance,
            _resolve_ref(root, schema["$ref"]),
            root_schema=root,
            path=path,
        )
        return

    if "oneOf" in schema:
        matches = 0
        for option in schema["oneOf"]:
            try:
                validate_json_schema_instance(
                    instance, option, root_schema=root, path=path
                )
                matches += 1
            except ContractError:
                pass
        if matches != 1:
            raise ContractError(f"{path}: expected exactly one schema match")
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
        if expected_type not in type_map:
            raise ContractError(f"{path}: unsupported schema type {expected_type!r}")
        if expected_type == "integer":
            valid_type = isinstance(instance, int) and not isinstance(instance, bool)
        elif expected_type == "boolean":
            valid_type = isinstance(instance, bool)
        else:
            valid_type = isinstance(instance, type_map[expected_type])
        if not valid_type:
            raise ContractError(f"{path}: expected {expected_type}")

    if "const" in schema and not _json_equal(instance, schema["const"]):
        raise ContractError(f"{path}: value does not match const")
    if "enum" in schema and not any(
        _json_equal(instance, candidate) for candidate in schema["enum"]
    ):
        raise ContractError(f"{path}: value is not in enum")

    if isinstance(instance, dict):
        required = schema.get("required", [])
        missing = [key for key in required if key not in instance]
        if missing:
            raise ContractError(f"{path}: missing {missing}")
        properties = schema.get("properties", {})
        if schema.get("additionalProperties") is False:
            extra = [key for key in instance if key not in properties]
            if extra:
                raise ContractError(f"{path}: unexpected {extra}")
        for key, item in instance.items():
            if key in properties:
                validate_json_schema_instance(
                    item,
                    properties[key],
                    root_schema=root,
                    path=f"{path}.{key}",
                )

    if isinstance(instance, list):
        if len(instance) < schema.get("minItems", 0):
            raise ContractError(f"{path}: too few items")
        maximum = schema.get("maxItems")
        if maximum is not None and len(instance) > maximum:
            raise ContractError(f"{path}: too many items")
        if schema.get("uniqueItems"):
            encoded = [_json_identity(item) for item in instance]
            if len(encoded) != len(set(encoded)):
                raise ContractError(f"{path}: duplicate items")
        prefix_items = schema.get("prefixItems", [])
        for index, item_schema in enumerate(prefix_items):
            if index >= len(instance):
                break
            validate_json_schema_instance(
                instance[index],
                item_schema,
                root_schema=root,
                path=f"{path}[{index}]",
            )
        item_schema = schema.get("items")
        if item_schema is False and len(instance) > len(prefix_items):
            raise ContractError(f"{path}: unexpected additional items")
        if isinstance(item_schema, dict):
            start = len(prefix_items)
            for index, item in enumerate(instance[start:], start=start):
                validate_json_schema_instance(
                    item,
                    item_schema,
                    root_schema=root,
                    path=f"{path}[{index}]",
                )

    if isinstance(instance, str):
        if len(instance) < schema.get("minLength", 0):
            raise ContractError(f"{path}: string is too short")
        pattern = schema.get("pattern")
        if pattern is not None and re.search(pattern, instance) is None:
            raise ContractError(f"{path}: string does not match {pattern}")

    if isinstance(instance, int) and not isinstance(instance, bool):
        minimum = schema.get("minimum")
        if minimum is not None and instance < minimum:
            raise ContractError(f"{path}: integer is below minimum")


def _require_key_order(actual: Any, expected: Any, path: str) -> None:
    if isinstance(expected, dict):
        if not isinstance(actual, dict):
            raise ContractError(f"{path}: expected object")
        if list(actual) != list(expected):
            raise ContractError(
                f"{path}: key order differs; expected {list(expected)}, found {list(actual)}"
            )
        for key in expected:
            _require_key_order(actual[key], expected[key], f"{path}.{key}")
    elif isinstance(expected, list):
        if not isinstance(actual, list) or len(actual) != len(expected):
            raise ContractError(f"{path}: array shape differs")
        for index, (actual_item, expected_item) in enumerate(zip(actual, expected)):
            _require_key_order(actual_item, expected_item, f"{path}[{index}]")


def load_provider_schema() -> dict[str, Any]:
    schema = load_json_file(MANIFEST_SCHEMA_PATH)
    if not isinstance(schema, dict):
        raise ContractError("provider schema must be a JSON object")
    return schema


def load_frozen_runtime_contract() -> dict[str, Any]:
    contract = load_json_file(RUNTIME_CONTRACT_PATH)
    schema = load_provider_schema()
    try:
        expected = schema["properties"]["runtimeContract"]["const"]
    except (KeyError, TypeError) as error:
        raise ContractError("provider schema has no frozen runtimeContract const") from error
    if not isinstance(contract, dict) or not _json_equal(contract, expected):
        raise ContractError("runtime-semantics-v1.json differs from the provider schema")
    _require_key_order(contract, expected, "$.runtimeContract")
    if contract.get("contractId") != RUNTIME_CONTRACT_ID:
        raise ContractError("runtime contract ID is not frozen v1")
    return contract


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    try:
        with path.open("rb") as source:
            for chunk in iter(lambda: source.read(1024 * 1024), b""):
                digest.update(chunk)
    except OSError as error:
        raise ContractError(f"cannot hash {path}: {error}") from error
    return digest.hexdigest()


def _require_git_sha(value: str, name: str) -> str:
    if GIT_SHA_RE.fullmatch(value) is None:
        raise ContractError(f"{name} must be 40 lowercase hexadecimal characters")
    return value


def _git(
    repository: Path,
    *arguments: str,
    check: bool = True,
) -> subprocess.CompletedProcess[bytes]:
    command = ["git", "-C", str(repository), *arguments]
    try:
        result = subprocess.run(command, capture_output=True, check=False)
    except OSError as error:
        raise ContractError(f"cannot execute Git: {error}") from error
    if check and result.returncode != 0:
        detail = result.stderr.decode("utf-8", errors="replace").strip()
        raise ContractError(
            f"Git command failed ({' '.join(arguments)}): {detail or result.returncode}"
        )
    return result


def _git_text(repository: Path, *arguments: str) -> str:
    data = _git(repository, *arguments).stdout
    try:
        return data.decode("ascii").strip()
    except UnicodeDecodeError as error:
        raise ContractError(f"Git returned non-ASCII output for {' '.join(arguments)}") from error


def require_git_repository(repository: Path) -> None:
    if not repository.is_dir():
        raise ContractError(f"source repository is not a directory: {repository}")
    if _git(repository, "rev-parse", "--git-dir", check=False).returncode != 0:
        raise ContractError(f"source repository is not a Git repository: {repository}")


def require_object_type(repository: Path, object_id: str, expected: str, name: str) -> None:
    object_type = _git_text(repository, "cat-file", "-t", object_id)
    if object_type != expected:
        raise ContractError(f"{name} must identify a Git {expected}, found {object_type}")


def resolve_blob_oid(repository: Path, tree: str, path: str) -> str:
    result = _git(repository, "ls-tree", "-z", tree, "--", path).stdout
    records = [record for record in result.split(b"\0") if record]
    if len(records) != 1 or b"\t" not in records[0]:
        raise ContractError(f"compiled tree does not contain exactly one anchor: {path}")
    metadata, encoded_path = records[0].split(b"\t", 1)
    fields = metadata.split(b" ")
    if len(fields) != 3 or fields[1] != b"blob":
        raise ContractError(f"compiled-tree anchor is not a blob: {path}")
    try:
        actual_path = encoded_path.decode("utf-8")
        object_id = fields[2].decode("ascii")
    except UnicodeDecodeError as error:
        raise ContractError(f"compiled-tree anchor has invalid Git output: {path}") from error
    if actual_path != path:
        raise ContractError(f"compiled-tree anchor path changed: {actual_path}")
    return _require_git_sha(object_id, f"blob OID for {path}")


def read_tree_blob(repository: Path, tree: str, path: str) -> bytes:
    resolve_blob_oid(repository, tree, path)
    return _git(repository, "cat-file", "blob", f"{tree}:{path}").stdout


def source_anchor_document(repository: Path, compiled_tree: str) -> dict[str, Any]:
    require_git_repository(repository)
    _require_git_sha(compiled_tree, "compiled tree")
    require_object_type(repository, compiled_tree, "tree", "compiled tree")
    anchors = [
        {
            "path": path,
            "blobOid": {
                "grammar": "git-blob-sha1",
                "digest": resolve_blob_oid(repository, compiled_tree, path),
            },
        }
        for path in ANCHOR_PATHS
    ]
    return {"schema": SOURCE_ANCHORS_SCHEMA_ID, "anchors": anchors}


def canonical_json_bytes(value: Any) -> bytes:
    return (
        json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))
        + "\n"
    ).encode("utf-8")


def canonical_source_anchor_bytes(document: dict[str, Any]) -> bytes:
    validate_source_anchor_document(document)
    return canonical_json_bytes(document)


def validate_source_anchor_document(document: Any) -> None:
    schema = load_provider_schema()
    try:
        anchor_schema = schema["$defs"]["sourceAnchors"]
    except (KeyError, TypeError) as error:
        raise ContractError("provider schema has no sourceAnchors definition") from error
    validate_json_schema_instance(document, anchor_schema, root_schema=schema)
    paths = [anchor["path"] for anchor in document["anchors"]]
    if tuple(paths) != ANCHOR_PATHS:
        raise ContractError("source anchors are not the exact byte-sorted frozen path list")
    if paths != sorted(paths, key=lambda value: value.encode("utf-8")):
        raise ContractError("source anchors are not sorted by path byte order")


def load_canonical_source_anchor_bytes(path: Path) -> tuple[dict[str, Any], bytes]:
    try:
        data = path.read_bytes()
    except OSError as error:
        raise ContractError(f"cannot read expected anchors {path}: {error}") from error
    document = load_json_bytes(data, source=str(path))
    if not isinstance(document, dict):
        raise ContractError("source anchors must be a JSON object")
    canonical = canonical_source_anchor_bytes(document)
    if data != canonical:
        raise ContractError(
            "source anchors are not canonical UTF-8/sorted-key/compact JSON with one LF"
        )
    return document, data


def _write_exact_bytes(path: Path, data: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary_name: str | None = None
    try:
        with tempfile.NamedTemporaryFile(
            mode="wb", prefix=f".{path.name}.", dir=path.parent, delete=False
        ) as temporary:
            temporary_name = temporary.name
            temporary.write(data)
            temporary.flush()
            os.fsync(temporary.fileno())
        os.replace(temporary_name, path)
        temporary_name = None
    except OSError as error:
        raise ContractError(f"cannot write {path}: {error}") from error
    finally:
        if temporary_name is not None:
            try:
                Path(temporary_name).unlink()
            except FileNotFoundError:
                pass


def write_source_anchors(repository: Path, compiled_tree: str, output: Path) -> bytes:
    document = source_anchor_document(repository, compiled_tree)
    data = canonical_source_anchor_bytes(document)
    _write_exact_bytes(output, data)
    return data


def _walk_strings(value: Any):
    if isinstance(value, str):
        yield value
    elif isinstance(value, dict):
        for key, item in value.items():
            yield key
            yield from _walk_strings(item)
    elif isinstance(value, list):
        for item in value:
            yield from _walk_strings(item)


def validate_protocol_schema_bytes(data: bytes) -> dict[str, Any]:
    schema = load_json_bytes(data, source=PROTOCOL_SCHEMA_PATH)
    if not isinstance(schema, dict):
        raise ContractError("compiled protocol schema must be a JSON object")
    strings = set(_walk_strings(schema))
    missing = [method for method in ATTACHMENT_METHODS if method not in strings]
    if missing:
        raise ContractError(f"compiled protocol schema is missing attachment methods: {missing}")
    if PRIVATE_PROBE_TOOL in strings:
        raise ContractError("private attachment probe appears in the protocol schema")
    return schema


def parse_series_manifest(path: Path) -> tuple[str, str]:
    try:
        with path.open("rb") as source:
            manifest = tomllib.load(source)
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise ContractError(f"cannot parse series manifest {path}: {error}") from error
    base_commit = manifest.get("base_commit")
    final_tree = manifest.get("final_tree")
    if not isinstance(base_commit, str) or not isinstance(final_tree, str):
        raise ContractError("series manifest must contain base_commit and final_tree")
    return (
        _require_git_sha(base_commit, "series base_commit"),
        _require_git_sha(final_tree, "series final_tree"),
    )


def verify_source_provenance(
    repository: Path,
    *,
    upstream_commit: str,
    patched_tree: str,
    compiled_tree: str,
    series_manifest: Path | None = None,
) -> None:
    require_git_repository(repository)
    upstream_commit = _require_git_sha(upstream_commit, "upstream commit")
    patched_tree = _require_git_sha(patched_tree, "patched tree")
    compiled_tree = _require_git_sha(compiled_tree, "compiled tree")
    if patched_tree != compiled_tree:
        raise ContractError("patched tree and compiled tree differ")

    if series_manifest is not None:
        series_base, series_tree = parse_series_manifest(series_manifest)
        if series_base != upstream_commit:
            raise ContractError("series base_commit differs from supplied upstream commit")
        if series_tree != patched_tree:
            raise ContractError("series final_tree differs from supplied patched tree")

    require_object_type(repository, upstream_commit, "commit", "upstream commit")
    require_object_type(repository, patched_tree, "tree", "patched tree")
    require_object_type(repository, compiled_tree, "tree", "compiled tree")
    ancestry = _git(
        repository,
        "merge-base",
        "--is-ancestor",
        upstream_commit,
        "HEAD",
        check=False,
    )
    if ancestry.returncode == 1:
        raise ContractError("upstream commit is not an ancestor of patched HEAD")
    if ancestry.returncode != 0:
        detail = ancestry.stderr.decode("utf-8", errors="replace").strip()
        raise ContractError(f"cannot verify upstream ancestry: {detail or ancestry.returncode}")
    head_tree = _git_text(repository, "rev-parse", "HEAD^{tree}")
    if head_tree != patched_tree:
        raise ContractError(
            f"patched HEAD tree differs: expected {patched_tree}, found {head_tree}"
        )


def manifest_document(
    *,
    runtime_contract: dict[str, Any],
    schema_sha256: str,
    source_anchors_sha256: str,
    upstream_commit: str,
    source_revision: str,
    build_revision: str,
    patched_tree: str,
    compiled_tree: str,
    app_server_sha256: str,
) -> dict[str, Any]:
    return {
        "schema": MANIFEST_SCHEMA_ID,
        "runtimeContract": runtime_contract,
        "protocolSurface": {
            "schemaPath": PROTOCOL_SCHEMA_PATH,
            "schemaSha256": f"sha256:{schema_sha256}",
            "sourceAnchorsPath": SOURCE_ANCHORS_REPOSITORY_PATH,
            "sourceAnchorsSha256": f"sha256:{source_anchors_sha256}",
        },
        "source": {
            "upstreamCommit": upstream_commit,
            "sourceRevision": source_revision,
            "buildRevision": build_revision,
            "patchedTree": {"grammar": "git-tree-sha1", "digest": patched_tree},
            "compiledTree": {"grammar": "git-tree-sha1", "digest": compiled_tree},
            "patchedTreeEqualsCompiledTree": True,
        },
        "artifact": {
            "target": TARGET,
            "appServerSha256": app_server_sha256,
            "entrypoints": {
                "appServer": dict(APP_SERVER_ENTRYPOINT),
                "supervisor": {
                    **SUPERVISOR_ENTRYPOINT,
                    "argv": list(SUPERVISOR_ENTRYPOINT["argv"]),
                },
            },
        },
        "supervisor": {"contractVersion": SUPERVISOR_CONTRACT_VERSION},
    }


def _find_placeholder(value: Any, path: str = "$") -> str | None:
    if isinstance(value, str) and PLACEHOLDER_RE.search(value):
        return path
    if isinstance(value, dict):
        for key, item in value.items():
            found = _find_placeholder(item, f"{path}.{key}")
            if found is not None:
                return found
    if isinstance(value, list):
        for index, item in enumerate(value):
            found = _find_placeholder(item, f"{path}[{index}]")
            if found is not None:
                return found
    return None


def validate_manifest_document(manifest: Any) -> None:
    if not isinstance(manifest, dict):
        raise ContractError("runtime artifact manifest must be a JSON object")
    schema = load_provider_schema()
    validate_json_schema_instance(manifest, schema)
    runtime_contract = load_frozen_runtime_contract()
    if not _json_equal(manifest["runtimeContract"], runtime_contract):
        raise ContractError("manifest runtimeContract differs from frozen contract")
    _require_key_order(
        manifest["runtimeContract"], runtime_contract, "$.runtimeContract"
    )
    source = manifest["source"]
    if source["patchedTree"]["digest"] != source["compiledTree"]["digest"]:
        raise ContractError("manifest patched and compiled tree digests differ")
    if source["patchedTreeEqualsCompiledTree"] is not True:
        raise ContractError("manifest tree equality must be true")
    placeholder = _find_placeholder(manifest)
    if placeholder is not None:
        raise ContractError(f"manifest contains a placeholder at {placeholder}")


def deterministic_manifest_bytes(manifest: dict[str, Any]) -> bytes:
    validate_manifest_document(manifest)
    return (json.dumps(manifest, ensure_ascii=False, indent=2) + "\n").encode("utf-8")


def build_runtime_artifact_manifest(
    *,
    source_repository: Path,
    series_manifest: Path,
    upstream_commit: str,
    source_revision: str,
    build_revision: str,
    patched_tree: str,
    compiled_tree: str,
    app_server_executable: Path,
    expected_anchors: Path,
    output: Path,
    protocol_schema_output: Path,
) -> dict[str, Any]:
    upstream_commit = _require_git_sha(upstream_commit, "upstream commit")
    source_revision = _require_git_sha(source_revision, "source revision")
    build_revision = _require_git_sha(build_revision, "build revision")
    patched_tree = _require_git_sha(patched_tree, "patched tree")
    compiled_tree = _require_git_sha(compiled_tree, "compiled tree")
    if patched_tree != compiled_tree:
        raise ContractError("patched tree and compiled tree differ")
    if output.resolve() == protocol_schema_output.resolve():
        raise ContractError("manifest and protocol-schema evidence outputs must differ")
    if output.resolve() == expected_anchors.resolve() or (
        protocol_schema_output.resolve() == expected_anchors.resolve()
    ):
        raise ContractError("outputs must not overwrite canonical source anchors")

    runtime_contract = load_frozen_runtime_contract()
    verify_source_provenance(
        source_repository,
        upstream_commit=upstream_commit,
        patched_tree=patched_tree,
        compiled_tree=compiled_tree,
        series_manifest=series_manifest,
    )

    _anchor_document, expected_anchor_bytes = load_canonical_source_anchor_bytes(
        expected_anchors
    )
    generated_anchor_bytes = canonical_source_anchor_bytes(
        source_anchor_document(source_repository, compiled_tree)
    )
    if generated_anchor_bytes != expected_anchor_bytes:
        raise ContractError("compiled-tree source anchors differ from canonical anchors")

    protocol_schema_bytes = read_tree_blob(
        source_repository, compiled_tree, PROTOCOL_SCHEMA_PATH
    )
    validate_protocol_schema_bytes(protocol_schema_bytes)
    if not app_server_executable.is_file():
        raise ContractError(
            f"unpacked app-server executable is not a file: {app_server_executable}"
        )

    manifest = manifest_document(
        runtime_contract=runtime_contract,
        schema_sha256=sha256_bytes(protocol_schema_bytes),
        source_anchors_sha256=sha256_bytes(expected_anchor_bytes),
        upstream_commit=upstream_commit,
        source_revision=source_revision,
        build_revision=build_revision,
        patched_tree=patched_tree,
        compiled_tree=compiled_tree,
        app_server_sha256=sha256_file(app_server_executable),
    )
    manifest_bytes = deterministic_manifest_bytes(manifest)

    # Every contract/provenance check above runs before either output is changed.
    _write_exact_bytes(protocol_schema_output, protocol_schema_bytes)
    _write_exact_bytes(output, manifest_bytes)
    return manifest


def validate_real_manifest(
    *,
    source_repository: Path,
    manifest_path: Path,
    app_server_executable: Path,
    expected_anchors: Path = CANONICAL_ANCHORS_PATH,
) -> dict[str, Any]:
    try:
        manifest_bytes = manifest_path.read_bytes()
    except OSError as error:
        raise ContractError(f"cannot read manifest {manifest_path}: {error}") from error
    manifest = load_json_bytes(manifest_bytes, source=str(manifest_path))
    validate_manifest_document(manifest)
    if manifest_bytes != deterministic_manifest_bytes(manifest):
        raise ContractError("runtime artifact manifest is not deterministic indented JSON")

    source = manifest["source"]
    patched_tree = source["patchedTree"]["digest"]
    compiled_tree = source["compiledTree"]["digest"]
    verify_source_provenance(
        source_repository,
        upstream_commit=source["upstreamCommit"],
        patched_tree=patched_tree,
        compiled_tree=compiled_tree,
    )

    _document, anchor_bytes = load_canonical_source_anchor_bytes(expected_anchors)
    actual_anchor_bytes = canonical_source_anchor_bytes(
        source_anchor_document(source_repository, compiled_tree)
    )
    if anchor_bytes != actual_anchor_bytes:
        raise ContractError("manifest source anchors drifted from the compiled tree")
    expected_anchor_hash = manifest["protocolSurface"]["sourceAnchorsSha256"]
    if expected_anchor_hash != f"sha256:{sha256_bytes(anchor_bytes)}":
        raise ContractError("manifest source-anchor digest does not match canonical bytes")

    protocol_schema_bytes = read_tree_blob(
        source_repository, compiled_tree, PROTOCOL_SCHEMA_PATH
    )
    validate_protocol_schema_bytes(protocol_schema_bytes)
    expected_schema_hash = manifest["protocolSurface"]["schemaSha256"]
    if expected_schema_hash != f"sha256:{sha256_bytes(protocol_schema_bytes)}":
        raise ContractError("manifest schema digest does not match compiled-tree raw bytes")
    if not app_server_executable.is_file():
        raise ContractError(
            f"unpacked app-server executable is not a file: {app_server_executable}"
        )
    if manifest["artifact"]["appServerSha256"] != sha256_file(
        app_server_executable
    ):
        raise ContractError("manifest app-server digest does not match executable raw bytes")
    return manifest


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="operation", required=True)

    anchors = subparsers.add_parser("anchors", help="write canonical source anchors")
    anchors.add_argument("--source-repository", required=True, type=Path)
    anchors.add_argument("--compiled-tree", required=True)
    anchors.add_argument("--output", required=True, type=Path)

    manifest = subparsers.add_parser("manifest", help="write a runtime artifact manifest")
    manifest.add_argument("--source-repository", required=True, type=Path)
    manifest.add_argument("--series-manifest", required=True, type=Path)
    manifest.add_argument("--upstream-commit", required=True)
    manifest.add_argument("--source-revision", required=True)
    manifest.add_argument("--build-revision", required=True)
    manifest.add_argument("--patched-tree", required=True)
    manifest.add_argument("--compiled-tree", required=True)
    manifest.add_argument("--app-server-executable", required=True, type=Path)
    manifest.add_argument("--expected-anchors", required=True, type=Path)
    manifest.add_argument("--output", required=True, type=Path)
    manifest.add_argument("--protocol-schema-output", required=True, type=Path)
    return parser


def main(argv: list[str] | None = None) -> int:
    args = _build_parser().parse_args(argv)
    try:
        if args.operation == "anchors":
            data = write_source_anchors(
                args.source_repository, args.compiled_tree, args.output
            )
            print(f"wrote {args.output} ({sha256_bytes(data)})")
            return 0
        build_runtime_artifact_manifest(
            source_repository=args.source_repository,
            series_manifest=args.series_manifest,
            upstream_commit=args.upstream_commit,
            source_revision=args.source_revision,
            build_revision=args.build_revision,
            patched_tree=args.patched_tree,
            compiled_tree=args.compiled_tree,
            app_server_executable=args.app_server_executable,
            expected_anchors=args.expected_anchors,
            output=args.output,
            protocol_schema_output=args.protocol_schema_output,
        )
        print(f"wrote {args.output}")
        print(f"wrote {args.protocol_schema_output}")
        return 0
    except ContractError as error:
        print(f"runtime-artifact-v2: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
