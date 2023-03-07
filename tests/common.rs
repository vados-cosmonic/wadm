use std::env;
use std::collections::HashMap;

use async_trait::async_trait;
use anyhow::{Error, Result};
use port_selector::random_free_tcp_port;
use tokio::process::{Child, Command};
use bollard::Docker;
use bollard::container::{
    Config as ContainerConfig,
    CreateContainerOptions,
    StartContainerOptions,
};
use random_string::{generate as generate_random_string};

/// Variable that determines/differentiates CI environments
const ENV_VAR_CI: &str = "CI";

/// ENV Variable that determines whether docker is enabled
const ENV_VAR_DOCKER_DISABLED: &str = "TEST_DOCKER_DISABLED";
const DEFAULT_DOCKER_DISABLED: bool = true;

/// ENV Variable that determines where docker is located
const ENV_VAR_DOCKER_HOST: &str = "DOCKER_HOST";

/// ENV Variable that determines where the NATS is located
const ENV_VAR_NATS_BIN_PATH: &str = "TEST_NATS_BIN";
const DEFAULT_NATS_BIN_PATH: &str = "/usr/bin/nats-server";

const ENV_VAR_NATS_DOCKER_IMAGE: &str = "TEST_NATS_DOCKER_IMAGE";
const DEFAULT_NATS_DOCKER_IMAGE: &str = "nats:2.9.15-scratch@sha256:1a82b1b261d169994fc6162f2f25894a49e20971560f8b2dff4f90eefb32b421";

/// A test instance of a given requirement (ex. NATS)
#[async_trait]
pub trait EphemeralResource {
    /// Stop the instance
    async fn stop(&mut self) -> Result<()>;
}

/// A NATS instance that can be used for test
#[async_trait]
pub trait NatsInstance: EphemeralResource {
    /// Get the address of the nats instance
    async fn get_addr(&self) -> Result<String>;
}

const NUM_PORT_TRIES: u8 = 5;

/// Helper function that picks a random port
fn pick_random_port() -> Result<u16> {
    let mut tries = 0;
    let mut port = None;
    loop {
        tries += 1;
        if let Some(p) = random_free_tcp_port() {
            port = Some(p);
            break;
        }
        if tries >= 5 {
            return Err(Error::msg(format!("Failed to find port after trying {NUM_PORT_TRIES} times")));
        }
    }

    match port {
        Some(port) => Ok(port),
        None => Err(Error::msg("Failed to find free TCP port")),
    }
}

/// Start a NATS instance for use during test
pub(crate) async fn make_nats_instance() -> Result<Box<dyn NatsInstance>> {
    let in_ci = env::var(ENV_VAR_CI).is_ok();
    let docker_disabled = env::var(ENV_VAR_DOCKER_DISABLED).map(|v| v == "true").unwrap_or(DEFAULT_DOCKER_DISABLED);

    let port = pick_random_port()?;

    if docker_disabled || in_ci {
        Ok(Box::new(ProcessNats::new(port).await?))
    } else {
        Ok(Box::new(DockerizedNats::new(port).await?))
    }
}

/// A process-based NATS instance
struct ProcessNats {
    child: Child,
    port: u16,
}

impl ProcessNats {
    async fn new(port: u16) -> Result<ProcessNats> {
        // Determine path to NATS binary
        let nats_bin_path = env::var(ENV_VAR_NATS_BIN_PATH).unwrap_or(DEFAULT_NATS_BIN_PATH.into());

        // Start process in a local process
        let child = Command::new(&nats_bin_path)
            .arg("-js")
            .arg("-p")
            .arg(port.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| Error::msg(format!("nats command ({nats_bin_path}) failed to spawn: {e}")))?;

        // Wait until NATS is available (usually ~1s)
        tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

        Ok(ProcessNats { child, port })
    }
}

#[async_trait]
impl EphemeralResource for ProcessNats {
    /// Stop the instance
    async fn stop(&mut self) -> Result<()> {
        self.child.kill()
            .await
            .map_err(|e| Error::msg(format!("failed to stop NATS child process: {e}")))
    }
}

#[async_trait]
impl NatsInstance for ProcessNats {
    async fn get_addr(&self) -> Result<String> {
        Ok(format!("localhost:{}", self.port))
    }
}

/// A dockerized NATS instance
struct DockerizedNats {
    /// Docker instance
    docker: Docker,

    /// Port on which to start the NATS container
    port: u16,

    /// Docker container ID
    id: String,

    // Container name
    container_name: String
}

const CONTAINER_SUFFIX_ALPHABET: &str = "01234567890";

impl DockerizedNats {
    async fn new(port: u16) -> Result<DockerizedNats> {
        // Obtain connection to the docker host
        let docker = match env::var(ENV_VAR_DOCKER_HOST) {
            Ok(v) if v.starts_with("http") => Docker::connect_with_http_defaults()?,
            _ => Docker::connect_with_local_defaults()?,
        };

        let nats_image = env::var(ENV_VAR_NATS_DOCKER_IMAGE).unwrap_or(DEFAULT_NATS_DOCKER_IMAGE.into());

        let container_name = format!("nats-test-{}", generate_random_string(6, CONTAINER_SUFFIX_ALPHABET));
        let options = Some(CreateContainerOptions{
            name: &container_name,
            platform: None,
        });

        let config = ContainerConfig {
            image: Some(nats_image),
            cmd: Some(vec![
                "-js".into(),
                "-p".into(), port.to_string(),
            ]),
            exposed_ports: Some(HashMap::from([
                (format!("{}", &port), HashMap::new())
            ])),
            ..Default::default()
        };

        let create_resp = docker.create_container(options, config).await?;
        docker.start_container(&container_name, None::<StartContainerOptions<String>>).await?;

        Ok(DockerizedNats { docker, port, id: create_resp.id, container_name })
    }
}

#[async_trait]
impl EphemeralResource for DockerizedNats {
    /// Stop the instance
    async fn stop(&mut self) -> Result<()> {
        self.docker
            .stop_container(self.container_name.as_str(), None)
            .await
            .map_err(|e| Error::msg(format!("Failed to stop docker instance: {e}")))
    }
}

#[async_trait]
impl NatsInstance for DockerizedNats {
    async fn get_addr(&self) -> Result<String> {
        Ok(format!("localhost:{}", self.port))
    }
}
