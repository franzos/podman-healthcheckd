use std::fs::OpenOptions;
use std::os::unix::io::AsRawFd;
use log::{error, info, warn};
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use std::io::Write;
use serde::Serialize;
use std::sync::Arc;
use tokio::sync::Mutex as AsyncMutex;


#[derive(Serialize, Clone)]
struct ContainerStatus {
    name: String,
    healthy: bool,
    consecutive_failures: u32,
    last_check: String, // RFC3339 timestamp
}

type SharedStatus = Arc<AsyncMutex<HashMap<String, ContainerStatus>>>;

fn status_path() -> String {
    std::env::var("XDG_RUNTIME_DIR")
        .map(|d| format!("{d}/podman-healthcheckd-status.json"))
        .unwrap_or_else(|_| "/run/podman-healthcheckd-status.json".to_string())
}

async fn write_status_file(status: &SharedStatus) {
    // Clone the data out while holding the lock only briefly,
    // then serialize/write off the async runtime's thread.
    let snapshot = {
        let map = status.lock().await;
        map.clone()
    };

    let result = tokio::task::spawn_blocking(move || {
        let json = serde_json::to_string_pretty(&snapshot)?;
        std::fs::write(status_path(), json)?;
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await;

    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => warn!("failed to write status file: {e}"),
        Err(e) => warn!("status file write task panicked: {e}"),
    }
}

fn lock_path() -> String {
    std::env::var("XDG_RUNTIME_DIR")
        .map(|d| format!("{d}/podman-healthcheckd.lock"))
        .unwrap_or_else(|_| "/tmp/podman-healthcheckd.lock".to_string())
}

/// Acquires an exclusive, non-blocking advisory lock to ensure only one
/// instance of the daemon runs at a time. The lock is held for the
/// lifetime of the returned File and is automatically released by the
/// kernel when the process exits or dies for any reason (including
// crashes or SIGKILL), unlike a hand-written PID file.
fn acquire_single_instance_lock() -> std::io::Result<std::fs::File> {
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(lock_path())?;

    let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if ret != 0 {
        return Err(std::io::Error::last_os_error());
    }

    Ok(file)
}


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
        .kill_on_drop(true)
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
        .kill_on_drop(true)
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

async fn run_healthcheck(id: &str, timeout: Duration) -> bool {
    let fut = Command::new("podman")
        .args(["healthcheck", "run", id])
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(timeout, fut).await {
        Ok(Ok(o)) => o.status.success(),
        Ok(Err(e)) => { warn!("[{id}] exec error: {e}"); false }
        Err(_) => { warn!("[{id}] healthcheck timed out after {timeout:.1?}"); false }
    }
}

// --- Nanosecond helpers ---

fn ns_to_duration(ns: u64) -> Duration {
    if ns == 0 {
        Duration::from_secs(30) // podman default
    } else {
        Duration::from_nanos(ns)
    }
}
async fn restart_container(id: &str) -> bool {
    let output = match Command::new("podman")
        .args(["restart", id])
        .kill_on_drop(true)
        .output()
        .await
    {
        Ok(o) => o,
        Err(e) => {
            error!("failed to exec podman restart {id}: {e}");
            return false;
        }
    };
    if !output.status.success() {
        error!(
            "podman restart {id} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        return false;
    }
    true
}

// --- Per-container healthcheck loop ---

async fn healthcheck_loop(
    id: String,
    name: String,
    config: HealthcheckConfig,
    token: CancellationToken,
    status: SharedStatus,
) {
    let start_period = Duration::from_nanos(config.start_period); // 0 means no wait (docker/podman default)
    let interval = ns_to_duration(config.interval);

    if !start_period.is_zero() {
        info!("[{name}] waiting {start_period:.1?} start period");
        tokio::select! {
            _ = tokio::time::sleep(start_period) => {}
            _ = token.cancelled() => return,
        }
    }
    let timeout = ns_to_duration(config.timeout);
    info!("[{name}] healthcheck active, interval {interval:.1?}");
    let mut consecutive_failures: u32 = 0;
    let mut restart_triggered = false;
    loop {
        tokio::select! {
            ok = run_healthcheck(&id,timeout) => {
                if ok {
		            if consecutive_failures >= config.retries.max(1) {
                        info!("[{name}] healthcheck recovered, now healthy");
                    }
                    consecutive_failures = 0;
                    restart_triggered = false;
	                info!("[{name}] healthcheck passed");	
                } else {
			        consecutive_failures += 1;
                    if consecutive_failures >= config.retries.max(1) {
                            error!("[{name}] healthcheck failed {consecutive_failures} times, container unhealthy");
                        if !restart_triggered {
                            restart_triggered = true;
                            warn!("[{name}] restarting unhealthy container");
                            if restart_container(&id).await {
                                info!("[{name}] restart succeeded, resetting failure count");
                                consecutive_failures = 0;
                                } else {
                                error!("[{name}] restart failed, will retry restart on next unhealthy threshold hit");
                                restart_triggered = false;
                                }		
                        }	
	    		    } else {
		        	    warn!("[{name}] healthcheck failed ({consecutive_failures}/{})", config.retries);
    			    }
                }
                let healthy = consecutive_failures < config.retries.max(1);
                let changed = {
                    let mut map = status.lock().await;
                    let changed = match map.get(&id) {
                        Some(prev) => prev.healthy != healthy
                            || prev.consecutive_failures != consecutive_failures,
                        None => true, // first time we see this container
                    };
                    map.insert(id.clone(), ContainerStatus {
                        name: name.clone(),
                        healthy,
                        consecutive_failures,
                        last_check: chrono::Utc::now().to_rfc3339(),
                    });
                    changed
                };
                if changed {
                    write_status_file(&status).await;
                }
            }
            _ = token.cancelled() => return,
        }
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = token.cancelled() => return,
        }
    }
}

// --- Task manager ---

struct TaskManager {
    tasks: HashMap<String, (String, JoinHandle<()>)>,
    token: CancellationToken,
    status: SharedStatus,
}

impl TaskManager {
    fn new(token: CancellationToken, status: SharedStatus) -> Self {
        Self {
            tasks: HashMap::new(),
            token,
            status,
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
            Duration::from_nanos(config.start_period),
            config.retries,
        );
        let cid = id.clone();
        let cname = name.clone();
        let ct = self.token.clone();
        let handle = tokio::spawn(healthcheck_loop(cid, cname, config, ct, self.status.clone()));
        self.tasks.insert(id, (name, handle));
    }

    fn stop_container(&mut self, id: &str) {
        if let Some((name, handle)) = self.tasks.remove(id) {
            info!("[{name}] removing healthcheck timer");
            handle.abort();
            let status = self.status.clone();
            let id = id.to_string();
            tokio::spawn(async move {
                {
                    let mut map = status.lock().await;
                    map.remove(&id);
                }
                write_status_file(&status).await;
            });
        }
    }

    fn stop_all(&mut self) {
        for (_, (name, handle)) in self.tasks.drain() {
            info!("[{name}] stopping");
            handle.abort();
        }
    }
    async fn clear_status(&self) {
        {
            let mut map = self.status.lock().await;
            map.clear();
        }
        write_status_file(&self.status).await;
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
        .kill_on_drop(true)
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
    let _lock = match acquire_single_instance_lock() {
        Ok(f) => f,
        Err(e) => {
            error!("another instance of podman-healthcheckd is already running (or lock file inaccessible): {e}");
            std::process::exit(1);
        }
    };
    let mut f = &_lock;
    if let Err(e) = write!(f, "{}", std::process::id()) {
        warn!("failed to write PID to lock file: {e}");
    }
    info!("podman-healthcheckd starting");

    let token = CancellationToken::new();
    let status: SharedStatus = Arc::new(AsyncMutex::new(HashMap::new()));
    let mut manager = TaskManager::new(token.clone(), status);
    

    // Start event watcher first so events are buffered during enumeration
    let (tx, mut rx) = mpsc::channel::<Action>(1024);//or unbounded_channel
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
    manager.clear_status().await;
    let _ = watcher.await;
    info!("podman-healthcheckd stopped");
}
