//! Plugin child-process lifecycle and the newline-delimited JSON transport.
//!
//! Each plugin is one long-lived child process. The host keeps its stdin and
//! stdout open and issues request/response pairs. A plugin that dies is
//! restarted lazily on the next request, so a single broken plugin never
//! affects the others.

use anyhow::{anyhow, Context, Result};
use inkbuddy_protocol::{
    Block, Manifest, Method, PluginInfo, RenderParams, RenderResult, Request, Response,
    PROTOCOL_VERSION,
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
use tokio::time::{timeout_at, Duration, Instant};
use tracing::{debug, warn};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Cap on a single NDJSON line in either direction. A plugin that writes a
/// larger line is treated as broken and its process is replaced.
const MAX_LINE_BYTES: usize = 1024 * 1024;
/// Cap on non-protocol / stale-id stdout lines skipped in one call, so a
/// chatty plugin releases the per-plugin lock in bounded time.
const MAX_SKIPPED_LINES: usize = 64;
const MAX_BLOCKS: usize = 256;
const MAX_TEXT_BYTES: usize = 64 * 1024;

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
    ///
    /// One deadline covers the lock wait, the spawn, the write and the whole
    /// read loop. Any failure discards the child process and its pipes: a
    /// cancelled `write_all` may have left a partial line in stdin, and an
    /// unread response may still be sitting in stdout, so reusing the same
    /// pipes would make the next request silently fail.
    async fn call(&self, method: Method, params: Value) -> Result<Value> {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let req = Request { id, method, params };
        let line = serde_json::to_string(&req)?;
        if line.len() > MAX_LINE_BYTES {
            return Err(anyhow!("request line exceeds 1 MiB"));
        }

        let deadline = Instant::now() + self.timeout;
        let mut guard = timeout_at(deadline, self.state.lock())
            .await
            .context("plugin state lock timed out")?;

        let outcome = timeout_at(deadline, self.call_locked(&mut guard, &line, id)).await;
        match outcome {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => {
                self.poison(&mut guard).await;
                Err(e)
            }
            Err(_) => {
                self.poison(&mut guard).await;
                Err(anyhow!("plugin call timed out"))
            }
        }
    }

    /// The part of `call` that runs while holding the per-plugin state lock.
    async fn call_locked(
        &self,
        state: &mut Option<ChildState>,
        line: &str,
        id: u64,
    ) -> Result<Value> {
        self.ensure_running(state).await?;
        let s = state.as_mut().unwrap();

        let payload = format!("{line}\n");
        s.stdin
            .write_all(payload.as_bytes())
            .await
            .context("writing to plugin stdin")?;
        s.stdin.flush().await.context("flushing plugin stdin")?;

        let mut skipped = 0usize;
        loop {
            let mut buf: Vec<u8> = Vec::new();
            let n = s
                .stdout
                .read_until(b'\n', &mut buf)
                .await
                .context("reading plugin stdout")?;
            if n == 0 {
                return Err(anyhow!("plugin closed stdout"));
            }
            if buf.len() > MAX_LINE_BYTES {
                return Err(anyhow!("plugin response line exceeds 1 MiB"));
            }
            let text = String::from_utf8_lossy(&buf);
            let trimmed = text.trim();
            if trimmed.is_empty() {
                continue;
            }
            let resp: Response = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(_) => {
                    debug!(
                        plugin = self.id(),
                        line = %trimmed.chars().take(120).collect::<String>(),
                        "ignoring non-protocol stdout line"
                    );
                    skipped += 1;
                    if skipped > MAX_SKIPPED_LINES {
                        return Err(anyhow!("too many non-protocol stdout lines"));
                    }
                    continue;
                }
            };
            if resp.id != id {
                skipped += 1;
                if skipped > MAX_SKIPPED_LINES {
                    return Err(anyhow!("too many stale responses on stdout"));
                }
                continue;
            }
            if !resp.ok {
                return Err(anyhow!("{}", resp.error.unwrap_or_default()));
            }
            let result = resp.result.unwrap_or(Value::Null);
            if serde_json::to_string(&result)?.len() > MAX_LINE_BYTES {
                return Err(anyhow!("plugin result exceeds 1 MiB"));
            }
            return Ok(result);
        }
    }

    /// Kill and reap the child, leaving `state` empty so the next call gets a
    /// fresh process with clean pipes.
    async fn poison(&self, state: &mut Option<ChildState>) {
        if let Some(mut s) = state.take() {
            let _ = s.child.start_kill();
            let _ = s.child.wait().await;
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
        Ok(parse_render_result(self.id(), v))
    }

    /// Kept for the protocol: push host-side settings into a plugin.
    #[allow(dead_code)]
    pub async fn configure(&self, settings: Value) -> Result<()> {
        self.call(Method::Configure, settings).await?;
        Ok(())
    }

    pub async fn shutdown(&self) {
        // Only ask a live child to exit. Calling `call` on an unstarted plugin
        // would spawn a process just to immediately kill it.
        let live = {
            let mut guard = self.state.lock().await;
            guard
                .as_mut()
                .map(|s| s.child.try_wait().ok().flatten().is_none())
                .unwrap_or(false)
        };
        if live {
            let _ = self.call(Method::Shutdown, Value::Null).await;
        }
        let mut guard = self.state.lock().await;
        if let Some(mut s) = guard.take() {
            if s.child.try_wait().ok().flatten().is_none() {
                let _ = s.child.start_kill();
            }
            let _ = s.child.wait().await;
        }
    }
}

/// Parse a render response block-by-block. A malformed block is skipped and
/// the rest of that plugin's frame is kept.
fn parse_render_result(plugin_id: &str, raw: Value) -> RenderResult {
    let mut blocks = Vec::new();
    match raw.get("blocks").and_then(|v| v.as_array()) {
        Some(items) => {
            for (i, item) in items.iter().take(MAX_BLOCKS).enumerate() {
                match serde_json::from_value::<Block>(item.clone()) {
                    // `#[serde(other)]` variants are deserialise-only.
                    Ok(Block::Unknown) => {
                        debug!(plugin = plugin_id, block = i, "unknown block type skipped")
                    }
                    Ok(b) => match serde_json::to_string(&b) {
                        Ok(s) if s.len() <= MAX_TEXT_BYTES => blocks.push(b),
                        Ok(_) => warn!(plugin = plugin_id, block = i, "block too large, skipped"),
                        Err(e) => {
                            warn!(plugin = plugin_id, block = i, error = %e, "block not serializable, skipped")
                        }
                    },
                    Err(e) => warn!(
                        plugin = plugin_id,
                        block = i,
                        error = %e,
                        "invalid block skipped"
                    ),
                }
            }
        }
        None => warn!(plugin = plugin_id, "render result has no blocks array"),
    }
    let refresh_ms = raw.get("refresh_ms").and_then(|v| v.as_u64());
    RenderResult { blocks, refresh_ms }
}

/// All discovered plugins, keyed by id.
pub struct PluginSet {
    pub plugins: HashMap<String, Arc<PluginProcess>>,
    /// Stable display order (directory scan order).
    pub order: Vec<String>,
    /// Processes replaced or removed by `reconcile`. The caller drains this
    /// after releasing the write lock and shuts each one down, so an in-flight
    /// request keeps its original process until it finishes.
    pub pending_shutdown: Vec<Arc<PluginProcess>>,
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
                // Compare the whole manifest: changing name/version/
                // description must also refresh what /manage displays.
                let same = existing.manifest == f.manifest && existing.dir == f.dir;
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
            let previous: Vec<(String, Arc<PluginProcess>)> = self.plugins.drain().collect();
            let mut retired = Vec::new();
            for (id, old) in previous {
                match next.get(&id) {
                    Some(new) if Arc::ptr_eq(&old, new) => {}
                    _ => retired.push(old),
                }
            }
            self.pending_shutdown.extend(retired);
            self.plugins = next;
            self.order = order;
        }
        changed
    }

    /// Ids that are enabled. Used when the visitor omits `?plugins=`.
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

    /// Kept for callers that prefer to shut the whole set down at once.
    #[allow(dead_code)]
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
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                warn!(dir = %dir.display(), error = %e, "cannot scan plugin directory");
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(v) => v,
                Err(e) => {
                    warn!(dir = %dir.display(), error = %e, "cannot read directory entry");
                    continue;
                }
            };
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
            let id = &manifest.plugin.id;
            let id_ok = !id.is_empty()
                && id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
            if !id_ok || manifest.plugin.command.trim().is_empty() {
                warn!(file = %manifest_file.display(), id = %id, "invalid plugin id or empty command, skipped");
                continue;
            }
            // Keep an absolute directory: the child's `current_dir` and the
            // 相对 `command` 的解析固定以清单所在目录为基准，与宿主进程的
            // 启动目录无关。
            let dir = match path.canonicalize() {
                Ok(v) => v,
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "cannot resolve plugin directory, skipped");
                    continue;
                }
            };
            debug!(plugin = %id, dir = %dir.display(), "discovered plugin");
            found.push((manifest, dir));
        }
    }

    // read_dir 顺序不固定；排序让重复 id 的取舍保持稳定，避免重新扫描时
    // 换胜出的插件并重启进程。
    found.sort_by(|a, b| {
        a.0.plugin
            .id
            .cmp(&b.0.plugin.id)
            .then_with(|| a.1.cmp(&b.1))
    });
    found
}

pub fn discover(dirs: &[PathBuf]) -> Result<PluginSet> {
    let mut set = PluginSet {
        plugins: HashMap::new(),
        order: Vec::new(),
        pending_shutdown: Vec::new(),
    };
    set.reconcile(scan(dirs));
    set.pending_shutdown.clear(); // nothing was ever started
    Ok(set)
}

#[cfg(test)]
mod tests {
    use super::*;
    use inkbuddy_protocol::{PluginManifest, RuntimeManifest};

    fn manifest(id: &str, name: &str) -> Manifest {
        Manifest {
            plugin: PluginManifest {
                id: id.to_string(),
                name: name.to_string(),
                version: "0.1.0".to_string(),
                description: String::new(),
                command: "./plugin".to_string(),
            },
            runtime: RuntimeManifest::default(),
        }
    }

    #[test]
    fn render_result_skips_bad_blocks_but_keeps_good_ones() {
        let raw = serde_json::json!({
            "blocks": [
                {"type": "text", "text": "hello"},
                {"type": "future_widget", "x": 1},
                {"type": "lines", "lines": [
                    {"text": "hi", "emphasis": "bogus", "ruby": []}
                ]},
                {"type": "lines", "lines": "not-an-array"}
            ],
            "refresh_ms": 750
        });
        let r = parse_render_result("demo", raw);
        // text + lines; unknown type and malformed lines are skipped.
        assert_eq!(r.blocks.len(), 2);
        assert_eq!(r.refresh_ms, Some(750));
    }

    #[test]
    fn missing_blocks_array_yields_empty_frame() {
        let r = parse_render_result("demo", serde_json::json!({"refresh_ms": 500}));
        assert!(r.blocks.is_empty());
        assert_eq!(r.refresh_ms, Some(500));
    }

    #[test]
    fn reconcile_detects_display_field_change_and_collects_retired() {
        let dir = PathBuf::from("/tmp/inkbuddy-review-test");
        let mut set = PluginSet {
            plugins: HashMap::new(),
            order: Vec::new(),
            pending_shutdown: Vec::new(),
        };
        assert!(set.reconcile(vec![(manifest("demo", "Old"), dir.clone())]));
        set.pending_shutdown.clear();
        assert!(!set.reconcile(vec![(manifest("demo", "Old"), dir.clone())]));
        // 名称变化要重建插件，让 /manage 显示更新后的信息。
        assert!(set.reconcile(vec![(manifest("demo", "New"), dir.clone())]));
        assert_eq!(set.pending_shutdown.len(), 1);
        assert_eq!(set.order, vec!["demo".to_string()]);
    }

    #[test]
    fn reconcile_collects_removed_plugins() {
        let dir = PathBuf::from("/tmp/inkbuddy-review-test");
        let mut set = PluginSet {
            plugins: HashMap::new(),
            order: Vec::new(),
            pending_shutdown: Vec::new(),
        };
        set.reconcile(vec![(manifest("demo", "Demo"), dir)]);
        set.pending_shutdown.clear();
        assert!(set.reconcile(Vec::new()));
        assert!(set.plugins.is_empty());
        assert_eq!(set.pending_shutdown.len(), 1);
    }
}
