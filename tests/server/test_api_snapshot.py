# Copyright (c) 2026 Beijing Volcano Engine Technology Co., Ltd.
# SPDX-License-Identifier: AGPL-3.0
"""End-to-end tests for /api/v1/snapshot/*."""

import pytest
import pytest_asyncio

import httpx

pytestmark = pytest.mark.asyncio


@pytest_asyncio.fixture(scope="function")
async def client_with_no_repo(app):
    """Plain in-process client with no resources or commits added.

    The conftest's ``app`` fixture wires the service into the global
    dependency store without authentication, so a vanilla AsyncClient
    is enough to hit ``/api/v1/snapshot/log`` against an empty repo.
    """
    transport = httpx.ASGITransport(app=app)
    async with httpx.AsyncClient(transport=transport, base_url="http://testserver") as c:
        yield c


async def test_commit_creates_snapshot(client_with_resource):
    client, _root_uri = client_with_resource
    resp = await client.post(
        "/api/v1/snapshot/commit",
        json={"message": "first snapshot"},
    )
    assert resp.status_code == 200, resp.text
    body = resp.json()
    assert body["status"] == "ok"
    result = body["result"]
    assert result["result"] in ("created", "noop")
    assert isinstance(result["commit_oid"], str) and len(result["commit_oid"]) == 40


async def test_log_returns_recent_commits(client_with_resource):
    client, _ = client_with_resource
    await client.post("/api/v1/snapshot/commit", json={"message": "for log"})

    resp = await client.get("/api/v1/snapshot/log", params={"branch": "main", "limit": 5})
    assert resp.status_code == 200, resp.text
    body = resp.json()
    assert body["status"] == "ok"
    log = body["result"]
    assert isinstance(log, list) and len(log) >= 1
    assert "oid" in log[0] and "message" in log[0]


async def test_log_empty_repo_returns_404(client_with_no_repo):
    """When the branch has no commits, /log should surface 404."""
    client = client_with_no_repo
    resp = await client.get("/api/v1/snapshot/log", params={"branch": "main", "limit": 5})
    assert resp.status_code == 404
    assert resp.json()["status"] == "error"
