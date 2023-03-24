use tokio::process::Child;
use tokio::process::Command;

use anyhow::Result;

const DEFAULT_WASMCLOUD_PORT: u16 = 4000;
const DEFAULT_NATS_PORT: u16 = 4222;

#[derive(Debug)]
pub struct CleanupGuard {
    child: Option<Child>,

    already_running: bool,
}

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        if !self.already_running {
            match std::process::Command::new("wash").args(["down"]).output() {
                Ok(o) if !o.status.success() => {
                    eprintln!(
                        "Error stopping wasmcloud host: {}",
                        String::from_utf8_lossy(&o.stderr)
                    )
                }
                Err(e) => eprintln!("Error stopping wasmcloud host: {e}"),
                _ => (),
            }
        }
    }
}

/// Configuration struct for wash instances that are used for testing
#[derive(Debug)]
pub struct TestWashConfig {
    /// Port on which to run wasmCloud
    pub nats_port: Option<u16>,

    /// Only connect to pre-existing NATS instance
    pub nats_connect_only: bool,

    /// Port on which to run wasmCloud (via `wash up`)
    pub wasmcloud_port: Option<u16>,

    /// Whether wash should detach (-d)
    pub detach: bool,
}

impl Default for TestWashConfig {
    fn default() -> TestWashConfig {
        TestWashConfig {
            nats_port: None,
            nats_connect_only: false,
            wasmcloud_port: None,
            detach: false,
        }
    }
}

impl TestWashConfig {
    /// Build a test wash configuration with randomized ports
    pub async fn random() -> Result<TestWashConfig> {
        let nats_port = port_check::free_local_port();
        let wasmcloud_port = port_check::free_local_port();

        Ok(TestWashConfig {
            nats_port,
            wasmcloud_port,
            ..TestWashConfig::default()
        })
    }

    /// Get the washboard URL for this config
    pub fn washboard_url(&self) -> String {
        format!(
            "localhost:{}",
            self.wasmcloud_port
                .unwrap_or(DEFAULT_WASMCLOUD_PORT)
                .to_string()
        )
    }

    /// Get the NATS URL for this config
    pub fn nats_url(&self) -> String {
        format!(
            "127.0.0.1:{}",
            self.nats_port.unwrap_or(DEFAULT_NATS_PORT).to_string()
        )
    }
}

/// Start a local wash instance
async fn start_wash_instance(cfg: &TestWashConfig) -> Result<CleanupGuard> {
    let nats_port = cfg.nats_port.unwrap_or(DEFAULT_NATS_PORT).to_string();
    let wasmcloud_port = cfg
        .wasmcloud_port
        .unwrap_or(DEFAULT_WASMCLOUD_PORT)
        .to_string();

    // Build args
    let mut args: Vec<&str> = Vec::from(["up", "--nats-port", &nats_port]);
    if cfg.detach {
        args.push("-d");
    }
    if cfg.nats_connect_only {
        args.push("--nats-connect-only");
    }

    // Build the command
    let mut cmd = Command::new("wash");
    cmd.args(&args)
        .env("WASMCLOUD_PORT", &wasmcloud_port)
        .env("WASMCLOUD_DASHBOARD_PORT", &wasmcloud_port)
        .stderr(std::process::Stdio::null())
        .stdout(std::process::Stdio::null());

    // Build the cleanup guard that will be returned
    let mut guard = CleanupGuard {
        child: None,
        already_running: false,
    };

    // Launch command either detached or as an active child process
    if cfg.detach {
        // NOTE: In the detached case, we expect NATS to have been started prior (ex. via Makefile)
        //
        // Detached exeuction should finish immediately and return
        let output = cmd.status().await.expect("Unable to run detached wash up");
        assert!(output.success(), "Error trying to start host",);
    } else {
        // If doing a continuous execution, spawn into another thread
        // and hold on to the child process handle in the guard
        guard.child = Some(cmd.spawn()?);
    }

    // Make sure we can connect to washboard
    wait_for_server(&cfg.washboard_url()).await;

    // Give the host just a bit more time to get totally ready
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    Ok(guard)
}

/// Set up and run a wash instance that can be used for a test
pub async fn setup_test_wash(cfg: &TestWashConfig) -> CleanupGuard {
    match tokio::net::TcpStream::connect(cfg.washboard_url()).await {
        Err(_) => start_wash_instance(&cfg)
            .await
            .unwrap_or_else(|_| CleanupGuard {
                child: None,
                already_running: false,
            }),
        Ok(_) => CleanupGuard {
            child: None,
            already_running: true,
        },
    }
}

pub async fn wait_for_server(url: &str) {
    let mut wait_count = 1;
    loop {
        // Magic number: 10 + 1, since we are starting at 1 for humans
        if wait_count >= 11 {
            panic!("Ran out of retries waiting for host to start");
        }
        match tokio::net::TcpStream::connect(url).await {
            Ok(_) => break,
            Err(e) => {
                eprintln!("Waiting for server {url} to come up, attempt {wait_count}. Will retry in 1 second. Got error {e:?}");
                wait_count += 1;
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
    }
}

/// Runs wash with the given args and makes sure it runs successfully. Returns the contents of
/// stdout
pub async fn run_wash_command<I, S>(args: I) -> Vec<u8>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let output = Command::new("wash")
        .args(args)
        .output()
        .await
        .expect("Unable to run wash command");
    if !output.status.success() {
        panic!(
            "wash command didn't exit successfully: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    }
    output.stdout
}
