//! Async-native Git tree editor for building and modifying tree objects.

use std::collections::{BTreeMap, HashMap};

use gix_hash::ObjectId;
use gix_object::bstr::{BString, ByteSlice};
use gix_object::tree::{self, EntryKind};

use crate::git::error::GitError;

/// Type alias for tree entries mapping path components to tree entries
pub type TreeEntries = BTreeMap<BString, tree::Entry>;

/// Editor for constructing and modifying Git tree objects
pub struct TreeEditor {
    pub(crate) root: TreeEntries,
    pub(crate) subtrees: HashMap<BString, TreeEntries>,
}

impl TreeEditor {
    /// Create a new empty TreeEditor
    pub fn empty() -> Self {
        Self {
            root: BTreeMap::new(),
            subtrees: HashMap::new(),
        }
    }

    /// Split a path into components, validating each component.
    fn split_path(path: &str) -> Result<Vec<&str>, GitError> {
        if path.is_empty() {
            return Err(GitError::Other("empty path".into()));
        }

        let components: Vec<&str> = path.split('/').collect();
        for comp in &components {
            if comp.is_empty() {
                return Err(GitError::Other("empty path component".into()));
            }
        }
        Ok(components)
    }

    /// Join path components into a `dir1/dir2/...` BString key.
    fn join_prefix(parts: &[&str]) -> BString {
        let mut out = BString::default();
        for (i, p) in parts.iter().enumerate() {
            if i > 0 {
                out.push(b'/');
            }
            out.extend_from_slice(p.as_bytes());
        }
        out
    }

    /// Upsert a blob object at the given path.
    pub fn upsert(&mut self, path: &str, oid: ObjectId) -> Result<(), GitError> {
        let components = Self::split_path(path)?;
        let (filename, parent_dirs) = components
            .split_last()
            .ok_or_else(|| GitError::Other("empty path".into()))?;

        let leaf = tree::Entry {
            mode: EntryKind::Blob.into(),
            filename: (*filename).into(),
            oid,
        };

        if parent_dirs.is_empty() {
            self.root.insert((*filename).into(), leaf);
            return Ok(());
        }

        // Ensure every ancestor directory has a tree entry in its parent.
        // The OID of intermediate tree entries is computed later during write().
        for depth in 1..=parent_dirs.len() {
            let dir_name = parent_dirs[depth - 1];
            let parent_entries: &mut TreeEntries = if depth == 1 {
                &mut self.root
            } else {
                let parent_key = Self::join_prefix(&parent_dirs[..depth - 1]);
                self.subtrees.entry(parent_key).or_insert_with(BTreeMap::new)
            };

            match parent_entries.get(dir_name.as_bytes().as_bstr()) {
                Some(entry) if entry.mode != EntryKind::Tree.into() => {
                    return Err(GitError::Other(format!(
                        "path component '{dir_name}' is not a tree"
                    )));
                }
                Some(_) => {}
                None => {
                    parent_entries.insert(
                        dir_name.into(),
                        tree::Entry {
                            mode: EntryKind::Tree.into(),
                            filename: dir_name.into(),
                            oid: ObjectId::null(gix_hash::Kind::Sha1),
                        },
                    );
                }
            }
        }

        let leaf_key = Self::join_prefix(parent_dirs);
        let subtree = self.subtrees.entry(leaf_key).or_insert_with(BTreeMap::new);
        subtree.insert((*filename).into(), leaf);

        Ok(())
    }

    /// Remove a path from the tree. No-op if the path does not exist.
    pub fn remove(&mut self, path: &str) -> Result<(), GitError> {
        let components = Self::split_path(path)?;
        let (filename, parent_dirs) = components
            .split_last()
            .ok_or_else(|| GitError::Other("empty path".into()))?;

        if parent_dirs.is_empty() {
            self.root.remove(filename.as_bytes().as_bstr());
            return Ok(());
        }

        let prefix = Self::join_prefix(parent_dirs);
        if let Some(subtree) = self.subtrees.get_mut(&prefix) {
            subtree.remove(filename.as_bytes().as_bstr());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_oid() -> ObjectId {
        ObjectId::null(gix_hash::Kind::Sha1)
    }

    #[test]
    fn test_empty_editor() {
        let editor = TreeEditor::empty();
        assert!(editor.root.is_empty());
        assert!(editor.subtrees.is_empty());
    }

    #[test]
    fn test_upsert_single_file() {
        let mut editor = TreeEditor::empty();
        let oid = dummy_oid();

        editor.upsert("file.txt", oid).unwrap();

        assert_eq!(editor.root.len(), 1);
        let entry = editor.root.get("file.txt".as_bytes().as_bstr()).unwrap();
        assert_eq!(entry.mode, EntryKind::Blob.into());
        assert_eq!(entry.oid, oid);
        assert_eq!(entry.filename, "file.txt");
    }

    #[test]
    fn test_upsert_nested_path() {
        let mut editor = TreeEditor::empty();
        let oid = dummy_oid();

        editor.upsert("dir/subdir/file.txt", oid).unwrap();

        // Root has dir
        assert_eq!(editor.root.len(), 1);
        let dir_entry = editor.root.get("dir".as_bytes().as_bstr()).unwrap();
        assert_eq!(dir_entry.mode, EntryKind::Tree.into());

        // Subtrees has dir
        let dir_subtree = editor.subtrees.get("dir".as_bytes().as_bstr()).unwrap();
        assert_eq!(dir_subtree.len(), 1);
        let subdir_entry = dir_subtree.get("subdir".as_bytes().as_bstr()).unwrap();
        assert_eq!(subdir_entry.mode, EntryKind::Tree.into());

        // Subdir subtree
        let subdir_subtree = editor.subtrees.get("dir/subdir".as_bytes().as_bstr()).unwrap();
        assert_eq!(subdir_subtree.len(), 1);
        let file_entry = subdir_subtree.get("file.txt".as_bytes().as_bstr()).unwrap();
        assert_eq!(file_entry.mode, EntryKind::Blob.into());
        assert_eq!(file_entry.oid, oid);
    }

    #[test]
    fn test_upsert_overwrite() {
        let mut editor = TreeEditor::empty();
        let oid1 = dummy_oid();
        let oid2 = ObjectId::from_hex(b"abcdef1234567890abcdef1234567890abcdef12").unwrap();

        editor.upsert("file.txt", oid1).unwrap();
        editor.upsert("file.txt", oid2).unwrap();

        let entry = editor.root.get("file.txt".as_bytes().as_bstr()).unwrap();
        assert_eq!(entry.oid, oid2);
    }

    #[test]
    fn test_upsert_empty_component_rejected() {
        let mut editor = TreeEditor::empty();
        let oid = dummy_oid();

        assert!(editor.upsert("", oid).is_err());
        assert!(editor.upsert("file//txt", oid).is_err());
        assert!(editor.upsert("/file.txt", oid).is_err());
        assert!(editor.upsert("file.txt/", oid).is_err());
    }

    #[test]
    fn test_remove_existing() {
        let mut editor = TreeEditor::empty();
        let oid = dummy_oid();

        editor.upsert("dir/file.txt", oid).unwrap();
        assert_eq!(editor.root.len(), 1);

        editor.remove("dir/file.txt").unwrap();

        let dir_subtree = editor.subtrees.get("dir".as_bytes().as_bstr()).unwrap();
        assert!(dir_subtree.is_empty());
    }

    #[test]
    fn test_remove_nonexistent_is_noop() {
        let mut editor = TreeEditor::empty();
        editor.remove("nonexistent.txt").unwrap();
        editor.remove("dir/nonexistent.txt").unwrap();
    }

    #[test]
    fn test_upsert_top_level_file() {
        let mut editor = TreeEditor::empty();
        let oid = dummy_oid();

        editor.upsert("top-level.txt", oid).unwrap();

        assert_eq!(editor.root.len(), 1);
        let entry = editor.root.get("top-level.txt".as_bytes().as_bstr()).unwrap();
        assert_eq!(entry.mode, EntryKind::Blob.into());
        assert_eq!(entry.filename, "top-level.txt");
        assert_eq!(entry.oid, oid);
    }

    #[test]
    fn test_remove_top_level_file() {
        let mut editor = TreeEditor::empty();
        let oid = dummy_oid();

        editor.upsert("single.txt", oid).unwrap();
        assert_eq!(editor.root.len(), 1);

        editor.remove("single.txt").unwrap();
        assert_eq!(editor.root.len(), 0);
    }
}
