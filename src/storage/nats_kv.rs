//! Storage engine backed by NATS Kv
//!
//! This storage engine enables storing any WADM related information using NATS Kv as a backend

use async_trait::async_trait;
use std::path::PathBuf;

use async_nats::{
    jetstream::kv::Config as JetstreamKvConfig, jetstream::kv::Store as JetstreamKvStore,
    Client as NatsClient, ConnectOptions as NatsConnectOptions,
};

use tokio::fs::read as read_file;

use derive_builder::Builder;
use semver::{BuildMetadata, Prerelease, Version};
use serde::ser::StdError;
use serde::{Deserialize, Serialize};
use serde_json::{from_slice, to_vec, Error as SerdeJsonError};

use super::{EngineMetadata, LatticeStorage, StorageEngineError, StorageError, StoreOptions};

use super::state::{LatticeState, LatticeStateBuilder, LatticeStateBuilderError};

use crate::storage::{Store, StoreRead, StoreReadWrite, StoreWrite, WithStateMetadata};

const STORAGE_ENGINE_NAME: &str = "nats_kv";
const STORAGE_ENGINE_VERSION: Version = Version {
    major: 0,
    minor: 1,
    patch: 0,
    pre: Prerelease::EMPTY,
    build: BuildMetadata::EMPTY,
};

////////////
// Engine //
////////////

#[derive(Debug)]
pub struct NatsKvStorageEngine {
    /// Configuration for the NATS KV storage engine
    config: NatsKvStorageConfig,

    /// NATS client
    client: NatsClient,
}

#[derive(Debug, Serialize, Deserialize, Builder)]
pub struct NatsKvStorageConfig {
    /// URL to the nats instance
    pub nats_url: String,

    /// Prefix to use with lattice buckets
    pub lattice_bucket_prefix: Option<String>,

    /// Authentication config for use with NATS
    pub auth: Option<NatsAuthConfig>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct NatsAuthConfig {
    pub creds_file: Option<String>,
    pub jwt_seed: Option<String>,
    pub jwt_path: Option<PathBuf>,
}

impl From<nkeys::error::Error> for StorageEngineError {
    fn from(err: nkeys::error::Error) -> StorageEngineError {
        StorageEngineError::Engine(format!("failed to process nkeys: {err}"))
    }
}

impl From<std::io::Error> for StorageEngineError {
    fn from(err: std::io::Error) -> StorageEngineError {
        StorageEngineError::Engine(format!("unexpected I/O error: {err}"))
    }
}

async fn build_nats_options(
    cfg: &NatsAuthConfig,
) -> Result<NatsConnectOptions, StorageEngineError> {
    // Use JWT + seed if present
    if let (Some(path), Some(seed)) = (&cfg.jwt_path, &cfg.jwt_seed) {
        let file_contents = read_file(path).await?;
        let jwt = String::from_utf8_lossy(&file_contents);
        let kp = std::sync::Arc::new(nkeys::KeyPair::from_seed(seed)?);
        return Ok(async_nats::ConnectOptions::with_jwt(
            jwt.into(),
            move |nonce| {
                let key_pair = kp.clone();
                async move { key_pair.sign(&nonce).map_err(async_nats::AuthError::new) }
            },
        ));
    }

    // Use creds file if present
    if let Some(path) = &cfg.creds_file {
        return NatsConnectOptions::with_credentials_file(path.clone().into())
            .await
            .map_err(|e| StorageEngineError::Engine(format!("Failed to connect NATS: {e}")));
    }

    Ok(NatsConnectOptions::default())
}

impl NatsKvStorageEngine {
    #[allow(dead_code)]
    pub async fn new(
        config: NatsKvStorageConfig,
    ) -> Result<NatsKvStorageEngine, StorageEngineError> {
        // Build NATS config
        let nats_config = match &config.auth {
            Some(ac) => build_nats_options(ac).await?,
            None => NatsConnectOptions::default(),
        };

        // Connect NATS
        let client = async_nats::connect_with_options(&config.nats_url, nats_config)
            .await
            .map_err(|e| StorageEngineError::Engine(format!("failed to connect: {e}")))?;

        Ok(NatsKvStorageEngine { config, client })
    }

    /// Build the top level bucket for the lattice
    fn build_lattice_bucket_name(&self, lattice_id: String) -> String {
        let mut bucket = format!("lattice_{lattice_id}");
        if let Some(prefix) = &self.config.lattice_bucket_prefix {
            bucket = format!("{prefix}_{bucket}");
        }
        bucket
    }

    /// Get or create the KV store for NATS jetstream
    async fn get_or_create_kv_store(
        &self,
        bucket: String,
    ) -> Result<JetstreamKvStore, StorageError> {
        // Build jetstream client
        let jetstream = async_nats::jetstream::new(self.client.clone());

        // Use existing KV bucket if already present
        if let Ok(kv) = jetstream.get_key_value(&bucket).await {
            return Ok(kv);
        }

        // Create Jetstream KV store
        let kv = jetstream
            .create_key_value(JetstreamKvConfig {
                bucket,
                history: 10,
                ..Default::default()
            })
            .await
            .map_err(|e| StorageError::Engine(format!("failed to create jetstream kv: {e}")))?;

        Ok(kv)
    }
}

#[async_trait]
impl EngineMetadata for NatsKvStorageEngine {
    async fn name() -> String {
        String::from(STORAGE_ENGINE_NAME)
    }

    async fn version() -> Version {
        STORAGE_ENGINE_VERSION
    }
}

#[async_trait]
impl Store for NatsKvStorageEngine {
    type State = LatticeState;
    type StateId = String;
    type StateUpdate = LatticeStateBuilder;

    async fn get(&self, id: String) -> Result<WithStateMetadata<LatticeState>, StorageError> {
        Ok(self.read(id).await?)
    }

    async fn store(
        &mut self,
        state: LatticeState,
        store_opts: StoreOptions,
    ) -> Result<WithStateMetadata<String>, StorageError> {
        Ok(self.write(&state, None, store_opts).await?)
    }

    // Delete an existing piece of state
    async fn delete(&self, id: String) -> Result<(), StorageError> {
        // Retrieve the kv store for a given lattice
        let bucket = self.build_lattice_bucket_name(id);
        let kv = self.get_or_create_kv_store(bucket.clone()).await?;

        let _ = kv.delete(bucket).await.map_err(|err| {
            StorageError::Engine(format!("failed to delete lattice state: {err}"))
        })?;

        Ok(())
    }
}

impl LatticeStorage for NatsKvStorageEngine {}

/////////////////////////////
// Storage Implementations //
/////////////////////////////

//////////////////////////
// Store - LatticeState //
//////////////////////////

impl From<SerdeJsonError> for StorageError {
    fn from(e: SerdeJsonError) -> Self {
        StorageError::Unknown(format!("de/serialization error occurred: {e}"))
    }
}

impl From<Box<dyn StdError + std::marker::Send + Sync>> for StorageError {
    fn from(e: Box<dyn StdError + std::marker::Send + Sync>) -> Self {
        StorageError::Unknown(format!("de/serialization error occurred: {e}"))
    }
}

impl From<LatticeStateBuilderError> for StorageError {
    fn from(e: LatticeStateBuilderError) -> Self {
        StorageError::Unknown(format!("failed to build lattice: {e}"))
    }
}

const LATTICE_BUCKET_STATE_KEY: &str = "state";

/// Implementation of lattice state storage for NATS KV
///
/// For every lattice this implementation stores the all lattice state under a single key
/// determined by LATTICE_BUCKET_STATE_KEY.
/// For example, a lattice bucket "lattice_1", you can find serialized lattice state at "lattice_1.state".
#[async_trait]
impl StoreRead<LatticeState, String> for NatsKvStorageEngine {
    async fn read(&self, id: String) -> Result<WithStateMetadata<LatticeState>, StorageError> {
        let bucket = self.build_lattice_bucket_name(id);
        let kv = self.get_or_create_kv_store(bucket.clone()).await?;

        let lattice;
        match kv.entry(LATTICE_BUCKET_STATE_KEY).await? {
            Some(entry) => {
                lattice = from_slice(&entry.value)?;
                Ok(WithStateMetadata {
                    state: lattice,
                    revision: Some(entry.revision),
                })
            }
            None => Err(StorageError::Engine(format!(
                "No lattice state in bucket [{bucket}]"
            ))),
        }
    }
}

/// Saves LatticeState to a NATS Jetstream KV bucket
///
/// This implementation opts for coarse grained status storage, as NATS KV can only
/// guarantee atomicity for a single key. If compare and set semantics are preferred, StoreOptions
/// should be configured with `force_cas` and `revision`.
#[async_trait]
impl StoreWrite<LatticeState, String> for NatsKvStorageEngine {
    async fn write(
        &mut self,
        obj: &LatticeState,
        id: Option<String>,
        opts: StoreOptions,
    ) -> Result<WithStateMetadata<String>, StorageError> {
        let id = id.unwrap_or(obj.id.clone());
        let bucket = self.build_lattice_bucket_name(id.clone());
        let kv = self.get_or_create_kv_store(bucket.clone()).await?;

        let revision = match opts {
            // Compare and set semantics
            StoreOptions {
                require_cas: true,
                revision: Some(rev),
                ..
            } => {
                kv.update(LATTICE_BUCKET_STATE_KEY, to_vec(&obj)?.into(), rev)
                    .await?
            }

            // Best-effort semantics
            _ => {
                kv.put(LATTICE_BUCKET_STATE_KEY, to_vec(&obj)?.into())
                    .await?
            }
        };

        Ok(WithStateMetadata {
            state: id,
            revision: Some(revision),
        })
    }
}

impl StoreReadWrite<LatticeState, String> for NatsKvStorageEngine {}
