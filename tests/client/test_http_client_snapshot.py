"""Unit tests for AsyncHTTPClient git_* methods that drive /api/v1/snapshot/*."""

from typing import Any, Dict, List, Optional

import pytest

from openviking_cli.client.http import AsyncHTTPClient

pytestmark = pytest.mark.asyncio


class _FakeHTTPClient:
    """Records the last request and returns a canned response."""

    def __init__(self):
        self.calls: List[Dict[str, Any]] = []
        self.next_response: Any = None

    async def get(self, path, *, params=None, headers=None):
        self.calls.append({"method": "GET", "path": path, "params": params, "headers": headers})
        return self.next_response

    async def post(self, path, *, json=None, headers=None):
        self.calls.append({"method": "POST", "path": path, "json": json, "headers": headers})
        return self.next_response


def _client_with_fake() -> tuple[AsyncHTTPClient, _FakeHTTPClient]:
    client = AsyncHTTPClient(url="http://localhost:1933")
    fake = _FakeHTTPClient()
    client._http = fake
    client._handle_response = lambda response: {"commit_oid": "a" * 40, "result": "created", "changed": 1}
    return client, fake


async def test_git_commit_posts_to_snapshot_commit():
    client, fake = _client_with_fake()
    fake.next_response = object()

    result = await client.git_commit(
        message="hello",
        paths=["viking://resources/a.md"],
        branch="main",
        author_name="bot",
        author_email="bot@example.com",
    )

    assert result == {"commit_oid": "a" * 40, "result": "created", "changed": 1}
    call = fake.calls[-1]
    assert call["method"] == "POST"
    assert call["path"] == "/api/v1/snapshot/commit"
    assert call["json"] == {
        "message": "hello",
        "paths": ["viking://resources/a.md"],
        "branch": "main",
        "author_name": "bot",
        "author_email": "bot@example.com",
    }


async def test_git_commit_omits_none_fields():
    client, fake = _client_with_fake()
    fake.next_response = object()

    await client.git_commit(message="hi")

    call = fake.calls[-1]
    assert call["json"] == {"message": "hi", "branch": "main"}
