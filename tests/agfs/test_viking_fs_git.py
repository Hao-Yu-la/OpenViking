"""End-to-end tests for VikingFS git commit/restore/show/log Python layer.

These exercise the full path: VikingFS.commit -> AsyncAGFSClient -> Rust
RAGFSBindingClient -> GitService, plus URI<->tree-path conversion and the
double-encryption invariant called out in the design doc.
"""

from __future__ import annotations

import asyncio
import os
import secrets
import tempfile
import shutil
from pathlib import Path
from typing import Tuple

import pytest


ragfs_python = pytest.importorskip("ragfs_python")

from openviking.pyagfs.exceptions import (
    AGFSNotFoundError,
    AGFSNotSupportedError,
)
from openviking.server.identity import RequestContext, Role
from openviking.storage.viking_fs import VikingFS
from openviking_cli.session.user_id import UserIdentifier


# ----------------------------- helpers -----------------------------


def _make_ctx(account: str = "acct_t", user: str = "user1") -> RequestContext:
    return RequestContext(user=UserIdentifier(account, user), role=Role.ROOT)


def _write_workspace(tmp_root: Path) -> Tuple[Path, Path]:
    """Lay out an fs/ dir for localfs and a git/ dir for git objects; return
    (config_path, localfs_root)."""
    fs_root = tmp_root / "fs"
    git_root = tmp_root / "git"
    fs_root.mkdir(parents=True, exist_ok=True)
    git_root.mkdir(parents=True, exist_ok=True)
    cfg = tmp_root / "ragfs.toml"
    cfg.write_text(
        f"""
[git]
enabled = true
backend = "local"
default_branch = "main"
author_name = "test-bot"
author_email = "test@example.com"

[git.local]
base_dir = "{git_root}"
fsync = "off"
"""
    )
    return cfg, fs_root


def _build_client(config_path: Path, fs_root: Path):
    c = ragfs_python.RAGFSBindingClient(config_path=str(config_path))
    c.mount("localfs", "/local", {"local_dir": str(fs_root)})
    return c


# ----------------------------- fixtures -----------------------------


@pytest.fixture
def workspace():
    root = Path(tempfile.mkdtemp(prefix="ov-vfs-git-"))
    try:
        yield root
    finally:
        shutil.rmtree(root, ignore_errors=True)


@pytest.fixture
def vfs(workspace):
    cfg, fs_root = _write_workspace(workspace)
    client = _build_client(cfg, fs_root)
    return VikingFS(agfs=client)


@pytest.fixture
def vfs_disabled(workspace):
    cfg = workspace / "ragfs.toml"
    cfg.write_text(
        """
[git]
enabled = false
"""
    )
    fs_root = workspace / "fs"
    fs_root.mkdir()
    client = ragfs_python.RAGFSBindingClient(config_path=str(cfg))
    client.mount("localfs", "/local", {"local_dir": str(fs_root)})
    return VikingFS(agfs=client)


# =========================================================================
# 1. URI <-> tree path
# =========================================================================


class TestUriToTreePath:
    def test_resources_uri(self, vfs):
        ctx = _make_ctx()
        assert vfs._uri_to_tree_path("viking://resources/a.md", ctx=ctx) == "resources/a.md"
        assert (
            vfs._uri_to_tree_path("viking://resources/proj_a/docs/a.md", ctx=ctx)
            == "resources/proj_a/docs/a.md"
        )

    def test_session_uri(self, vfs):
        ctx = _make_ctx()
        assert vfs._uri_to_tree_path("viking://session", ctx=ctx) == "session"

    def test_trailing_slash_kept_as_directory(self, vfs):
        # Normalization may strip trailing slash; this is acceptable
        ctx = _make_ctx()
        out = vfs._uri_to_tree_path("viking://resources/proj_a/", ctx=ctx)
        assert out.rstrip("/") == "resources/proj_a"

    def test_internal_scope_rejected(self, vfs):
        ctx = _make_ctx()
        for uri in (
            "viking://temp/x",
            "viking://queue/y",
            "viking://upload/z",
        ):
            with pytest.raises(ValueError):
                vfs._uri_to_tree_path(uri, ctx=ctx)

    def test_root_uri_rejected(self, vfs):
        ctx = _make_ctx()
        with pytest.raises(ValueError):
            vfs._uri_to_tree_path("viking://", ctx=ctx)

    def test_tree_path_to_uri_inverse(self, vfs):
        assert vfs._tree_path_to_uri("resources/a.md") == "viking://resources/a.md"
        assert vfs._tree_path_to_uri("/resources/a.md/") == "viking://resources/a.md"

    def test_tree_path_empty_rejected(self, vfs):
        with pytest.raises(ValueError):
            vfs._tree_path_to_uri("")


# =========================================================================
# 2. commit / show / log
# =========================================================================


@pytest.mark.asyncio
class TestCommitShowLog:
    async def test_commit_then_show_roundtrip(self, vfs):
        ctx = _make_ctx()
        await vfs.write_file("viking://resources/a.md", b"hello", ctx=ctx)
        resp = await vfs.commit(
            message="initial",
            paths=["viking://resources/a.md"],
            ctx=ctx,
        )
        assert resp["result"] == "created"
        assert resp["changed"] == 1
        assert len(resp["commit_oid"]) == 40

        # show with path -> bytes
        body = await vfs.show("main", path="viking://resources/a.md", ctx=ctx)
        assert body == b"hello"

        # show without path -> commit metadata
        meta = await vfs.show("main", ctx=ctx)
        assert meta["message"].startswith("initial")
        assert meta["oid"] == resp["commit_oid"]
        assert meta["parents"] == []
        assert meta["author"]["name"] == "viking-bot"

    async def test_commit_with_paths_none_enumerates_account(self, vfs):
        ctx = _make_ctx(account="acct_full")
        await vfs.write_file("viking://resources/a.md", b"a", ctx=ctx)
        await vfs.write_file("viking://resources/b.md", b"b", ctx=ctx)
        resp = await vfs.commit(message="all", ctx=ctx)
        assert resp["result"] == "created"
        assert resp["changed"] == 2

    async def test_log_walks_parent_chain(self, vfs):
        ctx = _make_ctx(account="acct_log")
        await vfs.write_file("viking://resources/a.md", b"v1", ctx=ctx)
        c1 = await vfs.commit(message="c1", paths=["viking://resources/a.md"], ctx=ctx)
        await vfs.write_file("viking://resources/a.md", b"v2", ctx=ctx)
        c2 = await vfs.commit(message="c2", paths=["viking://resources/a.md"], ctx=ctx)
        await vfs.write_file("viking://resources/a.md", b"v3", ctx=ctx)
        c3 = await vfs.commit(message="c3", paths=["viking://resources/a.md"], ctx=ctx)

        history = await vfs.log(limit=10, ctx=ctx)
        oids = [h["oid"] for h in history]
        assert oids == [c3["commit_oid"], c2["commit_oid"], c1["commit_oid"]]

        limited = await vfs.log(limit=2, ctx=ctx)
        assert [h["oid"] for h in limited] == [c3["commit_oid"], c2["commit_oid"]]

    async def test_show_missing_branch_raises(self, vfs):
        ctx = _make_ctx(account="acct_missing")
        with pytest.raises(AGFSNotFoundError):
            await vfs.show("main", ctx=ctx)


# =========================================================================
# 3. restore
# =========================================================================


@pytest.mark.asyncio
class TestRestore:
    async def test_restore_reverts_file_and_advances_head(self, vfs):
        ctx = _make_ctx(account="acct_r")
        await vfs.write_file("viking://resources/proj/a.md", b"v1", ctx=ctx)
        c1 = await vfs.commit(message="v1", paths=["viking://resources/proj/a.md"], ctx=ctx)

        await vfs.write_file("viking://resources/proj/a.md", b"v2", ctx=ctx)
        c2 = await vfs.commit(message="v2", paths=["viking://resources/proj/a.md"], ctx=ctx)

        result = await vfs.restore(
            project_dir="viking://resources/proj",
            source_commit=c1["commit_oid"],
            ctx=ctx,
        )
        assert result["result"] == "applied"
        assert result["source_commit"] == c1["commit_oid"]
        assert result["parent_commit"] == c2["commit_oid"]
        assert result["new_commit_oid"] != c2["commit_oid"]
        assert "resources/proj/a.md" in result["written_paths"]

        # File reverted via VFS
        body = await vfs.read("viking://resources/proj/a.md", ctx=ctx)
        assert body == b"v1"

        # HEAD moved forward (NOT back to c1)
        head = await vfs.show("main", ctx=ctx)
        assert head["oid"] == result["new_commit_oid"]
        assert head["parents"] == [c2["commit_oid"]]

    async def test_restore_dry_run_does_not_mutate(self, vfs):
        ctx = _make_ctx(account="acct_dry")
        await vfs.write_file("viking://resources/proj/a.md", b"v1", ctx=ctx)
        c1 = await vfs.commit(message="v1", paths=["viking://resources/proj/a.md"], ctx=ctx)
        await vfs.write_file("viking://resources/proj/a.md", b"v2", ctx=ctx)
        await vfs.commit(message="v2", paths=["viking://resources/proj/a.md"], ctx=ctx)

        result = await vfs.restore(
            project_dir="viking://resources/proj",
            source_commit=c1["commit_oid"],
            dry_run=True,
            ctx=ctx,
        )
        assert result["result"] == "dry_run"
        assert any(item["path"] == "a.md" for item in result["diff"]["to_write"])

        body = await vfs.read("viking://resources/proj/a.md", ctx=ctx)
        assert body == b"v2"

    async def test_restore_internal_scope_rejected(self, vfs):
        ctx = _make_ctx(account="acct_inv")
        with pytest.raises(ValueError):
            await vfs.restore(
                project_dir="viking://temp/xx",
                source_commit="main",
                ctx=ctx,
            )


# =========================================================================
# 4. Cross-scope atomicity (resources + user in one commit)
# =========================================================================


@pytest.mark.asyncio
async def test_cross_scope_atomic_commit_and_restore(vfs):
    ctx = _make_ctx(account="acct_cross")
    # Two files in distinct scopes
    await vfs.write_file("viking://resources/a.md", b"R1", ctx=ctx)
    await vfs.write_file("viking://session/b.py", b"S1", ctx=ctx)
    c1 = await vfs.commit(
        message="initial",
        paths=["viking://resources/a.md", "viking://session/b.py"],
        ctx=ctx,
    )
    assert c1["result"] == "created"
    assert c1["changed"] == 2

    # Both files modified
    await vfs.write_file("viking://resources/a.md", b"R2", ctx=ctx)
    await vfs.write_file("viking://session/b.py", b"S2", ctx=ctx)
    await vfs.commit(
        message="v2",
        paths=["viking://resources/a.md", "viking://session/b.py"],
        ctx=ctx,
    )

    # Restore only the resources scope to c1; session scope must remain at v2
    await vfs.restore(
        project_dir="viking://resources",
        source_commit=c1["commit_oid"],
        ctx=ctx,
    )
    assert await vfs.read("viking://resources/a.md", ctx=ctx) == b"R1"
    assert await vfs.read("viking://session/b.py", ctx=ctx) == b"S2"

    # Restore the session scope too -> both back to c1
    await vfs.restore(
        project_dir="viking://session",
        source_commit=c1["commit_oid"],
        ctx=ctx,
    )
    assert await vfs.read("viking://resources/a.md", ctx=ctx) == b"R1"
    assert await vfs.read("viking://session/b.py", ctx=ctx) == b"S1"


# =========================================================================
# 5. Derived files (.abstract.md etc.) versioned with source
# =========================================================================


@pytest.mark.asyncio
async def test_derived_files_versioned_with_source(vfs):
    ctx = _make_ctx(account="acct_derived")
    await vfs.write_file("viking://resources/x.md", b"x-body", ctx=ctx)
    await vfs.write_file("viking://resources/x.md.abstract.md", b"abstract-v1", ctx=ctx)
    c1 = await vfs.commit(message="v1", ctx=ctx)
    assert c1["result"] == "created"
    assert c1["changed"] == 2

    # show finds both
    assert await vfs.show("main", path="viking://resources/x.md.abstract.md", ctx=ctx) == b"abstract-v1"

    # Update derived file
    await vfs.write_file("viking://resources/x.md.abstract.md", b"abstract-v2", ctx=ctx)
    await vfs.commit(message="v2", paths=["viking://resources/x.md.abstract.md"], ctx=ctx)

    # Restore to c1 -> derived file reverts too
    await vfs.restore(
        project_dir="viking://resources",
        source_commit=c1["commit_oid"],
        ctx=ctx,
    )
    body = await vfs.read("viking://resources/x.md.abstract.md", ctx=ctx)
    assert body == b"abstract-v1"


# =========================================================================
# 6. Account isolation
# =========================================================================


@pytest.mark.asyncio
async def test_account_isolation_show_misses_other_account(vfs):
    ctx_a = _make_ctx(account="acct_iso_a")
    ctx_b = _make_ctx(account="acct_iso_b")
    await vfs.write_file("viking://resources/a.md", b"a", ctx=ctx_a)
    await vfs.commit(message="m", paths=["viking://resources/a.md"], ctx=ctx_a)

    with pytest.raises(AGFSNotFoundError):
        await vfs.show("main", ctx=ctx_b)


# =========================================================================
# 7. Double-encryption end-to-end (the §3.1 invariant)
# =========================================================================


@pytest.fixture
def encryptor(workspace):
    from openviking.crypto.encryptor import FileEncryptor
    from openviking.crypto.providers import LocalFileProvider

    key_file = workspace / "master.key"
    key_file.write_text(secrets.token_bytes(32).hex())
    os.chmod(key_file, 0o600)
    provider = LocalFileProvider(key_file=str(key_file))
    return FileEncryptor(provider)


@pytest.fixture
def vfs_encrypted(workspace, encryptor):
    cfg, fs_root = _write_workspace(workspace)
    client = _build_client(cfg, fs_root)
    return VikingFS(agfs=client, encryptor=encryptor)


@pytest.mark.asyncio
async def test_double_encryption_restore_preserves_plaintext(vfs_encrypted):
    """Write plaintext via encrypted VikingFS, commit (ciphertext stored in
    git), modify, restore. After restore, VikingFS.read MUST return the
    original plaintext — proving the Rust restore path bypasses the
    VikingFS encryption layer (writes ciphertext back through MountableFS,
    which then decrypts correctly on read).
    """
    ctx = _make_ctx(account="acct_enc")
    plaintext_v1 = b"top-secret-v1"
    plaintext_v2 = b"top-secret-v2"

    await vfs_encrypted.write_file("viking://resources/secret.md", plaintext_v1, ctx=ctx)
    c1 = await vfs_encrypted.commit(
        message="v1", paths=["viking://resources/secret.md"], ctx=ctx,
    )
    assert c1["result"] == "created"

    # Modify
    await vfs_encrypted.write_file("viking://resources/secret.md", plaintext_v2, ctx=ctx)
    await vfs_encrypted.commit(
        message="v2", paths=["viking://resources/secret.md"], ctx=ctx,
    )
    assert (
        await vfs_encrypted.read("viking://resources/secret.md", ctx=ctx)
        == plaintext_v2
    )

    # Restore
    result = await vfs_encrypted.restore(
        project_dir="viking://resources",
        source_commit=c1["commit_oid"],
        ctx=ctx,
    )
    assert result["result"] == "applied"
    assert "resources/secret.md" in result["written_paths"]

    # The critical assertion: read returns original plaintext, not garbled
    # double-encrypted bytes.
    restored = await vfs_encrypted.read("viking://resources/secret.md", ctx=ctx)
    assert restored == plaintext_v1


# =========================================================================
# 8. Feature disabled
# =========================================================================


@pytest.mark.asyncio
async def test_feature_disabled_raises_not_supported(vfs_disabled):
    ctx = _make_ctx()
    with pytest.raises(AGFSNotSupportedError):
        await vfs_disabled.commit(message="m", paths=["viking://resources/a.md"], ctx=ctx)
    with pytest.raises(AGFSNotSupportedError):
        await vfs_disabled.show("main", ctx=ctx)
    with pytest.raises(AGFSNotSupportedError):
        await vfs_disabled.restore(
            project_dir="viking://resources/proj",
            source_commit="main",
            ctx=ctx,
        )


# =========================================================================
# 9. Reindex redirect for derived files
# =========================================================================


def test_reindex_uri_for_tree_path_redirects_derived(vfs):
    # Per-file sidecars redirect to parent directory
    assert (
        vfs._reindex_uri_for_tree_path("resources/proj/x.md.abstract.md")
        == "viking://resources/proj"
    )
    assert (
        vfs._reindex_uri_for_tree_path("resources/proj/x.md.overview.md")
        == "viking://resources/proj"
    )
    # Directory-level markers redirect to the directory itself
    assert (
        vfs._reindex_uri_for_tree_path("resources/proj/.abstract.md")
        == "viking://resources/proj"
    )
    assert (
        vfs._reindex_uri_for_tree_path("resources/proj/.overview.md")
        == "viking://resources/proj"
    )
    assert (
        vfs._reindex_uri_for_tree_path("resources/proj/.relations.json")
        == "viking://resources/proj"
    )
    # Source files pass through unchanged
    assert (
        vfs._reindex_uri_for_tree_path("resources/proj/x.md")
        == "viking://resources/proj/x.md"
    )
    # Derived file with no parent -> None (defensive)
    assert vfs._reindex_uri_for_tree_path(".abstract.md") is None


@pytest.mark.asyncio
async def test_restore_schedules_reindex_for_derived_only_change(vfs, monkeypatch):
    """When a restore only changes a `.abstract.md` (source file unchanged),
    the parent directory URI must still be scheduled for reindex so the
    L0/L1 vectors get refreshed from the restored on-disk content.
    """
    calls: list[str] = []

    class _SpyExecutor:
        async def execute(self, *, uri, mode, wait, ctx):
            calls.append(uri)
            return {"ok": True}

    import openviking.service.reindex_executor as reindex_mod
    monkeypatch.setattr(reindex_mod, "ReindexExecutor", _SpyExecutor)

    ctx = _make_ctx(account="acct_derived_only")
    await vfs.write_file("viking://resources/proj/x.md", b"body", ctx=ctx)
    await vfs.write_file(
        "viking://resources/proj/x.md.abstract.md", b"abs-v1", ctx=ctx
    )
    c1 = await vfs.commit(message="v1", ctx=ctx)
    assert c1["result"] == "created"

    # Modify ONLY the derived sidecar; source file untouched
    await vfs.write_file(
        "viking://resources/proj/x.md.abstract.md", b"abs-v2", ctx=ctx
    )
    c2 = await vfs.commit(
        message="v2",
        paths=["viking://resources/proj/x.md.abstract.md"],
        ctx=ctx,
    )
    assert c2["result"] == "created"
    assert c2["changed"] == 1

    result = await vfs.restore(
        project_dir="viking://resources/proj",
        source_commit=c1["commit_oid"],
        ctx=ctx,
    )
    assert result["result"] == "applied"
    assert "resources/proj/x.md.abstract.md" in result["written_paths"]

    # Let the fire-and-forget tasks run
    await asyncio.sleep(0)
    await asyncio.sleep(0)

    assert calls == ["viking://resources/proj"]


@pytest.mark.asyncio
async def test_restore_schedules_reindex_dedupes_source_and_sidecar(vfs, monkeypatch):
    """Source file + its sidecar in the same restore -> two distinct URIs
    (one for the file's DETAIL, one for the parent dir's L0/L1), no dup.
    """
    calls: list[str] = []

    class _SpyExecutor:
        async def execute(self, *, uri, mode, wait, ctx):
            calls.append(uri)
            return {"ok": True}

    import openviking.service.reindex_executor as reindex_mod
    monkeypatch.setattr(reindex_mod, "ReindexExecutor", _SpyExecutor)

    ctx = _make_ctx(account="acct_dedup")
    await vfs.write_file("viking://resources/proj/x.md", b"v1", ctx=ctx)
    await vfs.write_file(
        "viking://resources/proj/x.md.abstract.md", b"a-v1", ctx=ctx
    )
    await vfs.write_file(
        "viking://resources/proj/y.md.abstract.md", b"b-v1", ctx=ctx
    )
    c1 = await vfs.commit(message="v1", ctx=ctx)

    await vfs.write_file("viking://resources/proj/x.md", b"v2", ctx=ctx)
    await vfs.write_file(
        "viking://resources/proj/x.md.abstract.md", b"a-v2", ctx=ctx
    )
    await vfs.write_file(
        "viking://resources/proj/y.md.abstract.md", b"b-v2", ctx=ctx
    )
    await vfs.commit(message="v2", ctx=ctx)

    await vfs.restore(
        project_dir="viking://resources/proj",
        source_commit=c1["commit_oid"],
        ctx=ctx,
    )
    await asyncio.sleep(0)
    await asyncio.sleep(0)

    # Two distinct sidecars in same dir -> dir URI appears once; source file
    # adds its own URI.
    assert sorted(calls) == sorted([
        "viking://resources/proj",
        "viking://resources/proj/x.md",
    ])
