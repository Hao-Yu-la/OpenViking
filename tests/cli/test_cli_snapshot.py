# Copyright (c) 2026 Beijing Volcano Engine Technology Co., Ltd.
# SPDX-License-Identifier: AGPL-3.0
"""CLI snapshot (git version control) tests."""

import re
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
        out = r["stdout"]
        assert re.match(
            r"^(Created [0-9a-f]{12}|No changes)", out
        ), f"unexpected commit stdout: {out[:200]}"


class TestSnapshotLog:
    def test_log_lists_commits(self, test_pack_uri):
        r_before = ov(["snapshot", "log", "--limit", "100"], timeout=60)
        assert r_before["exit_code"] == 0
        before_lines = [ln for ln in r_before["stdout"].splitlines() if ln.strip()]

        commit = _commit(f"log-test setup {uuid.uuid4().hex[:6]}")
        short_oid = commit["commit_oid"][:12]

        r_after = ov(["snapshot", "log", "--limit", "100"], timeout=60)
        assert r_after["exit_code"] == 0
        after_lines = [ln for ln in r_after["stdout"].splitlines() if ln.strip()]

        # The new commit's short oid must appear in the log, proving log reflects HEAD.
        assert any(short_oid in ln for ln in after_lines), (
            f"new commit {short_oid} should appear in log; "
            f"first few lines: {after_lines[:3]}"
        )
        # A single commit should add at most one row (noop commits add zero).
        delta = len(after_lines) - len(before_lines)
        assert 0 <= delta <= 1, (
            f"expected log delta of 0 or 1 after one commit, got {delta} "
            f"(before={len(before_lines)}, after={len(after_lines)})"
        )

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

    def test_show_blob_to_stdout(self, test_file_uri, tmp_path):
        commit = _commit(f"show-stdout setup {uuid.uuid4().hex[:6]}")
        oid = commit["commit_oid"]

        # Capture canonical bytes via `get` (writes the file directly — no shell echo
        # framing).
        canonical_path = tmp_path / "canonical.bin"
        r_get = ov(
            ["get", test_file_uri, str(canonical_path), "-o", "json"],
            timeout=60,
        )
        assert r_get["exit_code"] == 0, f"get failed: {r_get['stderr'][:300]}"
        expected_bytes = canonical_path.read_bytes()

        r_show = ov(["snapshot", "show", oid, "--path", test_file_uri], timeout=60)
        assert r_show["exit_code"] == 0, f"snapshot show failed: {r_show['stderr'][:300]}"

        # `ov()` returns stdout as a stripped str; the CLI's `echo_command=True` may
        # prepend the command line. Locate the blob body by suffix match: stdout must
        # end with the file's content (with the encoding `ov()` used to decode bytes).
        try:
            expected_text = expected_bytes.decode("utf-8")
        except UnicodeDecodeError:
            pytest.skip("fixture content is not utf-8; this test assumes a text blob")
        assert r_show["stdout"].endswith(expected_text.rstrip()), (
            f"snapshot show stdout did not end with canonical blob content; "
            f"tail of show stdout: {r_show['stdout'][-200:]!r} vs "
            f"expected tail: {expected_text[-200:]!r}"
        )

    def test_show_blob_to_file(self, test_file_uri, tmp_path):
        commit = _commit(f"show-blob setup {uuid.uuid4().hex[:6]}")
        oid = commit["commit_oid"]

        canonical_path = tmp_path / "canonical.bin"
        r_get = ov(
            ["get", test_file_uri, str(canonical_path), "-o", "json"],
            timeout=60,
        )
        assert r_get["exit_code"] == 0, f"get failed: {r_get['stderr'][:300]}"
        expected_bytes = canonical_path.read_bytes()

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

        contents = out_path.read_bytes()
        assert contents == expected_bytes, (
            f"out-file bytes ({len(contents)} bytes) should match canonical "
            f"({len(expected_bytes)} bytes)"
        )


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
