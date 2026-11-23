//! Plugin child-process lifecycle and the newline-delimited JSON transport.
//!
//! Each plugin is one long-lived child process. The host keeps its stdin and
//! stdout open and issues request/response pairs. A plugin that dies is
//! restarted lazily on the next request, so a single broken plugin never
//! affects the others.

use anyhow::{anyhow, Context, Result};
use inkbuddy_protocol::{
    Manifest, Method, PluginInfo, RenderParams, RenderResult, Request, Response, PROTOCOL_VERSION,
};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::time::{timeout, Duration};
use tracing::{debug, warn};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// One installed plugin: its manifest plus a lazily-started child process.
pub struct PluginProcess {
    pub manifest: Manifest,
    pub dir: PathBuf,
    /// Absolute path to the executable after resolving `command`.
    pub command: PathBuf,
    state: Mutex<Option<ChildState>>,
    timeout: Duration,
}

struct ChildState {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl PluginProcess {
    pub fn new(manifest: Manifest, dir: PathBuf) -> Self {
        let raw = Path::new(&manifest.plugin.command);
        let command = if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            dir.join(raw)
        };
        // A relative program path is ambiguous when the child also has a
        // `current_dir`: on macOS it is resolved against that directory, not
        // the host's cwd, so a manifest path like ../../target/release/x
        // would be applied twice and fail to spawn. Canonicalize to make it
        // absolute. If the binary is not built yet, keep the joined path so
        // spawn reports a clear error.
        let command = command.canonicalize().unwrap_or(command);
        let timeout_ms = manifest.runtime.timeout_ms.max(100);
        Self {
            manifest,
            dir,
            command,
            state: Mutex::new(None),
            timeout: Duration::from_millis(timeout_ms),
        }
    }

    pub fn id(&self) -> &str {
        &self.manifest.plugin.id
    }

    /// Spawn the child if it is not running (or if the previous one exited).
    async fn ensure_running(&self, state: &mut Option<ChildState>) -> Result<()> {
        if let Some(s) = state.as_mut() {
            // Non-blocking check: still alive means reuse it.
            if s.child.try_wait()?.is_none() {
                return Ok(());
            }
            debug!(plugin = self.id(), "child exited, restarting");
            *state = None;
        }

        let mut cmd = Command::new(&self.command);
        cmd.args(&self.manifest.runtime.args)
            .current_dir(&self.dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit()) // plugin diagnostics go to the host log
            .kill_on_drop(true);

        let mut child = cmd.spawn().with_context(|| {
            format!(
                "spawning plugin {} at {}",
                self.id(),
                self.command.display()
            )
        })?;

        let stdin = child.stdin.take().context("plugin stdin not piped")?;
        let stdout = child.stdout.take().context("plugin stdout not piped")?;
        *state = Some(ChildState {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        });
        debug!(plugin = self.id(), command = %self.command.display(), "plugin started");
        Ok(())
    }

    /// Send one request and read the matching response line.
    /// Non-JSON lines on stdout (accidental prints) are skipped.
    async fn call(&self, method: Method, params: Value) -> Result<Value> {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let req = Request { id, method, params };
        let line = serde_json::to_string(&req)?;

        let mut guard = self.state.lock().await;
        self.ensure_running(&mut guard).await?;

        let s = guard.as_mut().unwrap();
        let payload = format!("{line}\n");
        let write = s.stdin.write_all(payload.as_bytes());
        timeout(self.timeout, write)
            .await
            .context("plugin write timed out")?
            .context("writing to plugin stdin")?;
        let flush = s.stdin.flush();
        timeout(self.timeout, flush)
            .await
            .context("plugin flush timed out")?
            .context("flushing plugin stdin")?;

        // Read lines until we get a parseable Response with our id.
        loop {
            let mut buf = String::new();
            let read = s.stdout.read_line(&mut buf);
            let n = timeout(self.timeout, read)
                .await
                .context("plugin read timed out")?
                .context("reading plugin stdout")?;
            if n == 0 {
                return Err(anyhow!("plugin closed stdout"));
            }
            let trimmed = buf.trim();
            if trimmed.is_empty() {
                continue;
            }
            let resp: Response = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(_) => {
                    debug!(plugin = self.id(), line = %trimmed.chars().take(120).collect::<String>(), "ignoring non-protocol stdout line");
                    continue;
                }
            };
            if resp.id != id {
                continue;
            }
            if !resp.ok {
                return Err(anyhow!("{}", resp.error.unwrap_or_default()));
            }
            return Ok(resp.result.unwrap_or(Value::Null));
        }
    }

    /// Kept for the protocol: health checks and future plugin settings UI.
    #[allow(dead_code)]
    pub async fn ping(&self) -> Result<()> {
        self.call(Method::Ping, Value::Null).await?;
        Ok(())
    }

    pub async fn describe(&self) -> Result<PluginInfo> {
        let v = self.call(Method::Describe, Value::Null).await?;
        let info: PluginInfo = serde_json::from_value(v).context("parsing describe result")?;
        if info.protocol != PROTOCOL_VERSION {
            warn!(
                plugin = self.id(),
                got = info.protocol,
                expected = PROTOCOL_VERSION,
                "plugin protocol version mismatch"
            );
        }
        Ok(info)
    }

    pub async fn render(&self, params: &RenderParams) -> Result<RenderResult> {
        let v = self
            .call(Method::Render, serde_json::to_value(params)?)
            .await?;
        let result: RenderResult = serde_json::from_value(v).context("parsing render result")?;
        Ok(result)
    }

    /// Kept for the protocol: push host-side settings into a plugin.
    #[allow(dead_code)]
    pub async fn configure(&self, settings: Value) -> Result<()> {
        self.call(Method::Configure, settings).await?;
        Ok(())
    }

    pub async fn shutdown(&self) {
        if let Ok(v) = self.call(Method::Shutdown, Value::Null).await {
            let _ = v;
        }
        let mut guard = self.state.lock().await;
        if let Some(mut s) = guard.take() {
            let _ = s.child.start_kill();
        }
    }
}

/// All discovered plugins, keyed by id.
pub struct PluginSet {
    pub plugins: HashMap<String, Arc<PluginProcess>>,
    /// Stable display order (directory scan order).
    pub order: Vec<String>,
}

/// A scanned manifest, before any process is started.
struct Found {
    manifest: Manifest,
    dir: PathBuf,
}

impl PluginSet {
    pub fn get(&self, id: &str) -> Option<Arc<PluginProcess>> {
        self.plugins.get(id).cloned()
    }

    /// Reconcile with a fresh directory scan. Plug-ins that were added or
    /// whose manifest changed are (re)created; removed ones are dropped,
    /// which kills their child process via `kill_on_drop`.
    /// Returns true when anything changed.
    pub fn reconcile(&mut self, found: Vec<(Manifest, PathBuf)>) -> bool {
        let mut fresh: HashMap<String, Found> = HashMap::new();
        let mut order = Vec::new();
        for (manifest, dir) in found {
            let id = manifest.plugin.id.clone();
            if fresh.contains_key(&id) {
                continue;
            }
            order.push(id.clone());
            fresh.insert(id, Found { manifest, dir });
        }

        let mut changed = false;
        let mut next: HashMap<String, Arc<PluginProcess>> = HashMap::new();
        for id in &order {
            let f = fresh.get(id).unwrap();
            if let Some(existing) = self.plugins.get(id) {
                let same = existing.manifest.plugin.command == f.manifest.plugin.command
                    && existing.manifest.runtime == f.manifest.runtime
                    && existing.dir == f.dir;
                if same {
                    next.insert(id.clone(), existing.clone());
                    continue;
                }
                debug!(plugin = %id, "manifest changed, recreating plugin");
            }
            changed = true;
            next.insert(
                id.clone(),
                Arc::new(PluginProcess::new(f.manifest.clone(), f.dir.clone())),
            );
        }
        if self.order != order {
            changed = true;
        }
        if changed {
            self.plugins = next;
            self.order = order;
        }
        changed
    }

    /// Ids that are enabled. Used when the visitor does not pass `?plugins=`.
    pub fn default_ids(&self) -> Vec<String> {
        self.order
            .iter()
            .filter(|id| {
                self.plugins
                    .get(*id)
                    .map(|p| p.manifest.runtime.default_on && p.manifest.runtime.enabled)
                    .unwrap_or(false)
            })
            .cloned()
            .collect()
    }

    pub async fn shutdown_all(&self) {
        for id in &self.order {
            if let Some(p) = self.plugins.get(id) {
                p.shutdown().await;
            }
        }
    }
}

/// Scan directories for `plugin.toml` files. Later directories do not
/// override an id that was already found (first match wins).
pub fn scan(dirs: &[PathBuf]) -> Vec<(Manifest, PathBuf)> {
    let mut found = Vec::new();

    for dir in dirs {
        let entries = match std::fs::read_dir(dir) {
            Ok(v) => v,
            Err(_) => continue, // missing dir is fine
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let manifest_file = path.join("plugin.toml");
            if !manifest_file.exists() {
                continue;
            }
            let text = match std::fs::read_to_string(&manifest_file) {
                Ok(v) => v,
                Err(e) => {
                    warn!(file = %manifest_file.display(), error = %e, "cannot read plugin.toml");
                    continue;
                }
            };
            let manifest: Manifest = match toml::from_str(&text) {
                Ok(v) => v,
                Err(e) => {
                    warn!(file = %manifest_file.display(), error = %e, "invalid plugin.toml");
                    continue;
                }
            };
            // Keep an absolute directory: the child's `current_dir` and the
            // relative `command` in the manifest must not depend on where
            // the host process happened to be started.
            let dir = path.canonicalize().unwrap_or(path);
            debug!(plugin = %manifest.plugin.id, dir = %dir.display(), "discovered plugin");
            found.push((manifest, dir));
        }
    }

    found
}

pub fn discover(dirs: &[PathBuf]) -> Result<PluginSet> {
    let mut set = PluginSet {
        plugins: HashMap::new(),
        order: Vec::new(),
    };
    set.reconcile(scan(dirs));
    Ok(set)
}
