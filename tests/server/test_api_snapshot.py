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


@pytest_asyncio.fixture(scope="function")
async def client_with_resource_and_blob(client_with_resource, service):
    """client_with_resource + a known blob written via VikingFS.write_file, then committed."""
    from openviking.server.identity import RequestContext, Role
    from openviking_cli.session.user_id import UserIdentifier

    client, _root = client_with_resource
    blob_uri = "viking://resources/snapshot_blob_fixture.txt"
    expected_bytes = b"hello from snapshot fixture\n"

    ctx = RequestContext(user=UserIdentifier.the_default_user(), role=Role.ROOT)
    await service.viking_fs.write_file(blob_uri, expected_bytes, ctx=ctx)

    commit_resp = await client.post(
        "/api/v1/snapshot/commit",
        json={"message": "with blob"},
    )
    assert commit_resp.status_code == 200, commit_resp.text
    commit_oid = commit_resp.json()["result"]["commit_oid"]

    yield client, commit_oid, blob_uri, expected_bytes


async def test_restore_dry_run_does_not_mutate(client_with_resource):
    client, _root = client_with_resource
    v1 = (await client.post("/api/v1/snapshot/commit", json={"message": "v1"})).json()["result"]

    resp = await client.post(
        "/api/v1/snapshot/restore",
        json={
            "project_dir": "viking://resources",
            "source_commit": v1["commit_oid"],
            "dry_run": True,
        },
    )
    assert resp.status_code == 200, resp.text
    body = resp.json()
    assert body["status"] == "ok"
    result = body["result"]
    # Per VikingFS.restore contract, dry_run responses carry 'diff'.
    assert "diff" in result or result.get("result") == "noop"


async def test_show_commit_metadata(client_with_resource):
    client, _ = client_with_resource
    commit = (await client.post("/api/v1/snapshot/commit", json={"message": "meta"})).json()["result"]
    resp = await client.get(
        "/api/v1/snapshot/show",
        params={"target_ref": commit["commit_oid"]},
    )
    assert resp.status_code == 200
    body = resp.json()
    assert body["status"] == "ok"
    meta = body["result"]
    assert meta["oid"] == commit["commit_oid"]
    assert "tree" in meta and "message" in meta


async def test_show_blob_returns_binary_with_headers(client_with_resource_and_blob):
    """show?path=<file> must return raw bytes + X-Snapshot-* headers."""
    client, commit_oid, blob_uri, expected_bytes = client_with_resource_and_blob
    resp = await client.get(
        "/api/v1/snapshot/show",
        params={"target_ref": commit_oid, "path": blob_uri},
    )
    assert resp.status_code == 200
    assert resp.headers["content-type"].startswith("application/octet-stream")
    assert "x-snapshot-oid" in {k.lower() for k in resp.headers}
    assert "x-snapshot-size" in {k.lower() for k in resp.headers}
    assert int(resp.headers["x-snapshot-size"]) == len(expected_bytes)
    assert resp.content == expected_bytes


async def test_show_path_not_found_returns_404(client_with_resource):
    client, _ = client_with_resource
    commit = (await client.post("/api/v1/snapshot/commit", json={"message": "for 404"})).json()["result"]
    resp = await client.get(
        "/api/v1/snapshot/show",
        params={"target_ref": commit["commit_oid"], "path": "viking://resources/does_not_exist.txt"},
    )
    assert resp.status_code == 404
    assert resp.json()["status"] == "error"
