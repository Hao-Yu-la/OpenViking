//! Account-level `.ovgitignore` parsing and matching.
//!
//! The syntax is a documented OpenViking subset of root `.gitignore` rules.
//! It intentionally rejects negation so callers do not assume full Git index
//! semantics.

use std::path::Path;

use ignore::gitignore::{Gitignore, GitignoreBuilder};

use crate::git::error::GitError;

pub const OVGITIGNORE_PATH: &str = ".ovgitignore";
pub const OVGITIGNORE_MAX_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct IgnoreMatcher {
    inner: Option<Gitignore>,
}

impl Default for IgnoreMatcher {
    fn default() -> Self {
        Self::empty()
    }
}

impl IgnoreMatcher {
    pub fn empty() -> Self {
        Self { inner: None }
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, GitError> {
        if bytes.len() > OVGITIGNORE_MAX_BYTES {
            return Err(GitError::IgnoreFileTooLarge {
                path: OVGITIGNORE_PATH.to_string(),
                size: bytes.len() as u64,
                max: OVGITIGNORE_MAX_BYTES as u64,
            });
        }

        let text = std::str::from_utf8(bytes).map_err(|e| GitError::InvalidIgnoreFile {
            path: OVGITIGNORE_PATH.to_string(),
            reason: format!("must be UTF-8: {e}"),
        })?;

        let mut builder = GitignoreBuilder::new(Path::new(""));
        let mut added = false;
        for (idx, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line.starts_with('!') {
                return Err(GitError::InvalidIgnoreFile {
                    path: OVGITIGNORE_PATH.to_string(),
                    reason: format!(
                        "line {} uses unsupported negation: {}",
                        idx + 1,
                        line
                    ),
                });
            }
            builder.add_line(Some(OVGITIGNORE_PATH.into()), line).map_err(|e| {
                GitError::InvalidIgnoreFile {
                    path: OVGITIGNORE_PATH.to_string(),
                    reason: format!("line {} is invalid: {e}", idx + 1),
                }
            })?;
            added = true;
        }

        if !added {
            return Ok(Self::empty());
        }

        let inner = builder.build().map_err(|e| GitError::InvalidIgnoreFile {
            path: OVGITIGNORE_PATH.to_string(),
            reason: e.to_string(),
        })?;
        Ok(Self { inner: Some(inner) })
    }

    pub fn is_ignored(&self, rel_path: &str) -> bool {
        let Some(inner) = &self.inner else {
            return false;
        };
        let cleaned = rel_path.trim_matches('/');
        if cleaned.is_empty() || cleaned == OVGITIGNORE_PATH {
            return false;
        }
        inner
            .matched_path_or_any_parents(Path::new(cleaned), false)
            .is_ignore()
    }
}

pub fn should_track_path(rel_path: &str, matcher: &IgnoreMatcher) -> bool {
    if rel_path == OVGITIGNORE_PATH {
        return true;
    }
    !crate::git::enumerate::prune_path(rel_path) && !matcher.is_ignored(rel_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matcher(src: &str) -> IgnoreMatcher {
        IgnoreMatcher::parse(src.as_bytes()).expect("ignore file parses")
    }

    #[test]
    fn empty_comments_and_blank_lines_match_nothing() {
        let m = matcher("\n  \n# comment\n   # indented comment\n");
        assert!(!m.is_ignored("resources/a.log"));
        assert!(should_track_path("resources/a.log", &m));
    }

    #[test]
    fn basename_glob_matches_at_any_depth() {
        let m = matcher("*.log\n");
        assert!(m.is_ignored("resources/a.log"));
        assert!(m.is_ignored("resources/proj/nested/a.log"));
        assert!(!m.is_ignored("resources/a.md"));
    }

    #[test]
    fn double_star_glob_matches_nested_paths() {
        let m = matcher("**/*.bak\n");
        assert!(m.is_ignored("a.bak"));
        assert!(m.is_ignored("resources/proj/a.bak"));
        assert!(!m.is_ignored("resources/proj/a.md"));
    }

    #[test]
    fn root_relative_patterns_match_from_account_root() {
        let m = matcher("resources/tmp/**\n/resources/cache/**\n");
        assert!(m.is_ignored("resources/tmp/a.txt"));
        assert!(m.is_ignored("resources/tmp/nested/a.txt"));
        assert!(m.is_ignored("resources/cache/a.txt"));
        assert!(!m.is_ignored("user/default/resources/tmp/a.txt"));
    }

    #[test]
    fn directory_patterns_match_directory_contents() {
        let m = matcher("tmp/\n/cache/\n");
        assert!(m.is_ignored("resources/tmp/a.txt"));
        assert!(m.is_ignored("tmp/a.txt"));
        assert!(m.is_ignored("cache/a.txt"));
        assert!(!m.is_ignored("resources/cache/a.txt"));
    }

    #[test]
    fn ovgitignore_is_always_tracked() {
        let m = matcher("*\n.ovgitignore\n");
        assert!(m.is_ignored("resources/a.md"));
        assert!(should_track_path(OVGITIGNORE_PATH, &m));
    }

    #[test]
    fn system_prune_still_wins() {
        let m = IgnoreMatcher::empty();
        assert!(!should_track_path("_system/state.json", &m));
        assert!(!should_track_path("resources/index.faiss", &m));
        assert!(!should_track_path("resources/embedding_cache/a.bin", &m));
    }

    #[test]
    fn negation_is_rejected() {
        let err = IgnoreMatcher::parse(b"!keep.log\n").unwrap_err();
        assert!(matches!(err, GitError::InvalidIgnoreFile { .. }));
        assert!(err.to_string().contains("negation"));
    }

    #[test]
    fn non_utf8_is_rejected() {
        let err = IgnoreMatcher::parse(&[0xff, 0xfe]).unwrap_err();
        assert!(matches!(err, GitError::InvalidIgnoreFile { .. }));
        assert!(err.to_string().contains("UTF-8"));
    }

    #[test]
    fn oversized_file_is_rejected() {
        let bytes = vec![b'a'; OVGITIGNORE_MAX_BYTES + 1];
        let err = IgnoreMatcher::parse(&bytes).unwrap_err();
        assert!(matches!(err, GitError::IgnoreFileTooLarge { .. }));
    }
}
