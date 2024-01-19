use crate::{
    caching::cache::Cache, data_backends::storage_backend::StorageBackend,
    s3_frontend::utils::buffered_s3_sink::BufferedS3Sink, structs::ObjectLocation, trace_err,
};
use ahash::{HashSet, RandomState};
use anyhow::{anyhow, Result};
use aruna_file::{
    streamreadwrite::ArunaStreamReadWriter,
    transformer::ReadWriter,
    transformers::{
        encrypt::ChaCha20Enc, footer::FooterGenerator, hashing_transformer::HashingTransformer,
        size_probe::SizeProbe,
    },
};
use aruna_rust_api::api::dataproxy::services::v2::{ObjectInfo, ReplicationStatus};
use aruna_rust_api::api::{
    dataproxy::services::v2::{
        error_message, pull_replication_request::Message,
        pull_replication_response::Message as ResponseMessage, Chunk, ChunkAckMessage,
        InfoAckMessage, InitMessage, PullReplicationRequest, RetryChunkMessage,
    },
    storage::services::v2::UpdateReplicationStatusRequest,
};
use async_channel::{Receiver, Sender};
use dashmap::DashMap;
use diesel_ulid::DieselUlid;
use md5::{Digest, Md5};
use sha2::Sha256;
use std::{str::FromStr, sync::Arc};
use tokio::pin;
use tracing::trace;

pub struct ReplicationMessage {
    pub direction: Direction,
    pub endpoint_id: DieselUlid,
}

#[derive(Debug, Hash, Eq, PartialEq, Clone)]
pub enum Direction {
    Push(DieselUlid),
    Pull(DieselUlid),
}

#[derive(Debug, Hash, Eq, PartialEq, Clone)]
pub enum RcvSync {
    Info(DieselUlid, i64),  // object_id and how many chunks
    Chunk(DieselUlid, i64), // object_id and which chunk
    Finish,
}
pub struct DataChunk {
    pub object_id: String,
    pub chunk_idx: i64,
    pub data: Vec<u8>,
    pub checksum: String,
}

pub struct ReplicationHandler {
    pub receiver: Receiver<ReplicationMessage>,
    pub backend: Arc<Box<dyn StorageBackend>>,
    pub cache: Arc<Cache>,
    pub self_id: String,
}

type ObjectHandler = Arc<
    DashMap<
        String,
        (
            Sender<DataChunk>,
            Receiver<DataChunk>,
            i64,
            Vec<u8>,
            i64,
            bool,
        ),
        RandomState,
    >,
>;
impl ReplicationHandler {
    #[tracing::instrument(level = "trace", skip(cache, backend, receiver))]
    pub fn new(
        receiver: Receiver<ReplicationMessage>,
        backend: Arc<Box<dyn StorageBackend>>,
        self_id: String,
        cache: Arc<Cache>,
    ) -> Self {
        Self {
            receiver,
            backend,
            self_id,
            cache,
        }
    }

    #[tracing::instrument(level = "trace", skip(self))]
    pub async fn run(self) -> Result<()> {
        // Has EndpointID: [Pull(object_id), Pull(object_id) ,...]
        let queue: Arc<DashMap<DieselUlid, Vec<Direction>, RandomState>> =
            Arc::new(DashMap::default());

        // Push messages into DashMap for further processing
        let queue_clone = queue.clone();
        let receiver = self.receiver.clone();
        let recieve = tokio::spawn(async move {
            while let Ok(ReplicationMessage {
                direction,
                endpoint_id,
            }) = receiver.recv().await
            {
                if queue_clone.contains_key(&endpoint_id) {
                    queue_clone.alter(&endpoint_id, |_, mut objects| {
                        objects.push(direction.clone());
                        objects
                    });
                } else {
                    queue_clone.insert(endpoint_id, vec![direction.clone()]);
                }
                trace!(?queue_clone);
            }
        });

        // Proccess DashMap entries in batches
        let process: tokio::task::JoinHandle<Result<()>> = tokio::spawn(async move {
            loop {
                // Process batches every 30 seconds
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                let batch = queue.clone();

                let result = trace_err!(self.process(batch).await)?;
                // Remove processed entries from shared map
                for (id, objects) in result {
                    queue.alter(&id, |_, directions| {
                        directions
                            .into_iter()
                            .filter(|dir| !objects.contains(dir))
                            .collect::<Vec<Direction>>()
                            .clone()
                    });
                    let mut is_empty = false;
                    if let Some(entry) = queue.get(&id) {
                        if entry.is_empty() {
                            is_empty = true;
                        }
                    }
                    if is_empty {
                        queue.remove(&id);
                    }
                }
            }
        });
        // Run both tasks simultaneously
        let (_, result) = trace_err!(tokio::try_join!(recieve, process))?;
        result?;
        Ok(())
    }

    #[tracing::instrument(level = "trace", skip(self))]
    // TODO
    // - Push logic
    async fn process(
        &self,
        batch: Arc<DashMap<DieselUlid, Vec<Direction>, RandomState>>,
    ) -> Result<Vec<(DieselUlid, Vec<Direction>)>> {
        // Vec for collecting all processed and finished endpoint batches
        let mut result = Vec::new();

        // Iterates over each endpoint
        for endpoint in batch.iter() {
            let self_id = self.self_id.clone();
            // Collects all objects for each direction
            let pull: Vec<DieselUlid> = endpoint
                .iter()
                .filter_map(|object| match object {
                    Direction::Pull(id) => Some(*id),
                    Direction::Push(_) => None,
                })
                .collect();
            // TODO: Push is currently not implemented
            let _push: Vec<DieselUlid> = endpoint
                .iter()
                .filter_map(|object| match object {
                    Direction::Push(id) => Some(*id),
                    Direction::Pull(_) => None,
                })
                .collect();
            // This is the initial message for the data transmission stream
            let init_request = PullReplicationRequest {
                message: Some(Message::InitMessage(InitMessage {
                    dataproxy_id: self_id.clone(),
                    object_ids: pull.iter().map(|o| o.to_string()).collect(),
                })),
            };
            if let Some(query_handler) = self.cache.aruna_client.read().await.as_ref() {
                let endpoint_id = *endpoint.key();
                // This query handler returns a channel for sending messages into the input stream
                // and the response stream
                let (request_sender, mut response_stream) = trace_err!(
                    query_handler
                        .pull_replication(init_request, endpoint_id)
                        .await
                )?;

                // This is the init message for object proccessing
                let (start_sender, start_receiver) = async_channel::bounded(1);
                // This channel is used to collect all proccessed objects and chunks
                let (sync_sender, sync_receiver) = async_channel::bounded(100);
                // This channel is only used to transmit the sync result to compare
                // recieved vs requested objects
                let (finish_sender, finish_receiver) = async_channel::bounded(1);

                // This map collects for each object_id a channel for datatransmission
                // TODO: This could be used to make parallel requests later
                let object_handler_map: ObjectHandler = Arc::new(DashMap::default());
                for object in pull {
                    trace_err!(
                        query_handler
                            .update_replication_status(UpdateReplicationStatusRequest {
                                object_id: object.to_string(),
                                endpoint_id: self_id.clone(),
                                status: ReplicationStatus::Running as i32,
                            })
                            .await
                    )?;
                    let (object_sdx, object_rcv) = async_channel::bounded(100);
                    object_handler_map.insert(
                        object.to_string(),
                        (
                            object_sdx.clone(),
                            object_rcv.clone(),
                            Default::default(),
                            Default::default(),
                            Default::default(),
                            false,
                        ),
                    );
                }

                trace!(?object_handler_map);
                // Response handler:
                // This is used to handle all requests and responses
                // to the other dataproxy
                let data_map = object_handler_map.clone();
                let sync_sender_clone = sync_sender.clone();
                let request_sender_clone = request_sender.clone();
                tokio::spawn(async move {
                    let mut counter = 0;
                    while let Some(response) = response_stream.message().await? {
                        match response.message {
                            Some(ResponseMessage::ObjectInfo(ObjectInfo {
                                object_id,
                                chunks,
                                block_list,
                                raw_size,
                                ..
                            })) => {
                                counter += 1;

                                // If ObjectInfo is send, a init msg is collected in sync ...
                                let id = trace_err!(DieselUlid::from_str(&object_id))?;
                                trace_err!(
                                    sync_sender_clone.send(RcvSync::Info(id, chunks)).await
                                )?;
                                // .. and a datachannel is created
                                let block_list = trace_err!(block_list
                                    .iter()
                                    .map(|block| u8::try_from(*block)
                                        .map_err(|_| anyhow!("Could not convert blocklist to u8")))
                                    .collect::<Result<Vec<u8>>>())?
                                .clone();
                                // and stored in object_handler_map ...
                                {
                                    data_map.alter(&object_id, |_, (sdx, rcv, ..)| {
                                        (sdx, rcv, chunks, block_list.clone(), raw_size, true)
                                    });
                                }
                                // ... and then ObjectInfo gets acknowledged
                                trace_err!(
                                    request_sender_clone
                                        .send(PullReplicationRequest {
                                            message: Some(Message::InfoAckMessage(
                                                InfoAckMessage { object_id }
                                            )),
                                        })
                                        .await
                                )?;
                                // This is needed to keep backend task in sync
                                if counter == 1 {
                                    trace_err!(start_sender.send(true).await)?;
                                }
                            }
                            Some(ResponseMessage::Chunk(Chunk {
                                object_id,
                                chunk_idx,
                                data,
                                checksum,
                            })) => {
                                trace!("Received chunk");
                                // If an entry is created inside the object_handler_map ...
                                if let Some(entry) = data_map.get(&object_id) {
                                    // Chunks get processed
                                    let chunk = DataChunk {
                                        object_id: object_id.clone(),
                                        chunk_idx,
                                        data,
                                        checksum,
                                    };
                                    entry.0.send(chunk).await?;
                                    let id = DieselUlid::from_str(&object_id)?;
                                    // Message is send to sync
                                    trace_err!(
                                        sync_sender_clone.send(RcvSync::Chunk(id, chunk_idx)).await
                                    )?;
                                    // Message is acknowledged
                                    trace_err!(
                                        request_sender_clone
                                            .send(PullReplicationRequest {
                                                message: Some(Message::ChunkAckMessage(
                                                    ChunkAckMessage {
                                                        object_id,
                                                        chunk_idx,
                                                    },
                                                )),
                                            })
                                            .await
                                    )?;
                                } else {
                                    // If no entry is found, ObjectInfo was not send
                                    trace_err!(request_sender_clone
                                        .send(
                                            PullReplicationRequest {
                                                message: Some(
                                                    Message::ErrorMessage(
                                                        aruna_rust_api::api::dataproxy::services::v2::ErrorMessage {
                                                            error: Some(
                                                                error_message::Error::RetryObjectId(
                                                                    object_id,
                                                                )
                                                            )
                                                        }
                                                    )
                                                )
                                            }
                                        )
                                        .await)?;
                                }
                            }
                            Some(ResponseMessage::FinishMessage(..)) => return Ok(()),
                            None => {
                                return Err(anyhow!(
                                    "No message provided in PullReplicationResponse"
                                ))
                            }
                        }
                    }
                    Ok::<(), anyhow::Error>(())
                });

                // Sync handler
                tokio::spawn(async move {
                    let mut sync = HashSet::default();
                    // Every InfoMsg and ChunkMsg is stored
                    while let Ok(msg) = sync_receiver.recv().await {
                        match msg {
                            info @ RcvSync::Info(..) => {
                                sync.insert(info);
                            }
                            chunk @ RcvSync::Chunk(..) => {
                                sync.insert(chunk);
                            }
                            // If finish is called, all stored messages will be returned
                            RcvSync::Finish => {
                                trace_err!(finish_sender.send(sync.clone()).await)?;
                            }
                        }
                    }
                    Ok::<(), anyhow::Error>(())
                });

                // Process each object
                let cache = self.cache.clone();
                let backend = self.backend.clone();
                let query_handler = query_handler.clone();
                let request_sdx = request_sender.clone();
                let finished_objects: Arc<DashMap<Direction, bool, RandomState>> =
                    Arc::new(DashMap::default()); // Syncs if object is already synced
                let finished_clone = finished_objects.clone();
                tokio::spawn(async move {
                    // For now, every entry of the object_handler_map is proccessed
                    // consecutively
                    while start_receiver.recv().await.is_ok() {
                        let mut batch_counter = 0;
                        loop {
                            batch_counter += 1;
                            let mut batch = Vec::new();
                            for entry in object_handler_map.iter() {
                                let (key, value) = entry.pair();
                                batch.push((key.clone(), value.clone()));
                            }
                            for entry in batch.clone() {
                                let (id, (_, rcv, chunks, blocklist, raw_size, synced)) = {
                                    let (key, value) = entry;
                                    (key.clone(), value.clone())
                                };
                                trace!("processing: {}", id);
                                let object_id = DieselUlid::from_str(&id)?;

                                // The object gets queried
                                let (object, location) = {
                                    let entry = trace_err!(cache
                                        .resources
                                        .get(&object_id)
                                        .ok_or_else(|| anyhow!("Object not found")))?;
                                    entry.clone()
                                };
                                trace!(?object);
                                // If no location is found, a new one is created
                                let mut location = if location.is_some() {
                                    // TODO:
                                    // - Skip if object was already synced
                                    finished_clone.insert(Direction::Pull(object_id), true);
                                    object_handler_map.remove(&id);
                                    continue;
                                } else if !synced {
                                    continue;
                                } else {
                                    trace_err!(
                                        backend
                                            .initialize_location(
                                                &object,
                                                Some(raw_size),
                                                None,
                                                false
                                            )
                                            .await
                                    )?
                                };
                                trace!("Load into backend");
                                // Send Chunks get processed
                                trace_err!(
                                    ReplicationHandler::load_into_backend(
                                        rcv.clone(),
                                        request_sdx.clone(),
                                        sync_sender.clone(),
                                        &mut location,
                                        backend.clone(),
                                        chunks,
                                        blocklist.clone(),
                                    )
                                    .await
                                )?;

                                trace!("Upsert object");
                                // TODO: This should probably happen after checking if all chunks were processed
                                // Sync with cache and db
                                let location: Option<ObjectLocation> = Some(location.clone());
                                trace_err!(
                                    cache.upsert_object(object.clone(), location.clone()).await
                                )?;

                                trace!("Update status");
                                // Send UpdateStatus to server
                                trace_err!(
                                    query_handler
                                        .update_replication_status(UpdateReplicationStatusRequest {
                                            object_id: object.id.to_string(),
                                            endpoint_id: self_id.clone(),
                                            status: ReplicationStatus::Finished as i32,
                                        })
                                        .await
                                )?;
                                {
                                    object_handler_map.remove(&id);
                                }
                                trace!( msg="Removed entry from map", map = ?object_handler_map);
                            }
                            if object_handler_map.is_empty() {
                                trace!("Object handler map is empty, finishing replication... ");
                                // Check if all chunks found in object infos are also processed
                                trace_err!(sync_sender.send(RcvSync::Finish).await)?;
                                break;
                            } else if batch_counter > 20 {
                                // Exit after arbitrary number of tries
                                trace_err!(request_sdx.send(
                                    PullReplicationRequest {
                                                    message: Some(
                                                        Message::ErrorMessage(
                                                            aruna_rust_api::api::dataproxy::services::v2::ErrorMessage {
                                                                error: Some(
                                                                    error_message::Error::Abort(aruna_rust_api::api::dataproxy::services::v2::Empty{})
                                                                )
                                                            }
                                                        )
                                                    )
                                                }
                                        ).await)?;
                                trace_err!(sync_sender.send(RcvSync::Finish).await)?;
                                break;
                            }
                        }
                    }

                    Ok::<(), anyhow::Error>(())
                });

                //TODO:
                // - If error, maybe set endpoint_status for each failed object to Error?
                // -> Then we do not have to do this additional check while loading into backend
                // -> User initiated replications then need to be implemented
                //let mut finished_objects = Vec::new();
                while let Ok(finished) = finish_receiver.recv().await {
                    // Collection ObjectInfo
                    let inits = finished.iter().filter_map(|msg| match msg {
                        RcvSync::Info(object_id, chunks) => Some((object_id, chunks)),
                        _ => None,
                    });
                    // For each object, check if all chunks were processed
                    for (object_id, chunks) in inits {
                        let collected = finished
                            .iter()
                            .filter_map(|msg| match msg {
                                RcvSync::Chunk(id, idx) if object_id == id => Some(idx),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .len();
                        if *chunks as usize != collected {
                            // Look if already synced
                            if let Some(object) = finished_objects.get(&Direction::Pull(*object_id))
                            {
                                let (_, is_synced) = object.pair();
                                if *is_synced {
                                    continue;
                                } else {
                                    trace!("Not all chunks received, aborting ...");
                                    // Send abort message if not all chunks were processed
                                    trace_err!(request_sender
                                            .send(
                                                PullReplicationRequest {
                                                    message: Some(
                                                        Message::ErrorMessage(
                                                            aruna_rust_api::api::dataproxy::services::v2::ErrorMessage {
                                                                error: Some(
                                                                    error_message::Error::Abort(aruna_rust_api::api::dataproxy::services::v2::Empty{})
                                                                )
                                                            }
                                                        )
                                                    )
                                                }
                                            )
                                            .await)?;
                                    return Err(anyhow!("Not all chunks recieved, aborting sync"));
                                }
                            } else {
                                trace!("Not all chunks received, aborting ...");
                                // Send abort message if not all chunks were processed
                                trace_err!(request_sender
                                            .send(
                                                PullReplicationRequest {
                                                    message: Some(
                                                        Message::ErrorMessage(
                                                            aruna_rust_api::api::dataproxy::services::v2::ErrorMessage {
                                                                error: Some(
                                                                    error_message::Error::Abort(aruna_rust_api::api::dataproxy::services::v2::Empty{})
                                                                )
                                                            }
                                                        )
                                                    )
                                                }
                                            )
                                            .await)?;
                                return Err(anyhow!("Not all chunks recieved, aborting sync"));
                            }
                        }
                        finished_objects.insert(Direction::Pull(*object_id), false);
                    }
                    // Send finish message if everything was processed
                    trace_err!(
                        request_sender
                            .send(PullReplicationRequest {
                                message: Some(Message::FinishMessage(
                                    aruna_rust_api::api::dataproxy::services::v2::Empty {}
                                ))
                            })
                            .await
                    )?;
                }
                trace!("Writing results");
                if let Some(map) = Arc::into_inner(finished_objects) {
                    let (objects, _): (Vec<Direction>, Vec<bool>) = map.into_iter().unzip();
                    let finished_objects = Vec::from_iter(objects);
                    result.push((endpoint_id, finished_objects));
                    // It is not that much of a problem if this does not get written, because it
                    // will be skipped when the next batch gets processed by the replication
                    // handler
                };
            };
            // Write endpoint into results
            //result.push(*endpoint.key());
        }

        trace!(?result);
        Ok(result)
    }
    async fn load_into_backend(
        data_receiver: Receiver<DataChunk>,
        stream_sender: tokio::sync::mpsc::Sender<PullReplicationRequest>,
        sync_sender: Sender<RcvSync>,
        location: &mut ObjectLocation,
        backend: Arc<Box<dyn StorageBackend>>,
        max_chunks: i64,
        blocklist: Vec<u8>,
    ) -> Result<()> {
        let mut expected = 0;
        let mut retry_counter = 0;

        trace!("Starting chunk processing");
        let (data_sender, data_stream) = async_channel::bounded(100);
        tokio::spawn(async move {
            while let Ok(data) = data_receiver.recv().await {
                let trace_message = format!(
                    "Recieved chunk with idx {:?} for object with id {:?} and size {}, expected {}",
                    data.chunk_idx,
                    data.object_id,
                    data.data.len(),
                    expected,
                );
                trace!(trace_message);
                let chunk = bytes::Bytes::from_iter(data.data.into_iter());
                // Check if chunk is missing
                let idx = data.chunk_idx;

                if idx != expected {
                    if retry_counter > 5 {
                        trace!("Exceeded retries");
                        return Err(anyhow!(
                            "Exceeded retries for chunk because of skipped chunk"
                        ));
                    } else {
                        // TODO:
                        // RetryChunk message
                        trace!("MissingChunk: Retry chunk {}", expected);
                        trace_err!(stream_sender
                            .send(PullReplicationRequest {
                                message: Some(Message::ErrorMessage(
                                    aruna_rust_api::api::dataproxy::services::v2::ErrorMessage {
                                        error: Some(error_message::Error::RetryChunk(
                                            RetryChunkMessage {
                                                object_id: data.object_id,
                                                chunk_idx: expected, // TODO: previous
                                            },
                                        )),
                                    },
                                )),
                            })
                            .await)?;
                        retry_counter += 1;
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        continue;
                    }
                } else {
                    expected += 1;
                };

                // Check checksum of chunk:
                let hash = data.checksum;
                // - create a Md5 hasher instance
                let mut hasher = Md5::new();
                // - process input message
                hasher.update(&chunk);
                // - acquire hash digest in the form of GenericArray,
                //   which in this case is equivalent to [u8; 16]
                let result = hasher.finalize();
                let calculated_hash = hex::encode(result);
                if calculated_hash != hash {
                    if retry_counter > 5 {
                        trace!("Exceeded retries");
                        return Err(anyhow!(
                            "Exceeded retries for chunk because of differing checksums"
                        ));
                    } else {
                        // TODO:
                        // RetryChunk message
                        trace!("HashError: Retry chunk {}", expected);
                        trace_err!(stream_sender
                            .send(PullReplicationRequest {
                                message: Some(Message::ErrorMessage(
                                    aruna_rust_api::api::dataproxy::services::v2::ErrorMessage {
                                        error: Some(error_message::Error::RetryChunk(
                                            RetryChunkMessage {
                                                object_id: data.object_id,
                                                chunk_idx: data.chunk_idx,
                                            },
                                        )),
                                    },
                                )),
                            })
                            .await)?;
                        retry_counter += 1;
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        continue;
                    }
                }

                trace_err!(data_sender.send(Ok(chunk)).await)?;

                // Message is send to sync
                trace_err!(
                    sync_sender
                        .send(RcvSync::Chunk(
                            trace_err!(DieselUlid::from_str(&data.object_id))?,
                            data.chunk_idx
                        ))
                        .await
                )?;
                if (idx + 1) == max_chunks {
                    return Ok(());
                }
            }
            Ok::<(), anyhow::Error>(())
        });

        // Initialize hashing transformers
        let (final_sha_trans, final_sha_recv) = HashingTransformer::new(Sha256::new());
        let (final_size_trans, final_size_recv) = SizeProbe::new();

        trace!("Starting ArunaStreamReadWriter taks");
        let location_clone = location.clone();
        let _ = trace_err!(
            tokio::spawn(async move {
                pin!(data_stream);

                trace!(?max_chunks);
                let mut awr = ArunaStreamReadWriter::new_with_sink(
                    data_stream,
                    BufferedS3Sink::new(
                        backend.clone(),
                        location_clone.clone(),
                        None,
                        None,
                        false,
                        None,
                        false,
                    )
                    .0,
                );

                if location_clone.raw_content_len > 5242880 + 80 * 28 {
                    trace!("adding footer generator");
                    awr = awr.add_transformer(FooterGenerator::new(Some(blocklist.clone())));
                }

                if let Some(enc_key) = &location_clone.encryption_key {
                    trace!("adding encryption transformer");
                    awr = awr.add_transformer(trace_err!(ChaCha20Enc::new(
                        true,
                        enc_key.to_string().into_bytes()
                    ))?);
                }

                trace!("Adding size and hash transformer");
                awr = awr.add_transformer(final_sha_trans);
                awr = awr.add_transformer(final_size_trans);
                trace_err!(awr.process().await)?;

                Ok::<(), anyhow::Error>(())
            })
            .await
        )?;

        // Fetch calculated hashes
        trace!("fetching hashes");
        let sha_final: String = trace_err!(final_sha_recv.try_recv())?;
        //let initial_size: u64 = trace_err!(initial_size_recv.try_recv())?;
        let final_size: u64 = trace_err!(final_size_recv.try_recv())?;

        // Put infos into location
        location.disk_content_len = final_size as i64;
        location.disk_hash = Some(sha_final.clone());

        Ok(())
    }
}