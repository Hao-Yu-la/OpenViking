"""Tests for git wiring in create_agfs_client.

Verifies that when GitConfig.enabled is True, create_agfs_client generates
a ragfs.toml at {storage_path}/.runtime/ragfs.toml with the right [git]
section and passes config_path to RAGFSBindingClient. When git is disabled
(or git_config is None), the old no-arg construction path is preserved.
"""
from pathlib import Path
from unittest.mock import patch, MagicMock

import pytest

from openviking_cli.utils.config import GitConfig, GitLocalConfig
from openviking.utils.agfs_utils import create_agfs_client


class _FakeAgfsConfig:
    """Minimal stand-in for StorageConfig.agfs — only what mount needs."""
    def __init__(self, path):
        self.path = str(path)
        self.backend = "local"
        self.s3 = None
        # queuefs default
        from types import SimpleNamespace
        self.queuefs = SimpleNamespace(
            backend="sqlite", recover_stale_sec=0, busy_timeout_ms=5000, db_path=None
        )


@pytest.fixture
def agfs_config(tmp_path):
    return _FakeAgfsConfig(tmp_path / "data")


@pytest.fixture
def fake_binding(monkeypatch):
    """Stub out RAGFSBindingClient to capture constructor kwargs."""
    instances = []

    class _FakeClient:
        def __init__(self, *args, **kwargs):
            self.args = args
            self.kwargs = kwargs
            instances.append(self)

        def mount(self, *a, **k):
            pass

        def unmount(self, *a, **k):
            pass

    from openviking import pyagfs as pyagfs_mod
    monkeypatch.setattr(
        pyagfs_mod, "get_binding_client", lambda: (_FakeClient, None)
    )
    return instances


def test_git_disabled_keeps_no_arg_construction(agfs_config, fake_binding):
    """git_config=None or disabled → RAGFSBindingClient() with no args."""
    client = create_agfs_client(agfs_config)
    assert len(fake_binding) == 1
    assert fake_binding[0].kwargs == {}
    assert fake_binding[0].args == ()


def test_git_disabled_explicit_keeps_no_arg_construction(agfs_config, fake_binding):
    """An explicitly-disabled GitConfig is equivalent to None."""
    cfg = GitConfig(enabled=False)
    client = create_agfs_client(agfs_config, git_config=cfg)
    assert fake_binding[0].kwargs == {}


def test_git_enabled_writes_toml_and_passes_config_path(agfs_config, fake_binding, tmp_path):
    """enabled=True → writes ragfs.toml under .runtime/, passes config_path kwarg."""
    cfg = GitConfig(
        enabled=True,
        backend="local",
        default_branch="main",
        author_name="viking-bot",
        author_email="bot@viking.local",
        local=GitLocalConfig(base_dir=str(tmp_path / "git"), fsync="off"),
    )
    client = create_agfs_client(agfs_config, git_config=cfg)

    # config_path was passed to the binding
    kwargs = fake_binding[0].kwargs
    assert "config_path" in kwargs
    toml_path = Path(kwargs["config_path"])
    assert toml_path.exists()
    assert toml_path.parent.name == ".runtime"
    assert toml_path.name == "ragfs.toml"
    # Lives under storage path, not in tmp_path root
    assert str(toml_path).startswith(str(Path(agfs_config.path).resolve()))

    body = toml_path.read_text()
    assert "[git]" in body
    assert "enabled = true" in body
    assert 'backend = "local"' in body
    assert 'author_name = "viking-bot"' in body
    assert "[git.local]" in body
    assert str(tmp_path / "git") in body


def test_git_enabled_with_empty_base_dir_defaults_to_storage_git(agfs_config, fake_binding):
    """When local.base_dir is empty, the generated toml should fill it with {storage_path}/git."""
    cfg = GitConfig(enabled=True, local=GitLocalConfig(base_dir="", fsync="off"))
    create_agfs_client(agfs_config, git_config=cfg)
    body = Path(fake_binding[0].kwargs["config_path"]).read_text()
    expected = str(Path(agfs_config.path).resolve() / "git")
    assert expected in body
