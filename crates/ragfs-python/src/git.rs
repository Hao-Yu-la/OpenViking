//! PyO3 binding helpers for the Git version-control service.
//!
//! This module owns:
//! - Backend construction (`build_git_service`) from a `GitConfig`
//! - Request parsers: `parse_commit_request`, `parse_restore_request`, `parse_show_request`
//!   (added in later tasks)
//! - Response converters: `commit_response_to_pydict`, `restore_response_to_pydict`,
//!   `show_response_to_pydict` (added in later tasks)
//! - Error mapping `map_git_error` (added in later tasks)
//!
//! The free functions are invoked from thin `#[pymethods]` wrappers in `lib.rs`.

use std::sync::Arc;

use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;

use ragfs::core::FileSystem;
use ragfs::git::{
    GitConfig, GitService, LocalObjectStore, LocalRefStore, ObjectStore, RefStore,
};

#[cfg(feature = "s3")]
use ragfs::git::{CasMode, S3Config, S3ObjectStore, S3RefStore};

/// Build a `GitService` from a `GitConfig` and the binding's MountableFS.
///
/// Returns `Ok(None)` when `enabled = false`; `Err(PyErr)` if the config is
/// invalid (missing required section, unknown backend, etc.).
///
/// Backend-specific notes:
/// - `local`: requires `[git.local]` with `base_dir`. Builds `LocalObjectStore`
///   and `LocalRefStore`, both rooted at `base_dir`.
/// - `s3` (feature-gated): requires `[git.s3]` with `bucket`, `region`.
///   `access_key_env` and `secret_key_env` are env-var names; their resolved
///   values are passed to `S3Config`. If the env vars are set in config but
///   missing in the process env, this returns a `PyValueError`.
pub fn build_git_service(
    cfg: &GitConfig,
    vfs: Arc<dyn FileSystem>,
) -> PyResult<Option<Arc<GitService>>> {
    if !cfg.enabled {
        return Ok(None);
    }

    let (object_store, ref_store): (Arc<dyn ObjectStore>, Arc<dyn RefStore>) =
        match cfg.backend.as_str() {
            "local" => {
                let lc = cfg
                    .local
                    .as_ref()
                    .ok_or_else(|| PyValueError::new_err("[git.local] missing"))?;
                let os = Arc::new(LocalObjectStore::new(lc.base_dir.clone()));
                let rs = Arc::new(LocalRefStore::new(lc.base_dir.clone()));
                (os, rs)
            }
            #[cfg(feature = "s3")]
            "s3" => build_s3_service(cfg)?,
            #[cfg(not(feature = "s3"))]
            "s3" => {
                return Err(PyRuntimeError::new_err(
                    "git backend 's3' requested but ragfs-python built without `s3` feature",
                ));
            }
            other => {
                return Err(PyValueError::new_err(format!(
                    "unsupported git backend: {}",
                    other
                )));
            }
        };

    Ok(Some(Arc::new(GitService::new(vfs, object_store, ref_store))))
}

#[cfg(feature = "s3")]
fn build_s3_service(
    cfg: &GitConfig,
) -> PyResult<(Arc<dyn ObjectStore>, Arc<dyn RefStore>)> {
    let sc = cfg
        .s3
        .as_ref()
        .ok_or_else(|| PyValueError::new_err("[git.s3] missing"))?;

    let access_key_id = match sc.access_key_env.as_deref() {
        Some(name) if !name.is_empty() => Some(std::env::var(name).map_err(|_| {
            PyValueError::new_err(format!(
                "access_key_env '{}' not set in process environment",
                name
            ))
        })?),
        _ => None,
    };
    let secret_access_key = match sc.secret_key_env.as_deref() {
        Some(name) if !name.is_empty() => Some(std::env::var(name).map_err(|_| {
            PyValueError::new_err(format!(
                "secret_key_env '{}' not set in process environment",
                name
            ))
        })?),
        _ => None,
    };

    let cas_mode = match sc.cas_mode.as_str() {
        "native" => CasMode::Native,
        "redis_lock" => CasMode::RedisLock,
        other => {
            return Err(PyValueError::new_err(format!(
                "unsupported cas_mode: {}",
                other
            )));
        }
    };

    let s3_config = S3Config {
        bucket: sc.bucket.clone(),
        prefix: sc.prefix.clone(),
        region: sc.region.clone(),
        endpoint: if sc.endpoint.is_empty() {
            None
        } else {
            Some(sc.endpoint.clone())
        },
        access_key_id,
        secret_access_key,
        use_path_style: sc.use_path_style,
        cas_mode,
    };

    let rt = tokio::runtime::Handle::try_current().map_err(|_| {
        PyRuntimeError::new_err("build_s3_service must run inside a tokio runtime")
    })?;
    let os_cfg = s3_config.clone();
    let object_store = Arc::new(
        rt.block_on(async move { S3ObjectStore::from_config(os_cfg).await })
            .map_err(|e| PyRuntimeError::new_err(format!("S3ObjectStore: {}", e)))?,
    ) as Arc<dyn ObjectStore>;

    let rs_cfg = s3_config;
    let ref_store = Arc::new(
        rt.block_on(async move { S3RefStore::from_config(rs_cfg).await })
            .map_err(|e| PyRuntimeError::new_err(format!("S3RefStore: {}", e)))?,
    ) as Arc<dyn RefStore>;

    Ok((object_store, ref_store))
}

/// Map a `GitError` to the appropriate Python exception.
///
/// Loads exception classes from the `openviking.pyagfs` module. When the
/// module is not importable (e.g. during unit tests), falls back to
/// `PyRuntimeError` with the same message.
pub fn map_git_error(py: Python<'_>, e: ragfs::git::GitError) -> PyErr {
    use ragfs::git::GitError;
    let msg = e.to_string();
    match e {
        GitError::FeatureDisabled => new_py_err(py, "AGFSNotSupportedError", msg),
        GitError::ConcurrentCommit { .. } => new_py_err(py, "GitConcurrentCommitError", msg),
        GitError::PathNotFound(_) => new_py_err(py, "AGFSNotFoundError", msg),
        GitError::PathIsDirectory(_) => new_py_err(py, "AGFSInvalidOperationError", msg),
        GitError::SubtreeNotFoundInCommit { .. } => new_py_err(py, "AGFSNotFoundError", msg),
        GitError::InvalidAccountId(_) => new_py_err(py, "AGFSInvalidPathError", msg),
        GitError::InvalidProjectDir(_) => new_py_err(py, "AGFSInvalidPathError", msg),
        GitError::BlobTooLarge { .. } => new_py_err(py, "AGFSInvalidOperationError", msg),
        GitError::TooManyFiles { .. } => new_py_err(py, "AGFSInvalidOperationError", msg),
        GitError::CorruptedObject(_) => new_py_err(py, "AGFSInternalError", msg),
        GitError::ObjectStore(_) | GitError::RefStore(_) | GitError::Vfs(_) | GitError::Other(_) => {
            PyRuntimeError::new_err(msg)
        }
    }
}

/// Local copy of the new_py_err pattern used in lib.rs. We duplicate it here
/// to keep git.rs self-contained — lib.rs's helper is private. If lib.rs's
/// helper is later made `pub(crate)`, this can be deleted in favor of that.
fn new_py_err(py: Python<'_>, name: &str, msg: String) -> PyErr {
    let exc = PyModule::import(py, "openviking.pyagfs")
        .and_then(|m| m.getattr(name))
        .and_then(|exc| Ok(exc.cast_into::<pyo3::types::PyType>()?));
    match exc {
        Ok(exc) => PyErr::from_type(exc, msg),
        Err(_) => PyRuntimeError::new_err(msg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use ragfs::core::MountableFS;
    use ragfs::git::GitError;

    fn local_cfg(base_dir: &str) -> ragfs::git::GitConfig {
        ragfs::git::GitConfig {
            enabled: true,
            backend: "local".into(),
            default_branch: "main".into(),
            author_name: "test".into(),
            author_email: "t@e".into(),
            local: Some(ragfs::git::GitLocalConfig {
                base_dir: base_dir.into(),
                fsync: "data".into(),
            }),
            s3: None,
            tuning: Default::default(),
        }
    }

    #[tokio::test]
    async fn build_git_service_disabled_returns_none() {
        let fs = Arc::new(MountableFS::new()) as Arc<dyn ragfs::core::FileSystem>;
        let mut cfg = local_cfg("/tmp/ov-git-test-disabled");
        cfg.enabled = false;
        let svc = build_git_service(&cfg, fs).expect("build ok");
        assert!(svc.is_none());
    }

    #[tokio::test]
    async fn build_git_service_local_returns_some() {
        let fs = Arc::new(MountableFS::new()) as Arc<dyn ragfs::core::FileSystem>;
        let cfg = local_cfg("/tmp/ov-git-test-local");
        let svc = build_git_service(&cfg, fs).expect("build ok");
        assert!(svc.is_some());
    }

    #[tokio::test]
    async fn build_git_service_unknown_backend_errors() {
        // Building a PyErr requires the Python interpreter to be initialized;
        // the `extension-module` feature disables auto-initialize.
        Python::initialize();
        let fs = Arc::new(MountableFS::new()) as Arc<dyn ragfs::core::FileSystem>;
        let mut cfg = local_cfg("/tmp/ov-git-test-bad");
        cfg.backend = "bogus".into();
        // `GitService` is not `Debug`, so we can't use `unwrap_err()`; match instead.
        let err = match build_git_service(&cfg, fs) {
            Ok(_) => panic!("expected error for bogus backend"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("unsupported git backend"));
    }

    #[tokio::test]
    async fn build_git_service_local_without_section_errors() {
        Python::initialize();
        let fs = Arc::new(MountableFS::new()) as Arc<dyn ragfs::core::FileSystem>;
        let mut cfg = local_cfg("/tmp/ov-git-test-nolocal");
        cfg.local = None;
        let err = match build_git_service(&cfg, fs) {
            Ok(_) => panic!("expected error when [git.local] missing"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("[git.local] missing"));
    }

    #[test]
    fn map_git_error_feature_disabled() {
        pyo3::prepare_freethreaded_python();
        Python::attach(|py| {
            let err = map_git_error(py, GitError::FeatureDisabled);
            // We don't require the openviking.pyagfs module to be importable
            // in this Rust-only test, so the fallback PyRuntimeError is fine.
            // We just assert that mapping does not panic and yields a PyErr.
            assert!(err.to_string().to_lowercase().contains("git"));
        });
    }

    #[test]
    fn map_git_error_concurrent_commit() {
        pyo3::prepare_freethreaded_python();
        Python::attach(|py| {
            let err = map_git_error(
                py,
                GitError::ConcurrentCommit {
                    ref_name: "refs/heads/main".into(),
                    expected: None,
                    actual: None,
                },
            );
            assert!(err.to_string().to_lowercase().contains("concurrent"));
        });
    }

    #[test]
    fn map_git_error_path_not_found() {
        pyo3::prepare_freethreaded_python();
        Python::attach(|py| {
            let err = map_git_error(py, GitError::PathNotFound("foo/bar".into()));
            assert!(err.to_string().contains("foo/bar"));
        });
    }

    #[test]
    fn map_git_error_invalid_account() {
        pyo3::prepare_freethreaded_python();
        Python::attach(|py| {
            let err = map_git_error(py, GitError::InvalidAccountId("../bad".into()));
            assert!(err.to_string().contains("bad"));
        });
    }

    #[test]
    fn map_git_error_blob_too_large() {
        pyo3::prepare_freethreaded_python();
        Python::attach(|py| {
            let err = map_git_error(
                py,
                GitError::BlobTooLarge { size: 200, limit: 100 },
            );
            assert!(err.to_string().contains("200"));
        });
    }
}
