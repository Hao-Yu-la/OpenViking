"""End-to-end tests for the git_commit/git_restore/git_show PyO3 bindings."""

import pytest


def test_git_concurrent_commit_error_class_exists():
    """GitConcurrentCommitError must be importable and inherit from AGFSClientError."""
    from openviking.pyagfs import GitConcurrentCommitError
    from openviking.pyagfs.exceptions import AGFSClientError
    assert issubclass(GitConcurrentCommitError, AGFSClientError)
