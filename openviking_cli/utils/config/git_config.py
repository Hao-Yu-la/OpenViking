# Copyright (c) 2026 Beijing Volcano Engine Technology Co., Ltd.
# SPDX-License-Identifier: AGPL-3.0
"""Git version control configuration for OpenViking."""
from typing import Literal

from pydantic import BaseModel, Field


class GitLocalConfig(BaseModel):
    """Configuration for the local git object backend."""

    base_dir: str = Field(
        default="",
        description="Filesystem directory holding git objects/refs. "
        "When empty, defaults to '{storage.path}/git'.",
    )
    fsync: Literal["on", "off"] = Field(
        default="off",
        description="Whether to fsync git object writes. 'off' is faster, 'on' is safer.",
    )

    model_config = {"extra": "forbid"}


class GitConfig(BaseModel):
    """Git multi-version management configuration."""

    enabled: bool = Field(
        default=False,
        description="Enable git-based multi-version management for VikingFS content.",
    )
    backend: Literal["local"] = Field(
        default="local",
        description="Git object backend. Only 'local' is supported in this phase.",
    )
    default_branch: str = Field(
        default="main",
        description="Default branch name for commits when not specified.",
    )
    author_name: str = Field(
        default="viking-bot",
        description="Default author name used when callers omit author_name.",
    )
    author_email: str = Field(
        default="bot@viking.local",
        description="Default author email used when callers omit author_email.",
    )
    local: GitLocalConfig = Field(
        default_factory=GitLocalConfig,
        description="Configuration for the 'local' backend.",
    )

    model_config = {"extra": "forbid"}
