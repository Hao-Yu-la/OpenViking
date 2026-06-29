# OpenViking account-level .ovgitignore design

## Summary

OpenViking's VikingFS version management should support an account-level `.ovgitignore` file that excludes matching files from future snapshot commits. The file lives at the Git tree root for the account, is itself versioned, and uses a documented glob subset close to common root `.gitignore` usage. The ignore rules affect `commit` only; `restore`, `show`, and `log` treat existing commit contents as authoritative and do not apply the current ignore file as a filter.

## Goals

- Provide a `.gitignore`-like user-controlled exclusion mechanism for VikingFS multi-version management.
- Align the ignore scope with the existing architecture: one Git-like repository per `account_id`.
- Keep `commit(paths=None)` and `commit(paths=[...])` behavior centralized in Rust `GitService`.
- Version `.ovgitignore` itself so rule changes are auditable and restorable.
- Prevent `.ovgitignore` from entering vector indexing or retrieval.
- Avoid changing the public `VikingFS.commit()` signature for the first implementation.

## Non-goals

- Full `.gitignore` compatibility.
- Per-project or nested `.ovgitignore` inheritance.
- `!` negation / re-include rules.
- Git index-compatible "tracked files remain tracked despite ignore" semantics.
- Exposing account-root `.ovgitignore` as a normal `viking://.ovgitignore` URI.

## Current context

The existing versioning implementation stores one logical Git repository per account. The root tree mirrors `/local/{account_id}/`, and `commit`, `restore`, and `show` are implemented in Rust `GitService` with Python wrappers in `VikingFS`.

Relevant existing extension points:

- `crates/ragfs/src/git/enumerate.rs::prune_path` contains system-level hardcoded pruning rules.
- `GitService::commit()` applies pruning for full and scoped commits.
- `GitService::restore()` computes diffs from Git trees and writes through VFS, generating a new forward commit.
- `openviking/storage/viking_fs.py::_uri_to_path()` maps `viking://` to `/local/{account_id}`.
- `openviking/storage/viking_fs.py::_uri_to_tree_path()` currently rejects account-root tree paths for public commit `paths` arguments.
- Account-root `ls` filters entries to listable scopes, so root dotfiles are not naturally exposed through normal listing.

## Design decisions

### 1. Ignore file location

The ignore file is stored at the account repository root:

```text
/local/{account_id}/.ovgitignore
```

Its Git tree path is:

```text
.ovgitignore
```

This matches the current `single repo per account_id` model. Rules apply to account-relative tree paths such as:

```text
resources/proj/a.log
user/default/memories/tmp.txt
agent/skills/foo.py
.ovgitignore
```

The file is not exposed as a normal `viking://.ovgitignore` content URI. Users manage it through dedicated API methods.

### 2. User-facing API

Add Python `VikingFS` methods:

```python
async def get_gitignore(self, ctx: Optional[RequestContext] = None) -> str
async def set_gitignore(self, content: str, ctx: Optional[RequestContext] = None) -> None
async def delete_gitignore(self, ctx: Optional[RequestContext] = None) -> None
```

These methods read/write/delete `/local/{account_id}/.ovgitignore` directly through AGFS using the current request context's account. They do not require users to address an account-root URI.

`VikingFS.commit()` keeps its existing signature. If `.ovgitignore` is absent, commit behavior remains unchanged.

### 3. Versioning of `.ovgitignore`

`.ovgitignore` is always eligible for versioning. Even if user rules match `.ovgitignore`, the commit path must preserve it:

```text
should_track(".ovgitignore") = true
```

This guarantees rule changes are included in snapshots, visible through history, and restorable.

### 4. Restore semantics

Ignore rules affect only `commit`. `restore`, `show`, and `log` do not read the current `.ovgitignore` as a filter.

This is required because restore should reproduce the selected source commit's tracked content. If the current ignore file filtered restore writes, restoring a historical snapshot could silently omit files that existed in that snapshot.

Restore behavior:

- Subtree restore does not touch root `.ovgitignore` unless the restore target includes the account root.
- Account-root restore treats `.ovgitignore` as an ordinary tracked file and restores it from the source commit.
- Files that match the current `.ovgitignore` can still be restored if they exist in the source commit.
- The follow-up restore commit is constructed from Git tree state and must not re-apply ignore filtering.

### 5. Snapshot exclude semantics

The feature is not a Git index clone. It is an OpenViking snapshot exclude mechanism.

If a file was present in earlier commits and later matches `.ovgitignore`, the next commit should remove it from the new tree while leaving the working VFS file untouched. This differs from Git, where `.gitignore` usually affects untracked files but does not stop already tracked files from being committed.

This behavior matches OpenViking's commit model: each commit builds a snapshot tree from the current VFS plus previous tree reuse optimizations.

## Rule syntax

`.ovgitignore` is UTF-8 text. The first implementation supports a common glob subset:

- Empty lines are ignored.
- Lines whose first non-space character is `#` are comments.
- Leading and trailing whitespace are trimmed.
- `!` negation is unsupported and should fail commit with a clear invalid-ignore error.
- Git-style escaping rules are unsupported.
- The file should have a size limit, recommended at 64 KiB.

Path matching uses account-relative Git tree paths with `/` separators.

Recommended matching rules:

| Pattern | Meaning |
| --- | --- |
| `*.log` | Match any basename ending in `.log` at any depth. |
| `**/*.log` | Match `.log` files at any depth. |
| `resources/tmp/**` | Match everything under `resources/tmp/`. |
| `/resources/tmp/**` | Same as above, explicitly anchored at account root. |
| `tmp/` | Match any directory named `tmp` and its contents. |
| `/tmp/` | Match account-root `tmp/` and its contents. |

Normalization:

1. A leading `/` anchors the pattern at the account root.
2. A pattern containing `/` is matched relative to the account root.
3. A pattern without `/` is matched against basenames / path segments at any depth.
4. A trailing `/` marks a directory rule.

System pruning rules remain higher priority than user ignore rules. Users cannot re-include internal paths because negation is not supported.

## Rust implementation plan

### New ignore module

Add `crates/ragfs/src/git/ignore.rs`:

```rust
pub const OVGITIGNORE_PATH: &str = ".ovgitignore";

pub struct IgnoreMatcher {
    // compiled rules
}

impl IgnoreMatcher {
    pub fn empty() -> Self;
    pub fn parse(bytes: &[u8]) -> Result<Self, GitError>;
    pub fn is_ignored(&self, rel_path: &str) -> bool;
}
```

The crate already depends on `ignore = "0.4.25"` for grep behavior. The implementation may reuse that crate where it fits, but should keep a thin OpenViking wrapper so only the supported syntax and snapshot semantics are exposed.

Add a helper equivalent to:

```rust
fn should_track(path: &str, matcher: &IgnoreMatcher) -> bool {
    if path == OVGITIGNORE_PATH {
        return true;
    }
    !prune_path(path) && !matcher.is_ignored(path)
}
```

### Commit flow

Update `GitService::commit()`:

1. Resolve HEAD and load previous tree as today.
2. Read `/local/{account}/.ovgitignore` through VFS.
   - Not found means `IgnoreMatcher::empty()`.
   - Too large, invalid UTF-8, or unsupported syntax returns a `GitError` and fails the commit.
3. Compile ignore matcher once per commit.
4. Apply `should_track` to current VFS candidate paths.
5. Apply `should_track` to previous tree paths used for reuse and deletion detection.
6. Build and write the new tree as today.
7. Return the normal commit response with an added `ignored` count. The count includes current VFS candidates and previous-tree entries skipped because of `.ovgitignore`; it does not count system-pruned paths.

The previous-tree filtering is essential. Without it, files that were committed before becoming ignored would remain in later snapshots.

### Scoped `paths` commits

For `commit(paths=[...])`, ignore rules still apply to paths inside the requested scope. A path explicitly passed by the caller but ignored by `.ovgitignore` should be skipped rather than forced into the commit.

`.ovgitignore` itself remains the exception, although normal public URI conversion does not expose account-root `.ovgitignore` as a `paths` entry.

### Error types

Add Rust errors similar to:

```rust
GitError::InvalidIgnoreFile { path: String, reason: String }
GitError::IgnoreFileTooLarge { path: String, size: u64, max: u64 }
```

Map these to Python invalid-operation style errors. Commit should fail loudly on invalid ignore configuration instead of silently disabling rules.

## Python implementation plan

### Management methods

Add `VikingFS.get_gitignore`, `set_gitignore`, and `delete_gitignore`.

Implementation notes:

- Use `real_ctx.account_id` to build `/local/{account_id}/.ovgitignore`.
- `get_gitignore()` returns an empty string when the file is absent. This makes first-time setup simple and avoids forcing callers to special-case not-found.
- `set_gitignore()` accepts `str`, validates encodable UTF-8, and writes bytes directly.
- `delete_gitignore()` should be idempotent, like existing `rm` behavior where possible.
- These methods should not schedule semantic indexing.

### Vector indexing / restore reindex

`.ovgitignore` must not enter vector indexing or retrieval.

Update restore path classification in `VikingFS._classify_restore_path`:

```python
if tree_path == ".ovgitignore":
    return None
```

Also ensure semantic processing / reindex code skips `.ovgitignore` if it ever encounters it through low-level traversal.

Normal `ls("viking://")` does not need to show account-root `.ovgitignore`; existing root listing already filters to listable scopes.

## Compatibility

- Existing accounts without `.ovgitignore` behave exactly as before.
- Existing commits are immutable and unchanged.
- After `.ovgitignore` is created, only future commits are filtered.
- `show` can still read ignored files from historical commits.
- `restore` can still restore ignored files from historical commits.
- Commit response adds an `ignored` field; this is backward-compatible for dict consumers.
- `ignored` reports user-ignore skips only. System pruning remains existing behavior and is not counted.

## Testing strategy

### Rust ignore tests

- Empty file, comments, and blank lines.
- `*.log` matches basenames at any depth.
- `**/*.log` matches nested files.
- `resources/tmp/**` and `/resources/tmp/**` match root-relative paths.
- `tmp/` matches any `tmp` directory and its contents.
- `/tmp/` matches only root `tmp/`.
- `!foo` returns unsupported syntax.
- `.ovgitignore` is not ignored even if matched by `*`.
- Non-UTF-8 content fails.
- Oversized ignore file fails.

### Rust GitService tests

- No `.ovgitignore` preserves current commit behavior.
- Full commit excludes matching VFS files.
- Scoped commit excludes matching explicit paths.
- Adding ignore rules removes previously tracked matching files from the new snapshot.
- `.ovgitignore` itself is committed and visible through `show`.
- `restore` ignores current ignore rules and restores historical ignored files.
- Root restore restores `.ovgitignore` itself.
- Invalid `.ovgitignore` fails commit with a clear error.

### Python tests

- `get_gitignore`, `set_gitignore`, `delete_gitignore` behavior.
- `VikingFS.commit` with `.ovgitignore` excludes matching files.
- `.ovgitignore` is not classified for vector rebuild after restore.
- Account-root `.ovgitignore` is not shown by normal root listing.
- Existing `commit` API calls still work with no `.ovgitignore`.

## Documentation updates

Update `docs/design/git-version-control-design.md`:

- Extend path pruning / exclusion section with account-level `.ovgitignore`.
- Document snapshot exclude semantics and the difference from Git index semantics.
- Document restore behavior: ignore rules are not applied during restore.
- Add Python API methods for managing `.ovgitignore`.
- Update the current implementation progress section once implemented.
