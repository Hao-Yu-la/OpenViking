//! DTOs for the Git service API.

use gix_hash::ObjectId;

#[derive(Debug, Clone)]
pub struct CommitRequest {
    pub account: String,
    pub branch: String,                 // e.g. "main" — NOT the full "refs/heads/main"
    pub message: String,
    /// Explicit candidate paths (account-relative, e.g. "resources/a.md").
    /// `None` means "enumerate the whole account tree".
    pub paths: Option<Vec<String>>,
    pub author_name: String,
    pub author_email: String,
}

#[derive(Debug, Clone)]
pub enum CommitResponse {
    Created { commit_oid: ObjectId, changed: usize },
    /// No path produced an editor change; ref untouched. `commit_oid` is the
    /// existing HEAD (or `ObjectId::null` if the branch did not exist).
    Noop { commit_oid: ObjectId },
}

/// Per-path stat cache entry. Not persisted yet (Fast Path 1 is deferred),
/// but the type lives here so later work can fill in the index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    pub size: u64,
    pub mtime_ns: i128,
    pub oid: ObjectId,
}