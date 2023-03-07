use anyhow::Result;
use semver::Version;
use std::collections::HashMap;

use wadm::events::LinkdefBuilder;
use wadm::storage::{
    ActorBuilder, ClaimBuilder, HostBuilder, LatticeParameters, LatticeStateBuilder,
    NatsKvStorageConfig, NatsKvStorageEngine, ProviderBuilder, Store, StoreOptions,
};

mod common;
use common::make_nats_instance;

/// Test storing and retrieving an empty lattice
#[tokio::test]
async fn storage_nats_kv_empty_lattice() -> Result<()> {
    let mut nats = make_nats_instance().await?;

    // Create an empty lattice
    let nats_url = nats.get_addr().await?;

    let mut storage = NatsKvStorageEngine::new(NatsKvStorageConfig {
        nats_url,
        lattice_bucket_prefix: None,
        auth: None,
    })
    .await?;

    let lattice = LatticeStateBuilder::default()
        .id("test".into())
        .parameters(LatticeParameters::default())
        .hosts(HashMap::from([]))
        .actors(HashMap::from([]))
        .providers(HashMap::from([]))
        .link_defs(HashMap::from([]))
        .claims(HashMap::from([]))
        .build()?;

    // Store the lattice
    let stored = storage.store(lattice, StoreOptions::default()).await?;
    assert!(stored.revision.is_some(), "a Nats KV revision was present");

    // Re-read the stored lattice
    let reread = storage.get(stored.state).await?;
    assert_eq!(reread.revision, stored.revision);
    assert_eq!(reread.state.id, "test");

    nats.stop().await?;
    Ok(())
}

/// Test storing and retrieving an empty lattice
#[tokio::test]
async fn storage_nats_kv_full_lattice() -> Result<()> {
    let mut nats = make_nats_instance().await?;

    // Create an empty lattice
    let nats_url = nats.get_addr().await?;

    let mut storage = NatsKvStorageEngine::new(NatsKvStorageConfig {
        nats_url,
        lattice_bucket_prefix: None,
        auth: None,
    })
    .await?;

    let lattice = LatticeStateBuilder::default()
        .id("test".into())
        .parameters(LatticeParameters::default())
        .hosts(HashMap::from([(
            "12345".into(),
            HostBuilder::default()
                .id("12345".into())
                .name("host-0".into())
                .uptime_seconds(12345)
                .labels(vec!["test".into()])
                .version(Version::parse("1.2.3")?)
                .build()?,
        )]))
        .actors(HashMap::from([(
            "actor_0".into(),
            ActorBuilder::default()
                .id("actor_0".into())
                .name("actor_0".into())
                .capabilities(vec!["httpserver".into()])
                .issuer("fake-issuer".into())
                .tags(vec!["server".into()])
                .call_alias("actor0".into())
                .build()?,
        )]))
        .providers(HashMap::from([(
            "wasmcloud:httpserver".into(),
            HashMap::from([(
                "provider_0".into(),
                ProviderBuilder::default()
                    .id("provider_0".into())
                    .name("provider_0".into())
                    .issuer("fake-issuer".into())
                    .contract_id("wasmcloud:httpserver".into())
                    .tags(vec!["http".into()])
                    .build()?,
            )]),
        )]))
        .link_defs(HashMap::from([(
            "link_0".into(),
            LinkdefBuilder::default()
                .id("link_0".into())
                .actor_id("actor_0".into())
                .provider_id("provider_0".into())
                .contract_id("wasmcloud:httpserver".into())
                .link_name("default".into())
                .values(HashMap::new())
                .build()?,
        )]))
        .claims(HashMap::from([(
            "claim_0".into(),
            ClaimBuilder::default()
                .name("claim_0".into())
                .subscriber("???".into())
                .issuer("fake-issuer".into())
                .call_alias(None)
                .capabilities(vec!["httpserver".into()])
                .version(Version::parse("1.2.3")?)
                .revision(0)
                .tags(HashMap::new())
                .build()?,
        )]))
        .build()?;

    // Store the lattice
    let stored = storage
        .store(lattice.clone(), StoreOptions::default())
        .await?;
    assert!(stored.revision.is_some(), "a Nats KV revision was present");

    // Re-read the stored lattice, ensure that it's the exact same
    let reread = storage.get(stored.state).await?;
    assert_eq!(reread.state, lattice);

    nats.stop().await?;
    Ok(())
}
