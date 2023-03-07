use thiserror::Error;

mod nats_kv;
mod state;

use async_trait::async_trait;
use semver::Version;
use serde::{Deserialize, Serialize};
use state::LatticeState;

pub use nats_kv::{NatsAuthConfig, NatsKvStorageConfig, NatsKvStorageEngine};
pub use state::{
    Actor, ActorBuilder, Claim, ClaimBuilder, Host, HostBuilder, LatticeParameters,
    LatticeStateBuilder, Provider, ProviderBuilder,
};

/////////////
// Engines //
/////////////

/// Pluggable engines can be asked for their metadata
/// to enable generic (possibly dyn) usage
#[async_trait]
pub trait EngineMetadata {
    /// Get the name of the lattice storage
    async fn name() -> String;

    /// Get the version of the lattice storage
    async fn version() -> Version;
}

/////////////////////
// Storage Engines //
/////////////////////

/// Errors that are related to storage engines
#[derive(Debug, Error)]
pub enum StorageEngineError {
    /// When the underlying engine throws an error
    #[error("Storage engine encountered an error: {0}")]
    Engine(String),

    #[error("Storage engine [{0}] not connected")]
    NotConnected(String),

    /// When an operation is not implemented
    #[error("Operation not implemented: {0}")]
    NotImplemented(String),

    /// An error when the cause is unknown. This happens very rarely and should be considered fatal
    #[error("Unknown error has occured")]
    Unknown,
}

/////////////
// Storage //
/////////////

/// Errors that are related to storage
#[derive(Debug, Error)]
pub enum StorageError {
    /// Errors that arise at the engine layer
    #[error("Storage error: {0}")]
    Engine(String),

    /// Errors that arise at the network layer
    #[error("Network error: {0}")]
    NetworkError(String),

    /// An error when the cause is unknown. This happens very rarely and should be considered fatal
    #[error("Unknown error has occured: {0}")]
    Unknown(String),
}

/// Options and metadata that can be used to improve store operations
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct StoreOptions {
    /// Whether to force a store with compare-and-set semantics
    pub require_cas: bool,

    /// version identifier that enables stores to perform compare-and-set semantics
    pub revision: Option<u64>,
}

/// A trait that represents whether some struct can be read from a given store
///
/// Note that these must be this way because we can't write non-overlapping blanked impls yet:
/// https://github.com/rust-lang/rust/issues/20400
/// https://stackoverflow.com/questions/40392524/conflicting-trait-implementations-even-though-associated-types-differ
#[async_trait]
pub trait StoreRead<State, Id> {
    /// Read a given piece of state from the store
    async fn read(&self, id: Id) -> Result<WithStateMetadata<State>, StorageError>;
}

/// A trait that represents whether some struct can be written to a given store
#[async_trait]
pub trait StoreWrite<State, Id> {
    /// Write to a given store
    async fn write(
        &mut self,
        obj: &State,
        id: Option<Id>,
        opts: StoreOptions,
    ) -> Result<WithStateMetadata<Id>, StorageError>;
}

/// Trait for objects that can read and write some piece of state with a given identifier
#[async_trait]
pub trait StoreReadWrite<State, Id>: StoreRead<State, Id> + StoreWrite<State, Id> {}

#[derive(Debug, Serialize, Deserialize)]
pub struct WithStateMetadata<T> {
    /// Revision (if supported), often used for compare-and-set-semantics
    pub revision: Option<u64>,

    /// The state that was returned
    pub state: T,
}

/// A trait that indicates the ability of a struct to store state
/// Objects that implement Store can save some given State, identifiable by StateId, and updatable with the given StateBuilder
///
#[async_trait]
pub trait Store
where
    Self: EngineMetadata,
    Self: StoreRead<Self::State, Self::StateId> + StoreWrite<Self::State, Self::StateId>,
{
    type State;
    type StateId;
    type StateUpdate;

    /// Get a particular piece of state
    async fn get(&self, id: Self::StateId) -> Result<WithStateMetadata<Self::State>, StorageError>;

    /// Store a piece of state
    async fn store(
        &mut self,
        state: Self::State,
        opts: StoreOptions,
    ) -> Result<WithStateMetadata<Self::StateId>, StorageError>;

    /// Delete an existing piece of state
    async fn delete(&self, id: String) -> Result<(), StorageError>;
}

/// LatticeStorages are state stores that support storing lattice information
pub trait LatticeStorage:
    Store<State = LatticeState, StateId = String, StateUpdate = LatticeStateBuilder>
{
}
