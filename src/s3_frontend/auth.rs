use crate::caching::cache::Cache;
use s3s::{
    auth::{S3Auth, S3AuthContext, SecretKey},
    s3_error, S3Result,
};
use std::sync::Arc;
use tracing::debug;

/// Aruna authprovider
pub struct AuthProvider {
    cache: Arc<Cache>,
}

impl AuthProvider {
    #[tracing::instrument(level = "trace", skip(cache))]
    pub async fn new(cache: Arc<Cache>) -> Self {
        Self { cache }
    }
}

#[async_trait::async_trait]
impl S3Auth for AuthProvider {
    #[tracing::instrument(level = "trace", skip(self, access_key))]
    async fn get_secret_key(&self, access_key: &str) -> S3Result<SecretKey> {
        debug!(access_key);
        let secret = self
            .cache
            .get_secret(access_key)
            .await
            .map_err(|_| s3_error!(AccessDenied, "Invalid access key"))?;
        Ok(secret)
    }

    #[tracing::instrument(level = "trace", skip(self, cx))]
    async fn check_access(&self, cx: &mut S3AuthContext<'_>) -> S3Result<()> {
        debug!(path = ?cx.s3_path());

        match self.cache.auth.read().await.as_ref() {
            Some(auth) => {
                let result = auth
                    .check_access(cx.credentials(), cx.method(), cx.s3_path(), cx.headers())
                    .await?;

                cx.extensions_mut().insert(result);
                Ok(())
            }
            None => Ok(()),
        }
    }
}
