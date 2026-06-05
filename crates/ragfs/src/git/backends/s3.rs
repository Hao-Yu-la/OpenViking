//! S3 storage backend for Git objects and refs

use std::sync::Arc;

use async_trait::async_trait;
use aws_sdk_s3::config::BehaviorVersion;
use aws_sdk_s3::config::Credentials;
use aws_sdk_s3::config::Region;
use bytes::Bytes;
use gix_hash::ObjectId;

use crate::git::error::{ObjectStoreError, RefStoreError};
use crate::git::object_store::ObjectStore;
use crate::git::ref_store::RefStore;
use crate::git::util::validate_ref_name;

/// CAS (Compare-and-Swap) mode for S3 ref updates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CasMode {
    /// Use S3's native conditional writes with If-Match and ETag.
    /// This requires the S3 backend to support ETag and conditional headers.
    Native,
    /// Use a distributed lock (e.g., Redis) for CAS.
    /// Not yet implemented - placeholder for future.
    #[allow(dead_code)]
    RedisLock,
}

/// S3 configuration for Git storage
#[derive(Debug, Clone)]
pub struct S3Config {
    /// S3 bucket name
    pub bucket: String,
    /// Key prefix for Git storage (e.g. "git/")
    pub prefix: String,
    /// Region (e.g. "us-east-1")
    pub region: String,
    /// Optional endpoint URL (for MinIO, LocalStack, TOS, etc.)
    pub endpoint: Option<String>,
    /// Optional access key ID
    pub access_key_id: Option<String>,
    /// Optional secret access key
    pub secret_access_key: Option<String>,
    /// Whether to use path-style addressing (bucket/key vs bucket.host/key)
    pub use_path_style: bool,
    /// CAS mode for ref updates
    pub cas_mode: CasMode,
}

impl Default for S3Config {
    fn default() -> Self {
        Self {
            bucket: String::new(),
            prefix: String::new(),
            region: "us-east-1".to_string(),
            endpoint: None,
            access_key_id: None,
            secret_access_key: None,
            use_path_style: true,
            cas_mode: CasMode::Native,
        }
    }
}

/// S3-based object store implementation
pub struct S3ObjectStore {
    client: Arc<aws_sdk_s3::Client>,
    bucket: String,
    prefix: String,
}

impl S3ObjectStore {
    /// Create a new S3ObjectStore from an existing S3 client
    pub fn new(client: Arc<aws_sdk_s3::Client>, bucket: String, prefix: String) -> Self {
        Self {
            client,
            bucket,
            prefix,
        }
    }

    /// Create a new S3ObjectStore from configuration
    pub async fn from_config(config: S3Config) -> Result<Self, ObjectStoreError> {
        let mut s3_config_builder = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new(config.region))
            .force_path_style(config.use_path_style);

        // Set endpoint if provided (MinIO, LocalStack, TOS)
        if let Some(ep) = config.endpoint {
            s3_config_builder = s3_config_builder.endpoint_url(ep);
        }

        // Set credentials if provided, otherwise SDK uses default chain
        if let (Some(ak), Some(sk)) = (config.access_key_id, config.secret_access_key) {
            let creds = Credentials::new(ak, sk, None, None, "ragfs-git");
            s3_config_builder = s3_config_builder.credentials_provider(creds);
        }

        let s3_config = s3_config_builder.build();
        let client = Arc::new(aws_sdk_s3::Client::from_conf(s3_config));

        Ok(Self::new(client, config.bucket, config.prefix))
    }

    /// Build the full S3 key for a Git object
    fn object_key(&self, account: &str, oid: &ObjectId) -> String {
        let hex = oid.to_hex().to_string();
        let prefix = self.prefix.trim_end_matches('/');
        if prefix.is_empty() {
            format!("{}/objects/{}/{}", account, &hex[..2], &hex[2..])
        } else {
            format!(
                "{}/{}/objects/{}/{}",
                prefix,
                account,
                &hex[..2],
                &hex[2..]
            )
        }
    }
}

#[async_trait]
impl ObjectStore for S3ObjectStore {
    async fn put(
        &self,
        account: &str,
        oid: &ObjectId,
        zlib_body: Bytes,
    ) -> Result<(), ObjectStoreError> {
        let key = self.object_key(account, oid);

        // Use If-None-Match: "*" to ensure idempotency - only write if not exists
        match self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(&key)
            .body(zlib_body.to_vec().into())
            .if_none_match("*")
            .send()
            .await
        {
            Ok(_) => Ok(()),
            Err(aws_sdk_s3::error::SdkError::ServiceError(err)) => {
                // Check if the error indicates object already exists
                let err_str = format!("{:?}", err);
                if err_str.to_lowercase().contains("preconditionfailed")
                    || err_str.to_lowercase().contains("412")
                    || err_str.to_lowercase().contains("not modified")
                {
                    // Object already exists - that's fine for idempotency
                    Ok(())
                } else {
                    Err(ObjectStoreError::Backend(format!(
                        "S3 put error: {:?}",
                        err
                    )))
                }
            }
            Err(err) => Err(ObjectStoreError::Backend(format!("S3 put error: {:?}", err))),
        }
    }

    async fn get(&self, account: &str, oid: &ObjectId) -> Result<Bytes, ObjectStoreError> {
        let key = self.object_key(account, oid);

        match self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(&key)
            .send()
            .await
        {
            Ok(resp) => {
                let bytes = resp
                    .body
                    .collect()
                    .await
                    .map_err(|e| ObjectStoreError::Backend(format!("S3 read body error: {:?}", e)))?;
                Ok(Bytes::copy_from_slice(&bytes.to_vec()))
            }
            Err(err) => {
                // Check if the error indicates object not found
                let err_str = format!("{:?}", err);
                if err_str.to_lowercase().contains("no_such_key")
                    || err_str.to_lowercase().contains("404")
                {
                    Err(ObjectStoreError::NotFound(*oid))
                } else {
                    Err(ObjectStoreError::Backend(format!("S3 get error: {:?}", err)))
                }
            }
        }
    }

    async fn exists(&self, account: &str, oid: &ObjectId) -> Result<bool, ObjectStoreError> {
        let key = self.object_key(account, oid);

        match self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(&key)
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(err) => {
                // Check if the error indicates object not found
                let err_str = format!("{:?}", err);
                if err_str.to_lowercase().contains("not_found")
                    || err_str.to_lowercase().contains("404")
                {
                    Ok(false)
                } else {
                    Err(ObjectStoreError::Backend(format!("S3 head error: {:?}", err)))
                }
            }
        }
    }
}

/// S3-based ref store implementation
pub struct S3RefStore {
    client: Arc<aws_sdk_s3::Client>,
    bucket: String,
    prefix: String,
    cas_mode: CasMode,
}

impl S3RefStore {
    /// Create a new S3RefStore from an existing S3 client
    pub fn new(client: Arc<aws_sdk_s3::Client>, bucket: String, prefix: String) -> Self {
        Self {
            client,
            bucket,
            prefix,
            cas_mode: CasMode::Native,
        }
    }

    /// Create a new S3RefStore with explicit CAS mode
    pub fn with_cas_mode(
        client: Arc<aws_sdk_s3::Client>,
        bucket: String,
        prefix: String,
        cas_mode: CasMode,
    ) -> Self {
        Self {
            client,
            bucket,
            prefix,
            cas_mode,
        }
    }

    /// Create a new S3RefStore from configuration
    pub async fn from_config(config: S3Config) -> Result<Self, RefStoreError> {
        let mut s3_config_builder = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new(config.region))
            .force_path_style(config.use_path_style);

        // Set endpoint if provided (MinIO, LocalStack, TOS)
        if let Some(ep) = config.endpoint {
            s3_config_builder = s3_config_builder.endpoint_url(ep);
        }

        // Set credentials if provided, otherwise SDK uses default chain
        if let (Some(ak), Some(sk)) = (config.access_key_id, config.secret_access_key) {
            let creds = Credentials::new(ak, sk, None, None, "ragfs-git");
            s3_config_builder = s3_config_builder.credentials_provider(creds);
        }

        let s3_config = s3_config_builder.build();
        let client = Arc::new(aws_sdk_s3::Client::from_conf(s3_config));

        Ok(Self::with_cas_mode(
            client,
            config.bucket,
            config.prefix,
            config.cas_mode,
        ))
    }

    /// Build the full S3 key for a Git ref
    fn ref_key(&self, account: &str, ref_name: &str) -> String {
        let prefix = self.prefix.trim_end_matches('/');
        if prefix.is_empty() {
            format!("{}/{}", account, ref_name)
        } else {
            format!("{}/{}/{}", prefix, account, ref_name)
        }
    }

    /// Read the current value of a ref, returning None if it doesn't exist
    async fn read_ref_opt(
        &self,
        account: &str,
        ref_name: &str,
    ) -> Result<Option<(ObjectId, Option<String>)>, RefStoreError> {
        let key = self.ref_key(account, ref_name);

        match self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(&key)
            .send()
            .await
        {
            Ok(resp) => {
                let etag = resp.e_tag;
                let bytes = resp
                    .body
                    .collect()
                    .await
                    .map_err(|e| RefStoreError::Backend(format!("S3 read body error: {:?}", e)))?;
                let vec_bytes = bytes.to_vec();
                let content = String::from_utf8_lossy(&vec_bytes);
                let trimmed = content.trim();
                let oid = trimmed
                    .parse::<ObjectId>()
                    .map_err(|_| RefStoreError::Backend(format!("invalid oid in ref: {}", trimmed)))?;
                Ok(Some((oid, etag)))
            }
            Err(err) => {
                // Check if the error indicates ref not found
                let err_str = format!("{:?}", err);
                if err_str.to_lowercase().contains("no_such_key")
                    || err_str.to_lowercase().contains("404")
                {
                    Ok(None)
                } else {
                    Err(RefStoreError::Backend(format!("S3 get error: {:?}", err)))
                }
            }
        }
    }

    /// Perform native CAS with S3 conditional headers
    async fn cas_native(
        &self,
        account: &str,
        ref_name: &str,
        expected: Option<ObjectId>,
        new: ObjectId,
    ) -> Result<(), RefStoreError> {
        let key = self.ref_key(account, ref_name);

        // First, read to get the current value and ETag
        let (current_value, current_etag) = match self.read_ref_opt(account, ref_name).await? {
            Some((oid, etag)) => (Some(oid), etag),
            None => (None, None),
        };

        // Verify the expected value matches
        if current_value != expected {
            return Err(RefStoreError::Conflict {
                expected,
                actual: current_value,
            });
        }

        // Prepare the conditional put request
        let body = format!("{}\n", new.to_hex());
        let mut put_builder = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(&key)
            .body(body.into_bytes().into());

        put_builder = match (current_etag, expected) {
            (Some(etag), Some(_)) => {
                // Existing ref - use If-Match with the current ETag
                put_builder.if_match(etag)
            }
            (None, None) => {
                // New ref - use If-None-Match: "*" to ensure it doesn't exist
                put_builder.if_none_match("*")
            }
            _ => {
                // This shouldn't happen after our check, but just in case
                return Err(RefStoreError::Conflict {
                    expected,
                    actual: current_value,
                });
            }
        };

        match put_builder.send().await {
            Ok(_) => Ok(()),
            Err(aws_sdk_s3::error::SdkError::ServiceError(err)) => {
                let err_str = format!("{:?}", err);
                if err_str.to_lowercase().contains("preconditionfailed")
                    || err_str.to_lowercase().contains("412")
                {
                    // Conditional check failed - re-read and report conflict
                    let actual = self.read_ref_opt(account, ref_name).await?.map(|(oid, _)| oid);
                    Err(RefStoreError::Conflict { expected, actual })
                } else {
                    Err(RefStoreError::Backend(format!("S3 put error: {:?}", err)))
                }
            }
            Err(err) => Err(RefStoreError::Backend(format!("S3 put error: {:?}", err))),
        }
    }
}

#[async_trait]
impl RefStore for S3RefStore {
    async fn read(&self, account: &str, ref_name: &str) -> Result<ObjectId, RefStoreError> {
        // Validate ref name
        validate_ref_name(ref_name)?;

        self.read_ref_opt(account, ref_name)
            .await?
            .map(|(oid, _)| oid)
            .ok_or_else(|| RefStoreError::NotFound(ref_name.to_string()))
    }

    async fn cas_update(
        &self,
        account: &str,
        ref_name: &str,
        expected: Option<ObjectId>,
        new: ObjectId,
    ) -> Result<(), RefStoreError> {
        // Validate ref name first
        validate_ref_name(ref_name)?;

        match self.cas_mode {
            CasMode::Native => {
                self.cas_native(account, ref_name, expected, new).await
            }
            CasMode::RedisLock => {
                // Redis lock mode not yet implemented
                Err(RefStoreError::Backend(
                    "RedisLock CAS mode not yet implemented".to_string(),
                ))
            }
        }
    }

    async fn list(
        &self,
        account: &str,
        prefix: &str,
    ) -> Result<Vec<(String, ObjectId)>, RefStoreError> {
        let key_prefix = self.ref_key(account, prefix);
        let key_prefix = if key_prefix.ends_with('/') {
            key_prefix
        } else {
            format!("{}/", key_prefix)
        };

        let mut result = Vec::new();
        let mut continuation_token = None;

        loop {
            let mut req = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(&key_prefix);

            if let Some(token) = continuation_token {
                req = req.continuation_token(token);
            }

            let resp = req
                .send()
                .await
                .map_err(|e| RefStoreError::Backend(format!("S3 list error: {:?}", e)))?;

            let next_token = resp.next_continuation_token().map(|s| s.to_string());

            for obj in resp.contents() {
                if let Some(key) = obj.key() {
                    // Skip directory markers
                    if key.ends_with('/') {
                        continue;
                    }

                    // Strip the base prefix to get the ref name
                    let base_prefix = self.ref_key(account, "");
                    let ref_name = key.strip_prefix(&base_prefix).unwrap_or(key);

                    // Read the ref value (without ETag)
                    if let Ok(Some((oid, _))) = self.read_ref_opt(account, ref_name).await {
                        result.push((ref_name.to_string(), oid));
                    }
                }
            }

            if resp.is_truncated() == Some(true) {
                continuation_token = next_token;
            } else {
                break;
            }
        }

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cas_mode_variants() {
        assert_eq!(CasMode::Native, CasMode::Native);
        assert_ne!(CasMode::Native, CasMode::RedisLock);
    }

    #[test]
    fn test_s3_config_default() {
        let config = S3Config::default();
        assert_eq!(config.region, "us-east-1");
        assert_eq!(config.use_path_style, true);
        assert_eq!(config.cas_mode, CasMode::Native);
    }
}
