//! The one admission boundary and the accept loop (docs/M6-SURFACE.md).
//!
//! Every request through every door — SQL, typed, MCP — is parsed, charged against its tenant's
//! windowed quota (Prism's discipline), and enqueued on the tenant's bounded queue; one worker
//! thread per tenant owns that tenant's plane and executes serially. Fairness is structural: a
//! loud tenant fills its own queue and answers `Overloaded`; it cannot occupy another tenant's
//! worker. Determinism per tenant is schweepd's one-thread-one-engine law, kept.

use crate::config::{Config, QuotaConfig, TenantConfig, QUARANTINE_NOTICE, SURFACE_VERSION};
use crate::metrics::{trace, Metrics};
use crate::plane::{PlaneError, TenantPlane, WriteRequest};
use schweep_server::wire::{respond, respond_error, ErrorKind};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// One parsed request: method, path, query, body, and the bearer token if any. A fork of the
/// parent wire reader that additionally captures `Authorization` — the operator door's key.
#[derive(Clone, Debug)]
pub struct HttpRequest {
    pub method: String,
    pub path: String,
    pub query: BTreeMap<String, String>,
    pub body: Vec<u8>,
    pub bearer: Option<String>,
}

pub fn read_request(stream: &TcpStream) -> std::io::Result<Option<HttpRequest>> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let mut parts = line.trim_end().split(' ');
    let method = parts.next().unwrap_or_default().to_owned();
    let target = parts.next().unwrap_or_default().to_owned();
    if method.is_empty() || target.is_empty() {
        return Ok(None);
    }
    let mut length = 0usize;
    let mut bearer = None;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 {
            break;
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap_or(0);
            }
            if name.eq_ignore_ascii_case("authorization") {
                bearer = value
                    .trim()
                    .strip_prefix("Bearer ")
                    .map(str::to_owned)
                    .or_else(|| Some(value.trim().to_owned()));
            }
        }
    }
    let mut body = vec![0u8; length];
    if length > 0 {
        reader.read_exact(&mut body)?;
    }
    let (path, raw_query) = match target.split_once('?') {
        Some((path, query)) => (path.to_owned(), query),
        None => (target.clone(), ""),
    };
    let mut query = BTreeMap::new();
    for pair in raw_query.split('&').filter(|p| !p.is_empty()) {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        query.insert(percent_decode(name), percent_decode(value));
    }
    Ok(Some(HttpRequest {
        method,
        path,
        query,
        body,
        bearer,
    }))
}

fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes.get(index) {
            Some(b'%') => {
                let hex = raw.get(index + 1..index + 3);
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    None => {
                        out.push(b'%');
                        index += 1;
                    }
                }
            }
            Some(byte) => {
                out.push(*byte);
                index += 1;
            }
            None => break,
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The Prism-discipline windowed quota: requests and bytes per rolling second. Public because
/// every door — including `--mcp-stdio` — must charge the same gate; a door without a window
/// is exactly the bypass tooth (a) exists to catch.
pub struct QuotaWindow {
    config: QuotaConfig,
    window_start: Instant,
    requests: u64,
    bytes: u64,
}

impl QuotaWindow {
    #[must_use]
    pub fn new(config: QuotaConfig) -> QuotaWindow {
        QuotaWindow {
            config,
            window_start: Instant::now(),
            requests: 0,
            bytes: 0,
        }
    }

    pub fn charge(&mut self, bytes: u64) -> Result<(), String> {
        if self.window_start.elapsed().as_millis() >= 1_000 {
            self.window_start = Instant::now();
            self.requests = 0;
            self.bytes = 0;
        }
        if self.requests + 1 > self.config.requests_per_sec {
            return Err(format!(
                "over the {} requests/sec quota; retry after the window",
                self.config.requests_per_sec
            ));
        }
        if self.bytes + bytes > self.config.bytes_per_sec {
            return Err(format!(
                "over the {} bytes/sec quota; retry after the window",
                self.config.bytes_per_sec
            ));
        }
        self.requests += 1;
        self.bytes += bytes;
        Ok(())
    }
}

/// What a worker sends back: text, raw bytes (`format=frames`), or a JSON value (MCP).
pub enum Reply {
    Text(String),
    Bytes(Vec<u8>),
    Json(serde_json::Value),
}

type JobResult = Result<Reply, (ErrorKind, String)>;

enum Job {
    Http {
        request: HttpRequest,
        door: &'static str,
        operator: bool,
        reply: SyncSender<JobResult>,
    },
    Mcp {
        request: serde_json::Value,
        reply: SyncSender<JobResult>,
    },
    /// Drain (structural), compact, checkpoint, close: the M7 sleep. The worker exits after.
    Sleep { reply: SyncSender<JobResult> },
    /// Teardown without a checkpoint promise (removal). The worker exits after.
    Shutdown { reply: SyncSender<JobResult> },
}

/// One registered tenant as the running server holds it: its config, its quota window, and —
/// only while awake — its worker's queue.
struct TenantRuntime {
    config: TenantConfig,
    quota: QuotaWindow,
    live: Option<SyncSender<Job>>,
}

struct Fleet {
    tenants: Mutex<BTreeMap<String, TenantRuntime>>,
    registry: Mutex<crate::fleet::FleetRegistry>,
    data_dir: std::path::PathBuf,
    embedding: crate::config::EmbeddingConfig,
    checkpoint_every: u64,
    metrics: Arc<Metrics>,
}

impl Fleet {
    fn publish_gauges(&self) {
        let (registered, resident) = self
            .tenants
            .lock()
            .map(|tenants| {
                (
                    tenants.len(),
                    tenants.values().filter(|t| t.live.is_some()).count(),
                )
            })
            .unwrap_or((0, 0));
        self.metrics
            .gauge("mutiny_fleet_registered", registered as i64);
        self.metrics.gauge("mutiny_fleet_resident", resident as i64);
    }

    /// Wake-on-delta's engine: return the tenant's live queue, waking it if it sleeps. Wakes are
    /// serialized fleet-wide (single-flight) — stated in docs/M7-FLEET.md.
    fn ensure_awake(&self, name: &str) -> Result<SyncSender<Job>, (ErrorKind, String)> {
        let mut tenants = self
            .tenants
            .lock()
            .map_err(|_| (ErrorKind::Internal, "fleet lock poisoned".to_owned()))?;
        let runtime = tenants
            .get_mut(name)
            .ok_or_else(|| (ErrorKind::NotFound, format!("unknown tenant {name:?}")))?;
        if let Some(sender) = &runtime.live {
            return Ok(sender.clone());
        }
        let state = self
            .registry
            .lock()
            .map_err(|_| (ErrorKind::Internal, "registry lock poisoned".to_owned()))?
            .rows
            .get(name)
            .map(|row| row.state)
            .ok_or_else(|| (ErrorKind::NotFound, format!("unregistered tenant {name:?}")))?;
        let config = runtime.config.clone();
        let opened = match state {
            crate::fleet::RowState::Asleep => TenantPlane::wake(
                &self.data_dir,
                &config,
                &self.embedding,
                self.checkpoint_every,
                Arc::clone(&self.metrics),
            ),
            crate::fleet::RowState::Awake => TenantPlane::open(
                &self.data_dir,
                &config,
                &self.embedding,
                self.checkpoint_every,
                Arc::clone(&self.metrics),
            ),
        };
        let plane = opened.map_err(|error| (error.kind(), error.to_string()))?;
        let (sender, receiver) = sync_channel::<Job>(config.quota.queue_depth);
        let name_owned = name.to_owned();
        let worker_metrics = Arc::clone(&self.metrics);
        std::thread::spawn(move || {
            worker(plane, receiver, &name_owned, &worker_metrics);
        });
        runtime.live = Some(sender.clone());
        self.metrics.inc(&format!(
            "mutiny_fleet_wakes_total{{path=\"{}\"}}",
            match state {
                crate::fleet::RowState::Asleep => "checkpoint",
                crate::fleet::RowState::Awake => "replay",
            }
        ));
        if state == crate::fleet::RowState::Asleep {
            let mut registry = self
                .registry
                .lock()
                .map_err(|_| (ErrorKind::Internal, "registry lock poisoned".to_owned()))?;
            if let Some(row) = registry.rows.get_mut(name) {
                row.state = crate::fleet::RowState::Awake;
            }
            let _ = registry.save();
        }
        drop(tenants);
        self.publish_gauges();
        trace("fleet_wake", &[("tenant", name.to_owned())]);
        Ok(sender)
    }

    /// Sleep one tenant: drain (queue order), compact, checkpoint, close. Idempotent.
    fn sleep_tenant(&self, name: &str) -> Result<String, (ErrorKind, String)> {
        // A registered-but-not-resident tenant is first woken so it can sleep WITH a checkpoint —
        // the contract records asleep rows as bounded-wakeable, and a row without a checkpoint
        // must never claim to be.
        let sender = self.ensure_awake(name)?;
        let (reply_sender, reply_receiver) = sync_channel::<JobResult>(1);
        sender
            .send(Job::Sleep {
                reply: reply_sender,
            })
            .map_err(|_| (ErrorKind::Internal, "the worker is gone".to_owned()))?;
        let outcome = reply_receiver.recv().map_err(|_| {
            (
                ErrorKind::Internal,
                "the worker dropped the sleep".to_owned(),
            )
        })?;
        match outcome {
            Ok(Reply::Text(report)) => {
                if let Ok(mut tenants) = self.tenants.lock() {
                    if let Some(runtime) = tenants.get_mut(name) {
                        runtime.live = None;
                    }
                }
                if let Ok(mut registry) = self.registry.lock() {
                    if let Some(row) = registry.rows.get_mut(name) {
                        row.state = crate::fleet::RowState::Asleep;
                    }
                    let _ = registry.save();
                }
                self.publish_gauges();
                trace("fleet_sleep", &[("tenant", name.to_owned())]);
                Ok(report)
            }
            Ok(_) => Err((ErrorKind::Internal, "unexpected sleep reply".to_owned())),
            Err(error) => Err(error),
        }
    }

    fn register_tenant(&self, config: TenantConfig) -> Result<(), (ErrorKind, String)> {
        crate::config::validate_tenant(&config)
            .map_err(|error| (ErrorKind::Refused, error.to_string()))?;
        let mut tenants = self
            .tenants
            .lock()
            .map_err(|_| (ErrorKind::Internal, "fleet lock poisoned".to_owned()))?;
        if tenants.contains_key(&config.name) {
            return Err((
                ErrorKind::Rejected,
                format!("tenant {:?} is already registered", config.name),
            ));
        }
        let mut registry = self
            .registry
            .lock()
            .map_err(|_| (ErrorKind::Internal, "registry lock poisoned".to_owned()))?;
        registry.rows.insert(
            config.name.clone(),
            crate::fleet::FleetRow {
                config: config.clone(),
                state: crate::fleet::RowState::Awake,
            },
        );
        registry
            .save()
            .map_err(|error| (ErrorKind::Internal, error.to_string()))?;
        tenants.insert(
            config.name.clone(),
            TenantRuntime {
                quota: QuotaWindow::new(config.quota),
                config,
                live: None,
            },
        );
        drop(registry);
        drop(tenants);
        self.publish_gauges();
        Ok(())
    }

    /// Removal is teardown with byte accounting (the M5 rewind discipline, fleet edition):
    /// worker stopped, directory deleted, registry row gone. Returns the bytes freed.
    fn remove_tenant(&self, name: &str) -> Result<u64, (ErrorKind, String)> {
        let mut tenants = self
            .tenants
            .lock()
            .map_err(|_| (ErrorKind::Internal, "fleet lock poisoned".to_owned()))?;
        let runtime = tenants
            .get_mut(name)
            .ok_or_else(|| (ErrorKind::NotFound, format!("unknown tenant {name:?}")))?;
        if let Some(sender) = runtime.live.take() {
            let (reply_sender, reply_receiver) = sync_channel::<JobResult>(1);
            let _ = sender.send(Job::Shutdown {
                reply: reply_sender,
            });
            let _ = reply_receiver.recv();
        }
        tenants.remove(name);
        let mut registry = self
            .registry
            .lock()
            .map_err(|_| (ErrorKind::Internal, "registry lock poisoned".to_owned()))?;
        registry.rows.remove(name);
        registry
            .save()
            .map_err(|error| (ErrorKind::Internal, error.to_string()))?;
        drop(registry);
        drop(tenants);
        let dir = self.data_dir.join(name);
        let freed = dir_bytes(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        self.publish_gauges();
        trace("fleet_remove", &[("tenant", name.to_owned())]);
        Ok(freed)
    }

    fn status(&self) -> String {
        let mut out = String::new();
        let registry = self.registry.lock();
        let tenants = self.tenants.lock();
        if let (Ok(registry), Ok(tenants)) = (registry, tenants) {
            let resident = tenants.values().filter(|t| t.live.is_some()).count();
            out.push_str(&format!(
                "registered {}\nresident {}\n",
                registry.rows.len(),
                resident
            ));
            for (name, row) in &registry.rows {
                let live = tenants.get(name).map(|t| t.live.is_some()).unwrap_or(false);
                out.push_str(&format!(
                    "tenant {name} state {:?} resident {live}\n",
                    row.state
                ));
            }
        }
        out
    }
}

fn dir_bytes(dir: &std::path::Path) -> u64 {
    let mut total = 0;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                total += dir_bytes(&path);
            } else if let Ok(meta) = entry.metadata() {
                total += meta.len();
            }
        }
    }
    total
}

/// The server: one listener, the fleet, one metrics registry.
pub struct MutinyServer {
    listener: TcpListener,
    fleet: Arc<Fleet>,
    metrics: Arc<Metrics>,
    operator_token: String,
    running: Arc<AtomicBool>,
}

impl MutinyServer {
    pub fn bind(config: &Config) -> Result<MutinyServer, PlaneError> {
        let listener = TcpListener::bind(&config.listen)
            .map_err(|e| PlaneError::Internal(format!("bind {}: {e}", config.listen)))?;
        let metrics = Arc::new(Metrics::default());
        let registry = crate::fleet::FleetRegistry::load_or_seed(&config.data_dir, config)
            .map_err(|e| PlaneError::Internal(e.to_string()))?;
        let mut tenants = BTreeMap::new();
        for (name, row) in &registry.rows {
            tenants.insert(
                name.clone(),
                TenantRuntime {
                    quota: QuotaWindow::new(row.config.quota),
                    config: row.config.clone(),
                    live: None,
                },
            );
        }
        let fleet = Arc::new(Fleet {
            tenants: Mutex::new(tenants),
            registry: Mutex::new(registry),
            data_dir: config.data_dir.clone(),
            embedding: config.embedding.clone(),
            checkpoint_every: config.checkpoint_every,
            metrics: Arc::clone(&metrics),
        });
        fleet.publish_gauges();
        Ok(MutinyServer {
            listener,
            fleet,
            metrics,
            operator_token: config.operator_token.clone(),
            running: Arc::new(AtomicBool::new(true)),
        })
    }

    pub fn address(&self) -> Result<SocketAddr, PlaneError> {
        self.listener
            .local_addr()
            .map_err(|e| PlaneError::Internal(e.to_string()))
    }

    #[must_use]
    pub fn metrics(&self) -> Arc<Metrics> {
        Arc::clone(&self.metrics)
    }

    #[must_use]
    pub fn running_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.running)
    }

    /// Serve until `/shutdown` (operator). Tenants wake on demand — including on the first
    /// request after a restart, which is how a crashed mutinyd recovers its fleet lazily.
    pub fn serve(self) -> Result<(), PlaneError> {
        while self.running.load(Ordering::SeqCst) {
            let (stream, _) = match self.listener.accept() {
                Ok(pair) => pair,
                Err(_) => break,
            };
            let fleet = Arc::clone(&self.fleet);
            let metrics = Arc::clone(&self.metrics);
            let operator_token = self.operator_token.clone();
            let running = Arc::clone(&self.running);
            std::thread::spawn(move || {
                handle_connection(stream, &fleet, &metrics, &operator_token, &running);
            });
        }
        Ok(())
    }
}

fn handle_connection(
    mut stream: TcpStream,
    fleet: &Arc<Fleet>,
    metrics: &Arc<Metrics>,
    operator_token: &str,
    running: &Arc<AtomicBool>,
) {
    let request = match read_request(&stream) {
        Ok(Some(request)) => request,
        _ => return,
    };
    let operator = request.bearer.as_deref() == Some(operator_token);

    // ---- unadmitted ops endpoints: metrics, fleet control, shutdown ---------------------------
    if request.method == "GET" && request.path == "/metrics" {
        let _ = respond(&mut stream, metrics.render().as_bytes());
        return;
    }
    if request.path.starts_with("/fleet/") || request.path == "/shutdown" {
        if !operator {
            let _ = respond_error(
                &mut stream,
                ErrorKind::Rejected,
                "fleet control is operator-only: present the operator bearer token",
            );
            return;
        }
        let answer: Result<String, (ErrorKind, String)> =
            match (request.method.as_str(), request.path.as_str()) {
                ("POST", "/shutdown") => {
                    // Graceful shutdown = fleet-wide sleep: every resident tenant drains,
                    // compacts, and checkpoints, so the restart wakes bounded.
                    let names: Vec<String> = fleet
                        .tenants
                        .lock()
                        .map(|tenants| {
                            tenants
                                .iter()
                                .filter(|(_, t)| t.live.is_some())
                                .map(|(name, _)| name.clone())
                                .collect()
                        })
                        .unwrap_or_default();
                    let mut report = String::from("shutdown\n");
                    for name in names {
                        match fleet.sleep_tenant(&name) {
                            Ok(drained) => report.push_str(&format!("tenant {name}\n{drained}")),
                            Err((_, message)) => {
                                report.push_str(&format!("tenant {name} sleep failed: {message}\n"))
                            }
                        }
                    }
                    running.store(false, Ordering::SeqCst);
                    if let Ok(address) = stream.local_addr() {
                        let _ = TcpStream::connect(address);
                    }
                    Ok(report)
                }
                ("POST", "/fleet/register") => {
                    match serde_json::from_slice::<TenantConfig>(&request.body) {
                        Ok(config) => {
                            let name = config.name.clone();
                            fleet
                                .register_tenant(config)
                                .map(|()| format!("registered {name}\n"))
                        }
                        Err(error) => Err((
                            ErrorKind::Refused,
                            format!("the body is not a TenantConfig: {error}"),
                        )),
                    }
                }
                ("POST", "/fleet/sleep") => body_tenant(&request)
                    .and_then(|name| fleet.sleep_tenant(&name).map(|r| format!("asleep\n{r}"))),
                ("POST", "/fleet/wake") => body_tenant(&request)
                    .and_then(|name| fleet.ensure_awake(&name).map(|_| format!("awake {name}\n"))),
                ("POST", "/fleet/remove") => body_tenant(&request).and_then(|name| {
                    fleet
                        .remove_tenant(&name)
                        .map(|freed| format!("removed {name}\nfreed_bytes {freed}\n"))
                }),
                ("GET", "/fleet/status") => Ok(fleet.status()),
                ("GET", "/fleet/mapping") => {
                    let tenant = request.query.get("tenant").cloned().unwrap_or_default();
                    let looked = fleet
                        .tenants
                        .lock()
                        .ok()
                        .and_then(|tenants| tenants.get(&tenant).map(|t| t.config.clone()));
                    match looked {
                        // Observed WITHOUT waking: the mapping reads the compute plane's persisted
                        // registration file and binds through its public binder (MD-1 R2).
                        Some(config) => crate::plane::circuit_mapping_for(
                            &fleet.data_dir.join(&tenant).join("compute"),
                            &config,
                        )
                        .map(|mapping| {
                            let mut out = String::new();
                            for (circuit, tables) in mapping {
                                out.push_str(&format!(
                                    "{circuit}: {}\n",
                                    tables.into_iter().collect::<Vec<_>>().join(",")
                                ));
                            }
                            out
                        })
                        .map_err(|error| (error.kind(), error.to_string())),
                        None => Err((ErrorKind::NotFound, format!("unknown tenant {tenant:?}"))),
                    }
                }
                _ => Err((
                    ErrorKind::NotFound,
                    format!("no fleet route {} {}", request.method, request.path),
                )),
            };
        match answer {
            Ok(body) => {
                let _ = respond(&mut stream, body.as_bytes());
            }
            Err((kind, message)) => {
                let _ = respond_error(&mut stream, kind, &message);
            }
        }
        return;
    }

    // ---- tenant-scoped: /v1/<tenant>/... -------------------------------------------------------
    let segments: Vec<&str> = request.path.split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() < 3 || segments[0] != "v1" {
        let _ = respond_error(
            &mut stream,
            ErrorKind::NotFound,
            &format!("no route {} {}", request.method, request.path),
        );
        return;
    }
    let tenant = segments[1].to_owned();
    let door: &'static str = match segments[2] {
        "sql" => "sql",
        "mcp" => "mcp",
        _ => "typed",
    };

    let is_operator_route = matches!(segments.get(2).copied(), Some("action"))
        && segments.get(3).copied() == Some("execute")
        || segments.get(2).copied() == Some("taint");
    if is_operator_route && !operator {
        let _ = respond_error(
            &mut stream,
            ErrorKind::Rejected,
            "this route is operator-only: present the operator bearer token",
        );
        return;
    }

    // ---- the admission boundary ----------------------------------------------------------------
    {
        let mut tenants = match fleet.tenants.lock() {
            Ok(tenants) => tenants,
            Err(_) => {
                let _ = respond_error(&mut stream, ErrorKind::Internal, "fleet lock poisoned");
                return;
            }
        };
        let Some(runtime) = tenants.get_mut(&tenant) else {
            let _ = respond_error(
                &mut stream,
                ErrorKind::NotFound,
                &format!("unknown tenant {tenant:?}"),
            );
            return;
        };
        if let Err(reason) = runtime.quota.charge(request.body.len() as u64) {
            metrics.inc(&format!(
                "mutiny_refused_total{{tenant=\"{tenant}\",door=\"{door}\",kind=\"Overloaded\"}}"
            ));
            let _ = respond_error(&mut stream, ErrorKind::Overloaded, &reason);
            return;
        }
    }

    // ---- wake-on-delta: an admitted request for a sleeping tenant wakes it --------------------
    let sender = match fleet.ensure_awake(&tenant) {
        Ok(sender) => sender,
        Err((kind, message)) => {
            let _ = respond_error(&mut stream, kind, &message);
            return;
        }
    };

    let (reply_sender, reply_receiver) = sync_channel::<JobResult>(1);
    let job = if door == "mcp" {
        match serde_json::from_slice::<serde_json::Value>(&request.body) {
            Ok(value) => Job::Mcp {
                request: value,
                reply: reply_sender,
            },
            Err(error) => {
                let _ = respond_error(
                    &mut stream,
                    ErrorKind::Refused,
                    &format!("the MCP body is not JSON: {error}"),
                );
                return;
            }
        }
    } else {
        Job::Http {
            request: request.clone(),
            door,
            operator,
            reply: reply_sender,
        }
    };

    match sender.try_send(job) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => {
            metrics.inc(&format!(
                "mutiny_refused_total{{tenant=\"{tenant}\",door=\"{door}\",kind=\"Overloaded\"}}"
            ));
            let _ = respond_error(
                &mut stream,
                ErrorKind::Overloaded,
                "the tenant's queue is full; retry",
            );
            return;
        }
        Err(TrySendError::Disconnected(_)) => {
            let _ = respond_error(
                &mut stream,
                ErrorKind::Internal,
                "the tenant worker is gone",
            );
            return;
        }
    }
    metrics.inc(&format!(
        "mutiny_admitted_total{{tenant=\"{tenant}\",door=\"{door}\"}}"
    ));

    match reply_receiver.recv() {
        Ok(Ok(Reply::Text(text))) => {
            let _ = respond(&mut stream, text.as_bytes());
        }
        Ok(Ok(Reply::Bytes(bytes))) => {
            let _ = respond(&mut stream, &bytes);
        }
        Ok(Ok(Reply::Json(value))) => {
            let _ = respond(&mut stream, value.to_string().as_bytes());
        }
        Ok(Err((kind, message))) => {
            metrics.inc(&format!(
                "mutiny_refused_total{{tenant=\"{tenant}\",door=\"{door}\",kind=\"{}\"}}",
                kind.name()
            ));
            let _ = respond_error(&mut stream, kind, &message);
        }
        Err(_) => {
            let _ = respond_error(
                &mut stream,
                ErrorKind::Internal,
                "the worker dropped the job",
            );
        }
    }
}

fn body_tenant(request: &HttpRequest) -> Result<String, (ErrorKind, String)> {
    serde_json::from_slice::<serde_json::Value>(&request.body)
        .ok()
        .and_then(|value| {
            value
                .get("tenant")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
        })
        .ok_or_else(|| {
            (
                ErrorKind::Refused,
                "the body must be JSON with a \"tenant\" field".to_owned(),
            )
        })
}

fn worker(plane: TenantPlane, receiver: Receiver<Job>, tenant: &str, metrics: &Arc<Metrics>) {
    let mut plane = Some(plane);
    while let Ok(job) = receiver.recv() {
        let Some(live) = plane.as_mut() else { break };
        match job {
            Job::Http {
                request,
                door,
                operator,
                reply,
            } => {
                let result = dispatch(live, &request, door, operator)
                    .map_err(|error| (error.kind(), error.to_string()));
                metrics.gauge(
                    &format!("mutiny_engine_epoch{{tenant=\"{tenant}\"}}"),
                    live.engine_epoch() as i64,
                );
                let _ = reply.send(result);
            }
            Job::Mcp { request, reply } => {
                let response = crate::mcp::handle(live, &request);
                let _ = reply.send(Ok(Reply::Json(response)));
            }
            Job::Sleep { reply } => {
                let taken = plane.take();
                let result = match taken {
                    Some(plane) => {
                        // The M6-SURFACE drain report is additive-only: epoch, pending_appends,
                        // and registrations stay; the checkpoint line is the M7 addition.
                        let health = plane.health();
                        let line = |name: &str| {
                            health
                                .lines()
                                .find(|l| l.starts_with(name))
                                .unwrap_or_default()
                                .to_owned()
                        };
                        let epoch = plane.engine_epoch();
                        let pending = line("pending_appends");
                        let registrations = line("registrations");
                        plane
                            .sleep()
                            .map(|()| {
                                Reply::Text(format!(
                                    "epoch {epoch}\n{pending}\n{registrations}\ncheckpointed true\n"
                                ))
                            })
                            .map_err(|error| (error.kind(), error.to_string()))
                    }
                    None => Err((ErrorKind::Internal, "no plane to sleep".to_owned())),
                };
                let _ = reply.send(result);
                break;
            }
            Job::Shutdown { reply } => {
                let taken = plane.take();
                let result = match taken {
                    Some(mut plane) => plane
                        .shutdown()
                        .map(|drained| {
                            Reply::Text(format!(
                                "epoch {}\npending_appends {}\nregistrations {}\n",
                                drained.epoch, drained.pending_appends, drained.registrations
                            ))
                        })
                        .map_err(|error| (error.kind(), error.to_string())),
                    None => Err((ErrorKind::Internal, "no plane to shut down".to_owned())),
                };
                let _ = reply.send(result);
                break;
            }
        }
    }
}

/// One typed/SQL request against the plane. Every arm is a thin translation; the door decides
/// nothing — that is the same-door law's foundation.
fn dispatch(
    plane: &mut TenantPlane,
    request: &HttpRequest,
    _door: &str,
    operator: bool,
) -> Result<Reply, PlaneError> {
    let segments: Vec<&str> = request.path.split('/').filter(|s| !s.is_empty()).collect();
    let tail = &segments[2..];
    let body_text = || {
        std::str::from_utf8(&request.body)
            .map(str::to_owned)
            .map_err(|_| PlaneError::Refused("the body is not UTF-8".to_owned()))
    };
    let param =
        |name: &str| {
            request.query.get(name).cloned().ok_or_else(|| {
                PlaneError::Refused(format!("the query parameter {name:?} is required"))
            })
        };
    let u64_param = |name: &str| {
        param(name)?
            .parse::<u64>()
            .map_err(|_| PlaneError::Refused(format!("{name:?} must be a number")))
    };
    let json_body = || {
        serde_json::from_slice::<serde_json::Value>(&request.body)
            .map_err(|e| PlaneError::Refused(format!("the body is not JSON: {e}")))
    };
    let field = |value: &serde_json::Value, name: &str| -> Result<String, PlaneError> {
        value
            .get(name)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| PlaneError::Refused(format!("the field {name:?} is required")))
    };

    match (request.method.as_str(), tail) {
        // ---- the SQL door and the typed query door: the same engine calls ---------------------
        ("POST", ["sql", "register"]) => {
            let sql = body_text()?;
            let unbounded = request.query.get("unbounded").cloned();
            Ok(Reply::Text(format!(
                "{}\n",
                plane.register(&sql, unbounded.as_deref())?
            )))
        }
        ("POST", ["query", "register"]) => {
            let body = json_body()?;
            let sql = field(&body, "sql")?;
            let unbounded = body
                .get("unbounded")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            Ok(Reply::Text(format!(
                "{}\n",
                plane.register(&sql, unbounded.as_deref())?
            )))
        }
        ("POST", ["sql", "deregister"] | ["query", "deregister"]) => {
            plane.deregister(u64_param("handle")?)?;
            Ok(Reply::Text("ok\n".to_owned()))
        }
        ("GET", ["sql", "read"] | ["query", "read"]) => {
            let handle = u64_param("handle")?;
            if request.query.get("format").map(String::as_str) == Some("frames") {
                return Ok(Reply::Bytes(plane.read_frames(handle)?));
            }
            let (epoch, answer) = plane.read(handle)?;
            Ok(Reply::Text(format!("epoch {epoch}\n{answer}")))
        }
        ("GET", ["sql", "oneshot"]) => {
            let sql = match request.query.get("sql") {
                Some(sql) => sql.clone(),
                None => body_text()?,
            };
            Ok(Reply::Text(plane.oneshot(&sql)?))
        }
        ("POST", ["query", "oneshot"]) => {
            let body = json_body()?;
            Ok(Reply::Text(plane.oneshot(&field(&body, "sql")?)?))
        }
        ("GET", ["sql", "subscribe"] | ["query", "subscribe"]) => {
            let (next, deltas) = plane.subscribe(u64_param("handle")?, u64_param("from")?)?;
            let mut out = format!("token {next}\nepochs {}\n", deltas.len());
            for delta in deltas {
                out.push_str(&format!("epoch {}\n{}", delta.epoch, delta.rendered));
            }
            Ok(Reply::Text(out))
        }
        ("GET", ["sql", "plan"] | ["query", "plan"]) => {
            Ok(Reply::Text(plane.plan_of(u64_param("handle")?)?))
        }
        ("GET", ["sql", "counters"] | ["query", "counters"]) => Ok(Reply::Text(plane.counters())),

        // ---- the typed door: writes and the branch lifecycle ----------------------------------
        ("POST", ["write"]) => {
            let body = json_body()?;
            let sources = body
                .get("sources")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| PlaneError::Refused("the field \"sources\" is required".to_owned()))?
                .iter()
                .map(|source| Ok((field(source, "system")?, field(source, "record")?)))
                .collect::<Result<Vec<_>, PlaneError>>()?;
            let rows = body
                .get("rows")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| PlaneError::Refused("the field \"rows\" is required".to_owned()))?
                .iter()
                .map(|row| {
                    row.as_array().cloned().ok_or_else(|| {
                        PlaneError::Refused("each row is an array of values".to_owned())
                    })
                })
                .collect::<Result<Vec<_>, PlaneError>>()?;
            let receipt = plane.write(&WriteRequest {
                actor: field(&body, "actor")?,
                session: field(&body, "session")?,
                branch: field(&body, "branch")?,
                intent: field(&body, "intent")?,
                sources,
                table: field(&body, "table")?,
                rows,
            })?;
            Ok(Reply::Text(format!(
                "commit {}\nepoch {}\nrows {}\n",
                receipt.commit_seq,
                receipt.epoch.map_or("none".to_owned(), |e| e.to_string()),
                receipt.rows
            )))
        }
        ("POST", ["session", "open"]) => {
            let body = json_body()?;
            let session = field(&body, "session")?;
            let token = plane.session_open(&session)?;
            Ok(Reply::Json(serde_json::json!({
                "session": session,
                "branch": session,
                "token": token,
            })))
        }
        ("POST", ["branch", "fork"]) => {
            let body = json_body()?;
            plane.fork(
                &field(&body, "session")?,
                &field(&body, "from")?,
                &field(&body, "child")?,
            )?;
            Ok(Reply::Text("ok\n".to_owned()))
        }
        ("POST", ["branch", "merge"]) => {
            let body = json_body()?;
            let merged = plane.merge(
                &field(&body, "session")?,
                &field(&body, "child")?,
                &field(&body, "into")?,
            )?;
            Ok(Reply::Text(format!("merged {merged}\n")))
        }
        ("POST", ["branch", "rewind"]) => {
            let body = json_body()?;
            let freed = plane.rewind(&field(&body, "session")?, &field(&body, "child")?)?;
            Ok(Reply::Text(format!("freed {freed}\n")))
        }
        ("GET", ["semantic", "answer"]) => {
            let hits = plane.semantic_answer(&param("branch")?, &param("query")?)?;
            let mut out = String::new();
            for hit in hits {
                out.push_str(&format!(
                    "{}. {} score={:.6}\n",
                    hit.rank, hit.key, hit.score
                ));
            }
            Ok(Reply::Text(out))
        }
        ("GET", ["semantic", "groups"]) => {
            let groups = plane.semantic_groups(&param("branch")?, &param("group")?)?;
            let mut out = String::new();
            for group in groups {
                out.push_str(&format!(
                    "group {}: count={} avg_cost={:.6} error_rate={:.6} exemplar={} members={}\n",
                    group.group_id,
                    group.count,
                    group.avg_cost,
                    group.error_rate,
                    group.exemplar_key,
                    group.member_keys.join(",")
                ));
            }
            Ok(Reply::Text(out))
        }
        ("POST", ["action", "propose"]) => {
            let body = json_body()?;
            let justified: Vec<String> = body
                .get("justified_by")
                .and_then(serde_json::Value::as_array)
                .map(|keys| {
                    keys.iter()
                        .filter_map(serde_json::Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            let key = plane.propose(
                &field(&body, "actor")?,
                &field(&body, "branch")?,
                &field(&body, "action_type")?,
                &field(&body, "target")?,
                &field(&body, "idempotency_key")?,
                &justified,
            )?;
            Ok(Reply::Text(format!("proposed {key}\n")))
        }
        ("POST", ["action", "execute"]) => {
            // Reached only with the operator bearer token; the route guard upstream enforced it,
            // and this assertion keeps the law structural rather than positional.
            if !operator {
                return Err(PlaneError::Rejected(
                    "action.execute is operator-only".to_owned(),
                ));
            }
            let body = json_body()?;
            let record = plane.execute(&field(&body, "proposal")?)?;
            Ok(Reply::Text(format!(
                "action {}\nstatus {:?}\nreceipt {}\n",
                record.id.as_str(),
                record.status,
                record.receipt().unwrap_or("none")
            )))
        }
        ("POST", ["taint"]) => {
            if !operator {
                return Err(PlaneError::Rejected("taint is operator-only".to_owned()));
            }
            let body = json_body()?;
            let outcome = plane.taint(&field(&body, "system")?, &field(&body, "record")?)?;
            Ok(Reply::Text(format!(
                "resolved {}\nsemantic_healed {}\n\n{}",
                outcome.resolved, outcome.semantic_healed, outcome.report
            )))
        }
        ("GET", ["health"]) => Ok(Reply::Text(plane.health())),
        ("GET", ["explain-state"]) => Ok(Reply::Text(plane.explain_state()?)),
        ("GET", ["explain-maintenance"]) => Ok(Reply::Text(plane.explain_maintenance())),
        _ => Err(PlaneError::NotFound(format!(
            "no route {} {}",
            request.method, request.path
        ))),
    }
}

/// The banner every door reports.
#[must_use]
pub fn banner() -> String {
    format!("mutinyd {SURFACE_VERSION} — {QUARANTINE_NOTICE}")
}
