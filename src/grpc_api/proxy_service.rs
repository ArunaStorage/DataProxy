use std::sync::{Arc, RwLock};

use aruna_rust_api::api::dataproxy::services::v2::{
    dataproxy_service_server::DataproxyService, InitReplicationRequest, InitReplicationResponse,
    RequestReplicationRequest, RequestReplicationResponse,
};

use crate::caching::cache::Cache;

pub struct ProxyService {
    pub cache: Arc<RwLock<Cache>>,
}

impl ProxyService {
    pub fn _new(cache: Arc<RwLock<Cache>>) -> Self {
        Self { cache }
    }
}

#[tonic::async_trait]
impl DataproxyService for ProxyService {
    /// RequestReplication
    ///
    /// Status: BETA
    ///
    /// Creates a replication request
    async fn request_replication(
        &self,
        _request: tonic::Request<RequestReplicationRequest>,
    ) -> Result<tonic::Response<RequestReplicationResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("Not implemented"))
    }
    /// InitReplication
    ///
    /// Status: BETA
    ///
    /// Provides the necessary url to init replication
    async fn init_replication(
        &self,
        _request: tonic::Request<InitReplicationRequest>,
    ) -> Result<tonic::Response<InitReplicationResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("Not implemented"))
    }
}
