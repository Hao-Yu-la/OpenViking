"""Unit tests for GitConfig pydantic model."""
import pytest
from pydantic import ValidationError

from openviking_cli.utils.config.git_config import GitConfig, GitLocalConfig


class TestGitConfigDefaults:
    def test_disabled_by_default(self):
        cfg = GitConfig()
        assert cfg.enabled is False
        assert cfg.backend == "local"
        assert cfg.default_branch == "main"
        assert cfg.author_name == "viking-bot"
        assert cfg.author_email == "bot@viking.local"

    def test_local_subconfig_defaults(self):
        cfg = GitConfig()
        assert isinstance(cfg.local, GitLocalConfig)
        assert cfg.local.base_dir == ""
        assert cfg.local.fsync == "off"


class TestGitConfigValidation:
    def test_invalid_backend_rejected(self):
        with pytest.raises(ValidationError):
            GitConfig(backend="ftp")

    def test_unknown_field_rejected(self):
        with pytest.raises(ValidationError):
            GitConfig(unknown_thing=True)

    def test_enabled_with_local_backend_ok(self):
        cfg = GitConfig(enabled=True, backend="local", local=GitLocalConfig(base_dir="/tmp/git"))
        assert cfg.enabled is True
        assert cfg.local.base_dir == "/tmp/git"

    def test_fsync_value(self):
        cfg = GitConfig(local=GitLocalConfig(fsync="on"))
        assert cfg.local.fsync == "on"
