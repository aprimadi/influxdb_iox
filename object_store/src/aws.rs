//! This module contains the IOx implementation for using S3 as the object
//! store.
use crate::{
    buffer::slurp_stream_tempfile,
    path::{cloud::CloudPath, DELIMITER},
    ListResult, ObjectMeta, ObjectStoreApi,
};
use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures::{
    stream::{self, BoxStream},
    Future, Stream, StreamExt, TryStreamExt,
};
use futures_retry::{FutureRetry, RetryPolicy};
use rusoto_core::ByteStream;
use rusoto_credential::{InstanceMetadataProvider, StaticProvider};
use rusoto_s3::S3;
use snafu::{OptionExt, ResultExt, Snafu};
use std::{convert::TryFrom, fmt, io, time::Duration};

/// A specialized `Result` for object store-related errors
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A specialized `Error` for object store-related errors
#[derive(Debug, Snafu)]
#[allow(missing_docs)]
pub enum Error {
    #[snafu(display("Expected streamed data to have length {}, got {}", expected, actual))]
    DataDoesNotMatchLength { expected: usize, actual: usize },

    #[snafu(display("Did not receive any data. Bucket: {}, Location: {}", bucket, location))]
    NoData { bucket: String, location: String },

    #[snafu(display(
        "Unable to DELETE data. Bucket: {}, Location: {}, Error: {}",
        bucket,
        location,
        source,
    ))]
    UnableToDeleteData {
        source: rusoto_core::RusotoError<rusoto_s3::DeleteObjectError>,
        bucket: String,
        location: String,
    },

    #[snafu(display(
        "Unable to GET data. Bucket: {}, Location: {}, Error: {}",
        bucket,
        location,
        source,
    ))]
    UnableToGetData {
        source: rusoto_core::RusotoError<rusoto_s3::GetObjectError>,
        bucket: String,
        location: String,
    },

    #[snafu(display(
        "Unable to GET part of the data. Bucket: {}, Location: {}, Error: {}",
        bucket,
        location,
        source,
    ))]
    UnableToGetPieceOfData {
        source: std::io::Error,
        bucket: String,
        location: String,
    },

    #[snafu(display(
        "Unable to PUT data. Bucket: {}, Location: {}, Error: {}",
        bucket,
        location,
        source,
    ))]
    UnableToPutData {
        source: rusoto_core::RusotoError<rusoto_s3::PutObjectError>,
        bucket: String,
        location: String,
    },

    #[snafu(display("Unable to list data. Bucket: {}, Error: {}", bucket, source))]
    UnableToListData {
        source: rusoto_core::RusotoError<rusoto_s3::ListObjectsV2Error>,
        bucket: String,
    },

    #[snafu(display(
        "Unable to parse last modified date. Bucket: {}, Error: {}",
        bucket,
        source
    ))]
    UnableToParseLastModified {
        source: chrono::ParseError,
        bucket: String,
    },

    #[snafu(display("Unable to buffer data into temporary file, Error: {}", source))]
    UnableToBufferStream { source: std::io::Error },

    #[snafu(display(
        "Could not parse `{}` as an AWS region. Regions should look like `us-east-2`. {:?}",
        region,
        source
    ))]
    InvalidRegion {
        region: String,
        source: rusoto_core::region::ParseRegionError,
    },

    #[snafu(display("Missing aws-access-key"))]
    MissingAccessKey,

    #[snafu(display("Missing aws-secret-access-key"))]
    MissingSecretAccessKey,
}

/// Configuration for connecting to [Amazon S3](https://aws.amazon.com/s3/).
pub struct AmazonS3 {
    client: rusoto_s3::S3Client,
    bucket_name: String,
}

impl fmt::Debug for AmazonS3 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AmazonS3")
            .field("client", &"rusoto_s3::S3Client")
            .field("bucket_name", &self.bucket_name)
            .finish()
    }
}

#[async_trait]
impl ObjectStoreApi for AmazonS3 {
    type Path = CloudPath;
    type Error = Error;

    fn new_path(&self) -> Self::Path {
        CloudPath::default()
    }

    async fn put<F, S>(&self, location: &Self::Path, bytes: F, length: Option<usize>) -> Result<()>
    where
        F: Fn() -> S + Clone + Send + Sync + Unpin + 'static,
        S: Stream<Item = io::Result<Bytes>> + Send + Sync + 'static,
    {
        let bucket_name = self.bucket_name.clone();
        let key = location.to_raw();
        let request_factory = move || {
            let bytes = bytes.clone();
            async move {
                let bytes = bytes();
                let bytes = match length {
                    Some(length) => ByteStream::new_with_size(bytes, length),
                    None => {
                        let bytes = slurp_stream_tempfile(bytes).await.unwrap();
                        let length = bytes.size();
                        ByteStream::new_with_size(bytes, length)
                    }
                };

                rusoto_s3::PutObjectRequest {
                    bucket: bucket_name.clone(),
                    key: key.clone(),
                    body: Some(bytes),
                    ..Default::default()
                }
            }
        };

        let s3 = self.client.clone();

        s3_request(move || {
            let (s3, request_factory) = (s3.clone(), request_factory.clone());

            async move { Ok(async move { s3.put_object(request_factory().await).await.map(drop) }) }
        })
        .await
        .context(UnableToPutData {
            bucket: &self.bucket_name,
            location: location.to_raw(),
        })?;

        Ok(())
    }

    async fn get(&self, location: &Self::Path) -> Result<BoxStream<'static, Result<Bytes>>> {
        let key = location.to_raw();
        let get_request = rusoto_s3::GetObjectRequest {
            bucket: self.bucket_name.clone(),
            key: key.clone(),
            ..Default::default()
        };
        let bucket_name = self.bucket_name.clone();
        Ok(self
            .client
            .get_object(get_request)
            .await
            .context(UnableToGetData {
                bucket: self.bucket_name.to_owned(),
                location: key.clone(),
            })?
            .body
            .context(NoData {
                bucket: self.bucket_name.to_owned(),
                location: key.clone(),
            })?
            .map_err(move |source| Error::UnableToGetPieceOfData {
                source,
                bucket: bucket_name.clone(),
                location: key.clone(),
            })
            .err_into()
            .boxed())
    }

    async fn delete(&self, location: &Self::Path) -> Result<()> {
        let key = location.to_raw();
        let bucket_name = self.bucket_name.clone();

        let request_factory = move || rusoto_s3::DeleteObjectRequest {
            bucket: bucket_name.clone(),
            key: key.clone(),
            ..Default::default()
        };

        let s3 = self.client.clone();

        s3_request(move || {
            let (s3, request_factory) = (s3.clone(), request_factory.clone());

            async move { Ok(async move { s3.delete_object(request_factory()).await }) }
        })
        .await
        .context(UnableToDeleteData {
            bucket: &self.bucket_name,
            location: location.to_raw(),
        })?;

        Ok(())
    }

    async fn list<'a>(
        &'a self,
        prefix: Option<&'a Self::Path>,
    ) -> Result<BoxStream<'a, Result<Vec<Self::Path>>>> {
        Ok(self
            .list_objects_v2(prefix, None)
            .await?
            .map_ok(|list_objects_v2_result| {
                let contents = list_objects_v2_result.contents.unwrap_or_default();

                let names = contents
                    .into_iter()
                    .flat_map(|object| object.key.map(CloudPath::raw))
                    .collect();

                names
            })
            .boxed())
    }

    async fn list_with_delimiter(&self, prefix: &Self::Path) -> Result<ListResult<Self::Path>> {
        Ok(self
            .list_objects_v2(Some(prefix), Some(DELIMITER.to_string()))
            .await?
            .try_fold(
                ListResult {
                    next_token: None,
                    common_prefixes: vec![],
                    objects: vec![],
                },
                |acc, list_objects_v2_result| async move {
                    let mut res = acc;
                    let contents = list_objects_v2_result.contents.unwrap_or_default();
                    let mut objects = contents
                        .into_iter()
                        .map(|object| {
                            let location = CloudPath::raw(
                                object.key.expect("object doesn't exist without a key"),
                            );
                            let last_modified = match object.last_modified {
                                Some(lm) => DateTime::parse_from_rfc3339(&lm)
                                    .context(UnableToParseLastModified {
                                        bucket: &self.bucket_name,
                                    })?
                                    .with_timezone(&Utc),
                                None => Utc::now(),
                            };
                            let size = usize::try_from(object.size.unwrap_or(0))
                                .expect("unsupported size on this platform");

                            Ok(ObjectMeta {
                                location,
                                last_modified,
                                size,
                            })
                        })
                        .collect::<Result<Vec<_>>>()?;

                    res.objects.append(&mut objects);

                    res.common_prefixes.extend(
                        list_objects_v2_result
                            .common_prefixes
                            .unwrap_or_default()
                            .into_iter()
                            .map(|p| {
                                CloudPath::raw(
                                    p.prefix.expect("can't have a prefix without a value"),
                                )
                            }),
                    );

                    Ok(res)
                },
            )
            .await?)
    }
}

/// Configure a connection to Amazon S3 using the specified credentials in
/// the specified Amazon region and bucket.
///
/// Note do not expose the AmazonS3::new() function to allow it to be
/// swapped out when the aws feature is not enabled
pub(crate) fn new_s3(
    access_key_id: Option<impl Into<String>>,
    secret_access_key: Option<impl Into<String>>,
    region: impl Into<String>,
    bucket_name: impl Into<String>,
    endpoint: Option<impl Into<String>>,
    session_token: Option<impl Into<String>>,
) -> Result<AmazonS3> {
    let region = region.into();
    let region: rusoto_core::Region = match endpoint {
        None => region.parse().context(InvalidRegion { region })?,
        Some(endpoint) => rusoto_core::Region::Custom {
            name: region,
            endpoint: endpoint.into(),
        },
    };

    let http_client = rusoto_core::request::HttpClient::new()
        .expect("Current implementation of rusoto_core has no way for this to fail");

    let client = match (access_key_id, secret_access_key, session_token) {
        (Some(access_key_id), Some(secret_access_key), Some(session_token)) => {
            let credentials_provider = StaticProvider::new(
                access_key_id.into(),
                secret_access_key.into(),
                Some(session_token.into()),
                None,
            );
            rusoto_s3::S3Client::new_with(http_client, credentials_provider, region)
        }
        (Some(access_key_id), Some(secret_access_key), None) => {
            let credentials_provider =
                StaticProvider::new_minimal(access_key_id.into(), secret_access_key.into());
            rusoto_s3::S3Client::new_with(http_client, credentials_provider, region)
        }
        (None, Some(_), _) => return Err(Error::MissingAccessKey),
        (Some(_), None, _) => return Err(Error::MissingSecretAccessKey),
        _ => {
            let credentials_provider = InstanceMetadataProvider::new();
            rusoto_s3::S3Client::new_with(http_client, credentials_provider, region)
        }
    };

    Ok(AmazonS3 {
        client,
        bucket_name: bucket_name.into(),
    })
}

pub(crate) fn new_failing_s3() -> Result<AmazonS3> {
    new_s3(
        Some("foo"),
        Some("bar"),
        "us-east-1",
        "bucket",
        None as Option<&str>,
        None as Option<&str>,
    )
}

impl AmazonS3 {
    async fn list_objects_v2(
        &self,
        prefix: Option<&CloudPath>,
        delimiter: Option<String>,
    ) -> Result<BoxStream<'_, Result<rusoto_s3::ListObjectsV2Output>>> {
        #[derive(Clone)]
        enum ListState {
            Start,
            HasMore(String),
            Done,
        }
        use ListState::*;

        let raw_prefix = prefix.map(|p| p.to_raw());
        let bucket = self.bucket_name.clone();

        let request_factory = move || rusoto_s3::ListObjectsV2Request {
            bucket,
            prefix: raw_prefix.clone(),
            delimiter: delimiter.clone(),
            ..Default::default()
        };

        Ok(stream::unfold(ListState::Start, move |state| {
            let request_factory = request_factory.clone();
            let s3 = self.client.clone();

            async move {
                let continuation_token = match state.clone() {
                    HasMore(continuation_token) => Some(continuation_token),
                    Done => {
                        return None;
                    }
                    // If this is the first request we've made, we don't need to make any
                    // modifications to the request
                    Start => None,
                };

                let resp = s3_request(move || {
                    let (s3, request_factory, continuation_token) = (
                        s3.clone(),
                        request_factory.clone(),
                        continuation_token.clone(),
                    );

                    async move {
                        Ok(async move {
                            s3.list_objects_v2(rusoto_s3::ListObjectsV2Request {
                                continuation_token,
                                ..request_factory()
                            })
                            .await
                        })
                    }
                })
                .await;

                let resp = match resp {
                    Ok(resp) => resp,
                    Err(e) => return Some((Err(e), state)),
                };

                // The AWS response contains a field named `is_truncated` as well as
                // `next_continuation_token`, and we're assuming that `next_continuation_token`
                // is only set when `is_truncated` is true (and therefore not
                // checking `is_truncated`).
                let next_state =
                    if let Some(next_continuation_token) = &resp.next_continuation_token {
                        ListState::HasMore(next_continuation_token.to_string())
                    } else {
                        ListState::Done
                    };

                Some((Ok(resp), next_state))
            }
        })
        .map_err(move |e| Error::UnableToListData {
            source: e,
            bucket: self.bucket_name.clone(),
        })
        .boxed())
    }
}

async fn s3_request<E, F, G, H, R>(future_factory: F) -> Result<R, rusoto_core::RusotoError<E>>
where
    F: Fn() -> G + Unpin + Clone + Send + Sync + 'static,
    G: Future<Output = Result<H, rusoto_core::RusotoError<E>>> + Send,
    H: Future<Output = Result<R, rusoto_core::RusotoError<E>>> + Send,
{
    let mut attempts = 0;
    // TODO: configurable
    let n_retries = 10;
    // TODO: let backoff =

    FutureRetry::new(
        move || {
            let future_factory = future_factory.clone();

            async move {
                let request = future_factory().await?;

                request.await
            }
        },
        // retry
        {
            move |e| {
                attempts += 1;
                let should_retry = matches!(
                    e,
                    rusoto_core::RusotoError::Unknown(ref response)
                        if response.status.is_server_error()
                );

                if attempts > n_retries || !should_retry {
                    RetryPolicy::ForwardError(e)
                } else {
                    RetryPolicy::WaitRetry(Duration::from_millis(200))
                }
            }
        },
    )
    .await
    // TODO: log number of attempts?
    .map(|(response, _attempts)| response)
    .map_err(|(err, _attempts)| err)
}

impl Error {
    #[cfg(test)]
    fn s3_error_due_to_credentials(&self) -> bool {
        use rusoto_core::RusotoError;
        use Error::*;

        matches!(
            self,
            UnableToPutData {
                source: RusotoError::Credentials(_),
                bucket: _,
                location: _,
            } | UnableToGetData {
                source: RusotoError::Credentials(_),
                bucket: _,
                location: _,
            } | UnableToDeleteData {
                source: RusotoError::Credentials(_),
                bucket: _,
                location: _,
            } | UnableToListData {
                source: RusotoError::Credentials(_),
                bucket: _,
            }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        tests::{get_nonexistent_object, list_with_delimiter, put_get_delete_list},
        Error as ObjectStoreError, ObjectStore, ObjectStoreApi, ObjectStorePath,
    };
    use bytes::Bytes;
    use std::env;

    type TestError = Box<dyn std::error::Error + Send + Sync + 'static>;
    type Result<T, E = TestError> = std::result::Result<T, E>;

    const NON_EXISTENT_NAME: &str = "nonexistentname";

    #[derive(Debug)]
    struct AwsConfig {
        access_key_id: String,
        secret_access_key: String,
        region: String,
        bucket: String,
        endpoint: Option<String>,
        token: Option<String>,
    }

    // Helper macro to skip tests if TEST_INTEGRATION and the AWS environment variables are not set.
    macro_rules! maybe_skip_integration {
        () => {{
            dotenv::dotenv().ok();

            let required_vars = [
                "AWS_DEFAULT_REGION",
                "INFLUXDB_IOX_BUCKET",
                "AWS_ACCESS_KEY_ID",
                "AWS_SECRET_ACCESS_KEY",
            ];
            let unset_vars: Vec<_> = required_vars
                .iter()
                .filter_map(|&name| match env::var(name) {
                    Ok(_) => None,
                    Err(_) => Some(name),
                })
                .collect();
            let unset_var_names = unset_vars.join(", ");

            let force = env::var("TEST_INTEGRATION");

            if force.is_ok() && !unset_var_names.is_empty() {
                panic!(
                    "TEST_INTEGRATION is set, \
                            but variable(s) {} need to be set",
                    unset_var_names
                );
            } else if force.is_err() {
                eprintln!(
                    "skipping AWS integration test - set {}TEST_INTEGRATION to run",
                    if unset_var_names.is_empty() {
                        String::new()
                    } else {
                        format!("{} and ", unset_var_names)
                    }
                );
                return;
            } else {
                AwsConfig {
                    access_key_id: env::var("AWS_ACCESS_KEY_ID")
                        .expect("already checked AWS_ACCESS_KEY_ID"),
                    secret_access_key: env::var("AWS_SECRET_ACCESS_KEY")
                        .expect("already checked AWS_SECRET_ACCESS_KEY"),
                    region: env::var("AWS_DEFAULT_REGION")
                        .expect("already checked AWS_DEFAULT_REGION"),
                    bucket: env::var("INFLUXDB_IOX_BUCKET")
                        .expect("already checked INFLUXDB_IOX_BUCKET"),
                    endpoint: env::var("AWS_ENDPOINT").ok(),
                    token: env::var("AWS_SESSION_TOKEN").ok(),
                }
            }
        }};
    }

    fn check_credentials<T>(r: Result<T>) -> Result<T> {
        if let Err(e) = &r {
            let e = &**e;
            if let Some(e) = e.downcast_ref::<Error>() {
                if e.s3_error_due_to_credentials() {
                    eprintln!(
                        "Try setting the AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY \
                               environment variables"
                    );
                }
            }
        }

        r
    }

    #[tokio::test]
    async fn s3_test() {
        let config = maybe_skip_integration!();
        let integration = ObjectStore::new_amazon_s3(
            Some(config.access_key_id),
            Some(config.secret_access_key),
            config.region,
            config.bucket,
            config.endpoint,
            config.token,
        )
        .expect("Valid S3 config");

        check_credentials(put_get_delete_list(&integration).await).unwrap();
        check_credentials(list_with_delimiter(&integration).await).unwrap();
    }

    #[tokio::test]
    async fn s3_test_get_nonexistent_region() {
        let mut config = maybe_skip_integration!();
        // Assumes environment variables do not provide credentials to AWS US West 1
        config.region = "us-west-1".into();

        let integration = ObjectStore::new_amazon_s3(
            Some(config.access_key_id),
            Some(config.secret_access_key),
            config.region,
            &config.bucket,
            config.endpoint,
            config.token,
        )
        .expect("Valid S3 config");

        let mut location = integration.new_path();
        location.set_file_name(NON_EXISTENT_NAME);

        let err = get_nonexistent_object(&integration, Some(location))
            .await
            .unwrap_err();
        if let Some(ObjectStoreError::AwsObjectStoreError {
            source: Error::UnableToListData { source, bucket },
        }) = err.downcast_ref::<ObjectStoreError>()
        {
            assert!(matches!(source, rusoto_core::RusotoError::Unknown(_)));
            assert_eq!(bucket, &config.bucket);
        } else {
            panic!("unexpected error type: {:?}", err);
        }
    }

    #[tokio::test]
    async fn s3_test_get_nonexistent_location() {
        let config = maybe_skip_integration!();
        let integration = ObjectStore::new_amazon_s3(
            Some(config.access_key_id),
            Some(config.secret_access_key),
            config.region,
            &config.bucket,
            config.endpoint,
            config.token,
        )
        .expect("Valid S3 config");

        let mut location = integration.new_path();
        location.set_file_name(NON_EXISTENT_NAME);

        let err = get_nonexistent_object(&integration, Some(location))
            .await
            .unwrap_err();
        if let Some(ObjectStoreError::AwsObjectStoreError {
            source:
                Error::UnableToGetData {
                    source,
                    bucket,
                    location,
                },
        }) = err.downcast_ref::<ObjectStoreError>()
        {
            assert!(matches!(
                source,
                rusoto_core::RusotoError::Service(rusoto_s3::GetObjectError::NoSuchKey(_))
            ));
            assert_eq!(bucket, &config.bucket);
            assert_eq!(location, NON_EXISTENT_NAME);
        } else {
            panic!("unexpected error type: {:?}", err);
        }
    }

    #[tokio::test]
    async fn s3_test_get_nonexistent_bucket() {
        let mut config = maybe_skip_integration!();
        config.bucket = NON_EXISTENT_NAME.into();

        let integration = ObjectStore::new_amazon_s3(
            Some(config.access_key_id),
            Some(config.secret_access_key),
            config.region,
            &config.bucket,
            config.endpoint,
            config.token,
        )
        .expect("Valid S3 config");

        let mut location = integration.new_path();
        location.set_file_name(NON_EXISTENT_NAME);

        let err = get_nonexistent_object(&integration, Some(location))
            .await
            .unwrap_err();
        if let Some(ObjectStoreError::AwsObjectStoreError {
            source: Error::UnableToListData { source, bucket },
        }) = err.downcast_ref::<ObjectStoreError>()
        {
            assert!(matches!(
                source,
                rusoto_core::RusotoError::Service(rusoto_s3::ListObjectsV2Error::NoSuchBucket(_))
            ));
            assert_eq!(bucket, &config.bucket);
        } else {
            panic!("unexpected error type: {:?}", err);
        }
    }

    #[tokio::test]
    async fn s3_test_put_nonexistent_region() {
        let mut config = maybe_skip_integration!();
        // Assumes environment variables do not provide credentials to AWS US West 1
        config.region = "us-west-1".into();

        let integration = ObjectStore::new_amazon_s3(
            Some(config.access_key_id),
            Some(config.secret_access_key),
            config.region,
            &config.bucket,
            config.endpoint,
            config.token,
        )
        .expect("Valid S3 config");

        let mut location = integration.new_path();
        location.set_file_name(NON_EXISTENT_NAME);

        let data = Bytes::from("arbitrary data");
        let data_len = data.len();
        let stream_fn = move || {
            let stream_data = std::io::Result::Ok(data.clone());
            futures::stream::once(async move { stream_data })
        };

        let err = integration
            .put(&location, stream_fn, Some(data_len))
            .await
            .unwrap_err();

        if let ObjectStoreError::AwsObjectStoreError {
            source:
                Error::UnableToPutData {
                    source,
                    bucket,
                    location,
                },
        } = err
        {
            assert!(matches!(source, rusoto_core::RusotoError::Unknown(_)));
            assert_eq!(bucket, config.bucket);
            assert_eq!(location, NON_EXISTENT_NAME);
        } else {
            panic!("unexpected error type: {:?}", err);
        }
    }

    #[tokio::test]
    async fn s3_test_put_nonexistent_bucket() {
        let mut config = maybe_skip_integration!();
        config.bucket = NON_EXISTENT_NAME.into();

        let integration = ObjectStore::new_amazon_s3(
            Some(config.access_key_id),
            Some(config.secret_access_key),
            config.region,
            &config.bucket,
            config.endpoint,
            config.token,
        )
        .expect("Valid S3 config");

        let mut location = integration.new_path();
        location.set_file_name(NON_EXISTENT_NAME);

        let data = Bytes::from("arbitrary data");
        let data_len = data.len();

        let stream_fn = move || {
            let stream_data = std::io::Result::Ok(data.clone());
            futures::stream::once(async move { stream_data })
        };

        let err = integration
            .put(&location, stream_fn, Some(data_len))
            .await
            .unwrap_err();

        if let ObjectStoreError::AwsObjectStoreError {
            source:
                Error::UnableToPutData {
                    source,
                    bucket,
                    location,
                },
        } = err
        {
            assert!(matches!(source, rusoto_core::RusotoError::Unknown(_)));
            assert_eq!(bucket, config.bucket);
            assert_eq!(location, NON_EXISTENT_NAME);
        } else {
            panic!("unexpected error type: {:?}", err);
        }
    }

    #[tokio::test]
    async fn s3_test_delete_nonexistent_location() {
        let config = maybe_skip_integration!();
        let integration = ObjectStore::new_amazon_s3(
            Some(config.access_key_id),
            Some(config.secret_access_key),
            config.region,
            config.bucket,
            config.endpoint,
            config.token,
        )
        .expect("Valid S3 config");

        let mut location = integration.new_path();
        location.set_file_name(NON_EXISTENT_NAME);

        let result = integration.delete(&location).await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn s3_test_delete_nonexistent_region() {
        let mut config = maybe_skip_integration!();
        // Assumes environment variables do not provide credentials to AWS US West 1
        config.region = "us-west-1".into();

        let integration = ObjectStore::new_amazon_s3(
            Some(config.access_key_id),
            Some(config.secret_access_key),
            config.region,
            &config.bucket,
            config.endpoint,
            config.token,
        )
        .expect("Valid S3 config");

        let mut location = integration.new_path();
        location.set_file_name(NON_EXISTENT_NAME);

        let err = integration.delete(&location).await.unwrap_err();
        if let ObjectStoreError::AwsObjectStoreError {
            source:
                Error::UnableToDeleteData {
                    source,
                    bucket,
                    location,
                },
        } = err
        {
            assert!(matches!(source, rusoto_core::RusotoError::Unknown(_)));
            assert_eq!(bucket, config.bucket);
            assert_eq!(location, NON_EXISTENT_NAME);
        } else {
            panic!("unexpected error type: {:?}", err);
        }
    }

    #[tokio::test]
    async fn s3_test_delete_nonexistent_bucket() {
        let mut config = maybe_skip_integration!();
        config.bucket = NON_EXISTENT_NAME.into();

        let integration = ObjectStore::new_amazon_s3(
            Some(config.access_key_id),
            Some(config.secret_access_key),
            config.region,
            &config.bucket,
            config.endpoint,
            config.token,
        )
        .expect("Valid S3 config");

        let mut location = integration.new_path();
        location.set_file_name(NON_EXISTENT_NAME);

        let err = integration.delete(&location).await.unwrap_err();
        if let ObjectStoreError::AwsObjectStoreError {
            source:
                Error::UnableToDeleteData {
                    source,
                    bucket,
                    location,
                },
        } = err
        {
            assert!(matches!(source, rusoto_core::RusotoError::Unknown(_)));
            assert_eq!(bucket, config.bucket);
            assert_eq!(location, NON_EXISTENT_NAME);
        } else {
            panic!("unexpected error type: {:?}", err);
        }
    }
}
