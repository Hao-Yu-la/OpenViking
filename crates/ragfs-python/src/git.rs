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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use ragfs::core::MountableFS;

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
}
