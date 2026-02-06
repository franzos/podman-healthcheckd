use log::{error, info, warn};
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

// --- Serde types matching real Podman 5.x JSON ---

#[derive(Deserialize)]
struct PsEntry {
    #[serde(alias = "Id")]
    id: String,
    #[serde(alias = "Names")]
    names: Vec<String>,
    #[serde(alias = "State")]
    state: String,
}

#[derive(Deserialize)]
struct InspectResult {
    #[serde(alias = "Config")]
    config: InspectConfig,
}

#[derive(Deserialize)]
struct InspectConfig {
    #[serde(alias = "Healthcheck")]
    healthcheck: Option<HealthcheckConfig>,
}

#[derive(Deserialize, Clone, Debug)]
#[allow(dead_code)]
struct HealthcheckConfig {
    #[serde(alias = "Test")]
    test: Vec<String>,
    #[serde(alias = "Interval", default)]
    interval: u64,
    #[serde(alias = "Timeout", default)]
    timeout: u64,
    #[serde(alias = "StartPeriod", default)]
    start_period: u64,
    #[serde(alias = "Retries", default)]
    retries: u32,
}

#[derive(Deserialize)]
struct PodmanEvent {
    #[serde(alias = "ID")]
    id: String,
    #[serde(alias = "Name")]
    name: String,
    #[serde(alias = "Status")]
    status: String,
    #[serde(alias = "Type", rename = "Type")]
    r#type: String,
}

// --- Podman CLI helpers ---

async fn podman_ps() -> Result<Vec<PsEntry>, String> {
    let output = Command::new("podman")
        .args(["ps", "--format", "json"])
        .output()
        .await
        .map_err(|e| format!("failed to run podman ps: {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "podman ps failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.trim().is_empty() {
        return Ok(Vec::new());
    }

    serde_json::from_str(&stdout)
        .map_err(|e| format!("failed to parse podman ps output: {e}"))
}

async fn get_healthcheck(id: &str) -> Option<HealthcheckConfig> {
    let output = Command::new("podman")
        .args(["inspect", id])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        warn!(
            "podman inspect {id} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        return None;
    }
    let results: Vec<InspectResult> =
        serde_json::from_slice(&output.stdout).ok()?;
    let hc = results.into_iter().next()?.config.healthcheck?;
    // Skip containers where healthcheck is just ["NONE"]
    if hc.test.first().map(|s| s.as_str()) == Some("NONE") {
        return None;
    }
    Some(hc)
}

async fn run_healthcheck(id: &str) -> bool {
    let output = match Command::new("podman")
        .args(["healthcheck", "run", id])
        .output()
        .await
    {
        Ok(o) => o,
        Err(e) => {
            warn!("failed to exec podman healthcheck run {id}: {e}");
            return false;
        }
    };
    output.status.success()
}

// --- Nanosecond helpers ---

fn ns_to_duration(ns: u64) -> Duration {
    if ns == 0 {
        Duration::from_secs(30) // podman default
    } else {
        Duration::from_nanos(ns)
    }
}

// --- Per-container healthcheck loop ---

async fn healthcheck_loop(
    id: String,
    name: String,
    config: HealthcheckConfig,
    token: CancellationToken,
) {
    let start_period = ns_to_duration(config.start_period);
    let interval = ns_to_duration(config.interval);

    if !start_period.is_zero() {
        info!("[{name}] waiting {start_period:.1?} start period");
        tokio::select! {
            _ = tokio::time::sleep(start_period) => {}
            _ = token.cancelled() => return,
        }
    }

    info!("[{name}] healthcheck active, interval {interval:.1?}");
    loop {
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = token.cancelled() => return,
        }
        tokio::select! {
            ok = run_healthcheck(&id) => {
                if ok {
                    info!("[{name}] healthcheck passed");
                } else {
                    warn!("[{name}] healthcheck failed");
                }
            }
            _ = token.cancelled() => return,
        }
    }
}

// --- Task manager ---

struct TaskManager {
    tasks: HashMap<String, (String, JoinHandle<()>)>,
    token: CancellationToken,
}

impl TaskManager {
    fn new(token: CancellationToken) -> Self {
        Self {
            tasks: HashMap::new(),
            token,
        }
    }

    async fn start_container(&mut self, id: String, name: String) {
        if self.tasks.contains_key(&id) {
            return;
        }
        let config = match get_healthcheck(&id).await {
            Some(c) => c,
            None => return,
        };
        info!("[{name}] scheduling healthcheck (interval={:.1?}, start_period={:.1?}, retries={})",
            ns_to_duration(config.interval),
            ns_to_duration(config.start_period),
            config.retries,
        );
        let cid = id.clone();
        let cname = name.clone();
        let ct = self.token.clone();
        let handle = tokio::spawn(healthcheck_loop(cid, cname, config, ct));
        self.tasks.insert(id, (name, handle));
    }

    fn stop_container(&mut self, id: &str) {
        if let Some((name, handle)) = self.tasks.remove(id) {
            info!("[{name}] removing healthcheck timer");
            handle.abort();
        }
    }

    fn stop_all(&mut self) {
        for (_, (name, handle)) in self.tasks.drain() {
            info!("[{name}] stopping");
            handle.abort();
        }
    }

    fn count(&self) -> usize {
        self.tasks.len()
    }
}

// --- Event actions ---

enum Action {
    Start { id: String, name: String },
    Stop { id: String },
}

// --- Event watcher ---

async fn watch_events(tx: mpsc::Sender<Action>, token: CancellationToken) {
    loop {
        info!("starting podman events watcher");
        match watch_events_inner(&tx, &token).await {
            Ok(()) => {
                if token.is_cancelled() {
                    break;
                }
                warn!("podman events exited unexpectedly");
            }
            Err(e) => {
                if token.is_cancelled() {
                    break;
                }
                error!("podman events failed: {e}");
            }
        }
        info!("restarting event watcher in 5s");
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(5)) => {}
            _ = token.cancelled() => break,
        }
    }
}

async fn watch_events_inner(
    tx: &mpsc::Sender<Action>,
    token: &CancellationToken,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut child = Command::new("podman")
        .args([
            "events",
            "--format",
            "json",
            "--filter",
            "type=container",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    let stdout = child.stdout.take().expect("stdout piped");
    let reader = BufReader::new(stdout);
    let mut lines = reader.lines();

    loop {
        tokio::select! {
            result = lines.next_line() => {
                match result? {
                    Some(line) => {
                        let event: PodmanEvent = match serde_json::from_str(&line) {
                            Ok(e) => e,
                            Err(e) => {
                                warn!("failed to parse event: {e}");
                                continue;
                            }
                        };

                        if event.r#type != "container" {
                            continue;
                        }

                        let action = match event.status.as_str() {
                            "start" => Some(Action::Start {
                                id: event.id,
                                name: event.name,
                            }),
                            "died" | "stop" | "remove" => Some(Action::Stop { id: event.id }),
                            _ => None,
                        };

                        if let Some(a) = action {
                            if tx.send(a).await.is_err() {
                                break;
                            }
                        }
                    }
                    None => break,
                }
            }
            _ = token.cancelled() => break,
        }
    }

    let _ = child.kill().await;
    Ok(())
}

// --- Startup enumeration ---

async fn enumerate_existing(manager: &mut TaskManager) {
    info!("enumerating existing containers");

    let containers = {
        let mut attempts = 0;
        loop {
            match podman_ps().await {
                Ok(entries) => break entries,
                Err(e) => {
                    attempts += 1;
                    if attempts >= 5 {
                        error!("podman ps failed after {attempts} attempts: {e}");
                        return;
                    }
                    warn!("podman ps failed (attempt {attempts}/5): {e}, retrying in 3s");
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
            }
        }
    };

    let mut found = 0;
    for entry in containers {
        if entry.state != "running" {
            continue;
        }
        let name = entry.names.into_iter().next().unwrap_or_default();
        manager.start_container(entry.id, name).await;
        found += 1;
    }
    info!(
        "startup scan complete: {found} running containers, {} with healthchecks",
        manager.count()
    );
}

// --- Main ---

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_secs()
        .init();

    info!("podman-healthcheckd starting");

    let token = CancellationToken::new();
    let mut manager = TaskManager::new(token.clone());

    // Start event watcher first so events are buffered during enumeration
    let (tx, mut rx) = mpsc::channel::<Action>(64);
    let watcher = tokio::spawn(watch_events(tx, token.clone()));

    enumerate_existing(&mut manager).await;

    let mut sigterm =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to register SIGTERM handler");

    loop {
        tokio::select! {
            Some(action) = rx.recv() => {
                match action {
                    Action::Start { id, name } => {
                        manager.start_container(id, name).await;
                    }
                    Action::Stop { id } => {
                        manager.stop_container(&id);
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("received SIGINT, shutting down");
                break;
            }
            _ = sigterm.recv() => {
                info!("received SIGTERM, shutting down");
                break;
            }
        }
    }

    token.cancel();
    manager.stop_all();
    let _ = watcher.await;
    info!("podman-healthcheckd stopped");
}
