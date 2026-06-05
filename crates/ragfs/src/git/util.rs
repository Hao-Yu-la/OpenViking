//! Utility functions for Git module

use crate::git::error::RefStoreError;

/// Validate a ref name follows Git naming conventions.
///
/// Checks for:
/// - Empty name
/// - Contains ".."
/// - Starts or ends with "/"
/// - Contains invalid characters
pub fn validate_ref_name(ref_name: &str) -> Result<(), RefStoreError> {
    if ref_name.is_empty() {
        return Err(RefStoreError::InvalidName(
            "ref name cannot be empty".to_string(),
        ));
    }
    if ref_name.contains("..") {
        return Err(RefStoreError::InvalidName(
            "ref name cannot contain '..'".to_string(),
        ));
    }
    if ref_name.starts_with('/') || ref_name.ends_with('/') {
        return Err(RefStoreError::InvalidName(
            "ref name cannot start or end with '/'".to_string(),
        ));
    }
    if ref_name.contains(' ')
        || ref_name.contains('\x00')
        || ref_name.contains('~')
        || ref_name.contains('^')
        || ref_name.contains(':')
        || ref_name.contains('?')
        || ref_name.contains('[')
        || ref_name.contains('*')
    {
        return Err(RefStoreError::InvalidName(
            "ref name contains invalid characters".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_ref_name() {
        assert!(validate_ref_name("refs/heads/main").is_ok());
        assert!(validate_ref_name("refs/tags/v1.0").is_ok());
        assert!(validate_ref_name("HEAD").is_ok());

        assert!(validate_ref_name("").is_err());
        assert!(validate_ref_name("..").is_err());
        assert!(validate_ref_name("refs/../heads").is_err());
        assert!(validate_ref_name("/refs/heads").is_err());
        assert!(validate_ref_name("refs/heads/ ").is_err());
        assert!(validate_ref_name("refs~head").is_err());
        assert!(validate_ref_name("refs^head").is_err());
        assert!(validate_ref_name("refs:head").is_err());
        assert!(validate_ref_name("refs?head").is_err());
        assert!(validate_ref_name("refs[head]").is_err());
        assert!(validate_ref_name("refs*head").is_err());
    }
}
