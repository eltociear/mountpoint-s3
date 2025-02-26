use async_trait::async_trait;
use auto_impl::auto_impl;
use futures::Stream;
use std::ops::Range;
use thiserror::Error;
use time::OffsetDateTime;

/// A single element of the [ObjectClient::get_object] response is a pair of offset within the
/// object and the bytes starting at that offset.
pub type GetBodyPart = (u64, Box<[u8]>);

/// An [ObjectClient] is an S3-like blob storage interface
#[async_trait]
#[auto_impl(Arc)]
pub trait ObjectClient {
    type GetObjectResult: Stream<Item = ObjectClientResult<GetBodyPart, GetObjectError, Self::ClientError>> + Send;
    type ClientError: std::error::Error + Send + Sync + 'static;

    /// Delete a single object from the object store.
    ///
    /// DeleteObject will succeed even if the object within the bucket does not exist.
    async fn delete_object(
        &self,
        bucket: &str,
        key: &str,
    ) -> ObjectClientResult<DeleteObjectResult, DeleteObjectError, Self::ClientError>;

    /// Get an object from the object store. Returns a stream of body parts of the object. Parts are
    /// guaranteed to be returned by the stream in order and contiguously.
    async fn get_object(
        &self,
        bucket: &str,
        key: &str,
        range: Option<Range<u64>>,
    ) -> ObjectClientResult<Self::GetObjectResult, GetObjectError, Self::ClientError>;

    /// List the objects in a bucket under a given prefix
    async fn list_objects(
        &self,
        bucket: &str,
        continuation_token: Option<&str>,
        delimiter: &str,
        max_keys: usize,
        prefix: &str,
    ) -> ObjectClientResult<ListObjectsResult, ListObjectsError, Self::ClientError>;

    /// Retrieve object metadata without retrieving the object contents
    async fn head_object(
        &self,
        bucket: &str,
        key: &str,
    ) -> ObjectClientResult<HeadObjectResult, HeadObjectError, Self::ClientError>;

    /// Put an object into the object store.
    /// The contents are provided by the client as an async stream of buffers.
    async fn put_object(
        &self,
        bucket: &str,
        key: &str,
        params: &PutObjectParams,
        contents: impl Stream<Item = impl AsRef<[u8]> + Send> + Send,
    ) -> ObjectClientResult<PutObjectResult, PutObjectError, Self::ClientError>;
}

/// Errors returned by calls to an [ObjectClient]. Errors that are explicitly modeled on a
/// per-request-type basis are [ServiceError]s. Other generic or unhandled errors are
/// [ClientError]s.
///
/// The distinction between these two types of error can sometimes be blurry. As a rough heuristic,
/// [ServiceError]s are those that *any reasonable implementation* of an object client would be
/// capable of experiencing, and [ClientError]s are anything else. For example, any object client
/// could experience a "no such key" error, but only object clients that implement a permissions
/// system could experience "permission denied" errors. When in doubt, we err towards *not* adding
/// new [ServiceError]s, as they are public API for *every* object client.
#[derive(Debug, Error)]
pub enum ObjectClientError<S, C> {
    /// An error returned by the service itself
    #[error("Service error")]
    ServiceError(#[source] S),

    /// An error within the object client (for example, an unexpected response, or a failure to
    /// construct the request).
    #[error("Client error")]
    ClientError(#[from] C),
}

pub type ObjectClientResult<T, S, C> = Result<T, ObjectClientError<S, C>>;

#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum GetObjectError {
    #[error("The bucket does not exist")]
    NoSuchBucket,

    #[error("The key does not exist")]
    NoSuchKey,
}

/// Result of a [ObjectClient::list_objects] request
#[derive(Debug)]
#[non_exhaustive]
pub struct ListObjectsResult {
    /// The name of the bucket.
    pub bucket: String,

    /// The list of objects.
    pub objects: Vec<ObjectInfo>,

    /// The list of common prefixes. This rolls up all of the objects with a common prefix up to
    /// the next instance of the delimiter.
    pub common_prefixes: Vec<String>,

    /// If present, the continuation token to use to query more results.
    pub next_continuation_token: Option<String>,
}

#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum ListObjectsError {
    #[error("The bucket does not exist")]
    NoSuchBucket,
}

/// Result of a [ObjectClient::head_object] request
#[derive(Debug)]
#[non_exhaustive]
pub struct HeadObjectResult {
    /// The name of the bcuket
    pub bucket: String,

    /// Object metadata
    pub object: ObjectInfo,
}

#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum HeadObjectError {
    /// Note that HeadObject cannot distinguish between NoSuchBucket and NoSuchKey errors
    #[error("The object was not found")]
    NotFound,
}

/// Result of a [ObjectClient::delete_object] request
///
/// Note: DeleteObject calls on a non-existent object within a bucket are considered a success.
///
/// TODO: Populate this struct with return fields from the S3 API, e.g., version id, delete marker.
#[derive(Debug)]
#[non_exhaustive]
pub struct DeleteObjectResult {}

#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum DeleteObjectError {
    #[error("The bucket does not exist")]
    NoSuchBucket,
}

/// Parameters to a [ObjectClient::put_object] request
/// TODO: Populate this struct with parameters from the S3 API, e.g., storage class, encryption.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct PutObjectParams {}

/// Result of a [ObjectClient::put_object] request
/// TODO: Populate this struct with return fields from the S3 API, e.g., etag.
#[derive(Debug)]
#[non_exhaustive]
pub struct PutObjectResult {}

#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum PutObjectError {
    #[error("The bucket does not exist")]
    NoSuchBucket,
}

/// Metadata about a single S3 object.
/// See https://docs.aws.amazon.com/AmazonS3/latest/API/API_Object.html for more details.
#[derive(Debug)]
pub struct ObjectInfo {
    /// Key for this object.
    pub key: String,

    /// Size of this object in bytes.
    pub size: u64,

    /// The time this object was last modified.
    pub last_modified: OffsetDateTime,

    /// Storage class for this object. Optional because head_object does not return
    /// the storage class in its response. See examples here:
    /// https://docs.aws.amazon.com/AmazonS3/latest/API/API_HeadObject.html#API_HeadObject_Examples
    pub storage_class: Option<String>,

    /// Entity tag of this object.
    pub etag: String,
}
