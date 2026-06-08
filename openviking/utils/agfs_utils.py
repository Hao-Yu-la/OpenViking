# Copyright (c) 2026 Beijing Volcano Engine Technology Co., Ltd.
# SPDX-License-Identifier: AGPL-3.0
"""
RAGFS Client utilities for creating and configuring RAGFS clients.
"""

import multiprocessing
import os
from pathlib import Path
from typing import Any, Dict

from openviking_cli.utils.logger import get_logger

logger = get_logger(__name__)


def resolve_queuefs_mount_point(config: Any = None) -> str:
    """Resolve QueueFS mount point for the current process.

    `shared` keeps the historical global queue root (`/queue`).
    `worker` isolates each worker under `/queue/worker-<index|pid>`.
    """
    mode = None
    if config is not None:
        storage = getattr(config, "storage", None)
        if storage is None and hasattr(config, "agfs"):
            storage = config
        agfs = getattr(storage, "agfs", None) if storage is not None else None
        queuefs = getattr(agfs, "queuefs", None) if agfs is not None else None
        mode = getattr(queuefs, "mode", None)

    if not mode:
        try:
            from openviking_cli.utils.config import get_openviking_config

            mode = get_openviking_config().storage.agfs.queuefs.mode
        except Exception:
            mode = "shared"

    if mode == "worker":
        identity = getattr(multiprocessing.current_process(), "_identity", ())
        if identity:
            worker_id = str(identity[0] - 1)
        else:
            worker_id = str(os.getpid())
        return f"/queue/worker-{worker_id}"
    return "/queue"


def _build_queuefs_plugin_config(agfs_config: Any, data_path: Path) -> Dict[str, Any]:
    """Build QueueFS plugin configuration from AGFS config with legacy compatibility."""
    default_queue_db_path = data_path / "_system" / "queue" / "queue.db"
    queuefs_config = getattr(agfs_config, "queuefs", None)

    backend = getattr(queuefs_config, "backend", "sqlite") if queuefs_config else "sqlite"
    plugin_config: Dict[str, Any] = {
        "backend": backend,
        "recover_stale_sec": getattr(queuefs_config, "recover_stale_sec", 0),
        "busy_timeout_ms": getattr(queuefs_config, "busy_timeout_ms", 5000),
    }

    if backend in {"sqlite", "sqlite3"}:
        configured_queue_db_path = None
        if queuefs_config is not None:
            configured_queue_db_path = getattr(queuefs_config, "db_path", None)
        if not configured_queue_db_path:
            configured_queue_db_path = getattr(agfs_config, "queue_db_path", None)

        if configured_queue_db_path:
            queue_db_path = str(Path(configured_queue_db_path).expanduser().resolve())
        else:
            queue_db_path = str(default_queue_db_path)

        plugin_config["db_path"] = queue_db_path

    return plugin_config


def _generate_plugin_config(agfs_config: Any, data_path: Path) -> Dict[str, Any]:
    """Dynamically generate RAGFS plugin configuration based on backend type."""
    config = {
        "serverinfofs": {
            "enabled": True,
            "path": "/serverinfo",
            "config": {
                "version": "1.0.0",
            },
        },
        "queuefs": {
            "enabled": True,
            "path": "/queue",
            "config": _build_queuefs_plugin_config(agfs_config, data_path),
        },
    }

    backend = getattr(agfs_config, "backend", "local")
    s3_config = getattr(agfs_config, "s3", None)
    vikingfs_path = data_path / "viking"

    if backend == "local":
        config["localfs"] = {
            "enabled": True,
            "path": "/local",
            "config": {
                "local_dir": str(vikingfs_path),
            },
        }
    elif backend == "s3" and s3_config:
        s3_plugin_config = {
            "bucket": s3_config.bucket,
            "region": s3_config.region,
            "access_key_id": s3_config.access_key,
            "secret_access_key": s3_config.secret_key,
            "endpoint": s3_config.endpoint,
            "prefix": s3_config.prefix,
            "disable_ssl": not s3_config.use_ssl,
            "use_path_style": s3_config.use_path_style,
            "directory_marker_mode": s3_config.directory_marker_mode.value
            if hasattr(s3_config.directory_marker_mode, "value")
            else s3_config.directory_marker_mode,
            "disable_batch_delete": s3_config.disable_batch_delete,
            "normalize_encoding_chars": s3_config.normalize_encoding_chars,
        }

        config["s3fs"] = {
            "enabled": True,
            "path": "/local",
            "config": s3_plugin_config,
        }
    elif backend == "memory":
        config["memfs"] = {
            "enabled": True,
            "path": "/local",
        }
    return config


def _render_git_toml(git_config: Any, storage_path: Path) -> str:
    """Render a TOML body with [git] and [git.local] sections from a GitConfig.

    The format mirrors the working fixture used in the ragfs_python binding tests
    (see tests/agfs/test_viking_fs_git.py). When ``git_config.local.base_dir`` is
    empty, it defaults to ``{storage_path}/git``.
    """
    local_cfg = getattr(git_config, "local", None)
    base_dir = getattr(local_cfg, "base_dir", "") if local_cfg is not None else ""
    if not base_dir:
        base_dir = str(storage_path / "git")
    else:
        base_dir = str(Path(base_dir).expanduser())
    fsync = getattr(local_cfg, "fsync", "off") if local_cfg is not None else "off"

    enabled = "true" if getattr(git_config, "enabled", False) else "false"
    backend = getattr(git_config, "backend", "local")
    default_branch = getattr(git_config, "default_branch", "main")
    author_name = getattr(git_config, "author_name", "viking-bot")
    author_email = getattr(git_config, "author_email", "bot@viking.local")

    return (
        "[git]\n"
        f"enabled = {enabled}\n"
        f'backend = "{backend}"\n'
        f'default_branch = "{default_branch}"\n'
        f'author_name = "{author_name}"\n'
        f'author_email = "{author_email}"\n'
        "\n"
        "[git.local]\n"
        f'base_dir = "{base_dir}"\n'
        f'fsync = "{fsync}"\n'
    )


def create_agfs_client(agfs_config: Any, *, git_config: Any = None) -> Any:
    """
    Create a RAGFS client based on the provided configuration.

    Args:
        agfs_config: RAGFS configuration object.
        git_config: Optional GitConfig. When provided and ``enabled`` is True,
            a ragfs.toml is generated under ``{agfs_config.path}/.runtime/`` and
            its path is passed to ``RAGFSBindingClient`` via ``config_path=`` so
            the binding exposes git_* methods. When None or disabled, the client
            is constructed with no args (legacy behavior).

    Returns:
        A RAGFSBindingClient instance.
    """
    # Ensure agfs_config is not None
    if agfs_config is None:
        raise ValueError("agfs_config cannot be None")

    # Import binding client
    from openviking.pyagfs import get_binding_client

    RAGFSBindingClient, _ = get_binding_client()

    if RAGFSBindingClient is None:
        raise ImportError(
            "RAGFS binding client is not available. The native library (ragfs_python) "
            "could not be loaded. Please run 'pip install -e .' in the project root "
            "to build and install the RAGFS SDK with native bindings."
        )

    if git_config is not None and getattr(git_config, "enabled", False):
        path_str = getattr(agfs_config, "path", None)
        if path_str is None:
            raise ValueError("agfs_config.path is required when git is enabled")
        storage_path = Path(path_str).resolve()
        runtime_dir = storage_path / ".runtime"
        runtime_dir.mkdir(parents=True, exist_ok=True)
        toml_path = runtime_dir / "ragfs.toml"
        toml_path.write_text(_render_git_toml(git_config, storage_path))
        client = RAGFSBindingClient(config_path=str(toml_path))
    else:
        client = RAGFSBindingClient()

    # Automatically mount backend for binding client
    mount_agfs_backend(client, agfs_config)

    return client


def mount_agfs_backend(agfs: Any, agfs_config: Any) -> None:
    """
    Mount backend filesystem for a RAGFS client based on configuration.

    Args:
        agfs: RAGFS client instance.
        agfs_config: RAGFS configuration object containing backend settings.
    """
    # Check for the presence of a `mount` method
    if not callable(getattr(agfs, "mount", None)):
        return

    path_str = getattr(agfs_config, "path", None)
    if path_str is None:
        raise ValueError("agfs_config.path is required for mounting backend")

    data_path = Path(path_str).resolve()
    vikingfs_path = data_path / "viking"

    vikingfs_path.mkdir(parents=True, exist_ok=True)

    # 1. Mount standard plugins
    config = _generate_plugin_config(agfs_config, data_path)

    for plugin_name, plugin_config in config.items():
        mount_path = plugin_config["path"]
        # Ensure localfs directory exists before mounting
        if plugin_name == "localfs" and "local_dir" in plugin_config.get("config", {}):
            local_dir = plugin_config["config"]["local_dir"]
            os.makedirs(local_dir, exist_ok=True)
            logger.debug("[RAGFSUtils] Ensured localfs storage directory exists")
        # Ensure queuefs db_path parent directory exists before mounting
        if plugin_name == "queuefs" and "db_path" in plugin_config.get("config", {}):
            db_path = plugin_config["config"]["db_path"]
            os.makedirs(os.path.dirname(db_path), exist_ok=True)

        try:
            agfs.unmount(mount_path)
        except Exception:
            pass
        try:
            agfs.mount(plugin_name, mount_path, plugin_config.get("config", {}))
            logger.debug(f"[RAGFSUtils] Successfully mounted {plugin_name}")
        except Exception:
            logger.error(f"[RAGFSUtils] Failed to mount {plugin_name}")
