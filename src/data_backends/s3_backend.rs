use anyhow::anyhow;
use anyhow::Result;
use async_channel::{Receiver, Sender};
use async_trait::async_trait;
use aws_sdk_s3::{
    config::Region,
    primitives::ByteStream,
    types::{CompletedMultipartUpload, CompletedPart},
    Client,
};
use tokio_stream::StreamExt;

use super::storage_backend::Location;
use super::storage_backend::PartETag;
use super::storage_backend::StorageBackend;

#[derive(Debug, Clone)]
pub struct S3Backend {
    pub s3_client: Client,
}

impl S3Backend {
    pub async fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let s3_endpoint = dotenvy::var("AWS_S3_HOST").unwrap();

        let config = aws_config::load_from_env().await;
        let s3_config = aws_sdk_s3::config::Builder::from(&config)
            .region(Region::new("RegionOne"))
            .endpoint_url(&s3_endpoint)
            .build();

        let s3_client = aws_sdk_s3::Client::from_conf(s3_config);

        let handler = S3Backend { s3_client };
        Ok(handler)
    }
}

// Data backend for an S3 based storage.
#[async_trait]
impl StorageBackend for S3Backend {
    // Uploads a single object in chunks
    // Objects are uploaded in chunks that come from a channel to allow modification in the data middleware
    // The receiver can directly will be wrapped and will then be directly passed into the s3 client
    async fn put_object(
        &self,
        recv: Receiver<Result<bytes::Bytes>>,
        location: Location,
        content_len: i64,
    ) -> Result<()> {
        self.check_and_create_bucket(location.bucket.clone())
            .await?;

        let hyper_body = hyper::Body::wrap_stream(recv);
        let bytestream = ByteStream::from(hyper_body);

        match self
            .s3_client
            .put_object()
            .set_bucket(Some(location.bucket))
            .set_key(Some(location.path))
            .set_content_length(Some(content_len))
            .body(bytestream)
            .send()
            .await
        {
            Ok(_) => {}
            Err(err) => {
                log::error!("{}", err);
                return Err(err.into());
            }
        }

        Ok(())
    }

    // Downloads the given object from the s3 storage
    // The body is wrapped into an async reader and reads the data in chunks.
    // The chunks are then transfered into the sender.
    async fn get_object(
        &self,
        location: Location,
        range: Option<String>,
        sender: Sender<Result<bytes::Bytes, Box<dyn std::error::Error + Send + Sync>>>,
    ) -> Result<()> {
        let object = self
            .s3_client
            .get_object()
            .set_bucket(Some(location.bucket))
            .set_key(Some(location.path))
            .set_range(range);

        let mut object_request = match object.send().await {
            Ok(value) => value,
            Err(err) => {
                log::error!("{}", err);
                return Err(err.into());
            }
        };

        while let Some(bytes) = object_request.body.next().await {
            sender.send(Ok(bytes?)).await?;
        }
        return Ok(());
    }

    async fn head_object(&self, location: Location) -> Result<i64> {
        let object = self
            .s3_client
            .head_object()
            .set_bucket(Some(location.bucket))
            .set_key(Some(location.path))
            .send()
            .await;

        Ok(object?.content_length())
    }

    // Initiates a multipart upload in s3 and returns the associated upload id.
    async fn init_multipart_upload(&self, location: Location) -> Result<String> {
        self.check_and_create_bucket(location.bucket.clone())
            .await?;

        let multipart = self
            .s3_client
            .create_multipart_upload()
            .set_bucket(Some(location.bucket))
            .set_key(Some(location.path))
            .send()
            .await?;

        return Ok(multipart.upload_id().unwrap().to_string());
    }

    async fn upload_multi_object(
        &self,
        recv: Receiver<Result<bytes::Bytes>>,
        location: Location,
        upload_id: String,
        content_len: i64,
        part_number: i32,
    ) -> Result<PartETag> {
        log::debug!("Submitted content-length was: {:#?}", content_len);
        let hyper_body = hyper::Body::wrap_stream(recv);
        let bytestream = ByteStream::from(hyper_body);

        let upload = self
            .s3_client
            .upload_part()
            .set_bucket(Some(location.bucket))
            .set_key(Some(location.path))
            .set_part_number(Some(part_number))
            .set_content_length(Some(content_len))
            .set_upload_id(Some(upload_id))
            .body(bytestream)
            .send()
            .await?;

        return Ok(PartETag {
            part_number: part_number as i64,
            etag: upload.e_tag.ok_or_else(|| anyhow!("Missing etag"))?,
        });
    }

    async fn finish_multipart_upload(
        &self,
        location: Location,
        parts: Vec<PartETag>,
        upload_id: String,
    ) -> Result<()> {
        let mut completed_parts = Vec::new();
        for etag in parts {
            let part_number = i32::try_from(etag.part_number)?;

            let completed_part = CompletedPart::builder()
                .e_tag(etag.etag.replace('-', ""))
                .part_number(part_number)
                .build();

            completed_parts.push(completed_part);
        }

        log::debug!("{:?}", completed_parts);

        self.s3_client
            .complete_multipart_upload()
            .bucket(location.bucket)
            .key(location.path)
            .upload_id(upload_id)
            .multipart_upload(
                CompletedMultipartUpload::builder()
                    .set_parts(Some(completed_parts))
                    .build(),
            )
            .send()
            .await?;

        return Ok(());
    }

    async fn create_bucket(&self, bucket: String) -> Result<()> {
        self.check_and_create_bucket(bucket).await
    }

    /// Delete a object from the storage system
    /// # Arguments
    /// * `location` - The location of the object
    async fn delete_object(&self, location: Location) -> Result<()> {
        self.s3_client
            .delete_object()
            .bucket(location.bucket)
            .key(location.path)
            .send()
            .await?;
        Ok(())
    }
}

impl S3Backend {
    pub async fn check_and_create_bucket(&self, bucket: String) -> Result<()> {
        match self
            .s3_client
            .get_bucket_location()
            .bucket(bucket.clone())
            .send()
            .await
        {
            Ok(_) => Ok(()),
            Err(_) => match self.s3_client.create_bucket().bucket(bucket).send().await {
                Ok(_) => Ok(()),
                Err(err) => {
                    log::error!("{}", err);
                    Err(err.into())
                }
            },
        }
    }
}