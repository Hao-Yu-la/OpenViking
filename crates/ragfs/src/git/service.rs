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
    error::{GitError, RefStoreError},
    object_store::ObjectStore,
    ref_store::RefStore,
    types::{CommitRequest, CommitResponse, ShowRequest, ShowResponse},
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

        let commit_oid = resolve_ref(self.ref_store.as_ref(), &account, &target_ref).await?;
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
}

/// Resolve `target_ref` to a commit OID.
///
/// Accepts:
///   1. 40-hex commit OID (validated by `ObjectId::from_hex`)
///   2. Full ref path beginning with `refs/` (passed through `validate_ref_name`,
///      then read from `ref_store`)
///   3. Short branch name (e.g. "main") — auto-prefixed to `refs/heads/{name}`,
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
    account: &str,
    target_ref: &str,
) -> Result<ObjectId, GitError> {
    // 1. 40-hex commit OID — ASCII hex (case-insensitive), exactly len 40.
    if target_ref.len() == 40 && target_ref.bytes().all(|b| b.is_ascii_hexdigit()) {
        return ObjectId::from_hex(target_ref.as_bytes())
            .map_err(|e| GitError::Other(format!("invalid oid {target_ref}: {e}")));
    }

    // 2 & 3. Normalize to full ref path then read.
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
    }

    impl MockVfs {
        fn new(account: &str) -> Arc<Self> {
            Arc::new(Self {
                account: account.to_string(),
                files: Arc::new(Mutex::new(HashMap::new())),
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
        async fn remove(&self, _path: &str) -> Result<()> {
            unimplemented!()
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
            _path: &str,
            _data: &[u8],
            _offset: u64,
            _flags: WriteFlag,
        ) -> Result<u64> {
            unimplemented!()
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
}
