# Copyright (c) 2026 Beijing Volcano Engine Technology Co., Ltd.
# SPDX-License-Identifier: AGPL-3.0
"""CLI snapshot (git version control) tests."""

import json
import os
import tempfile
import time
import uuid

import pytest
from conftest import ov

pytestmark = pytest.mark.cli_remote


def _commit(message: str):
    """Helper: take a snapshot, return the full envelope dict.

    Retries briefly on transient server busy errors.
    """
    r = None
    for _attempt in range(5):
        r = ov(["snapshot", "commit", "-m", message, "-o", "json"], timeout=120)
        if r["exit_code"] == 0:
            break
        if "busy" in r["stderr"].lower() or "internal" in r["stderr"].lower():
            time.sleep(10)
            continue
        break
    assert r["exit_code"] == 0, f"snapshot commit failed: {r['stderr'][:300]}"
    assert r["json"] is not None, f"expected JSON, got: {r['stdout'][:200]}"
    assert r["json"].get("ok") is True, f"expected ok=true, got {r['json']}"
    return r["json"]["result"]


class TestSnapshotCommit:
    def test_commit_returns_oid_json(self, test_pack_uri):
        # test_pack_uri ensures at least one resource exists
        result = _commit(f"cli-test commit {uuid.uuid4().hex[:6]}")
        assert "commit_oid" in result, f"expected commit_oid in result, got {result}"
        assert isinstance(result["commit_oid"], str) and len(result["commit_oid"]) >= 12

    def test_commit_human_prints_short_oid(self, test_pack_uri):
        msg = f"cli-test human {uuid.uuid4().hex[:6]}"
        r = ov(["snapshot", "commit", "-m", msg], timeout=120)
        assert r["exit_code"] == 0, f"snapshot commit failed: {r['stderr'][:300]}"
        # Either "Created <12-char-hex> (N files changed)" or "No changes" or just the oid
        out = r["stdout"]
        assert (
            "Created" in out
            or "No changes" in out
            or any(c in "0123456789abcdef" for c in out[:1])
        ), f"unexpected commit stdout: {out[:200]}"


class TestSnapshotLog:
    def test_log_lists_commits(self, test_pack_uri):
        _commit(f"log-test setup {uuid.uuid4().hex[:6]}")
        r = ov(["snapshot", "log", "--limit", "20"], timeout=60)
        assert r["exit_code"] == 0, f"snapshot log failed: {r['stderr'][:300]}"
        # Human output: each line starts with a 12-char short oid followed by two spaces.
        lines = [ln for ln in r["stdout"].splitlines() if ln.strip()]
        assert len(lines) >= 1, f"expected at least one log entry, got: {r['stdout'][:300]}"

    def test_log_json_returns_array(self, test_pack_uri):
        _commit(f"log-json setup {uuid.uuid4().hex[:6]}")
        r = ov(["snapshot", "log", "--limit", "5", "-o", "json"], timeout=60)
        assert r["exit_code"] == 0, f"snapshot log -o json failed: {r['stderr'][:300]}"
        # Server returns {"ok": true, "result": [...]}, so r["json"] works
        assert r["json"] is not None, f"expected JSON, got: {r['stdout'][:200]}"
        assert r["json"].get("ok") is True
        result = r["json"]["result"]
        assert isinstance(result, list), f"expected list, got {type(result).__name__}: {result}"
        assert len(result) >= 1
        first = result[0]
        assert "oid" in first and "message" in first


class TestSnapshotShow:
    def test_show_metadata(self, test_pack_uri):
        commit = _commit(f"show-meta setup {uuid.uuid4().hex[:6]}")
        oid = commit["commit_oid"]
        r = ov(["snapshot", "show", oid, "-o", "json"], timeout=60)
        assert r["exit_code"] == 0, f"snapshot show failed: {r['stderr'][:300]}"
        assert r["json"] is not None and r["json"].get("ok") is True
        meta = r["json"]["result"]
        assert meta.get("oid") == oid or meta.get("oid", "").startswith(oid[:12])
        assert "tree" in meta and "author" in meta

    def test_show_blob_to_file(self, test_file_uri, tmp_path):
        commit = _commit(f"show-blob setup {uuid.uuid4().hex[:6]}")
        oid = commit["commit_oid"]
        out_path = tmp_path / "blob.bin"
        r = ov(
            [
                "snapshot",
                "show",
                oid,
                "--path",
                test_file_uri,
                "--out-file",
                str(out_path),
            ],
            timeout=60,
        )
        assert r["exit_code"] == 0, f"snapshot show --out-file failed: {r['stderr'][:300]}"
        assert out_path.exists(), f"out-file {out_path} should exist"
        assert out_path.stat().st_size > 0, "out-file should be non-empty"


class TestSnapshotRestore:
    def test_restore_dry_run_does_not_mutate(self, test_pack_uri):
        # Capture ls before
        ls_before = ov(["ls", "viking://resources", "-r", "-o", "json", "-n", "50"], timeout=60)
        assert ls_before["exit_code"] == 0

        commit = _commit(f"restore-dry setup {uuid.uuid4().hex[:6]}")
        oid = commit["commit_oid"]
        r = ov(
            [
                "snapshot",
                "restore",
                "viking://resources",
                oid,
                "--dry-run",
                "-o",
                "json",
            ],
            timeout=60,
        )
        assert r["exit_code"] == 0, f"snapshot restore --dry-run failed: {r['stderr'][:300]}"
        assert r["json"] is not None and r["json"].get("ok") is True
        result = r["json"]["result"]
        # Dry-run shape includes a "diff" key
        assert "diff" in result, f"expected diff in dry-run result, got keys: {list(result.keys())}"

        # ls should be unchanged after dry-run
        ls_after = ov(["ls", "viking://resources", "-r", "-o", "json", "-n", "50"], timeout=60)
        assert ls_after["exit_code"] == 0
        # Compare result lists if both present
        if ls_before["json"] and ls_after["json"]:
            assert ls_before["json"].get("result") == ls_after["json"].get("result"), (
                "ls output should be unchanged after dry-run restore"
            )
