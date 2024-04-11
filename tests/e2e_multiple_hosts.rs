#![cfg(feature = "_e2e_tests")]
use std::path::PathBuf;
use std::time::Duration;

use futures::{FutureExt, StreamExt};
use wadm::server::{DeployResult, PutResult, StatusType};

mod e2e;
mod helpers;

use e2e::{assert_status, check_actors, check_providers, ClientInfo, ExpectedCount};
use helpers::{HELLO_COMPONENT_ID, HTTP_SERVER_COMPONENT_ID};

use crate::{
    e2e::check_status,
    helpers::{HELLO_IMAGE_REF, HTTP_SERVER_IMAGE_REF},
};

const MANIFESTS_PATH: &str = "test/data";
const DOCKER_COMPOSE_FILE: &str = "test/docker-compose-e2e.yaml";
const BLOBSTORE_FS_IMAGE_REF: &str = "ghcr.io/wasmcloud/blobstore-fs:0.6.0";
const BLOBSTORE_FS_PROVIDER_ID: &str = "fileserver";
const BLOBBY_IMAGE_REF: &str = "wasmcloud.azurecr.io/blobby:0.1.0";
const BLOBBY_COMPONENT_ID: &str = "littleblobbytables";

// NOTE(thomastaylor312): This exists because we need to have setup happen only once for all tests
// and then we want cleanup to run with `Drop`. I tried doing this with a `OnceCell`, but `static`s
// don't run drop, they only drop the memory (I also think OnceCell does the same thing too). So to
// get around this we have a top level test that runs everything
#[cfg(feature = "_e2e_tests")]
#[tokio::test(flavor = "multi_thread")]
async fn run_multiple_host_tests() {
    let root_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("Unable to find repo root"));
    let manifest_dir = root_dir.join(MANIFESTS_PATH);
    let compose_file = root_dir.join(DOCKER_COMPOSE_FILE);

    let mut client_info = ClientInfo::new(manifest_dir, compose_file).await;
    client_info.add_ctl_client("default", None).await;
    client_info.launch_wadm().await;

    // Wait for the first event on the lattice prefix before we start deploying and checking
    // statuses. Wadm can absolutely handle hosts starting before you start the wadm process, but the first event
    // on the lattice will initialize the lattice monitor and for the following test we quickly assert things.
    let mut sub = client_info
        .client
        .subscribe("wadm.evt.default".to_string())
        .await
        .expect("Should be able to subscribe to default events");
    let _ = sub.next().await;

    // Wait for hosts to start
    let mut did_start = false;
    for _ in 0..10 {
        match client_info.ctl_client("default").get_hosts().await {
            Ok(hosts) if hosts.len() == 5 => {
                eprintln!("Hosts {}/5 currently available", hosts.len());
                did_start = true;
                break;
            }
            Ok(hosts) => {
                eprintln!(
                    "Waiting for all hosts to be available {}/5 currently available",
                    hosts.len()
                );
            }
            Err(e) => {
                eprintln!("Error when fetching hosts: {e}",)
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    if !did_start {
        panic!("Hosts didn't start")
    }

    // NOTE(thomastaylor312): A nice to have here, but what I didn't want to figure out now, would
    // be to catch the panics from tests and label the backtrace with the appropriate information
    // about which test failed. Another issue is that only the first panic will be returned, so
    // capturing the backtraces and then printing them nicely would probably be good

    // We run this test first by itself because it is a basic test that wadm only spins up the exact
    // number of resources requested. If we were to run it in parallel, some of the shared resources
    // will be created with other tests (namely providers) and this test will fail
    test_no_requirements(&client_info).boxed().await;

    // The futures must be boxed or they're technically different types
    let tests = [
        test_spread_all_hosts(&client_info).boxed(),
        test_lotta_actors(&client_info).boxed(),
        test_complex_app(&client_info).boxed(),
    ];
    futures::future::join_all(tests).await;

    test_stop_host_rebalance(&client_info).await;
}

// This test does a basic check that all things exist in isolation and should be run first before
// other tests run
async fn test_no_requirements(client_info: &ClientInfo) {
    let stream = client_info.get_status_stream().await;
    stream
        .purge()
        .await
        .expect("shouldn't have errored purging stream");
    let resp = client_info
        .put_manifest_from_file("simple.yaml", None, None)
        .await;

    assert_ne!(
        resp.result,
        PutResult::Error,
        "Shouldn't have errored when creating manifest: {resp:?}"
    );

    let resp = client_info
        .deploy_manifest("hello-simple", None, None, None)
        .await;
    assert_ne!(
        resp.result,
        DeployResult::Error,
        "Shouldn't have errored when deploying manifest: {resp:?}"
    );

    // NOTE: This runs for a while, but it's because we're waiting for the provider to download,
    // which can take a bit
    assert_status(None, Some(7), || async {
        let inventory = client_info.get_all_inventory("default").await?;

        check_actors(&inventory, HELLO_IMAGE_REF, "hello-simple", 4)?;
        check_providers(&inventory, HTTP_SERVER_IMAGE_REF, ExpectedCount::Exactly(1))?;

        let links = client_info
            .ctl_client("default")
            .get_links()
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))?
            .response
            .expect("Should have links");

        if !links.iter().any(|ld| {
            ld.source_id == HTTP_SERVER_COMPONENT_ID
                && ld.target == HELLO_COMPONENT_ID
                && ld.wit_namespace == "wasi"
                && ld.wit_package == "http"
                && ld.interfaces == vec!["incoming-handler"]
                && ld.name == "default"
        }) {
            anyhow::bail!(
                "Link between http provider and hello component should exist: {:#?}",
                links
            )
        }

        check_status(&stream, "default", "hello-simple", StatusType::Deployed)
            .await
            .unwrap();

        Ok(())
    })
    .await;

    // Undeploy manifest
    let resp = client_info
        .undeploy_manifest("hello-simple", None, None)
        .await;

    assert_ne!(
        resp.result,
        DeployResult::Error,
        "Shouldn't have errored when undeploying manifest: {resp:?}"
    );

    // Once manifest is undeployed, status should be undeployed
    check_status(&stream, "default", "hello-simple", StatusType::Undeployed)
        .await
        .unwrap();

    // assert that no components or providers with annotations exist
    assert_status(None, None, || async {
        let inventory = client_info.get_all_inventory("default").await?;

        check_actors(&inventory, HELLO_IMAGE_REF, "hello-simple", 0)?;
        check_providers(&inventory, HTTP_SERVER_IMAGE_REF, ExpectedCount::Exactly(0))?;

        let links = client_info
            .ctl_client("default")
            .get_links()
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))?
            .response
            .expect("Should have links");

        assert!(
            links.is_empty(),
            "The link between the http provider and hello component should be removed"
        );

        check_status(&stream, "default", "hello-simple", StatusType::Undeployed)
            .await
            .unwrap();

        Ok(())
    })
    .await;
}

async fn test_lotta_actors(client_info: &ClientInfo) {
    let resp = client_info
        .put_manifest_from_file("lotta_actors.yaml", None, None)
        .await;

    assert_ne!(
        resp.result,
        PutResult::Error,
        "Shouldn't have errored when creating manifest: {resp:?}"
    );

    let resp = client_info
        .deploy_manifest("lotta-actors", None, None, None)
        .await;
    assert_ne!(
        resp.result,
        DeployResult::Error,
        "Shouldn't have errored when deploying manifest: {resp:?}"
    );

    // NOTE: This runs for a while, but it's because we're waiting for the provider to download,
    // which can take a bit
    assert_status(None, Some(7), || async {
        let inventory = client_info.get_all_inventory("default").await?;

        check_actors(&inventory, HELLO_IMAGE_REF, "lotta-actors", 9001)?;
        check_providers(&inventory, HTTP_SERVER_IMAGE_REF, ExpectedCount::AtLeast(1))?;

        Ok(())
    })
    .await;
}

async fn test_spread_all_hosts(client_info: &ClientInfo) {
    let resp = client_info
        .put_manifest_from_file("all_hosts.yaml", None, None)
        .await;

    assert_ne!(
        resp.result,
        PutResult::Error,
        "Shouldn't have errored when creating manifest: {resp:?}"
    );

    // Deploy manifest
    let resp = client_info
        .deploy_manifest("hello-all-hosts", None, None, None)
        .await;
    assert_ne!(
        resp.result,
        DeployResult::Error,
        "Shouldn't have errored when deploying manifest: {resp:?}"
    );

    assert_status(None, Some(7), || async {
        let inventory = client_info.get_all_inventory("default").await?;

        check_actors(&inventory, HELLO_IMAGE_REF, "hello-all-hosts", 5)?;
        check_providers(&inventory, HTTP_SERVER_IMAGE_REF, ExpectedCount::AtLeast(5))?;

        Ok(())
    })
    .await;
}

async fn test_complex_app(client_info: &ClientInfo) {
    let resp = client_info
        .put_manifest_from_file("complex.yaml", None, None)
        .await;

    assert_ne!(
        resp.result,
        PutResult::Error,
        "Shouldn't have errored when creating manifest: {resp:?}"
    );

    // Deploy manifest
    let resp = client_info
        .deploy_manifest("complex", None, None, None)
        .await;
    assert_ne!(
        resp.result,
        DeployResult::Error,
        "Shouldn't have errored when deploying manifest: {resp:?}"
    );

    assert_status(None, Some(7), || async {
        let inventory = client_info.get_all_inventory("default").await?;

        check_actors(&inventory, BLOBBY_IMAGE_REF, "complex", 5)?;
        check_providers(&inventory, HTTP_SERVER_IMAGE_REF, ExpectedCount::AtLeast(3))?;
        check_providers(
            &inventory,
            BLOBSTORE_FS_IMAGE_REF,
            ExpectedCount::Exactly(1),
        )?;

        let links = client_info
            .ctl_client("default")
            .get_links()
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))?
            .response
            .expect("Should have links");

        if !links.iter().any(|ld| {
            ld.source_id == HTTP_SERVER_COMPONENT_ID
                && ld.target == BLOBBY_COMPONENT_ID
                && ld.wit_namespace == "wasi"
                && ld.wit_package == "http"
                && ld.interfaces == vec!["incoming-handler"]
                && ld.name == "default"
        }) {
            anyhow::bail!(
                "Link between blobby component and http provider should exist: {:#?}",
                links
            );
        }

        if !links.iter().any(|ld| {
            ld.source_id == BLOBBY_COMPONENT_ID
                && ld.target == BLOBSTORE_FS_PROVIDER_ID
                && ld.wit_namespace == "wasi"
                && ld.wit_package == "blobstore"
                && ld.interfaces == vec!["blobstore"]
                && ld.name == "default"
        }) {
            anyhow::bail!(
                "Link between blobby component and blobstore-fs provider should exist: {:#?}",
                links
            );
        }

        // Make sure nothing is running on things it shouldn't be on
        if inventory.values().any(|inv| {
            inv.labels
                .get("region")
                .map(|region| region == "us-taylor-west" || region == "us-brooks-east")
                .unwrap_or(false)
                && inv
                    .providers
                    .iter()
                    .any(|prov| prov.id == BLOBSTORE_FS_PROVIDER_ID)
        }) {
            anyhow::bail!("Provider should only be running on the moon");
        }
        let moon_inventory = inventory
            .values()
            .find(|inv| {
                inv.labels
                    .get("region")
                    .map(|region| region == "moon")
                    .unwrap_or(false)
            })
            .unwrap();

        if moon_inventory
            .components
            .iter()
            .any(|actor| actor.id == BLOBBY_COMPONENT_ID)
        {
            anyhow::bail!("Actors shouldn't be running on the moon");
        }

        Ok(())
    })
    .await;
}

// This test should be run after other tests have finished since we are stopping one of the hosts
async fn test_stop_host_rebalance(client_info: &ClientInfo) {
    let resp = client_info
        .put_manifest_from_file("host_stop.yaml", None, None)
        .await;

    assert_ne!(
        resp.result,
        PutResult::Error,
        "Shouldn't have errored when creating manifest: {resp:?}"
    );

    // Deploy manifest
    let resp = client_info
        .deploy_manifest("host-stop", None, None, None)
        .await;
    assert_ne!(
        resp.result,
        DeployResult::Error,
        "Shouldn't have errored when deploying manifest: {resp:?}"
    );

    // Make sure everything deploys first
    assert_status(None, Some(7), || async {
        let inventory = client_info.get_all_inventory("default").await?;

        check_actors(&inventory, HELLO_IMAGE_REF, "host-stop", 5)?;

        Ok(())
    })
    .await;

    // Now get the inventory and figure out which host is running the most actors of the spread and
    // stop that one
    let host_to_stop = client_info
        .get_all_inventory("default")
        .await
        .expect("Unable to fetch inventory")
        .into_iter()
        .filter(|(_, inv)| {
            inv.labels
                .get("region")
                .map(|region| region == "us-brooks-east")
                .unwrap_or(false)
        })
        .max_by_key(|(_, inv)| {
            inv.components
                .iter()
                .find(|actor| actor.id == HELLO_COMPONENT_ID)
                .map(|desc| desc.max_instances)
                .unwrap_or(0)
        })
        .map(|(host_id, _)| host_id)
        .unwrap();

    client_info
        .ctl_client("default")
        .stop_host(&host_to_stop, None)
        .await
        .expect("Should have stopped host");

    // Just to make sure state has time to update and the host shuts down, wait for a bit before
    // checking
    tokio::time::sleep(Duration::from_secs(4)).await;

    // Now wait for us to get to 5 again
    assert_status(None, Some(7), || async {
        let inventory = client_info.get_all_inventory("default").await?;

        check_actors(&inventory, HELLO_IMAGE_REF, "host-stop", 5)?;

        Ok(())
    })
    .await;
}

// NOTE(thomastaylor312): Future tests could include actually making sure the app works as expected
