//! `GitService` - high-level integration tying together object/ref stores,
//! VFS enumeration, tree building, and commit-object construction.
//!
//! See design §8.1 for the `commit()` algorithm. Fast Path 1 (persistent
//! stat cache `commit_index.bin`) and Fast Path 3 (`exists()` dedup before
//! blob write) are intentionally deferred — they are not necessary for
//! correctness because `write_object` is idempotent.

use std::sync::Arc;

use gix_hash::ObjectId;

use crate::core::filesystem::FileSystem;
use crate::git::{
    error::{GitError, ObjectStoreError, RefStoreError},
    object_store::ObjectStore,
    ref_store::RefStore,
    types::{CommitRequest, CommitResponse, RestoreRequest, RestoreResponse, ShowRequest, ShowResponse},
};

/// `GitService` orchestrates the full commit pipeline against a `FileSystem`
/// (the working tree), an `ObjectStore`, and a `RefStore`.
pub struct GitService {
    pub vfs: Arc<dyn FileSystem>,
    pub object_store: Arc<dyn ObjectStore>,
    pub ref_store: Arc<dyn RefStore>,
}

impl GitService {
    pub fn new(
        vfs: Arc<dyn FileSystem>,
        object_store: Arc<dyn ObjectStore>,
        ref_store: Arc<dyn RefStore>,
    ) -> Self {
        Self {
            vfs,
            object_store,
            ref_store,
        }
    }

    /// Build a new commit on `branch` reflecting the current state of the
    /// account's VFS subtree.
    ///
    /// - If `paths` is `Some`, only those account-relative paths are
    ///   considered (each is still pruned via `enumerate::prune_path`).
    /// - If `paths` is `None`, the full `/local/{account}` subtree is
    ///   enumerated via `enumerate::collect_all`.
    ///
    /// On no-op (no editor change) the branch ref is untouched and
    /// `CommitResponse::Noop` is returned.
    ///
    /// On a CAS conflict, returns `GitError::ConcurrentCommit` so the
    /// caller can decide whether to retry. There is intentionally no
    /// retry loop inside `commit()`.
    pub async fn commit(&self, req: CommitRequest) -> Result<CommitResponse, GitError> {
        let CommitRequest {
            account,
            branch,
            message,
            paths,
            author_name,
            author_email,
        } = req;
        let ref_name = format!("refs/heads/{branch}");

        // 1. Resolve current HEAD (may not exist → root commit).
        let prev_head: Option<ObjectId> = match self.ref_store.read(&account, &ref_name).await {
            Ok(oid) => Some(oid),
            Err(RefStoreError::NotFound(_)) => None,
            Err(e) => return Err(e.into()),
        };
        let prev_tree: Option<ObjectId> = match prev_head {
            Some(commit_oid) => Some(
                load_commit_meta(self.object_store.as_ref(), &account, &commit_oid)
                    .await?
                    .tree,
            ),
            None => None,
        };

        // 2. Build TreeEditor from prev tree if any; otherwise start empty.
        //    (The well-known empty-tree oid is not guaranteed to exist in the
        //    store, so we cannot blindly hand it to `from_tree`.)
        let mut editor = match prev_tree {
            Some(t) => crate::git::tree_builder::TreeEditor::from_tree(
                self.object_store.as_ref(),
                &account,
                t,
            )
            .await?,
            None => crate::git::tree_builder::TreeEditor::empty(),
        };

        // 3. Determine candidate paths.
        let candidates: Vec<String> = match paths {
            Some(ps) => ps
                .into_iter()
                .filter(|p| !crate::git::enumerate::prune_path(p))
                .collect(),
            None => crate::git::enumerate::collect_all(&self.vfs, &account).await?,
        };

        // 4. For each candidate: detect delete vs upsert. Unconditionally
        //    write blobs (write_object is idempotent — see Fast Path 3 note).
        let mut changed = 0usize;
        for rel_path in candidates {
            let abs = format!("/local/{}/{}", account, rel_path);
            match self.vfs.stat(&abs).await {
                Ok(info) if info.is_dir => continue, // ignore directories
                // TODO: dir↔file type transitions (path used to be a file,
                // is now a directory or vice-versa) are not handled — the
                // stale entry of the opposite kind lingers in the tree.
                Ok(_) => {
                    let bytes = self.vfs.read(&abs, 0, 0).await?;
                    let oid = crate::git::util::write_object(
                        self.object_store.as_ref(),
                        &account,
                        gix_object::Kind::Blob,
                        &bytes,
                    )
                    .await?;
                    // Skip the upsert if prev_tree already has this exact
                    // path+oid — re-writing the same blob is not an editor
                    // change and shouldn't count toward the no-op decision.
                    let prev_entry = match prev_tree {
                        Some(t) => crate::git::tree_builder::lookup(
                            self.object_store.as_ref(),
                            &account,
                            t,
                            &rel_path,
                        )
                        .await?,
                        None => None,
                    };
                    if prev_entry.map(|(o, _)| o) == Some(oid) {
                        continue;
                    }
                    editor.upsert(&rel_path, oid)?;
                    changed += 1;
                }
                Err(e) if is_not_found(&e) => {
                    // Only count as a change if the path actually existed
                    // in prev_tree, since TreeEditor::remove silently no-ops
                    // for missing paths. With no prev_tree (root commit) a
                    // missing path is just irrelevant.
                    let prev_entry = match prev_tree {
                        Some(t) => crate::git::tree_builder::lookup(
                            self.object_store.as_ref(),
                            &account,
                            t,
                            &rel_path,
                        )
                        .await?,
                        None => None,
                    };
                    if prev_entry.is_some() {
                        editor.remove(&rel_path)?;
                        changed += 1;
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }

        // 5. No-op short-circuit.
        if changed == 0 {
            return Ok(CommitResponse::Noop {
                commit_oid: prev_head.unwrap_or_else(|| ObjectId::null(gix_hash::Kind::Sha1)),
            });
        }

        // 6. Write the new tree + the commit object.
        let new_tree = editor.write(self.object_store.as_ref(), &account).await?;
        let parents: Vec<ObjectId> = prev_head.iter().copied().collect();
        let commit_oid = crate::git::commit::write_commit(
            self.object_store.as_ref(),
            &account,
            new_tree,
            parents,
            &author_name,
            &author_email,
            &message,
        )
        .await?;

        // 7. CAS update the branch ref. Map Conflict → ConcurrentCommit.
        match self
            .ref_store
            .cas_update(&account, &ref_name, prev_head, commit_oid)
            .await
        {
            Ok(()) => {}
            Err(RefStoreError::Conflict { expected, actual }) => {
                return Err(GitError::ConcurrentCommit {
                    ref_name,
                    expected,
                    actual,
                });
            }
            Err(other) => return Err(other.into()),
        }

        Ok(CommitResponse::Created {
            commit_oid,
            changed,
        })
    }

    /// Read a commit's metadata, or a single blob's bytes from inside a commit's tree.
    ///
    /// `target_ref` resolution: 40-hex OID / "main" / "refs/heads/main".
    ///
    /// - `path = None`  → returns `ShowResponse::Commit { oid, tree, parents, author, committer, message }`.
    /// - `path = Some(p)` → returns `ShowResponse::Blob { oid, size, bytes }` for the path inside
    ///   the commit's tree. Missing path → `GitError::PathNotFound(p)`. Path that resolves to
    ///   a tree (not a blob) → `GitError::PathIsDirectory(p)` — distinct from missing so callers
    ///   can tell apart "no such path" from "path exists but is a directory, not a file".
    ///
    /// Missing ref → `GitError::RefStore(RefStoreError::NotFound)`.
    /// Missing commit object → `GitError::ObjectStore(ObjectStoreError::NotFound)`.
    pub async fn show(&self, req: ShowRequest) -> Result<ShowResponse, GitError> {
        let ShowRequest { account, target_ref, path } = req;

        let commit_oid = resolve_ref(
            self.ref_store.as_ref(),
            self.object_store.as_ref(),
            &account,
            &target_ref,
        )
        .await?;
        let meta = load_commit_meta(self.object_store.as_ref(), &account, &commit_oid).await?;

        match path {
            None => Ok(ShowResponse::Commit {
                oid: commit_oid,
                tree: meta.tree,
                parents: meta.parents,
                author: meta.author,
                committer: meta.committer,
                message: meta.message,
            }),
            Some(p) => {
                let entry = crate::git::tree_builder::lookup(
                    self.object_store.as_ref(),
                    &account,
                    meta.tree,
                    &p,
                ).await?;
                let (blob_oid, mode) = entry.ok_or_else(|| GitError::PathNotFound(p.clone()))?;
                // Reject trees masquerading as paths: callers asked for blob bytes.
                if mode.is_tree() {
                    return Err(GitError::PathIsDirectory(p));
                }
                let raw = crate::git::util::read_object(
                    self.object_store.as_ref(),
                    &account,
                    &blob_oid,
                ).await?;
                let (kind, payload_size, hdr) = crate::git::util::parse_object_header(&raw)?;
                if kind != gix_object::Kind::Blob {
                    return Err(GitError::CorruptedObject(format!(
                        "expected blob at {p}, got {kind:?}"
                    )));
                }
                // `raw` is already a `Bytes`; `slice` is O(1) and shares the
                // backing buffer instead of allocating a fresh payload copy.
                let bytes = raw.slice(hdr..);
                Ok(ShowResponse::Blob {
                    oid: blob_oid,
                    size: payload_size,
                    bytes,
                })
            }
        }
    }

    /// Restore a subtree at `project_dir` to the state it had in `source_commit`,
    /// producing a new commit whose parent is the **current HEAD** (not
    /// `source_commit`). HEAD always moves forward.
    ///
    /// See design §8.2 for the full algorithm and `RestoreResponse` for the
    /// three possible outcomes (`Applied` / `Noop` / `DryRun`).
    ///
    /// Errors:
    /// - `GitError::InvalidProjectDir` — `project_dir` is empty / malformed.
    /// - `GitError::RefStore(NotFound)` — branch HEAD or source_commit ref missing.
    /// - `GitError::SubtreeNotFoundInCommit` — `project_dir` does not resolve
    ///   to a subtree in `source_commit`'s tree.
    /// - `GitError::ConcurrentCommit` — branch ref changed between our read
    ///   and the CAS swap.
    pub async fn restore(&self, req: RestoreRequest) -> Result<RestoreResponse, GitError> {
        let RestoreRequest {
            account,
            branch,
            project_dir,
            source_commit,
            dry_run,
            message: _,
            author_name: _,
            author_email: _,
        } = &req;

        validate_project_dir(project_dir)?;
        let ref_name = format!("refs/heads/{branch}");

        // 1. Resolve both commits.
        let source_oid = resolve_ref(
            self.ref_store.as_ref(),
            self.object_store.as_ref(),
            account,
            source_commit,
        )
        .await?;
        let head_oid = self.ref_store.read(account, &ref_name).await?;
        let source_meta = load_commit_meta(self.object_store.as_ref(), account, &source_oid).await?;
        let head_meta = load_commit_meta(self.object_store.as_ref(), account, &head_oid).await?;

        // 2. Extract project_dir subtree from each. Source missing → error.
        //    Head missing → treat as empty (every file is a fresh write).
        let source_subtree = match crate::git::tree_builder::lookup(
            self.object_store.as_ref(),
            account,
            source_meta.tree,
            project_dir,
        )
        .await?
        {
            Some((oid, mode)) if mode.is_tree() => oid,
            _ => {
                return Err(GitError::SubtreeNotFoundInCommit {
                    project_dir: project_dir.clone(),
                    commit: source_oid,
                });
            }
        };
        let head_subtree = match crate::git::tree_builder::lookup(
            self.object_store.as_ref(),
            account,
            head_meta.tree,
            project_dir,
        )
        .await?
        {
            Some((oid, mode)) if mode.is_tree() => Some(oid),
            _ => None,
        };

        // 3. Flatten and diff (paths in the result are subtree-relative).
        let source_entries = crate::git::tree_builder::flatten(
            self.object_store.as_ref(),
            account,
            source_subtree,
            &None,
        )
        .await?;
        let head_entries = match head_subtree {
            Some(oid) => {
                crate::git::tree_builder::flatten(
                    self.object_store.as_ref(),
                    account,
                    oid,
                    &None,
                )
                .await?
            }
            None => Vec::new(),
        };
        let diff = compute_subtree_diff(&source_entries, &head_entries);

        // 4. dry_run short-circuits BEFORE any writes.
        if *dry_run {
            return Ok(RestoreResponse::DryRun {
                diff,
                head: head_oid,
                source: source_oid,
            });
        }

        // 5. Source == head → noop.
        if diff.to_write.is_empty() && diff.to_delete.is_empty() {
            return Ok(RestoreResponse::Noop {
                head: head_oid,
                source: source_oid,
            });
        }

        // 6. Read blob bytes for to_write entries, then writeback through VFS.
        //    Paths in the diff are relative to project_dir — prefix here.
        use futures::stream::{self, StreamExt, TryStreamExt};

        let abs_prefix = format!("/local/{}/{}", account, project_dir);
        let writes_planned = diff.to_write.len();
        let deletes_planned = diff.to_delete.len();
        let unchanged_count = diff.unchanged.len();

        let object_store_ref = self.object_store.clone();
        let vfs_ref = self.vfs.clone();
        let account_owned = account.clone();
        let abs_prefix_for_writes = abs_prefix.clone();

        stream::iter(diff.to_write.clone().into_iter())
            .map(|(rel, blob_oid)| {
                let object_store = object_store_ref.clone();
                let vfs = vfs_ref.clone();
                let account = account_owned.clone();
                let abs_prefix = abs_prefix_for_writes.clone();
                async move {
                    let bytes =
                        read_blob_payload(object_store.as_ref(), &account, &blob_oid).await?;
                    let abs = format!("{}/{}", abs_prefix, rel);
                    crate::core::filesystem::FileSystem::write(
                        vfs.as_ref(),
                        &abs,
                        &bytes,
                        0,
                        crate::core::types::WriteFlag::Create,
                    )
                    .await?;
                    Ok::<(), GitError>(())
                }
            })
            .buffer_unordered(32)
            .try_collect::<()>()
            .await?;

        let abs_prefix_for_deletes = abs_prefix.clone();
        let vfs_for_deletes = self.vfs.clone();
        stream::iter(diff.to_delete.clone().into_iter())
            .map(|rel| {
                let vfs = vfs_for_deletes.clone();
                let abs_prefix = abs_prefix_for_deletes.clone();
                async move {
                    let abs = format!("{}/{}", abs_prefix, rel);
                    // Restore is idempotent: a path the diff wants to delete may
                    // already be absent from the VFS (e.g. derived files like
                    // `.abstract.md` that were removed or regenerated out of band).
                    // Treat NotFound as success rather than aborting the restore.
                    match crate::core::filesystem::FileSystem::remove(vfs.as_ref(), &abs).await {
                        Ok(_) => Ok::<(), GitError>(()),
                        Err(crate::core::errors::Error::NotFound(_)) => Ok(()),
                        Err(e) => Err(e.into()),
                    }
                }
            })
            .buffer_unordered(32)
            .try_collect::<()>()
            .await?;

        // 6b. Prune directories left empty by the deletes above. Git does not
        //     track directories, so `to_delete` only ever lists files; removing
        //     the last file in a directory would otherwise leave an empty husk
        //     in the VFS. Walk each deleted file's ancestor directories (within
        //     project_dir, deepest first) and drop any that are now empty.
        //     Best-effort: a directory that still holds entries, or has already
        //     vanished, is simply skipped — pruning never aborts the restore.
        use std::collections::BTreeSet;
        // (depth, rel_dir): BTreeSet iterates ascending, so reversing yields the
        // deepest directories first — children are pruned before their parents,
        // letting a parent that held only pruned subdirs be removed in turn.
        let mut prune_candidates: BTreeSet<(usize, String)> = BTreeSet::new();
        for rel in &diff.to_delete {
            let mut dir = rel.as_str();
            while let Some(idx) = dir.rfind('/') {
                dir = &dir[..idx];
                prune_candidates.insert((dir.split('/').count(), dir.to_string()));
            }
        }
        for (_depth, rel_dir) in prune_candidates.into_iter().rev() {
            let abs = format!("{}/{}", abs_prefix, rel_dir);
            let is_empty = match crate::core::filesystem::FileSystem::read_dir(
                self.vfs.as_ref(),
                &abs,
            )
            .await
            {
                Ok(entries) => entries.is_empty(),
                // Missing or not a directory → nothing to prune.
                Err(_) => false,
            };
            if is_empty {
                // Ignore failures: a concurrent writer may have repopulated the
                // directory, or it may already be gone. Either way the restore
                // itself has succeeded.
                let _ =
                    crate::core::filesystem::FileSystem::remove(self.vfs.as_ref(), &abs).await;
            }
        }

        // 7. Build the new tree: load head.tree into an editor and splice
        //    source_subtree at project_dir.
        let mut editor = crate::git::tree_builder::TreeEditor::from_tree(
            self.object_store.as_ref(),
            account,
            head_meta.tree,
        )
        .await?;
        editor.upsert_subtree(project_dir, source_subtree)?;
        let new_tree_oid = editor.write(self.object_store.as_ref(), account).await?;

        // 8. Construct the new commit. parent = head_oid (NOT source_oid).
        let msg = req.message.clone().unwrap_or_else(|| {
            format!(
                "restore {} from {}",
                project_dir,
                &source_oid.to_hex().to_string()[..12.min(40)]
            )
        });
        let new_commit_oid = crate::git::commit::write_commit(
            self.object_store.as_ref(),
            account,
            new_tree_oid,
            vec![head_oid],
            &req.author_name,
            &req.author_email,
            &msg,
        )
        .await?;

        // 9. CAS-swap the branch ref. Map Conflict → ConcurrentCommit.
        match self
            .ref_store
            .cas_update(account, &ref_name, Some(head_oid), new_commit_oid)
            .await
        {
            Ok(()) => {}
            Err(crate::git::error::RefStoreError::Conflict { expected, actual }) => {
                return Err(GitError::ConcurrentCommit {
                    ref_name,
                    expected,
                    actual,
                });
            }
            Err(other) => return Err(other.into()),
        }

        // Prefix diff paths with project_dir to produce account-relative paths.
        let project_dir_ref = &project_dir;
        let written_paths: Vec<String> = diff
            .to_write
            .iter()
            .map(|(rel, _)| format!("{}/{}", project_dir_ref, rel))
            .collect();
        let deleted_paths: Vec<String> = diff
            .to_delete
            .iter()
            .map(|rel| format!("{}/{}", project_dir_ref, rel))
            .collect();

        Ok(RestoreResponse::Applied {
            new_commit_oid,
            source_commit: source_oid,
            parent_commit: head_oid,
            written: writes_planned,
            deleted: deletes_planned,
            unchanged: unchanged_count,
            written_paths,
            deleted_paths,
        })
    }
}

/// Load a blob object and return only its payload bytes (header stripped).
///
/// Errors out with `CorruptedObject` if the loaded object is not a blob —
/// this should not happen on a well-formed store but is cheap to verify.
async fn read_blob_payload(
    store: &dyn ObjectStore,
    account: &str,
    blob_oid: &gix_hash::ObjectId,
) -> Result<bytes::Bytes, GitError> {
    let raw = crate::git::util::read_object(store, account, blob_oid).await?;
    let (kind, _, hdr) = crate::git::util::parse_object_header(&raw)?;
    if kind != gix_object::Kind::Blob {
        return Err(GitError::CorruptedObject(format!(
            "expected blob, got {kind:?}"
        )));
    }
    Ok(raw.slice(hdr..))
}

/// Resolve `target_ref` to a commit OID.
///
/// Accepts:
///   1. 40-hex commit OID (validated by `ObjectId::from_hex`)
///   2. Abbreviated OID (4–39 hex chars) — resolved by listing refs and
///      walking parent chains; returns `OidPrefixNotFound` or `AmbiguousOid`
///      on zero / multiple matches
///   3. Full ref path beginning with `refs/` (passed through `validate_ref_name`,
///      then read from `ref_store`)
///   4. Short branch name (e.g. "main") — auto-prefixed to `refs/heads/{name}`,
///      validated, then read from `ref_store`
///
/// Returns `RefStoreError::NotFound` (wrapped) if the ref doesn't exist;
/// `GitError::Other` if `target_ref` is neither a valid OID nor a valid ref name.
///
/// Note: a 40-char hex string is always interpreted as an OID, even if it
/// happens to also be a valid branch name (e.g. `deadbeefdeadbeef...`).
/// To disambiguate such a branch, pass the full ref path `refs/heads/<name>`.
async fn resolve_ref(
    ref_store: &dyn RefStore,
    object_store: &dyn ObjectStore,
    account: &str,
    target_ref: &str,
) -> Result<ObjectId, GitError> {
    // 1. 40-hex commit OID — ASCII hex (case-insensitive), exactly len 40.
    if target_ref.len() == 40 && target_ref.bytes().all(|b| b.is_ascii_hexdigit()) {
        return ObjectId::from_hex(target_ref.as_bytes())
            .map_err(|e| GitError::Other(format!("invalid oid {target_ref}: {e}")));
    }

    // 2. Abbreviated OID (4–39 hex chars) — list refs and walk parent chains.
    if target_ref.len() >= 4 && target_ref.bytes().all(|b| b.is_ascii_hexdigit()) {
        return resolve_abbreviated_oid(ref_store, object_store, account, target_ref).await;
    }

    // 3 & 4. Normalize to full ref path then read.
    let full = if target_ref.starts_with("refs/") {
        target_ref.to_string()
    } else {
        format!("refs/heads/{target_ref}")
    };
    crate::git::util::validate_ref_name(&full)?;
    Ok(ref_store.read(account, &full).await?)
}

/// Decoded commit metadata used by `commit()` (just the tree) and `show()`
/// (full set). Owned so callers don't have to juggle the raw buffer.
struct CommitMeta {
    tree: ObjectId,
    parents: Vec<ObjectId>,
    author: crate::git::types::Actor,
    committer: crate::git::types::Actor,
    message: String,
}

/// Read a commit object and return its decoded metadata.
async fn load_commit_meta(
    store: &dyn ObjectStore,
    account: &str,
    commit_oid: &ObjectId,
) -> Result<CommitMeta, GitError> {
    let raw = crate::git::util::read_object(store, account, commit_oid).await?;
    let (kind, _, hdr) = crate::git::util::parse_object_header(&raw)?;
    if kind != gix_object::Kind::Commit {
        return Err(GitError::Other(format!(
            "expected commit object, got {kind:?}"
        )));
    }
    let parsed = gix_object::CommitRef::from_bytes(&raw[hdr..])
        .map_err(|e| GitError::Other(format!("commit decode: {e}")))?;
    Ok(CommitMeta {
        tree: parsed.tree(),
        parents: parsed.parents().collect(),
        author: actor_from_signature_ref(&parsed.author),
        committer: actor_from_signature_ref(&parsed.committer),
        message: parsed.message.to_string(),
    })
}

/// Resolve an abbreviated commit OID (4–39 hex chars) by walking the parent
/// chains from every ref tip in the account. The traversal is bounded by
/// `MAX_OID_RESOLVE_VISITED` to keep degenerate histories from running away.
///
/// Returns:
/// - `Ok(oid)` if exactly one commit's hex starts with `prefix`.
/// - `Err(GitError::OidPrefixNotFound)` if no commit matches.
/// - `Err(GitError::AmbiguousOid)` if 2+ commits match (lists up to 5 candidates).
///
/// Lowercases `prefix` before comparison; the input is already known to be
/// ASCII hex by the caller.
async fn resolve_abbreviated_oid(
    ref_store: &dyn RefStore,
    object_store: &dyn ObjectStore,
    account: &str,
    prefix: &str,
) -> Result<ObjectId, GitError> {
    use std::collections::HashSet;

    const MAX_OID_RESOLVE_VISITED: usize = 50_000;
    const MAX_REPORTED_CANDIDATES: usize = 5;

    let prefix_lc = prefix.to_ascii_lowercase();

    let refs = ref_store.list(account, "refs/").await?;
    let mut visited: HashSet<ObjectId> = HashSet::new();
    let mut queue: Vec<ObjectId> = refs.into_iter().map(|(_, oid)| oid).collect();
    let mut matches: Vec<ObjectId> = Vec::new();

    while let Some(oid) = queue.pop() {
        if !visited.insert(oid) {
            continue;
        }
        if visited.len() > MAX_OID_RESOLVE_VISITED {
            return Err(GitError::Other(format!(
                "OID prefix resolution aborted: scanned over {MAX_OID_RESOLVE_VISITED} commits without converging"
            )));
        }
        if oid.to_hex().to_string().starts_with(&prefix_lc) {
            matches.push(oid);
            if matches.len() > MAX_REPORTED_CANDIDATES {
                // Continue scanning a little longer to give a useful error,
                // but we already know it's ambiguous.
                break;
            }
        }
        let meta = match load_commit_meta(object_store, account, &oid).await {
            Ok(m) => m,
            Err(GitError::ObjectStore(ObjectStoreError::NotFound(_))) => continue,
            Err(GitError::Other(_)) => continue, // not a commit (tag etc.) — skip
            Err(e) => return Err(e),
        };
        for p in meta.parents {
            if !visited.contains(&p) {
                queue.push(p);
            }
        }
    }

    match matches.len() {
        0 => Err(GitError::OidPrefixNotFound {
            prefix: prefix.to_string(),
        }),
        1 => Ok(matches.into_iter().next().unwrap()),
        n => {
            let listed: Vec<String> = matches
                .iter()
                .take(MAX_REPORTED_CANDIDATES)
                .map(|o| o.to_hex().to_string())
                .collect();
            Err(GitError::AmbiguousOid {
                prefix: prefix.to_string(),
                count: n,
                candidates: listed.join(", "),
            })
        }
    }
}

/// Project a borrowed `gix_actor::SignatureRef` into our owned `Actor` DTO.
///
/// gix-actor 0.31.5 fields used: `SignatureRef.name: &BStr`, `.email: &BStr`,
/// `.time: gix_date::Time` (not the raw `&str` of later versions). `Time`
/// provides `.seconds: i64` and `.offset: i32`.
// TODO: gix_date::Time.sign dropped — Actor not roundtrip-safe for "-0000"
fn actor_from_signature_ref(sig: &gix_actor::SignatureRef<'_>) -> crate::git::types::Actor {
    crate::git::types::Actor {
        name: sig.name.to_string(),
        email: sig.email.to_string(),
        time_seconds: sig.time.seconds,
        tz_offset_seconds: sig.time.offset,
    }
}

/// Return true iff `e` is `Error::NotFound(_)`.
fn is_not_found(e: &crate::core::errors::Error) -> bool {
    matches!(e, crate::core::errors::Error::NotFound(_))
}

/// Validate `project_dir` matches the rules of `TreeEditor::upsert`:
/// non-empty, no leading/trailing `/`, no empty components.
fn validate_project_dir(project_dir: &str) -> Result<(), GitError> {
    if project_dir.is_empty() {
        return Err(GitError::InvalidProjectDir(
            "project_dir must be non-empty".into(),
        ));
    }
    if project_dir.starts_with('/') || project_dir.ends_with('/') {
        return Err(GitError::InvalidProjectDir(format!(
            "project_dir must not start or end with '/': {project_dir:?}"
        )));
    }
    if project_dir.split('/').any(|c| c.is_empty()) {
        return Err(GitError::InvalidProjectDir(format!(
            "project_dir contains empty segment: {project_dir:?}"
        )));
    }
    Ok(())
}

/// Pure-function diff between two flattened subtrees.
///
/// Both inputs are `(path, oid)` slices as returned by `tree_builder::flatten`
/// on a subtree OID — meaning the paths are already relative to the subtree
/// root (no `project_dir` prefix). Results are sorted by path.
fn compute_subtree_diff(
    source: &[(String, gix_hash::ObjectId)],
    head: &[(String, gix_hash::ObjectId)],
) -> crate::git::types::RestoreDiff {
    use std::collections::HashMap;
    let head_map: HashMap<&str, &gix_hash::ObjectId> =
        head.iter().map(|(p, o)| (p.as_str(), o)).collect();
    let source_map: HashMap<&str, &gix_hash::ObjectId> =
        source.iter().map(|(p, o)| (p.as_str(), o)).collect();

    let mut to_write = Vec::new();
    let mut unchanged = Vec::new();
    for (path, oid) in source {
        match head_map.get(path.as_str()) {
            Some(head_oid) if *head_oid == oid => unchanged.push(path.clone()),
            _ => to_write.push((path.clone(), *oid)),
        }
    }
    let mut to_delete: Vec<String> = head
        .iter()
        .filter(|(p, _)| !source_map.contains_key(p.as_str()))
        .map(|(p, _)| p.clone())
        .collect();

    to_write.sort_by(|a, b| a.0.cmp(&b.0));
    to_delete.sort();
    unchanged.sort();
    crate::git::types::RestoreDiff { to_write, to_delete, unchanged }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use crate::core::errors::{Error, Result};
    use crate::core::filesystem::FileSystem;
    use crate::core::types::{FileInfo, TreeEntry, WriteFlag};
    use crate::git::backends::local::{LocalObjectStore, LocalRefStore};
    use crate::git::error::RefStoreError;
    use crate::git::error::ObjectStoreError;
    use crate::git::tree_builder::{flatten, lookup};

    /// In-memory VFS mock that owns a map from absolute path to bytes.
    /// Root for the account is always `/local/{account}` — paths inserted
    /// must be the absolute path including this prefix.
    struct MockVfs {
        account: String,
        files: Arc<Mutex<HashMap<String, Vec<u8>>>>,
        /// When true, `remove` returns NotFound for absent paths (like the real
        /// VFS) instead of silently succeeding. Used to exercise the idempotent
        /// delete path in restore.
        strict_remove: bool,
    }

    impl MockVfs {
        fn new(account: &str) -> Arc<Self> {
            Arc::new(Self {
                account: account.to_string(),
                files: Arc::new(Mutex::new(HashMap::new())),
                strict_remove: false,
            })
        }

        fn new_strict_remove(account: &str) -> Arc<Self> {
            Arc::new(Self {
                account: account.to_string(),
                files: Arc::new(Mutex::new(HashMap::new())),
                strict_remove: true,
            })
        }

        /// Insert/update file content. `rel` is account-relative.
        fn put(&self, rel: &str, data: &[u8]) {
            let abs = format!("/local/{}/{}", self.account, rel);
            self.files.lock().unwrap().insert(abs, data.to_vec());
        }

        /// Delete a file by account-relative path.
        fn delete(&self, rel: &str) {
            let abs = format!("/local/{}/{}", self.account, rel);
            self.files.lock().unwrap().remove(&abs);
        }
    }

    #[async_trait]
    impl FileSystem for MockVfs {
        async fn create(&self, _path: &str) -> Result<()> {
            unimplemented!()
        }
        async fn mkdir(&self, _path: &str, _mode: u32) -> Result<()> {
            unimplemented!()
        }
        async fn remove(&self, path: &str) -> Result<()> {
            let existed = self.files.lock().unwrap().remove(path).is_some();
            if self.strict_remove && !existed {
                return Err(Error::not_found(path));
            }
            Ok(())
        }
        async fn remove_all(&self, _path: &str) -> Result<()> {
            unimplemented!()
        }

        async fn read(&self, path: &str, _offset: u64, _size: u64) -> Result<Vec<u8>> {
            let g = self.files.lock().unwrap();
            match g.get(path) {
                Some(bytes) => Ok(bytes.clone()),
                None => Err(Error::not_found(path)),
            }
        }

        async fn write(
            &self,
            path: &str,
            data: &[u8],
            _offset: u64,
            _flags: WriteFlag,
        ) -> Result<u64> {
            self.files
                .lock()
                .unwrap()
                .insert(path.to_string(), data.to_vec());
            Ok(data.len() as u64)
        }
        async fn read_dir(&self, _path: &str) -> Result<Vec<FileInfo>> {
            unimplemented!()
        }

        async fn stat(&self, path: &str) -> Result<FileInfo> {
            let g = self.files.lock().unwrap();
            if let Some(bytes) = g.get(path) {
                let name = path
                    .rsplit('/')
                    .next()
                    .unwrap_or(path)
                    .to_string();
                return Ok(FileInfo::new_file(name, bytes.len() as u64, 0o644));
            }
            Err(Error::not_found(path))
        }

        async fn rename(&self, _old_path: &str, _new_path: &str) -> Result<()> {
            unimplemented!()
        }
        async fn chmod(&self, _path: &str, _mode: u32) -> Result<()> {
            unimplemented!()
        }

        async fn tree_directory(
            &self,
            path: &str,
            _show_hidden: bool,
            _node_limit: Option<usize>,
            _level_limit: Option<usize>,
        ) -> Result<Vec<TreeEntry>> {
            let prefix = if path == "/" {
                "/".to_string()
            } else {
                format!("{}/", path)
            };
            let g = self.files.lock().unwrap();
            let mut out = Vec::new();
            for (full_path, _bytes) in g.iter() {
                if !full_path.starts_with(&prefix) {
                    continue;
                }
                let rel = full_path
                    .strip_prefix(&prefix)
                    .unwrap_or(full_path)
                    .to_string();
                let name = full_path
                    .rsplit('/')
                    .next()
                    .unwrap_or(full_path)
                    .to_string();
                let info = FileInfo::new_file(name, 0, 0o644);
                out.push(TreeEntry {
                    path: full_path.clone(),
                    rel_path: rel,
                    info,
                    extra: HashMap::new(),
                });
            }
            Ok(out)
        }
    }

    /// Helper: build a fresh GitService backed by a temp dir + a fresh
    /// in-memory VFS for the given account.
    fn make_service(
        account: &str,
    ) -> (
        tempfile::TempDir,
        Arc<MockVfs>,
        Arc<LocalObjectStore>,
        Arc<LocalRefStore>,
        GitService,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let object_store = Arc::new(LocalObjectStore::new(dir.path()));
        let ref_store = Arc::new(LocalRefStore::new(dir.path()));
        let vfs = MockVfs::new(account);
        let svc = GitService::new(
            vfs.clone() as Arc<dyn FileSystem>,
            object_store.clone() as Arc<dyn ObjectStore>,
            ref_store.clone() as Arc<dyn RefStore>,
        );
        (dir, vfs, object_store, ref_store, svc)
    }

    fn req(
        account: &str,
        branch: &str,
        message: &str,
        paths: Option<Vec<String>>,
    ) -> CommitRequest {
        CommitRequest {
            account: account.to_string(),
            branch: branch.to_string(),
            message: message.to_string(),
            paths,
            author_name: "tester".to_string(),
            author_email: "tester@example.com".to_string(),
        }
    }

    /// Load a commit's parent OIDs from the object store.
    async fn commit_parents(
        store: &dyn ObjectStore,
        account: &str,
        commit_oid: ObjectId,
    ) -> Vec<ObjectId> {
        let raw = crate::git::util::read_object(store, account, &commit_oid)
            .await
            .unwrap();
        let (_, _, hdr) = crate::git::util::parse_object_header(&raw).unwrap();
        let parsed = gix_object::CommitRef::from_bytes(&raw[hdr..]).unwrap();
        parsed.parents().collect()
    }

    async fn commit_tree(
        store: &dyn ObjectStore,
        account: &str,
        commit_oid: ObjectId,
    ) -> ObjectId {
        load_commit_meta(store, account, &commit_oid)
            .await
            .unwrap()
            .tree
    }

    /// Make a commit and return its OID.
    async fn make_commit(
        svc: &GitService,
        account: &str,
        branch: &str,
        msg: &str,
    ) -> ObjectId {
        match svc.commit(req(account, branch, msg, None)).await.unwrap() {
            CommitResponse::Created { commit_oid, .. } => commit_oid,
            other => panic!("expected Created, got {other:?}"),
        }
    }

    // ── 1 ──────────────────────────────────────────────────────────────
    #[tokio::test]
    async fn test_commit_first_creates_root_commit() {
        let (_dir, vfs, object_store, ref_store, svc) = make_service("acct");
        vfs.put("resources/a.md", b"hello");

        let resp = svc
            .commit(req("acct", "main", "first", None))
            .await
            .unwrap();

        match resp {
            CommitResponse::Created { commit_oid, changed } => {
                assert!(changed >= 1, "should record at least one change");
                let parents = commit_parents(
                    object_store.as_ref() as &dyn ObjectStore,
                    "acct",
                    commit_oid,
                )
                .await;
                assert!(parents.is_empty(), "root commit must have no parents");
                let tree = commit_tree(
                    object_store.as_ref() as &dyn ObjectStore,
                    "acct",
                    commit_oid,
                )
                .await;
                assert_ne!(tree, ObjectId::empty_tree(gix_hash::Kind::Sha1));
                let head = ref_store.read("acct", "refs/heads/main").await.unwrap();
                assert_eq!(head, commit_oid);
            }
            other => panic!("expected Created, got {other:?}"),
        }
    }

    // ── 2 ──────────────────────────────────────────────────────────────
    #[tokio::test]
    async fn test_commit_second_links_to_first() {
        let (_dir, vfs, object_store, _ref_store, svc) = make_service("acct");
        vfs.put("resources/a.md", b"hello");
        let first = svc.commit(req("acct", "main", "first", None)).await.unwrap();
        let first_oid = match first {
            CommitResponse::Created { commit_oid, .. } => commit_oid,
            other => panic!("expected Created, got {other:?}"),
        };

        vfs.put("resources/a.md", b"world");
        let second = svc
            .commit(req("acct", "main", "second", None))
            .await
            .unwrap();
        let second_oid = match second {
            CommitResponse::Created { commit_oid, .. } => commit_oid,
            other => panic!("expected Created, got {other:?}"),
        };

        let parents = commit_parents(
            object_store.as_ref() as &dyn ObjectStore,
            "acct",
            second_oid,
        )
        .await;
        assert_eq!(parents, vec![first_oid]);
    }

    // ── 3 ──────────────────────────────────────────────────────────────
    #[tokio::test]
    async fn test_commit_noop_when_nothing_changed() {
        let (_dir, vfs, _object_store, ref_store, svc) = make_service("acct");
        vfs.put("resources/a.md", b"hello");
        let first = svc.commit(req("acct", "main", "first", None)).await.unwrap();
        let first_oid = match first {
            CommitResponse::Created { commit_oid, .. } => commit_oid,
            other => panic!("expected Created, got {other:?}"),
        };

        let second = svc.commit(req("acct", "main", "noop", None)).await.unwrap();
        match second {
            CommitResponse::Noop { commit_oid } => assert_eq!(commit_oid, first_oid),
            other => panic!("expected Noop, got {other:?}"),
        }

        let head = ref_store.read("acct", "refs/heads/main").await.unwrap();
        assert_eq!(head, first_oid);
    }

    // ── 4 ──────────────────────────────────────────────────────────────
    #[tokio::test]
    async fn test_commit_handles_deletes() {
        let (_dir, vfs, object_store, _ref_store, svc) = make_service("acct");
        vfs.put("resources/a.md", b"hello");
        vfs.put("resources/b.md", b"world");
        let _ = svc
            .commit(req("acct", "main", "first", None))
            .await
            .unwrap();

        vfs.delete("resources/a.md");
        let resp = svc
            .commit(req("acct", "main", "delete-a", Some(vec!["resources/a.md".to_string()])))
            .await
            .unwrap();
        let second_oid = match resp {
            CommitResponse::Created { commit_oid, .. } => commit_oid,
            other => panic!("expected Created, got {other:?}"),
        };

        let tree = commit_tree(
            object_store.as_ref() as &dyn ObjectStore,
            "acct",
            second_oid,
        )
        .await;
        let all = flatten(
            object_store.as_ref() as &dyn ObjectStore,
            "acct",
            tree,
            &None,
        )
        .await
        .unwrap();
        let paths: Vec<String> = all.into_iter().map(|(p, _)| p).collect();
        assert_eq!(paths, vec!["resources/b.md".to_string()]);
    }

    // ── 5 ──────────────────────────────────────────────────────────────
    #[tokio::test]
    async fn test_commit_with_explicit_paths_skips_others() {
        let (_dir, vfs, object_store, _ref_store, svc) = make_service("acct");
        vfs.put("resources/a.md", b"A");
        vfs.put("resources/b.md", b"B");
        vfs.put("resources/c.md", b"C");

        let resp = svc
            .commit(req(
                "acct",
                "main",
                "only-a",
                Some(vec!["resources/a.md".to_string()]),
            ))
            .await
            .unwrap();
        let oid = match resp {
            CommitResponse::Created { commit_oid, .. } => commit_oid,
            other => panic!("expected Created, got {other:?}"),
        };

        let tree = commit_tree(
            object_store.as_ref() as &dyn ObjectStore,
            "acct",
            oid,
        )
        .await;
        let all = flatten(
            object_store.as_ref() as &dyn ObjectStore,
            "acct",
            tree,
            &None,
        )
        .await
        .unwrap();
        let paths: Vec<String> = all.into_iter().map(|(p, _)| p).collect();
        assert_eq!(paths, vec!["resources/a.md".to_string()]);
        // Sanity-check the blob is reachable via lookup too.
        let found = lookup(
            object_store.as_ref() as &dyn ObjectStore,
            "acct",
            tree,
            "resources/a.md",
        )
        .await
        .unwrap();
        assert!(found.is_some());
    }

    // ── 6 ──────────────────────────────────────────────────────────────

    /// Wrapping RefStore that forces the next `cas_update` call to fail
    /// with `Conflict`, then delegates to the inner store afterwards.
    struct ConflictOnceRef {
        inner: Arc<LocalRefStore>,
        fired: Mutex<bool>,
        actual: Option<ObjectId>,
    }

    #[async_trait]
    impl RefStore for ConflictOnceRef {
        async fn read(
            &self,
            account: &str,
            ref_name: &str,
        ) -> std::result::Result<ObjectId, RefStoreError> {
            self.inner.read(account, ref_name).await
        }

        async fn cas_update(
            &self,
            account: &str,
            ref_name: &str,
            expected: Option<ObjectId>,
            new: ObjectId,
        ) -> std::result::Result<(), RefStoreError> {
            let should_conflict = {
                let mut fired = self.fired.lock().unwrap();
                if !*fired {
                    *fired = true;
                    true
                } else {
                    false
                }
            };
            if should_conflict {
                return Err(RefStoreError::Conflict {
                    expected,
                    actual: self.actual,
                });
            }
            self.inner.cas_update(account, ref_name, expected, new).await
        }

        async fn list(
            &self,
            account: &str,
            prefix: &str,
        ) -> std::result::Result<Vec<(String, ObjectId)>, RefStoreError> {
            self.inner.list(account, prefix).await
        }
    }

    #[tokio::test]
    async fn test_commit_cas_conflict_surfaces_as_error() {
        let dir = tempfile::tempdir().unwrap();
        let object_store = Arc::new(LocalObjectStore::new(dir.path()));
        let inner_ref = Arc::new(LocalRefStore::new(dir.path()));
        let bogus = ObjectId::from_hex(b"deadbeefdeadbeefdeadbeefdeadbeefdeadbeef").unwrap();
        let ref_store = Arc::new(ConflictOnceRef {
            inner: inner_ref.clone(),
            fired: Mutex::new(false),
            actual: Some(bogus),
        });
        let vfs = MockVfs::new("acct");
        vfs.put("resources/a.md", b"hello");
        let svc = GitService::new(
            vfs.clone() as Arc<dyn FileSystem>,
            object_store.clone() as Arc<dyn ObjectStore>,
            ref_store.clone() as Arc<dyn RefStore>,
        );

        let result = svc.commit(req("acct", "main", "boom", None)).await;
        match result {
            Err(GitError::ConcurrentCommit {
                ref_name,
                expected,
                actual,
            }) => {
                assert_eq!(ref_name, "refs/heads/main");
                assert_eq!(expected, None);
                assert_eq!(actual, Some(bogus));
            }
            other => panic!("expected ConcurrentCommit, got {other:?}"),
        }
    }

    // ── 7 ──────────────────────────────────────────────────────────────
    // Verifies the incremental commit path reuses unchanged subtree OIDs:
    // modifying a file under `resources/` must NOT rewrite the `agent/`
    // subtree object — its OID must be byte-identical across commits.
    #[tokio::test]
    async fn test_commit_incremental_reuses_unchanged_subtree_oids() {
        let (_dir, vfs, object_store, _ref_store, svc) = make_service("acct");
        vfs.put("resources/a.md", b"hello");
        vfs.put("agent/b.py", b"print('hi')");

        let first = svc.commit(req("acct", "main", "first", None)).await.unwrap();
        let first_oid = match first {
            CommitResponse::Created { commit_oid, .. } => commit_oid,
            other => panic!("expected Created, got {other:?}"),
        };
        let first_tree = commit_tree(
            object_store.as_ref() as &dyn ObjectStore,
            "acct",
            first_oid,
        )
        .await;
        let agent_first = lookup(
            object_store.as_ref() as &dyn ObjectStore,
            "acct",
            first_tree,
            "agent",
        )
        .await
        .unwrap()
        .expect("agent subtree must exist after first commit");
        assert!(agent_first.1.is_tree(), "agent entry must be a tree");

        // Touch only resources/a.md.
        vfs.put("resources/a.md", b"world");
        let second = svc.commit(req("acct", "main", "second", None)).await.unwrap();
        let second_oid = match second {
            CommitResponse::Created { commit_oid, .. } => commit_oid,
            other => panic!("expected Created, got {other:?}"),
        };
        let second_tree = commit_tree(
            object_store.as_ref() as &dyn ObjectStore,
            "acct",
            second_oid,
        )
        .await;
        assert_ne!(
            first_tree, second_tree,
            "root tree must change because resources/a.md changed",
        );
        let agent_second = lookup(
            object_store.as_ref() as &dyn ObjectStore,
            "acct",
            second_tree,
            "agent",
        )
        .await
        .unwrap()
        .expect("agent subtree must still exist after second commit");

        assert_eq!(
            agent_first.0, agent_second.0,
            "unchanged agent/ subtree OID must be reused across commits",
        );
    }

    // ── 8 ──────────────────────────────────────────────────────────────
    #[tokio::test]
    async fn test_commit_skips_pruned_paths() {
        let (_dir, vfs, object_store, _ref_store, svc) = make_service("acct");
        vfs.put("resources/a.md", b"hello");
        vfs.put("resources/x.faiss", b"FAISS");
        vfs.put("_system/lock", b"L");

        let resp = svc
            .commit(req("acct", "main", "filtered", None))
            .await
            .unwrap();
        let oid = match resp {
            CommitResponse::Created { commit_oid, .. } => commit_oid,
            other => panic!("expected Created, got {other:?}"),
        };

        let tree = commit_tree(
            object_store.as_ref() as &dyn ObjectStore,
            "acct",
            oid,
        )
        .await;
        let all = flatten(
            object_store.as_ref() as &dyn ObjectStore,
            "acct",
            tree,
            &None,
        )
        .await
        .unwrap();
        let paths: Vec<String> = all.into_iter().map(|(p, _)| p).collect();
        assert_eq!(paths, vec!["resources/a.md".to_string()]);
    }

    // ── 9: show ────────────────────────────────────────────────────────
    #[tokio::test]
    async fn test_show_commit_meta_by_oid() {
        let (_dir, vfs, _object_store, _ref_store, svc) = make_service("acct");
        vfs.put("resources/a.md", b"hello");
        let oid = make_commit(&svc, "acct", "main", "first").await;

        let resp = svc
            .show(ShowRequest {
                account: "acct".into(),
                target_ref: oid.to_hex().to_string(),
                path: None,
            })
            .await
            .unwrap();

        match resp {
            ShowResponse::Commit {
                oid: returned,
                parents,
                message,
                author,
                committer,
                tree,
            } => {
                assert_eq!(returned, oid);
                assert!(parents.is_empty(), "root commit");
                assert_eq!(message, "first");
                assert_eq!(author.name, "tester");
                assert_eq!(author.email, "tester@example.com");
                assert_eq!(committer.name, "tester");
                assert_ne!(tree, ObjectId::empty_tree(gix_hash::Kind::Sha1));
            }
            other => panic!("expected Commit, got {other:?}"),
        }
    }

    // ── 10 ─────────────────────────────────────────────────────────────
    #[tokio::test]
    async fn test_show_resolves_branch_name_and_full_ref() {
        let (_dir, vfs, _object_store, _ref_store, svc) = make_service("acct");
        vfs.put("resources/a.md", b"hello");
        let oid = make_commit(&svc, "acct", "main", "first").await;

        for tref in ["main", "refs/heads/main"] {
            let resp = svc
                .show(ShowRequest {
                    account: "acct".into(),
                    target_ref: tref.into(),
                    path: None,
                })
                .await
                .unwrap();
            match resp {
                ShowResponse::Commit { oid: returned, .. } => assert_eq!(returned, oid),
                other => panic!("{tref}: expected Commit, got {other:?}"),
            }
        }
    }

    // ── 10b: abbreviated OID resolution ────────────────────────────────
    #[tokio::test]
    async fn test_show_resolves_short_oid_unique() {
        let (_dir, vfs, _object_store, _ref_store, svc) = make_service("acct");
        vfs.put("resources/a.md", b"hello");
        let oid = make_commit(&svc, "acct", "main", "first").await;
        let full = oid.to_hex().to_string();

        for len in [4usize, 7, 12, 39] {
            let short = &full[..len];
            let resp = svc
                .show(ShowRequest {
                    account: "acct".into(),
                    target_ref: short.into(),
                    path: None,
                })
                .await
                .unwrap_or_else(|e| panic!("short oid {short} (len {len}) should resolve, got {e}"));
            match resp {
                ShowResponse::Commit { oid: returned, .. } => assert_eq!(returned, oid),
                other => panic!("len {len}: expected Commit, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn test_show_short_oid_not_found_distinguished_from_branch() {
        let (_dir, vfs, _object_store, _ref_store, svc) = make_service("acct");
        vfs.put("resources/a.md", b"hello");
        let _ = make_commit(&svc, "acct", "main", "first").await;

        // A 4-hex string that almost-certainly does not match any commit.
        // (SHA-1 collision against a single commit is astronomically unlikely
        // for "ffff" — the first commit's hex is deterministic given the
        // test's actor/time-zero, so this is a stable miss.)
        let bogus = "ffff";
        let err = svc
            .show(ShowRequest {
                account: "acct".into(),
                target_ref: bogus.into(),
                path: None,
            })
            .await
            .unwrap_err();
        assert!(
            matches!(err, GitError::OidPrefixNotFound { ref prefix } if prefix == bogus),
            "expected OidPrefixNotFound({bogus}), got {err:?}",
        );
    }

    #[tokio::test]
    async fn test_short_oid_three_chars_falls_through_to_ref_lookup() {
        // 3 hex chars is below the 4-char floor for abbreviated OID; it
        // should be treated as a branch name (which doesn't exist), giving
        // a RefStore::NotFound error — NOT OidPrefixNotFound.
        let (_dir, vfs, _object_store, _ref_store, svc) = make_service("acct");
        vfs.put("resources/a.md", b"hello");
        let _ = make_commit(&svc, "acct", "main", "first").await;

        let err = svc
            .show(ShowRequest {
                account: "acct".into(),
                target_ref: "abc".into(),
                path: None,
            })
            .await
            .unwrap_err();
        assert!(
            matches!(err, GitError::RefStore(RefStoreError::NotFound(_))),
            "expected RefStore::NotFound for 3-char input, got {err:?}",
        );
    }

    // ── 11 ─────────────────────────────────────────────────────────────
    #[tokio::test]
    async fn test_show_blob_round_trip() {
        let (_dir, vfs, _object_store, _ref_store, svc) = make_service("acct");
        let body = b"hello world\n";
        vfs.put("resources/a.md", body);
        let _ = make_commit(&svc, "acct", "main", "first").await;

        let resp = svc
            .show(ShowRequest {
                account: "acct".into(),
                target_ref: "main".into(),
                path: Some("resources/a.md".into()),
            })
            .await
            .unwrap();

        match resp {
            ShowResponse::Blob { bytes, size, oid: _ } => {
                assert_eq!(bytes.as_ref(), body);
                assert_eq!(size, body.len() as u64);
            }
            other => panic!("expected Blob, got {other:?}"),
        }
    }

    // ── 12 ─────────────────────────────────────────────────────────────
    #[tokio::test]
    async fn test_show_blob_path_not_found() {
        let (_dir, vfs, _object_store, _ref_store, svc) = make_service("acct");
        vfs.put("resources/a.md", b"x");
        let _ = make_commit(&svc, "acct", "main", "first").await;

        let err = svc
            .show(ShowRequest {
                account: "acct".into(),
                target_ref: "main".into(),
                path: Some("resources/missing.md".into()),
            })
            .await
            .unwrap_err();

        match err {
            GitError::PathNotFound(p) => assert_eq!(p, "resources/missing.md"),
            other => panic!("expected PathNotFound, got {other:?}"),
        }
    }

    // ── 13 ─────────────────────────────────────────────────────────────
    #[tokio::test]
    async fn test_show_blob_rejects_directory_path() {
        let (_dir, vfs, _object_store, _ref_store, svc) = make_service("acct");
        vfs.put("resources/a.md", b"x");
        let _ = make_commit(&svc, "acct", "main", "first").await;

        let err = svc
            .show(ShowRequest {
                account: "acct".into(),
                target_ref: "main".into(),
                path: Some("resources".into()),
            })
            .await
            .unwrap_err();

        match err {
            GitError::PathIsDirectory(p) => assert_eq!(p, "resources"),
            other => panic!("expected PathIsDirectory, got {other:?}"),
        }
    }

    // ── 14 ─────────────────────────────────────────────────────────────
    #[tokio::test]
    async fn test_show_unknown_ref() {
        let (_dir, _vfs, _object_store, _ref_store, svc) = make_service("acct");
        let err = svc
            .show(ShowRequest {
                account: "acct".into(),
                target_ref: "nonexistent".into(),
                path: None,
            })
            .await
            .unwrap_err();

        match err {
            GitError::RefStore(RefStoreError::NotFound(name)) => {
                assert_eq!(name, "refs/heads/nonexistent");
            }
            other => panic!("expected RefStore NotFound, got {other:?}"),
        }
    }

    // ── 15 ─────────────────────────────────────────────────────────────
    #[tokio::test]
    async fn test_show_malformed_oid_input() {
        let (_dir, _vfs, _object_store, _ref_store, svc) = make_service("acct");
        let err = svc
            .show(ShowRequest {
                account: "acct".into(),
                target_ref: "z".repeat(40),
                path: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, GitError::Other(_) | GitError::RefStore(_)));
    }

    // ── 16 ─────────────────────────────────────────────────────────────
    /// Blob bytes survive a round-trip even when they contain NUL bytes,
    /// non-UTF-8 sequences, and multiple newlines. Guards against any
    /// future "treat blobs as strings" regression.
    #[tokio::test]
    async fn test_show_blob_binary_and_multiline() {
        let (_dir, vfs, _object_store, _ref_store, svc) = make_service("acct");
        // NUL, invalid UTF-8 (0xC3 0x28 is an invalid 2-byte sequence), CRLF, LF.
        let body: Vec<u8> = vec![
            b'h', b'i', 0x00, 0xC3, 0x28, b'\r', b'\n', b'l', b'i', b'n', b'e', b'2', b'\n',
            0xFF, 0xFE, 0xFD,
        ];
        vfs.put("resources/bin.dat", &body);
        let _ = make_commit(&svc, "acct", "main", "first").await;

        let resp = svc
            .show(ShowRequest {
                account: "acct".into(),
                target_ref: "main".into(),
                path: Some("resources/bin.dat".into()),
            })
            .await
            .unwrap();

        match resp {
            ShowResponse::Blob { bytes, size, .. } => {
                assert_eq!(bytes, body);
                assert_eq!(size as usize, body.len());
            }
            other => panic!("expected Blob, got {other:?}"),
        }
    }

    // ── 17 ─────────────────────────────────────────────────────────────
    /// Construct a commit whose author and committer differ, write it
    /// directly via `util::write_object`, point a ref at it, and verify
    /// `show()` decodes the two signatures into the two Actor fields
    /// without crossing them. Bypasses `commit()` because the public
    /// `CommitRequest` API only accepts one author (used for both).
    #[tokio::test]
    async fn test_show_distinguishes_committer_from_author() {
        use gix_object::{bstr::BString, Commit, WriteTo};

        let (_dir, vfs, object_store, ref_store, svc) = make_service("acct");
        vfs.put("resources/a.md", b"x");
        // First, create a normal commit just to get a real tree OID.
        let seed_oid = make_commit(&svc, "acct", "main", "seed").await;
        let seed_tree = load_commit_meta(object_store.as_ref() as &dyn ObjectStore, "acct", &seed_oid)
            .await
            .unwrap()
            .tree;

        // Build a commit with deliberately mismatched author/committer.
        let author = gix_actor::Signature {
            name: "Alice Author".into(),
            email: "alice@example.com".into(),
            time: gix_date::Time {
                seconds: 1_700_000_000,
                offset: 3600,
                sign: gix_date::time::Sign::Plus,
            },
        };
        let committer = gix_actor::Signature {
            name: "Carol Committer".into(),
            email: "carol@example.com".into(),
            time: gix_date::Time {
                seconds: 1_700_000_100,
                offset: -7200,
                sign: gix_date::time::Sign::Minus,
            },
        };
        let commit = Commit {
            tree: seed_tree,
            parents: Vec::new().into(),
            author,
            committer,
            encoding: None,
            message: BString::from("split-actors"),
            extra_headers: Vec::new(),
        };
        let mut buf = Vec::new();
        commit.write_to(&mut buf).unwrap();
        let oid = crate::git::util::write_object(
            object_store.as_ref() as &dyn ObjectStore,
            "acct",
            gix_object::Kind::Commit,
            &buf,
        )
        .await
        .unwrap();

        // Point a fresh branch at it so show() can find it by name.
        ref_store
            .cas_update("acct", "refs/heads/split", None, oid)
            .await
            .unwrap();

        let resp = svc
            .show(ShowRequest {
                account: "acct".into(),
                target_ref: "split".into(),
                path: None,
            })
            .await
            .unwrap();

        match resp {
            ShowResponse::Commit { author, committer, .. } => {
                assert_eq!(author.name, "Alice Author");
                assert_eq!(author.email, "alice@example.com");
                assert_eq!(author.time_seconds, 1_700_000_000);
                assert_eq!(author.tz_offset_seconds, 3600);
                assert_eq!(committer.name, "Carol Committer");
                assert_eq!(committer.email, "carol@example.com");
                assert_eq!(committer.time_seconds, 1_700_000_100);
                assert_eq!(committer.tz_offset_seconds, -7200);
            }
            other => panic!("expected Commit, got {other:?}"),
        }
    }

    // ── 18 ─────────────────────────────────────────────────────────────
    /// When an intermediate path component is a blob (not a tree),
    /// `tree_builder::lookup` returns `Ok(None)`, which `show()` maps
    /// to `PathNotFound`. Pin this so a future change can't silently
    /// reinterpret it as `PathIsDirectory` or `CorruptedObject`.
    #[tokio::test]
    async fn test_show_intermediate_path_component_is_blob() {
        let (_dir, vfs, _object_store, _ref_store, svc) = make_service("acct");
        vfs.put("resources/a.md", b"x");
        let _ = make_commit(&svc, "acct", "main", "first").await;

        let err = svc
            .show(ShowRequest {
                account: "acct".into(),
                target_ref: "main".into(),
                path: Some("resources/a.md/oops".into()),
            })
            .await
            .unwrap_err();

        match err {
            GitError::PathNotFound(p) => assert_eq!(p, "resources/a.md/oops"),
            other => panic!("expected PathNotFound, got {other:?}"),
        }
    }

    // ── 19 ─────────────────────────────────────────────────────────────
    /// Pin the current per-shape behavior for malformed paths so any
    /// future input normalization change is explicit:
    ///   - `""`     → `Other` (empty path rejected by `lookup` up-front)
    ///   - `"/x"`   → `Other` (first component is empty)
    ///   - `"x/"`   → `PathNotFound` (lookup fails on missing "x" before
    ///                ever inspecting the trailing empty component)
    ///   - `"a//b"` → `PathNotFound` (lookup fails on missing "a" before
    ///                ever inspecting the empty middle component)
    #[tokio::test]
    async fn test_show_path_with_invalid_form() {
        let (_dir, vfs, _object_store, _ref_store, svc) = make_service("acct");
        vfs.put("resources/a.md", b"x");
        let _ = make_commit(&svc, "acct", "main", "first").await;

        let cases: &[(&str, fn(&GitError) -> bool)] = &[
            ("", |e| matches!(e, GitError::Other(_))),
            ("/x", |e| matches!(e, GitError::Other(_))),
            ("x/", |e| matches!(e, GitError::PathNotFound(p) if p == "x/")),
            ("a//b", |e| matches!(e, GitError::PathNotFound(p) if p == "a//b")),
        ];

        for (bad, check) in cases {
            let err = svc
                .show(ShowRequest {
                    account: "acct".into(),
                    target_ref: "main".into(),
                    path: Some((*bad).into()),
                })
                .await
                .unwrap_err();
            assert!(check(&err), "path {bad:?}: unexpected error variant {err:?}");
        }
    }

    // ── 20 ─────────────────────────────────────────────────────────────
    /// If the commit's loose object file is removed from the store
    /// after the ref still points at it, `show()` must surface
    /// `ObjectStoreError::NotFound` (wrapped in `GitError::ObjectStore`).
    /// Guards against any future "swallow missing objects" regression
    /// inside `load_commit_meta`.
    #[tokio::test]
    async fn test_show_commit_object_missing_from_store() {
        let (dir, vfs, _object_store, _ref_store, svc) = make_service("acct");
        vfs.put("resources/a.md", b"x");
        let oid = make_commit(&svc, "acct", "main", "first").await;

        // LocalObjectStore layout: {base_dir}/{account}/objects/{aa}/{bb...}
        let hex = oid.to_hex().to_string();
        let path = dir
            .path()
            .join("acct")
            .join("objects")
            .join(&hex[..2])
            .join(&hex[2..]);
        std::fs::remove_file(&path).expect("loose commit object must exist before removal");

        let err = svc
            .show(ShowRequest {
                account: "acct".into(),
                target_ref: "main".into(),
                path: None,
            })
            .await
            .unwrap_err();

        match err {
            GitError::ObjectStore(ObjectStoreError::NotFound(missing)) => {
                assert_eq!(missing, oid);
            }
            other => panic!("expected ObjectStore(NotFound), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_mock_vfs_write_then_read_round_trip() {
        let vfs = MockVfs::new("acct");
        let path = "/local/acct/x.md";
        vfs.files.lock().unwrap().insert(path.to_string(), Vec::new());
        FileSystem::write(vfs.as_ref(), path, b"hello", 0, WriteFlag::Create)
            .await
            .unwrap();
        let got = FileSystem::read(vfs.as_ref(), path, 0, 0).await.unwrap();
        assert_eq!(got, b"hello");
        FileSystem::remove(vfs.as_ref(), path).await.unwrap();
        let err = FileSystem::read(vfs.as_ref(), path, 0, 0).await.unwrap_err();
        assert!(matches!(err, Error::NotFound(_)));
    }

    // ── restore: dry_run ───────────────────────────────────────────────
    #[tokio::test]
    async fn test_restore_dry_run_reports_diff_and_writes_nothing() {
        let (_dir, vfs, object_store, ref_store, svc) = make_service("acct");
        // Source state: resources/proj_a has files a.md, b.md
        vfs.put("resources/proj_a/a.md", b"A v1");
        vfs.put("resources/proj_a/b.md", b"B v1");
        let source_oid = make_commit(&svc, "acct", "main", "source").await;

        // HEAD state: a.md is rewritten, b.md is deleted, c.md is created.
        // We pass explicit paths (including the deleted b.md) so commit()
        // sees the tombstone — collect_all() only enumerates surviving files.
        vfs.put("resources/proj_a/a.md", b"A v2");
        vfs.delete("resources/proj_a/b.md");
        vfs.put("resources/proj_a/c.md", b"C new");
        let head_oid = match svc
            .commit(req(
                "acct",
                "main",
                "head",
                Some(vec![
                    "resources/proj_a/a.md".to_string(),
                    "resources/proj_a/b.md".to_string(),
                    "resources/proj_a/c.md".to_string(),
                ]),
            ))
            .await
            .unwrap()
        {
            CommitResponse::Created { commit_oid, .. } => commit_oid,
            other => panic!("expected Created, got {other:?}"),
        };

        let resp = svc
            .restore(RestoreRequest {
                account: "acct".into(),
                branch: "main".into(),
                project_dir: "resources/proj_a".into(),
                source_commit: source_oid.to_hex().to_string(),
                dry_run: true,
                message: None,
                author_name: "tester".into(),
                author_email: "tester@example.com".into(),
            })
            .await
            .unwrap();

        match resp {
            RestoreResponse::DryRun { diff, head, source } => {
                assert_eq!(source, source_oid);
                assert_eq!(head, head_oid);
                // a.md needs to roll back to v1, b.md needs to come back,
                // c.md needs to go away. Sorted alphabetically by path.
                assert_eq!(diff.to_write.len(), 2);
                assert_eq!(diff.to_write[0].0, "a.md");
                assert_eq!(diff.to_write[1].0, "b.md");
                assert_eq!(diff.to_delete, vec!["c.md".to_string()]);
                assert!(diff.unchanged.is_empty());
            }
            other => panic!("expected DryRun, got {other:?}"),
        }

        // CRITICAL: dry_run wrote nothing through the VFS — c.md and the v2
        // version of a.md must still be visible on disk.
        let files = vfs.files.lock().unwrap();
        assert_eq!(
            files.get("/local/acct/resources/proj_a/a.md").unwrap(),
            b"A v2",
            "dry_run must not overwrite a.md",
        );
        assert!(
            files.contains_key("/local/acct/resources/proj_a/c.md"),
            "dry_run must not delete c.md",
        );
        // Branch ref must still point at head_oid.
        let head_after = ref_store.read("acct", "refs/heads/main").await.unwrap();
        assert_eq!(head_after, head_oid);
        let _ = object_store; // silence unused warning
    }

    // ── restore: apply ─────────────────────────────────────────────────
    #[tokio::test]
    async fn test_restore_apply_writes_new_commit_with_head_as_parent() {
        let (_dir, vfs, object_store, ref_store, svc) = make_service("acct");
        vfs.put("resources/proj_a/a.md", b"A v1");
        vfs.put("resources/proj_a/b.md", b"B v1");
        let source_oid = make_commit(&svc, "acct", "main", "source").await;

        vfs.put("resources/proj_a/a.md", b"A v2");
        vfs.delete("resources/proj_a/b.md");
        vfs.put("resources/proj_a/c.md", b"C new");
        // IMPORTANT: use explicit paths so the deletion of b.md is captured
        let head_oid = match svc
            .commit(req(
                "acct",
                "main",
                "head",
                Some(vec![
                    "resources/proj_a/a.md".to_string(),
                    "resources/proj_a/b.md".to_string(),
                    "resources/proj_a/c.md".to_string(),
                ]),
            ))
            .await
            .unwrap()
        {
            CommitResponse::Created { commit_oid, .. } => commit_oid,
            other => panic!("expected Created, got {other:?}"),
        };

        let resp = svc
            .restore(RestoreRequest {
                account: "acct".into(),
                branch: "main".into(),
                project_dir: "resources/proj_a".into(),
                source_commit: source_oid.to_hex().to_string(),
                dry_run: false,
                message: Some("rewind proj_a".into()),
                author_name: "tester".into(),
                author_email: "tester@example.com".into(),
            })
            .await
            .unwrap();

        let new_oid = match resp {
            RestoreResponse::Applied {
                new_commit_oid,
                source_commit,
                parent_commit,
                written,
                deleted,
                unchanged,
                written_paths,
                deleted_paths,
            } => {
                assert_eq!(source_commit, source_oid);
                assert_eq!(parent_commit, head_oid, "parent MUST be HEAD, NOT source");
                assert_eq!(written, 2, "a.md (rewrite) + b.md (recreate) = 2");
                assert_eq!(deleted, 1, "c.md");
                assert_eq!(unchanged, 0);
                assert_eq!(written_paths.len(), 2);
                assert_eq!(deleted_paths.len(), 1);
                // Paths should be account-relative (project_dir-prefixed).
                for p in &written_paths {
                    assert!(
                        p.starts_with("resources/proj_a/"),
                        "written path missing project_dir prefix: {p}"
                    );
                }
                for p in &deleted_paths {
                    assert!(
                        p.starts_with("resources/proj_a/"),
                        "deleted path missing project_dir prefix: {p}"
                    );
                }
                new_commit_oid
            }
            other => panic!("expected Applied, got {other:?}"),
        };

        // Ref now points at new_oid.
        assert_eq!(
            ref_store.read("acct", "refs/heads/main").await.unwrap(),
            new_oid
        );
        // New commit's parents = [head_oid] (NOT source_oid — this is the key
        // invariant of restore vs. plain checkout).
        let parents = commit_parents(
            object_store.as_ref() as &dyn ObjectStore,
            "acct",
            new_oid,
        )
        .await;
        assert_eq!(parents, vec![head_oid]);

        // VFS rolled back as expected.
        let files = vfs.files.lock().unwrap();
        assert_eq!(
            files.get("/local/acct/resources/proj_a/a.md").unwrap(),
            b"A v1",
            "a.md rolled back",
        );
        assert_eq!(
            files.get("/local/acct/resources/proj_a/b.md").unwrap(),
            b"B v1",
            "b.md restored",
        );
        assert!(
            !files.contains_key("/local/acct/resources/proj_a/c.md"),
            "c.md deleted",
        );
    }

    // Regression: restoring to a revision where a whole subdirectory's files
    // are gone must not leave an empty directory husk behind. Git does not
    // track directories, so the delete diff only lists files — restore is
    // responsible for pruning directories emptied by those deletes.
    //
    // Backed by a real `LocalFileSystem`: the in-memory `MockVfs` models
    // directories implicitly (deleting the last file makes the dir vanish for
    // free) and so cannot reproduce the husk. LocalFS keeps the directory on
    // disk, exactly like production, which is what makes this test meaningful.
    #[tokio::test]
    async fn test_restore_prunes_directories_emptied_by_delete() {
        use crate::plugins::localfs::LocalFileSystem;

        let store_dir = tempfile::tempdir().unwrap();
        let object_store = Arc::new(LocalObjectStore::new(store_dir.path()));
        let ref_store = Arc::new(LocalRefStore::new(store_dir.path()));

        // Working tree root: /local/acct lives under this temp dir.
        let work_dir = tempfile::tempdir().unwrap();
        let acct_root = work_dir.path().join("local").join("acct");
        std::fs::create_dir_all(&acct_root).unwrap();
        let vfs: Arc<dyn FileSystem> =
            Arc::new(LocalFileSystem::new(work_dir.path().to_str().unwrap()).unwrap());

        let svc = GitService::new(vfs.clone(), object_store.clone(), ref_store.clone());

        // Source commit: keeper.md at the project root only.
        std::fs::create_dir_all(acct_root.join("resources/proj_a")).unwrap();
        std::fs::write(acct_root.join("resources/proj_a/keeper.md"), b"keep").unwrap();
        let source_oid = make_commit(&svc, "acct", "main", "source").await;

        // HEAD adds a nested subdir whose only files restore will delete.
        std::fs::create_dir_all(acct_root.join("resources/proj_a/nested/deep")).unwrap();
        std::fs::write(acct_root.join("resources/proj_a/nested/x.md"), b"x").unwrap();
        std::fs::write(acct_root.join("resources/proj_a/nested/deep/y.md"), b"y").unwrap();
        let _head_oid = make_commit(&svc, "acct", "main", "head").await;

        svc.restore(RestoreRequest {
            account: "acct".into(),
            branch: "main".into(),
            project_dir: "resources/proj_a".into(),
            source_commit: source_oid.to_hex().to_string(),
            dry_run: false,
            message: Some("rewind".into()),
            author_name: "tester".into(),
            author_email: "tester@example.com".into(),
        })
        .await
        .unwrap();

        // Files are gone, and so are the now-empty directories that held them
        // (deepest first: deep/, then nested/).
        assert!(
            !acct_root.join("resources/proj_a/nested/deep/y.md").exists(),
            "nested/deep/y.md must be deleted",
        );
        assert!(
            !acct_root.join("resources/proj_a/nested/x.md").exists(),
            "nested/x.md must be deleted",
        );
        assert!(
            !acct_root.join("resources/proj_a/nested/deep").exists(),
            "emptied directory nested/deep must be pruned",
        );
        assert!(
            !acct_root.join("resources/proj_a/nested").exists(),
            "emptied directory nested must be pruned",
        );
        // The surviving file and its (non-empty) parent are untouched.
        assert!(
            acct_root.join("resources/proj_a/keeper.md").exists(),
            "keeper.md must survive",
        );
        assert!(
            acct_root.join("resources/proj_a").is_dir(),
            "project_dir itself must remain (still holds keeper.md)",
        );
    }

    // Regression: a path the restore diff wants to delete may already be absent
    // from the VFS (e.g. a derived file like `.abstract.md` removed out of
    // band). The delete must be idempotent — restore should succeed and advance
    // the branch ref rather than aborting with a `vfs: not found` error.
    #[tokio::test]
    async fn test_restore_tolerates_already_deleted_path() {
        let dir = tempfile::tempdir().unwrap();
        let object_store = Arc::new(LocalObjectStore::new(dir.path()));
        let ref_store = Arc::new(LocalRefStore::new(dir.path()));
        let vfs = MockVfs::new_strict_remove("acct");
        let svc = GitService::new(
            vfs.clone() as Arc<dyn FileSystem>,
            object_store.clone() as Arc<dyn ObjectStore>,
            ref_store.clone() as Arc<dyn RefStore>,
        );

        // Source commit: a.md plus a derived file the diff will later delete.
        vfs.put("resources/proj_a/a.md", b"A v1");
        let source_oid = make_commit(&svc, "acct", "main", "source").await;

        // HEAD adds the derived file, so restoring source wants to delete it.
        vfs.put("resources/proj_a/.abstract.md", b"derived");
        vfs.put("resources/proj_a/a.md", b"A v2");
        let head_oid = match svc
            .commit(req(
                "acct",
                "main",
                "head",
                Some(vec![
                    "resources/proj_a/a.md".to_string(),
                    "resources/proj_a/.abstract.md".to_string(),
                ]),
            ))
            .await
            .unwrap()
        {
            CommitResponse::Created { commit_oid, .. } => commit_oid,
            other => panic!("expected Created, got {other:?}"),
        };

        // Simulate the derived file vanishing from the VFS out of band, so the
        // restore's delete step hits a missing path.
        vfs.delete("resources/proj_a/.abstract.md");

        let resp = svc
            .restore(RestoreRequest {
                account: "acct".into(),
                branch: "main".into(),
                project_dir: "resources/proj_a".into(),
                source_commit: source_oid.to_hex().to_string(),
                dry_run: false,
                message: Some("rewind".into()),
                author_name: "tester".into(),
                author_email: "tester@example.com".into(),
            })
            .await
            .expect("restore must tolerate an already-deleted path");

        let new_oid = match resp {
            RestoreResponse::Applied {
                deleted,
                deleted_paths,
                new_commit_oid,
                ..
            } => {
                // The diff still *plans* the delete (count is unchanged); the
                // VFS apply just no-ops on the missing path.
                assert_eq!(deleted, 1, ".abstract.md");
                assert_eq!(deleted_paths.len(), 1);
                new_commit_oid
            }
            other => panic!("expected Applied, got {other:?}"),
        };

        // Branch ref advanced to the new commit on top of HEAD.
        assert_eq!(
            ref_store.read("acct", "refs/heads/main").await.unwrap(),
            new_oid
        );
        let parents = commit_parents(
            object_store.as_ref() as &dyn ObjectStore,
            "acct",
            new_oid,
        )
        .await;
        assert_eq!(parents, vec![head_oid]);
    }

    #[tokio::test]
    async fn test_restore_noop_when_source_equals_head() {
        let (_dir, vfs, _object_store, ref_store, svc) = make_service("acct");
        vfs.put("resources/proj_a/a.md", b"only file");
        let only_oid = make_commit(&svc, "acct", "main", "only").await;

        // No further changes to proj_a — restoring from `only_oid` is a noop.
        let resp = svc
            .restore(RestoreRequest {
                account: "acct".into(),
                branch: "main".into(),
                project_dir: "resources/proj_a".into(),
                source_commit: only_oid.to_hex().to_string(),
                dry_run: false,
                message: None,
                author_name: "tester".into(),
                author_email: "tester@example.com".into(),
            })
            .await
            .unwrap();

        match resp {
            RestoreResponse::Noop { head, source } => {
                assert_eq!(head, only_oid);
                assert_eq!(source, only_oid);
            }
            other => panic!("expected Noop, got {other:?}"),
        }
        // Ref unchanged.
        assert_eq!(
            ref_store.read("acct", "refs/heads/main").await.unwrap(),
            only_oid
        );
    }

    #[tokio::test]
    async fn test_restore_invalid_project_dir() {
        let (_dir, _vfs, _object_store, _ref_store, svc) = make_service("acct");
        let err = svc
            .restore(RestoreRequest {
                account: "acct".into(),
                branch: "main".into(),
                project_dir: "".into(), // empty
                source_commit: "main".into(),
                dry_run: true,
                message: None,
                author_name: "x".into(),
                author_email: "x@x".into(),
            })
            .await
            .unwrap_err();
        assert!(matches!(err, GitError::InvalidProjectDir(_)));
    }

    #[tokio::test]
    async fn test_restore_unknown_source_ref() {
        let (_dir, vfs, _object_store, _ref_store, svc) = make_service("acct");
        vfs.put("resources/proj_a/a.md", b"x");
        let _ = make_commit(&svc, "acct", "main", "init").await;
        let err = svc
            .restore(RestoreRequest {
                account: "acct".into(),
                branch: "main".into(),
                project_dir: "resources/proj_a".into(),
                source_commit: "does-not-exist".into(),
                dry_run: true,
                message: None,
                author_name: "x".into(),
                author_email: "x@x".into(),
            })
            .await
            .unwrap_err();
        assert!(matches!(err, GitError::RefStore(RefStoreError::NotFound(_))));
    }

    #[tokio::test]
    async fn test_restore_unknown_branch_head() {
        let (_dir, vfs, _object_store, _ref_store, svc) = make_service("acct");
        vfs.put("resources/proj_a/a.md", b"x");
        let only = make_commit(&svc, "acct", "main", "only").await;
        let err = svc
            .restore(RestoreRequest {
                account: "acct".into(),
                branch: "ghost".into(), // doesn't exist
                project_dir: "resources/proj_a".into(),
                source_commit: only.to_hex().to_string(),
                dry_run: true,
                message: None,
                author_name: "x".into(),
                author_email: "x@x".into(),
            })
            .await
            .unwrap_err();
        assert!(matches!(err, GitError::RefStore(RefStoreError::NotFound(_))));
    }

    #[tokio::test]
    async fn test_restore_project_dir_missing_in_source_commit() {
        let (_dir, vfs, _object_store, _ref_store, svc) = make_service("acct");
        // Source commit only has resources/other_proj.
        vfs.put("resources/other_proj/x.md", b"x");
        let source_oid = make_commit(&svc, "acct", "main", "source").await;
        // HEAD has the project we will try to restore.
        vfs.put("resources/proj_a/a.md", b"a");
        let _ = make_commit(&svc, "acct", "main", "head").await;

        let err = svc
            .restore(RestoreRequest {
                account: "acct".into(),
                branch: "main".into(),
                project_dir: "resources/proj_a".into(),
                source_commit: source_oid.to_hex().to_string(),
                dry_run: true,
                message: None,
                author_name: "x".into(),
                author_email: "x@x".into(),
            })
            .await
            .unwrap_err();
        match err {
            GitError::SubtreeNotFoundInCommit { project_dir, commit } => {
                assert_eq!(project_dir, "resources/proj_a");
                assert_eq!(commit, source_oid);
            }
            other => panic!("expected SubtreeNotFoundInCommit, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_restore_cas_conflict_surfaces_as_error() {
        let dir = tempfile::tempdir().unwrap();
        let object_store = Arc::new(LocalObjectStore::new(dir.path()));
        let inner_ref = Arc::new(LocalRefStore::new(dir.path()));
        let vfs = MockVfs::new("acct");

        // Build a real first commit through a plain service so we have a HEAD.
        let bootstrap_svc = GitService::new(
            vfs.clone() as Arc<dyn FileSystem>,
            object_store.clone() as Arc<dyn ObjectStore>,
            inner_ref.clone() as Arc<dyn RefStore>,
        );
        vfs.put("resources/proj_a/a.md", b"v1");
        let source_oid = make_commit(&bootstrap_svc, "acct", "main", "source").await;
        vfs.put("resources/proj_a/a.md", b"v2");
        let head_oid = make_commit(&bootstrap_svc, "acct", "main", "head").await;

        // Now wrap the ref store to force the first cas_update to fail.
        let bogus =
            ObjectId::from_hex(b"deadbeefdeadbeefdeadbeefdeadbeefdeadbeef").unwrap();
        let conflict_ref = Arc::new(ConflictOnceRef {
            inner: inner_ref.clone(),
            fired: Mutex::new(false),
            actual: Some(bogus),
        });
        let svc = GitService::new(
            vfs.clone() as Arc<dyn FileSystem>,
            object_store.clone() as Arc<dyn ObjectStore>,
            conflict_ref as Arc<dyn RefStore>,
        );

        let err = svc
            .restore(RestoreRequest {
                account: "acct".into(),
                branch: "main".into(),
                project_dir: "resources/proj_a".into(),
                source_commit: source_oid.to_hex().to_string(),
                dry_run: false,
                message: None,
                author_name: "x".into(),
                author_email: "x@x".into(),
            })
            .await
            .unwrap_err();
        match err {
            GitError::ConcurrentCommit { ref_name, expected, actual } => {
                assert_eq!(ref_name, "refs/heads/main");
                assert_eq!(expected, Some(head_oid));
                assert_eq!(actual, Some(bogus));
            }
            other => panic!("expected ConcurrentCommit, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_restore_does_not_touch_paths_outside_project_dir() {
        let (_dir, vfs, object_store, _ref_store, svc) = make_service("acct");

        // Source: resources/proj_a + an UNRELATED file in another scope.
        vfs.put("resources/proj_a/a.md", b"A v1");
        vfs.put("agent/skills/unrelated.py", b"unrelated v1");
        let source_oid = make_commit(&svc, "acct", "main", "source").await;

        // HEAD: modify proj_a AND the unrelated file. Note we don't delete
        // anything in this test, so make_commit (which uses collect_all) is
        // fine — all files still exist in the VFS.
        vfs.put("resources/proj_a/a.md", b"A v2");
        vfs.put("agent/skills/unrelated.py", b"unrelated v2");
        vfs.put("agent/skills/new_skill.py", b"brand new");
        let _ = make_commit(&svc, "acct", "main", "head").await;

        let resp = svc
            .restore(RestoreRequest {
                account: "acct".into(),
                branch: "main".into(),
                project_dir: "resources/proj_a".into(),
                source_commit: source_oid.to_hex().to_string(),
                dry_run: false,
                message: None,
                author_name: "x".into(),
                author_email: "x@x".into(),
            })
            .await
            .unwrap();

        let new_oid = match resp {
            RestoreResponse::Applied { new_commit_oid, .. } => new_commit_oid,
            other => panic!("expected Applied, got {other:?}"),
        };

        // Verify the VFS: unrelated files keep their v2 / new state.
        let files = vfs.files.lock().unwrap();
        assert_eq!(
            files.get("/local/acct/agent/skills/unrelated.py").unwrap(),
            b"unrelated v2",
            "restore must NOT roll back unrelated.py",
        );
        assert!(
            files.contains_key("/local/acct/agent/skills/new_skill.py"),
            "restore must NOT delete new_skill.py",
        );
        // And proj_a/a.md DID roll back.
        assert_eq!(
            files.get("/local/acct/resources/proj_a/a.md").unwrap(),
            b"A v1",
        );
        drop(files);

        // Verify the tree: the new commit's tree should contain the v2 content
        // of unrelated.py and new_skill.py at their original oids. The easiest
        // way: lookup the oid of agent/skills/unrelated.py in both source and
        // new — they must DIFFER (source had v1, new still has v2).
        let new_tree = load_commit_meta(
            object_store.as_ref() as &dyn ObjectStore,
            "acct",
            &new_oid,
        )
        .await
        .unwrap()
        .tree;
        let source_tree = load_commit_meta(
            object_store.as_ref() as &dyn ObjectStore,
            "acct",
            &source_oid,
        )
        .await
        .unwrap()
        .tree;
        let unrelated_in_new = crate::git::tree_builder::lookup(
            object_store.as_ref() as &dyn ObjectStore,
            "acct",
            new_tree,
            "agent/skills/unrelated.py",
        )
        .await
        .unwrap()
        .unwrap();
        let unrelated_in_source = crate::git::tree_builder::lookup(
            object_store.as_ref() as &dyn ObjectStore,
            "acct",
            source_tree,
            "agent/skills/unrelated.py",
        )
        .await
        .unwrap()
        .unwrap();
        assert_ne!(
            unrelated_in_new.0, unrelated_in_source.0,
            "agent/skills/unrelated.py in the new tree must be HEAD's v2 oid, not source's v1 oid",
        );
        assert!(
            crate::git::tree_builder::lookup(
                object_store.as_ref() as &dyn ObjectStore,
                "acct",
                new_tree,
                "agent/skills/new_skill.py",
            )
            .await
            .unwrap()
            .is_some(),
            "new_skill.py must still be present in the new tree",
        );
    }

    #[tokio::test]
    async fn test_restore_then_show_reflects_old_content() {
        let (_dir, vfs, _object_store, _ref_store, svc) = make_service("acct");

        vfs.put("resources/proj_a/note.md", b"original");
        let src = make_commit(&svc, "acct", "main", "src").await;

        vfs.put("resources/proj_a/note.md", b"edited");
        let _ = make_commit(&svc, "acct", "main", "edit").await;

        // Sanity: show on HEAD shows "edited".
        let head_show = svc
            .show(ShowRequest {
                account: "acct".into(),
                target_ref: "main".into(),
                path: Some("resources/proj_a/note.md".into()),
            })
            .await
            .unwrap();
        match head_show {
            ShowResponse::Blob { bytes, .. } => assert_eq!(bytes.as_ref(), b"edited"),
            other => panic!("expected Blob, got {other:?}"),
        }

        // Restore.
        let new_oid = match svc
            .restore(RestoreRequest {
                account: "acct".into(),
                branch: "main".into(),
                project_dir: "resources/proj_a".into(),
                source_commit: src.to_hex().to_string(),
                dry_run: false,
                message: Some("rewind".into()),
                author_name: "x".into(),
                author_email: "x@x".into(),
            })
            .await
            .unwrap()
        {
            RestoreResponse::Applied { new_commit_oid, .. } => new_commit_oid,
            other => panic!("expected Applied, got {other:?}"),
        };

        // After restore: show on main should reflect the original content.
        let after_show = svc
            .show(ShowRequest {
                account: "acct".into(),
                target_ref: "main".into(),
                path: Some("resources/proj_a/note.md".into()),
            })
            .await
            .unwrap();
        match after_show {
            ShowResponse::Blob { bytes, .. } => assert_eq!(bytes.as_ref(), b"original"),
            other => panic!("expected Blob, got {other:?}"),
        }

        // And show on the new oid by hex resolves to the same content.
        let by_oid = svc
            .show(ShowRequest {
                account: "acct".into(),
                target_ref: new_oid.to_hex().to_string(),
                path: Some("resources/proj_a/note.md".into()),
            })
            .await
            .unwrap();
        match by_oid {
            ShowResponse::Blob { bytes, .. } => assert_eq!(bytes.as_ref(), b"original"),
            other => panic!("expected Blob, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod diff_tests {
    use super::*;
    use crate::git::types::RestoreDiff;
    use gix_hash::ObjectId;

    fn oid(byte: u8) -> ObjectId {
        let mut bytes = [0u8; 20];
        bytes.fill(byte);
        ObjectId::from_bytes_or_panic(&bytes)
    }

    #[test]
    fn diff_empty_both() {
        let got = compute_subtree_diff(&[], &[]);
        assert_eq!(got, RestoreDiff { to_write: vec![], to_delete: vec![], unchanged: vec![] });
    }

    #[test]
    fn diff_all_writes_when_head_empty() {
        let source = vec![("a.md".to_string(), oid(0xAA))];
        let got = compute_subtree_diff(&source, &[]);
        assert_eq!(got.to_write, vec![("a.md".to_string(), oid(0xAA))]);
        assert!(got.to_delete.is_empty());
        assert!(got.unchanged.is_empty());
    }

    #[test]
    fn diff_all_deletes_when_source_empty() {
        let head = vec![("b.md".to_string(), oid(0xBB))];
        let got = compute_subtree_diff(&[], &head);
        assert!(got.to_write.is_empty());
        assert_eq!(got.to_delete, vec!["b.md".to_string()]);
        assert!(got.unchanged.is_empty());
    }

    #[test]
    fn diff_unchanged_same_oid_same_path() {
        let entries = vec![("a.md".to_string(), oid(0xCC))];
        let got = compute_subtree_diff(&entries, &entries);
        assert!(got.to_write.is_empty());
        assert!(got.to_delete.is_empty());
        assert_eq!(got.unchanged, vec!["a.md".to_string()]);
    }

    #[test]
    fn diff_overwrite_when_same_path_different_oid() {
        let source = vec![("a.md".to_string(), oid(0xAA))];
        let head   = vec![("a.md".to_string(), oid(0xBB))];
        let got = compute_subtree_diff(&source, &head);
        assert_eq!(got.to_write, vec![("a.md".to_string(), oid(0xAA))]);
        assert!(got.to_delete.is_empty());
        assert!(got.unchanged.is_empty());
    }

    #[test]
    fn diff_mixed_buckets_sorted_deterministically() {
        let source = vec![
            ("keep.md".to_string(), oid(0x11)),
            ("change.md".to_string(), oid(0x22)),
            ("new.md".to_string(), oid(0x33)),
        ];
        let head = vec![
            ("keep.md".to_string(), oid(0x11)),
            ("change.md".to_string(), oid(0x99)),
            ("gone.md".to_string(), oid(0x44)),
        ];
        let got = compute_subtree_diff(&source, &head);
        assert_eq!(
            got.to_write,
            vec![
                ("change.md".to_string(), oid(0x22)),
                ("new.md".to_string(), oid(0x33)),
            ]
        );
        assert_eq!(got.to_delete, vec!["gone.md".to_string()]);
        assert_eq!(got.unchanged, vec!["keep.md".to_string()]);
    }

    #[test]
    fn diff_handles_nested_paths() {
        let source = vec![
            ("docs/a.md".to_string(), oid(0xAA)),
            ("docs/sub/b.md".to_string(), oid(0xBB)),
        ];
        let head = vec![("docs/a.md".to_string(), oid(0xAA))];
        let got = compute_subtree_diff(&source, &head);
        assert_eq!(
            got.to_write,
            vec![("docs/sub/b.md".to_string(), oid(0xBB))]
        );
        assert!(got.to_delete.is_empty());
        assert_eq!(got.unchanged, vec!["docs/a.md".to_string()]);
    }

    #[test]
    fn validate_rejects_empty_string() {
        let err = validate_project_dir("").unwrap_err();
        assert!(matches!(err, GitError::InvalidProjectDir(_)));
    }

    #[test]
    fn validate_rejects_leading_slash() {
        assert!(matches!(
            validate_project_dir("/resources/proj_a").unwrap_err(),
            GitError::InvalidProjectDir(_)
        ));
    }

    #[test]
    fn validate_rejects_trailing_slash() {
        assert!(matches!(
            validate_project_dir("resources/proj_a/").unwrap_err(),
            GitError::InvalidProjectDir(_)
        ));
    }

    #[test]
    fn validate_rejects_double_slash() {
        assert!(matches!(
            validate_project_dir("resources//proj_a").unwrap_err(),
            GitError::InvalidProjectDir(_)
        ));
    }

    #[test]
    fn validate_accepts_simple_path() {
        validate_project_dir("resources/proj_a").unwrap();
    }

    #[test]
    fn validate_accepts_single_segment() {
        validate_project_dir("resources").unwrap();
    }
}

