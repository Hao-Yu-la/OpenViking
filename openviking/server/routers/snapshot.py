# Copyright (c) 2026 Beijing Volcano Engine Technology Co., Ltd.
# SPDX-License-Identifier: AGPL-3.0
"""HTTP routes for git-style version control (snapshots).

Mirrors VikingFS.commit / VikingFS.restore / VikingFS.show / VikingFS.log,
which already implement the underlying semantics.
"""

from typing import List, Optional

from fastapi import APIRouter, Body, Depends, Query
from pydantic import BaseModel, ConfigDict

from openviking.pyagfs.exceptions import AGFSClientError, AGFSNotFoundError
from openviking.server.auth import get_request_context
from openviking.server.dependencies import get_service
from openviking.server.error_mapping import map_exception
from openviking.server.identity import RequestContext
from openviking.server.models import Response
from openviking_cli.exceptions import NotFoundError

router = APIRouter(prefix="/api/v1/snapshot", tags=["snapshot"])


class CommitRequest(BaseModel):
    """Request body for ``POST /api/v1/snapshot/commit``."""

    model_config = ConfigDict(extra="forbid")

    message: str
    paths: Optional[List[str]] = None
    branch: str = "main"
    author_name: Optional[str] = None
    author_email: Optional[str] = None


@router.post("/commit")
async def commit(
    request: CommitRequest = Body(...),
    _ctx: RequestContext = Depends(get_request_context),
):
    """Create a new snapshot of the current workspace state."""
    service = get_service()
    try:
        result = await service.fs.commit(
            message=request.message,
            paths=request.paths,
            branch=request.branch,
            author_name=request.author_name,
            author_email=request.author_email,
            ctx=_ctx,
        )
    except AGFSClientError as e:
        mapped = map_exception(e)
        if mapped is not None:
            raise mapped from e
        raise
    return Response(status="ok", result=result)


@router.get("/log")
async def log(
    branch: str = Query("main", description="Branch ref name"),
    limit: int = Query(20, ge=1, le=500, description="Max commits to return"),
    _ctx: RequestContext = Depends(get_request_context),
):
    """Walk commit history newest-first along parents[0]."""
    service = get_service()
    try:
        result = await service.fs.log(branch=branch, limit=limit, ctx=_ctx)
    except AGFSNotFoundError:
        raise NotFoundError(branch, "git_ref")
    except AGFSClientError as e:
        mapped = map_exception(e)
        if mapped is not None:
            raise mapped from e
        raise
    return Response(status="ok", result=result)
