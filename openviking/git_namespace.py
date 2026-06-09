# Copyright (c) 2026 Beijing Volcano Engine Technology Co., Ltd.
# SPDX-License-Identifier: AGPL-3.0
"""Git version control namespace for OpenViking clients.

Exposes git_* methods on BaseClient under a `client.git.*` namespace so
the user-facing API reads as `client.git.commit(...)` rather than the
flat `client.git_commit(...)` underneath.
"""
from __future__ import annotations

from typing import Any, Dict, List, Optional, TYPE_CHECKING

from openviking_cli.utils import run_async

if TYPE_CHECKING:
    from openviking.async_client import AsyncOpenViking
    from openviking.sync_client import SyncOpenViking
    from openviking_cli.client.http import AsyncHTTPClient
    from openviking_cli.client.sync_http import SyncHTTPClient


class AsyncGitNamespace:
    """Git version control methods on the async client.

    Forwards to the underlying BaseClient's git_* methods.
    """

    def __init__(self, client: "AsyncOpenViking"):
        self._client = client

    async def commit(
        self,
        *,
        message: str,
        paths: Optional[List[str]] = None,
        branch: str = "main",
        author_name: Optional[str] = None,
        author_email: Optional[str] = None,
    ) -> Dict[str, Any]:
        await self._client._ensure_initialized()
        return await self._client._client.git_commit(
            message=message,
            paths=paths,
            branch=branch,
            author_name=author_name,
            author_email=author_email,
        )

    async def restore(
        self,
        *,
        project_dir: str,
        source_commit: str,
        branch: str = "main",
        dry_run: bool = False,
        message: Optional[str] = None,
        author_name: Optional[str] = None,
        author_email: Optional[str] = None,
    ) -> Dict[str, Any]:
        await self._client._ensure_initialized()
        return await self._client._client.git_restore(
            project_dir=project_dir,
            source_commit=source_commit,
            branch=branch,
            dry_run=dry_run,
            message=message,
            author_name=author_name,
            author_email=author_email,
        )

    async def show(
        self,
        target_ref: str,
        *,
        path: Optional[str] = None,
    ) -> Any:
        await self._client._ensure_initialized()
        return await self._client._client.git_show(target_ref, path=path)

    async def log(
        self,
        *,
        branch: str = "main",
        limit: int = 20,
    ) -> List[Dict[str, Any]]:
        await self._client._ensure_initialized()
        return await self._client._client.git_log(branch=branch, limit=limit)


class SyncGitNamespace:
    """Synchronous wrapper around AsyncGitNamespace.

    Each method calls into the SyncOpenViking's underlying async client
    via run_async, matching the rest of the SyncOpenViking surface.
    """

    def __init__(self, client: "SyncOpenViking"):
        self._client = client

    def _ns(self) -> AsyncGitNamespace:
        return self._client._async_client.git

    def commit(
        self,
        *,
        message: str,
        paths: Optional[List[str]] = None,
        branch: str = "main",
        author_name: Optional[str] = None,
        author_email: Optional[str] = None,
    ) -> Dict[str, Any]:
        return run_async(
            self._ns().commit(
                message=message,
                paths=paths,
                branch=branch,
                author_name=author_name,
                author_email=author_email,
            )
        )

    def restore(
        self,
        *,
        project_dir: str,
        source_commit: str,
        branch: str = "main",
        dry_run: bool = False,
        message: Optional[str] = None,
        author_name: Optional[str] = None,
        author_email: Optional[str] = None,
    ) -> Dict[str, Any]:
        return run_async(
            self._ns().restore(
                project_dir=project_dir,
                source_commit=source_commit,
                branch=branch,
                dry_run=dry_run,
                message=message,
                author_name=author_name,
                author_email=author_email,
            )
        )

    def show(
        self,
        target_ref: str,
        *,
        path: Optional[str] = None,
    ) -> Any:
        return run_async(self._ns().show(target_ref, path=path))

    def log(
        self,
        *,
        branch: str = "main",
        limit: int = 20,
    ) -> List[Dict[str, Any]]:
        return run_async(self._ns().log(branch=branch, limit=limit))


class AsyncHTTPGitNamespace:
    """Git version control methods on the async HTTP client.

    Unlike AsyncGitNamespace (used by AsyncOpenViking), this is a single-layer
    wrapper that forwards directly to the AsyncHTTPClient's git_* methods —
    AsyncHTTPClient does not wrap another BaseClient.
    """

    def __init__(self, client: "AsyncHTTPClient"):
        self._client = client

    async def commit(
        self,
        *,
        message: str,
        paths: Optional[List[str]] = None,
        branch: str = "main",
        author_name: Optional[str] = None,
        author_email: Optional[str] = None,
    ) -> Dict[str, Any]:
        return await self._client.git_commit(
            message=message,
            paths=paths,
            branch=branch,
            author_name=author_name,
            author_email=author_email,
        )

    async def restore(
        self,
        *,
        project_dir: str,
        source_commit: str,
        branch: str = "main",
        dry_run: bool = False,
        message: Optional[str] = None,
        author_name: Optional[str] = None,
        author_email: Optional[str] = None,
    ) -> Dict[str, Any]:
        return await self._client.git_restore(
            project_dir=project_dir,
            source_commit=source_commit,
            branch=branch,
            dry_run=dry_run,
            message=message,
            author_name=author_name,
            author_email=author_email,
        )

    async def show(
        self,
        target_ref: str,
        *,
        path: Optional[str] = None,
    ) -> Any:
        return await self._client.git_show(target_ref, path=path)

    async def log(
        self,
        *,
        branch: str = "main",
        limit: int = 20,
    ) -> List[Dict[str, Any]]:
        return await self._client.git_log(branch=branch, limit=limit)


class SyncHTTPGitNamespace:
    """Synchronous wrapper for the HTTP client's git namespace."""

    def __init__(self, client: "SyncHTTPClient"):
        self._client = client

    def _ns(self) -> AsyncHTTPGitNamespace:
        return self._client._async_client.git

    def commit(
        self,
        *,
        message: str,
        paths: Optional[List[str]] = None,
        branch: str = "main",
        author_name: Optional[str] = None,
        author_email: Optional[str] = None,
    ) -> Dict[str, Any]:
        return run_async(
            self._ns().commit(
                message=message,
                paths=paths,
                branch=branch,
                author_name=author_name,
                author_email=author_email,
            )
        )

    def restore(
        self,
        *,
        project_dir: str,
        source_commit: str,
        branch: str = "main",
        dry_run: bool = False,
        message: Optional[str] = None,
        author_name: Optional[str] = None,
        author_email: Optional[str] = None,
    ) -> Dict[str, Any]:
        return run_async(
            self._ns().restore(
                project_dir=project_dir,
                source_commit=source_commit,
                branch=branch,
                dry_run=dry_run,
                message=message,
                author_name=author_name,
                author_email=author_email,
            )
        )

    def show(
        self,
        target_ref: str,
        *,
        path: Optional[str] = None,
    ) -> Any:
        return run_async(self._ns().show(target_ref, path=path))

    def log(
        self,
        *,
        branch: str = "main",
        limit: int = 20,
    ) -> List[Dict[str, Any]]:
        return run_async(self._ns().log(branch=branch, limit=limit))
