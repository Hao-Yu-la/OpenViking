"""Unit tests for GitConfig pydantic model."""
import json

import pytest
from pydantic import ValidationError

from openviking_cli.utils.config import GitConfig, GitLocalConfig, OpenVikingConfig


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


class TestGitConfigOnOpenVikingConfig:
    def test_open_viking_config_has_git_field_with_default(self):
        cfg = OpenVikingConfig(storage={"workspace": "/tmp/x"})
        assert isinstance(cfg.git, GitConfig)
        assert cfg.git.enabled is False
        assert cfg.git.backend == "local"
        assert isinstance(cfg.git.local, GitLocalConfig)

    def test_open_viking_config_accepts_git_section(self):
        cfg = OpenVikingConfig(
            storage={"workspace": "/tmp/x"},
            git={
                "enabled": True,
                "backend": "local",
                "local": {"base_dir": "/tmp/g", "fsync": "on"},
            },
        )
        assert cfg.git.enabled is True
        assert cfg.git.local.base_dir == "/tmp/g"
        assert cfg.git.local.fsync == "on"

    def test_git_config_round_trip_via_config_file(self, tmp_path, monkeypatch):
        """Round-trip a git section through OpenVikingConfig file loader.

        NOTE: `_load_from_file` in this project uses json.loads (not toml),
        so the on-disk file is JSON. We exercise the same load path the
        runtime uses, just to confirm the new `git` field survives the round trip.

        The loader lives on OpenVikingConfigSingleton (not on OpenVikingConfig
        itself); we call it directly to exercise the real file-load path.
        """
        from openviking_cli.utils.config.open_viking_config import (
            OpenVikingConfigSingleton,
        )

        cfg_dict = {
            "storage": {"workspace": str(tmp_path / "data")},
            "git": {
                "enabled": True,
                "backend": "local",
                "default_branch": "main",
                "author_name": "viking-bot",
                "author_email": "bot@viking.local",
                "local": {"base_dir": str(tmp_path / "git"), "fsync": "off"},
            },
        }
        cfg_path = tmp_path / "ov.conf"
        cfg_path.write_text(json.dumps(cfg_dict))

        if hasattr(OpenVikingConfig, "from_file"):
            cfg = OpenVikingConfig.from_file(str(cfg_path))
        else:
            cfg = OpenVikingConfigSingleton._load_from_file(str(cfg_path))

        assert cfg.git.enabled is True
        assert cfg.git.local.base_dir == str(tmp_path / "git")
