//! Git version control module
//!
//! This module provides Git-based version control capabilities for OpenViking,
//! allowing users to commit snapshots, checkout previous versions, and view
//! history.
//!
//! Architecture
//!
//! - [`object_store`]: Trait and implementations for content-addressable storage
//! - [`ref_store`]: Trait and implementations for named reference storage
//! - [`backends`]: Backend implementations (local filesystem, S3)
//!
//! Example
//!
//! ```rust,ignore
//! use ragfs::git::backends::local::{LocalObjectStore, LocalRefStore};
//! use ragfs::git::object_store::ObjectStore;
//! use ragfs::git::ref_store::RefStore;
//!
//! # #[tokio::main]
//! # async fn main() {
//! let object_store = LocalObjectStore::new("/data/git");
//! let ref_store = LocalRefStore::new("/data/git");
//!
//! // Use object_store and ref_store...
//! # }
//! ```

pub mod backends;
pub mod commit;
pub mod enumerate;
pub mod error;
pub mod object_store;
pub mod ref_store;
pub mod service;
pub mod tree_builder;
pub mod types;
pub mod util;

pub use error::{GitError, ObjectStoreError, RefStoreError};
pub use object_store::ObjectStore;
pub use ref_store::RefStore;
pub use service::GitService;
pub use tree_builder::{flatten, lookup, TreeEditor};
pub use types::{Actor, CommitRequest, CommitResponse, IndexEntry, ShowRequest, ShowResponse};

// Re-exports from backends
pub use backends::local::{LocalObjectStore, LocalRefStore};

#[cfg(feature = "s3")]
pub use backends::s3::{CasMode, S3Config, S3ObjectStore, S3RefStore};
