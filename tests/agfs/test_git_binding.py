"""End-to-end tests for the git_commit/git_restore/git_show PyO3 bindings.

These tests exercise the binding through ragfs_python.RAGFSBindingClient
directly so they don't require the higher-level VikingFS layer.
"""

import shutil
import tempfile
from pathlib import Path

import pytest


# Skip the whole module if the native extension is not built locally.
ragfs_python = pytest.importorskip("ragfs_python")


# ---------------- fixtures ----------------


@pytest.fixture
def git_workspace():
    """Create a temp workspace containing a localfs root and a [git] config TOML.

    Yields (config_path, localfs_root, git_root) and removes the dir on exit.
    """
    root = Path(tempfile.mkdtemp(prefix="ov-git-binding-"))
    localfs_root = root / "fs"
    git_root = root / "git"
    localfs_root.mkdir()
    git_root.mkdir()

    config_path = root / "ragfs.toml"
    config_path.write_text(
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

    yield config_path, localfs_root, git_root

    shutil.rmtree(root, ignore_errors=True)


@pytest.fixture
def git_disabled_workspace():
    """A workspace whose [git] section has enabled = false."""
    root = Path(tempfile.mkdtemp(prefix="ov-git-disabled-"))
    config_path = root / "ragfs.toml"
    config_path.write_text(
        """
[git]
enabled = false
"""
    )
    yield config_path
    shutil.rmtree(root, ignore_errors=True)


@pytest.fixture
def client(git_workspace):
    config_path, localfs_root, _ = git_workspace
    c = ragfs_python.RAGFSBindingClient(config_path=str(config_path))
    # Mount localfs at /local so we can write files into the account tree.
    c.mount("localfs", "/local", {"local_dir": str(localfs_root)})
    return c


# ---------------- helper: write a file into account tree ----------------


def _write(client, account: str, rel_path: str, body: bytes) -> str:
    """Write `body` to /local/<account>/<rel_path> via the binding."""
    path = f"/local/{account}/{rel_path}"
    client.ensure_parent_dirs(path)
    client.write(path, body)
    return path


# ---------------- tests ----------------


def test_git_concurrent_commit_error_class_exists():
    from openviking.pyagfs import GitConcurrentCommitError
    from openviking.pyagfs.exceptions import AGFSClientError
    assert issubclass(GitConcurrentCommitError, AGFSClientError)


def test_health_reports_git_enabled(client):
    h = client.health()
    assert h["git_enabled"] == "true"
    assert h.get("git_backend") == "local"


def test_commit_then_show_roundtrip(client):
    """Write a file, commit it, then show it back and verify bytes match."""
    account = "acct1"
    _write(client, account, "resources/a.md", b"hello world")

    resp = client.git_commit(
        account=account,
        branch="main",
        message="initial",
        author_name="alice",
        author_email="a@e.com",
        paths=["resources/a.md"],
    )
    assert resp["result"] == "created"
    assert resp["changed"] == 1
    commit_oid = resp["commit_oid"]
    assert len(commit_oid) == 40

    shown = client.git_show(
        account=account,
        target_ref="main",
        path="resources/a.md",
    )
    assert shown["bytes"] == b"hello world"
    assert shown["size"] == 11


def test_commit_then_show_commit_metadata(client):
    account = "acct1"
    _write(client, account, "resources/a.md", b"x")
    resp = client.git_commit(
        account=account,
        branch="main",
        message="m1",
        author_name="alice",
        author_email="a@e.com",
        paths=["resources/a.md"],
    )
    meta = client.git_show(account=account, target_ref="main")
    assert meta["message"].startswith("m1")
    assert meta["oid"] == resp["commit_oid"]
    assert meta["parents"] == []
    assert meta["author"]["name"] == "alice"
