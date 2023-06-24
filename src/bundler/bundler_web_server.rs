use crate::backends::storage_backend::StorageBackend;
use anyhow::Result;
use aruna_file::{
    streamreadwrite::ArunaStreamReadWriter,
    transformer::{FileContext, ReadWriter},
    transformers::{
        decrypt::ChaCha20Dec, gzip_comp::GzipEnc, hyper_sink::HyperSink, tar::TarEnc,
        zstd_decomp::ZstdDec,
    },
};
use axum::{
    error_handling::HandleErrorLayer,
    extract::{Path, State},
    response::{IntoResponse, Response},
    routing::get,
    BoxError, Router,
};
use hyper::StatusCode;
use std::{sync::Arc, time::Duration};
use tokio::sync::Mutex;
use tower::{buffer::BufferLayer, limit::RateLimitLayer, ServiceBuilder};

use super::bundler::Bundler;

struct RequestError(anyhow::Error);

pub async fn run_axum(
    bundler_addr: String,
    bundler: Arc<Mutex<Bundler>>,
    backend: Arc<Box<dyn StorageBackend>>,
) -> Result<()> {
    // build our application with a route
    let app = Router::new()
        // `GET /` goes to `root`
        .route("/:bundle_id/:file_name", get(get_bundle))
        .with_state((bundler.clone(), backend))
        .layer(
            ServiceBuilder::new()
                .layer(HandleErrorLayer::new(|err: BoxError| async move {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("Unhandled error: {}", err),
                    )
                }))
                .layer(BufferLayer::new(1024))
                .layer(RateLimitLayer::new(5, Duration::from_secs(1))),
        );

    // Axum Server
    // run our app with hyper, listening globally on port 3000
    axum::Server::bind(&bundler_addr.parse()?)
        .serve(app.into_make_service())
        .await?;

    Ok(())
}

async fn get_bundle(
    State((bundler, backend)): State<(Arc<Mutex<Bundler>>, Arc<Box<dyn StorageBackend>>)>,
    Path((bundle_id, file_name)): Path<(String, String)>,
) -> Result<impl IntoResponse, RequestError> {
    if !file_name.ends_with(".tar.gz") {
        return Err(RequestError(anyhow::anyhow!(
            "Only .tar.gz is currently supported"
        )));
    }
    let bundler_store = &bundler.lock().await.bundle_store;
    let bundles = bundler_store.get(&bundle_id);
    let (tx, rx) = async_channel::bounded(10);
    let (file_info_sender, file_info_receiver) = async_channel::bounded(10);
    match bundles {
        Some(b) => match b {
            Some(bundle) => {
                // TODO: check if bundle is expired
                let cloned_bundle = bundle.clone();
                tokio::spawn(async move {
                    let len = cloned_bundle.object_refs.len();
                    for (i, object_ref) in cloned_bundle.object_refs.into_iter().enumerate() {
                        let object_info = object_ref.object_info.unwrap_or_default();
                        let object_location =
                            object_ref.object_location.clone().unwrap_or_default();
                        file_info_sender
                            .send((
                                FileContext {
                                    file_name: object_info.filename,
                                    file_size: object_info.content_len as u64,
                                    file_path: Some(object_ref.sub_path),
                                    skip_decompression: object_location.is_compressed,
                                    skip_decryption: object_location.is_encrypted,
                                    encryption_key: object_ref
                                        .object_location
                                        .map(|e| e.encryption_key.into_bytes()),
                                    ..Default::default()
                                },
                                len == i,
                            ))
                            .await?;

                        backend
                            .get_object(object_location, None, tx.clone())
                            .await?;
                    }
                    Ok::<(), anyhow::Error>(())
                });
            }
            None => return Err(anyhow::anyhow!("Bundle is still initializing!").into()),
        },
        None => return Err(anyhow::anyhow!("Invalid bundle!").into()),
    }

    let (sink, body) = HyperSink::new();

    tokio::spawn(async move {
        let mut aruna_stream_writer = ArunaStreamReadWriter::new_with_sink(rx, sink)
            .add_transformer(ZstdDec::new())
            .add_transformer(ChaCha20Dec::new(None)?)
            .add_transformer(TarEnc::new())
            .add_transformer(GzipEnc::new());

        aruna_stream_writer
            .add_file_context_receiver(file_info_receiver)
            .await?;
        aruna_stream_writer.process().await
    });
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(
            "Content-Disposition",
            format!("attachment; filename=\"{file_name}\""),
        )
        .body(body)?)
}

// Tell axum how to convert `RequestError` into a response.
impl IntoResponse for RequestError {
    fn into_response(self) -> Response {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Something went wrong: {}", self.0),
        )
            .into_response()
    }
}

// This enables using `?` on functions that return `Result<_, anyhow::Error>` to turn them into
// `Result<_, AppError>`. That way you don't need to do that manually.
impl<E> From<E> for RequestError
where
    E: Into<anyhow::Error>,
{
    fn from(err: E) -> Self {
        Self(err.into())
    }
}
