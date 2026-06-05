//! Utility functions for Git module

use bytes::Bytes;
use std::io::{Read, Write};

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

/// Compress data using zlib (for Git loose object storage).
pub fn zlib_compress(data: &[u8]) -> Result<Vec<u8>, std::io::Error> {
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(data)?;
    encoder.finish()
}

/// Decompress zlib-compressed data (for reading Git loose objects).
pub fn zlib_decompress(data: &[u8]) -> Result<Vec<u8>, std::io::Error> {
    let mut decoder = flate2::read::ZlibDecoder::new(data);
    let mut decoded = Vec::new();
    decoder.read_to_end(&mut decoded)?;
    Ok(decoded)
}

/// Parse a Git loose object header, returning (kind, size, header_end_offset).
pub fn parse_object_header(data: &[u8]) -> Result<(gix_object::Kind, u64, usize), crate::git::error::ObjectStoreError> {
    gix_object::decode::loose_header(data).map_err(|e| {
        crate::git::error::ObjectStoreError::Backend(format!("invalid object header: {e}"))
    })
}

/// Read and decompress a Git object from ObjectStore, returning the full
/// uncompressed bytes (including header).
pub async fn read_object(
    store: &dyn crate::git::object_store::ObjectStore,
    account: &str,
    oid: &gix_hash::ObjectId,
) -> Result<Bytes, crate::git::error::ObjectStoreError> {
    let compressed = store.get(account, oid).await?;
    let decompressed = zlib_decompress(&compressed)
        .map_err(|e| crate::git::error::ObjectStoreError::Zlib(e.to_string()))?;
    Ok(Bytes::from(decompressed))
}

/// Serialize, compress, and write a Git object to ObjectStore.
/// Returns the object's ObjectId.
pub async fn write_object(
    store: &dyn crate::git::object_store::ObjectStore,
    account: &str,
    kind: gix_object::Kind,
    data: &[u8],
) -> Result<gix_hash::ObjectId, crate::git::error::ObjectStoreError> {
    let header = gix_object::encode::loose_header(kind, data.len() as u64);
    let oid = gix_object::compute_hash(gix_hash::Kind::Sha1, kind, data);
    let mut full = Vec::with_capacity(header.len() + data.len());
    full.extend_from_slice(&header);
    full.extend_from_slice(data);
    let compressed = zlib_compress(&full)?;
    store.put(account, &oid, Bytes::from(compressed)).await?;
    Ok(oid)
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

    #[test]
    fn test_zlib_round_trip() {
        let original = b"tree 15\0hello world!!!";
        let compressed = zlib_compress(original).unwrap();
        let decompressed = zlib_decompress(&compressed).unwrap();
        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_parse_object_header_tree() {
        let data = b"tree 15\0entries data";
        let (kind, size, offset) = parse_object_header(data).unwrap();
        assert_eq!(kind, gix_object::Kind::Tree);
        assert_eq!(size, 15);
        assert_eq!(offset, 8);
    }

    #[test]
    fn test_parse_object_header_blob() {
        let data = b"blob 5\0hello";
        let (kind, size, offset) = parse_object_header(data).unwrap();
        assert_eq!(kind, gix_object::Kind::Blob);
        assert_eq!(size, 5);
        assert_eq!(offset, 7);
    }

    #[tokio::test]
    async fn test_write_read_object_round_trip() {
        use tempfile::tempdir;
        use crate::git::backends::local::LocalObjectStore;

        let temp_dir = tempdir().unwrap();
        let store = LocalObjectStore::new(temp_dir.path());

        let data = b"hello tree bytes";
        let kind = gix_object::Kind::Blob;

        // Write the object
        let oid = write_object(&store, "test-account", kind, data).await.unwrap();

        // Read the object back
        let raw = read_object(&store, "test-account", &oid).await.unwrap();

        // Parse and validate header
        let (parsed_kind, size, offset) = parse_object_header(&raw).unwrap();
        assert_eq!(parsed_kind, kind);
        assert_eq!(size, data.len() as u64);

        // Validate body
        assert_eq!(&raw[offset..], data);

        // Validate OID matches expected
        let expected_oid = gix_object::compute_hash(gix_hash::Kind::Sha1, kind, data);
        assert_eq!(oid, expected_oid);
    }
}
