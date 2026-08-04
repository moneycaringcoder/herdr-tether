use std::{
    collections::HashMap,
    env,
    io::{self, BufRead, BufReader, Write},
    net::Shutdown,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use thiserror::Error;

const DEFAULT_SOCKET_TIMEOUT: Duration = Duration::from_secs(5);
const EVENT_READ_TIMEOUT: Duration = Duration::from_millis(250);
const RECONNECT_DELAY: Duration = Duration::from_millis(200);
const MAX_REQUEST_BYTES: usize = 256 * 1024;
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_PROMPT_BYTES: usize = 64 * 1024;
/// Lowest Herdr socket API protocol Tether speaks. Herdr 0.8.0 ships protocol 19.
pub const MIN_HERDR_PROTOCOL: u32 = 19;
/// Human-facing label for [`MIN_HERDR_PROTOCOL`], used in upgrade guidance.
pub const MIN_HERDR_VERSION_LABEL: &str = "0.8.0";

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Idle,
    Working,
    Blocked,
    Done,
    #[default]
    Unknown,
}

impl AgentStatus {
    pub const fn is_settled(self) -> bool {
        matches!(self, Self::Idle | Self::Done)
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Idle => "IDLE",
            Self::Working => "WORKING",
            Self::Blocked => "BLOCKED",
            Self::Done => "DONE",
            Self::Unknown => "UNKNOWN",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct HerdrAgentInfo {
    pub terminal_id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    pub agent_status: AgentStatus,
    #[serde(default)]
    pub tokens: HashMap<String, String>,
    pub workspace_id: String,
    pub tab_id: String,
    pub pane_id: String,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub state_change_seq: u64,
    #[serde(default)]
    pub revision: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct HerdrPaneInfo {
    pub pane_id: String,
    pub terminal_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub agent_status: AgentStatus,
    #[serde(default)]
    pub tokens: HashMap<String, String>,
    #[serde(default)]
    pub revision: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct HerdrSessionSnapshot {
    pub version: String,
    pub protocol: u32,
    #[serde(default)]
    pub focused_workspace_id: Option<String>,
    #[serde(default)]
    pub focused_tab_id: Option<String>,
    #[serde(default)]
    pub focused_pane_id: Option<String>,
    #[serde(default)]
    pub panes: Vec<HerdrPaneInfo>,
    #[serde(default)]
    pub agents: Vec<HerdrAgentInfo>,
}

impl HerdrSessionSnapshot {
    pub fn version_tuple(&self) -> Result<(u64, u64, u64)> {
        parse_version(&self.version)
    }

    /// Reports whether the connected server speaks a protocol Tether supports.
    ///
    /// Tether pins the wire contract rather than the marketing version, so a
    /// Herdr build that reports an older protocol is rejected even when its
    /// version string looks new enough.
    pub fn supports_protocol(&self) -> bool {
        self.protocol >= MIN_HERDR_PROTOCOL
    }

    /// Fails with an actionable upgrade message when the protocol is too old.
    pub fn require_supported_protocol(&self) -> Result<()> {
        if self.supports_protocol() {
            return Ok(());
        }
        bail!(
            "Tether requires Herdr {MIN_HERDR_VERSION_LABEL} or newer (API protocol \
             {MIN_HERDR_PROTOCOL}); the running Herdr {} speaks protocol {}. Upgrade Herdr, \
             then reopen Tether.",
            self.version,
            self.protocol
        )
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct HerdrAgentRead {
    pub pane_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    pub text: String,
    pub revision: u64,
    pub truncated: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventSignal {
    Resnapshot { reconnected: bool },
    Changed,
    Disconnected,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum PromptDeliveryError {
    #[error("Herdr rejected the prompt before delivery: {code}: {message}")]
    Rejected { code: String, message: String },
    #[error("prompt delivery outcome is uncertain; Tether will not retry automatically")]
    Uncertain,
}

#[derive(Clone, Debug)]
pub struct HerdrSocketClient {
    socket_path: Arc<PathBuf>,
    next_id: Arc<AtomicU64>,
    timeout: Duration,
}

impl HerdrSocketClient {
    pub fn from_env() -> Result<Self> {
        let socket_path = env::var_os("HERDR_SOCKET_PATH")
            .map(PathBuf::from)
            .filter(|path| !path.as_os_str().is_empty())
            .context("Herdr did not provide HERDR_SOCKET_PATH")?;
        Ok(Self::new(socket_path))
    }

    pub fn new(socket_path: PathBuf) -> Self {
        Self {
            socket_path: Arc::new(socket_path),
            next_id: Arc::new(AtomicU64::new(1)),
            timeout: DEFAULT_SOCKET_TIMEOUT,
        }
    }

    pub fn socket_path(&self) -> &Path {
        self.socket_path.as_ref()
    }

    pub fn snapshot(&self) -> Result<HerdrSessionSnapshot> {
        let result = self.request_value("session.snapshot", json!({}), self.timeout)?;
        require_result_type(&result, "session_snapshot")?;
        decode_field(&result, "snapshot", "Herdr session snapshot")
    }

    pub fn agent_read(&self, target: &str, lines: u32) -> Result<HerdrAgentRead> {
        if target.trim().is_empty() {
            bail!("agent read target must not be empty");
        }
        let result = self.request_value(
            "agent.read",
            json!({
                "target": target,
                "source": "recent_unwrapped",
                "lines": lines,
                "format": "text",
                "strip_ansi": true,
            }),
            self.timeout,
        )?;
        require_result_type(&result, "pane_read")?;
        decode_field(&result, "read", "Herdr agent read")
    }

    /// Asks Herdr why a target agent is in its current state.
    ///
    /// Herdr's `explain` payload is an open object whose shape is not pinned by
    /// the API schema, so Tether never guesses field names. It flattens whatever
    /// top-level scalars the running server sent into bounded, sanitized
    /// `key: value` lines. An unknown future field shows up as itself instead of
    /// being silently dropped.
    pub fn explain_agent(&self, target: &str) -> Result<Vec<(String, String)>> {
        if target.trim().is_empty() {
            bail!("agent explain target must not be empty");
        }
        let result =
            self.request_value("agent.explain", json!({"target": target}), self.timeout)?;
        require_result_type(&result, "agent_explain")?;
        let explain = result
            .get("explain")
            .context("Herdr agent explain response did not contain an explanation")?;
        Ok(flatten_explain(explain))
    }

    pub fn focus_agent(&self, target: &str) -> Result<HerdrAgentInfo> {
        if target.trim().is_empty() {
            bail!("agent focus target must not be empty");
        }
        let result = self.request_value("agent.focus", json!({"target": target}), self.timeout)?;
        require_result_type(&result, "agent_info")?;
        decode_field(&result, "agent", "focused Herdr agent")
    }

    pub fn wait_agent(&self, target: &str, timeout: Duration) -> Result<HerdrAgentInfo> {
        if target.trim().is_empty() {
            bail!("agent wait target must not be empty");
        }
        let result = self.request_value(
            "agent.wait",
            json!({
                "target": target,
                "until": ["idle", "done", "blocked"],
                "timeout_ms": duration_millis(timeout),
            }),
            timeout.saturating_add(self.timeout),
        )?;
        require_result_type(&result, "agent_info")?;
        decode_field(&result, "agent", "settled Herdr agent")
    }

    pub fn prompt_and_wait(
        &self,
        target: &str,
        prompt: &str,
        timeout: Duration,
    ) -> std::result::Result<HerdrAgentInfo, PromptDeliveryError> {
        if target.trim().is_empty() || prompt.trim().is_empty() || prompt.len() > MAX_PROMPT_BYTES {
            return Err(PromptDeliveryError::Rejected {
                code: "invalid_request".to_owned(),
                message: format!(
                    "target and prompt must be non-empty and prompt must not exceed {MAX_PROMPT_BYTES} bytes"
                ),
            });
        }
        let params = json!({
            "target": target,
            "text": prompt,
            "wait": {
                "until": ["idle", "done", "blocked"],
                "timeout_ms": duration_millis(timeout),
            },
        });
        let response = match self.request_envelope(
            "agent.prompt",
            params,
            timeout.saturating_add(self.timeout),
        ) {
            Ok(response) => response,
            Err(_) => return Err(PromptDeliveryError::Uncertain),
        };
        if let Some(error) = response.get("error") {
            let code = error
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned();
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Herdr rejected the request")
                .to_owned();
            if prompt_error_confirms_no_delivery(&code) {
                return Err(PromptDeliveryError::Rejected { code, message });
            }
            return Err(PromptDeliveryError::Uncertain);
        }
        let result = response
            .get("result")
            .cloned()
            .ok_or(PromptDeliveryError::Uncertain)?;
        if require_result_type(&result, "agent_prompted").is_err() {
            return Err(PromptDeliveryError::Uncertain);
        }
        decode_field(&result, "agent", "prompted Herdr agent")
            .map_err(|_| PromptDeliveryError::Uncertain)
    }

    pub fn report_pane_metadata(
        &self,
        pane_id: &str,
        source: &str,
        tokens: &HashMap<String, String>,
    ) -> Result<()> {
        if pane_id.trim().is_empty() || source.trim().is_empty() {
            bail!("pane metadata target and source must not be empty");
        }
        let result = self.request_value(
            "pane.report_metadata",
            json!({
                "pane_id": pane_id,
                "source": source,
                "tokens": tokens,
            }),
            self.timeout,
        )?;
        let result_type = result.get("type").and_then(Value::as_str);
        if !matches!(result_type, Some("pane_info" | "ok")) {
            bail!("Herdr pane metadata response had an unexpected type");
        }
        if result_type == Some("pane_info") {
            let pane: HerdrPaneInfo = decode_field(&result, "pane", "metadata pane")?;
            if pane.pane_id != pane_id {
                bail!("Herdr labeled a different pane than requested");
            }
        }
        Ok(())
    }

    pub(crate) fn request_value(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
        let response = self.request_envelope(method, params, timeout)?;
        if let Some(error) = response.get("error") {
            let code = error
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Herdr rejected the request");
            if let Some(guidance) = actionable_error_guidance(code) {
                bail!("{guidance} (Herdr {method}: {code}: {message})");
            }
            bail!("Herdr rejected {method}: {code}: {message}");
        }
        response
            .get("result")
            .cloned()
            .with_context(|| format!("Herdr {method} response did not contain a result"))
    }

    fn request_envelope(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = format!("tether:{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let request = encode_request(&id, method, params)?;
        let response = exchange(self.socket_path(), &request, timeout)?;
        let envelope: Value = serde_json::from_slice(&response)
            .with_context(|| format!("decode Herdr {method} response"))?;
        if envelope.get("id").and_then(Value::as_str) != Some(id.as_str()) {
            bail!("Herdr {method} response id did not match the request");
        }
        Ok(envelope)
    }

    pub fn subscribe(&self) -> HerdrEventMonitor {
        HerdrEventMonitor::spawn(self.clone())
    }
}

pub struct HerdrEventMonitor {
    receiver: Receiver<EventSignal>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl HerdrEventMonitor {
    fn spawn(client: HerdrSocketClient) -> Self {
        let (sender, receiver) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            let mut connected_once = false;
            while !worker_stop.load(Ordering::Acquire) {
                match run_subscription(&client, &worker_stop, &sender, connected_once) {
                    Ok(()) if worker_stop.load(Ordering::Acquire) => break,
                    Ok(()) | Err(_) => {
                        let _ = sender.send(EventSignal::Disconnected);
                        connected_once = true;
                        if wait_for_stop(&worker_stop, RECONNECT_DELAY) {
                            break;
                        }
                    }
                }
            }
        });
        Self {
            receiver,
            stop,
            handle: Some(handle),
        }
    }

    pub fn try_recv(&self) -> std::result::Result<EventSignal, mpsc::TryRecvError> {
        self.receiver.try_recv()
    }
}

impl Drop for HerdrEventMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(unix)]
fn run_subscription(
    client: &HerdrSocketClient,
    stop: &AtomicBool,
    sender: &mpsc::Sender<EventSignal>,
    reconnected: bool,
) -> Result<()> {
    let mut stream = UnixStream::connect(client.socket_path()).with_context(|| {
        format!(
            "connect to Herdr socket `{}`",
            client.socket_path().display()
        )
    })?;
    stream.set_read_timeout(Some(EVENT_READ_TIMEOUT))?;
    stream.set_write_timeout(Some(client.timeout))?;
    let id = format!(
        "tether:events:{}",
        client.next_id.fetch_add(1, Ordering::Relaxed)
    );
    let request = encode_request(
        &id,
        "events.subscribe",
        json!({
            "subscriptions": [
                {"type": "pane.created"},
                {"type": "pane.updated"},
                {"type": "pane.closed"},
                {"type": "pane.moved"},
                {"type": "pane.exited"},
                {"type": "pane.agent_detected"},
                {"type": "pane.agent_status_changed"}
            ]
        }),
    )?;
    stream.write_all(&request)?;
    stream.flush()?;
    let mut reader = BufReader::new(stream);
    let acknowledgement = read_bounded_line(&mut reader, MAX_RESPONSE_BYTES)?
        .context("Herdr event subscription closed before acknowledgement")?;
    let envelope: Value = serde_json::from_slice(&acknowledgement)
        .context("decode Herdr event subscription acknowledgement")?;
    if envelope.get("id").and_then(Value::as_str) != Some(id.as_str())
        || envelope.get("error").is_some()
        || envelope
            .get("result")
            .and_then(|result| result.get("type"))
            .and_then(Value::as_str)
            != Some("subscription_started")
    {
        bail!("Herdr rejected the event subscription");
    }
    sender
        .send(EventSignal::Resnapshot { reconnected })
        .context("Mission Control event receiver closed")?;
    while !stop.load(Ordering::Acquire) {
        match read_bounded_line(&mut reader, MAX_RESPONSE_BYTES) {
            Ok(Some(line)) => {
                let event: Value =
                    serde_json::from_slice(&line).context("decode Herdr subscription event")?;
                if event.get("event").and_then(Value::as_str).is_none() {
                    bail!("Herdr subscription emitted an unexpected record");
                }
                sender
                    .send(EventSignal::Changed)
                    .context("Mission Control event receiver closed")?;
            }
            Ok(None) => return Ok(()),
            Err(error)
                if matches!(
                    error.downcast_ref::<io::Error>().map(io::Error::kind),
                    Some(io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)
                ) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn run_subscription(
    _client: &HerdrSocketClient,
    _stop: &AtomicBool,
    _sender: &mpsc::Sender<EventSignal>,
    _reconnected: bool,
) -> Result<()> {
    bail!("Mission Control requires a Unix Herdr socket")
}

#[cfg(unix)]
fn exchange(path: &Path, request: &[u8], timeout: Duration) -> Result<Vec<u8>> {
    let mut stream = UnixStream::connect(path).map_err(|error| {
        // Herdr reports `server_not_running` for CLI calls, but a raw socket
        // connect only yields ENOENT/ECONNREFUSED. Classify it the same way so
        // a stopped server never reads as an unexplained I/O failure.
        if matches!(
            error.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
        ) {
            anyhow::anyhow!(
                "Herdr is not running (socket `{}` is not accepting connections). \
                 Start Herdr, then reopen Tether.",
                path.display()
            )
        } else {
            anyhow::Error::new(error)
                .context(format!("connect to Herdr socket `{}`", path.display()))
        }
    })?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    stream
        .write_all(request)
        .context("write Herdr socket request")?;
    stream.flush().context("flush Herdr socket request")?;
    stream
        .shutdown(Shutdown::Write)
        .context("finish Herdr socket request")?;
    let mut reader = BufReader::new(stream);
    read_bounded_line(&mut reader, MAX_RESPONSE_BYTES)?
        .context("Herdr socket closed without a response")
}

#[cfg(not(unix))]
fn exchange(_path: &Path, _request: &[u8], _timeout: Duration) -> Result<Vec<u8>> {
    bail!("Mission Control requires a Unix Herdr socket")
}

fn encode_request(id: &str, method: &str, params: Value) -> Result<Vec<u8>> {
    let mut encoded = serde_json::to_vec(&json!({
        "id": id,
        "method": method,
        "params": params,
    }))?;
    if encoded.len() > MAX_REQUEST_BYTES {
        bail!("Herdr socket request exceeded {MAX_REQUEST_BYTES} bytes");
    }
    encoded.push(b'\n');
    Ok(encoded)
}

fn read_bounded_line<R: BufRead>(reader: &mut R, limit: usize) -> Result<Option<Vec<u8>>> {
    let mut output = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            if output.is_empty() {
                return Ok(None);
            }
            bail!("Herdr socket record was not newline terminated");
        }
        let count = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if output.len().saturating_add(count) > limit {
            bail!("Herdr socket record exceeded {limit} bytes");
        }
        output.extend_from_slice(&available[..count]);
        reader.consume(count);
        if output.last() == Some(&b'\n') {
            return Ok(Some(output));
        }
    }
}

fn require_result_type(result: &Value, expected: &str) -> Result<()> {
    if result.get("type").and_then(Value::as_str) != Some(expected) {
        bail!("Herdr response type was not `{expected}`");
    }
    Ok(())
}

fn decode_field<T: DeserializeOwned>(result: &Value, field: &str, description: &str) -> Result<T> {
    serde_json::from_value(
        result
            .get(field)
            .cloned()
            .with_context(|| format!("{description} response did not contain `{field}`"))?,
    )
    .with_context(|| format!("decode {description}"))
}

fn parse_version(value: &str) -> Result<(u64, u64, u64)> {
    let value = value.strip_prefix('v').unwrap_or(value);
    let value = value.split_once('-').map_or(value, |(core, _)| core);
    let mut components = value.split('.');
    let major = components
        .next()
        .context("Herdr version did not contain a major component")?
        .parse()?;
    let minor = components
        .next()
        .context("Herdr version did not contain a minor component")?
        .parse()?;
    let patch = components
        .next()
        .context("Herdr version did not contain a patch component")?
        .parse()?;
    if components.next().is_some() {
        bail!("Herdr version contained too many components");
    }
    Ok((major, minor, patch))
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Largest number of explain fields Tether will surface.
const MAX_EXPLAIN_FIELDS: usize = 12;
/// Largest rendered width of a single explain key or value, in characters.
const MAX_EXPLAIN_FIELD_CHARS: usize = 120;

/// Flattens Herdr's open `explain` object into bounded, sanitized pairs.
///
/// Only top-level scalars are kept. Nested objects and arrays are reported by
/// shape rather than dumped, so an unexpectedly large or deep payload cannot
/// flood the Observer surface.
fn flatten_explain(explain: &Value) -> Vec<(String, String)> {
    let Some(object) = explain.as_object() else {
        return vec![("explain".to_owned(), explain_scalar(explain))];
    };
    let mut fields: Vec<(String, String)> = object
        .iter()
        .take(MAX_EXPLAIN_FIELDS)
        .map(|(key, value)| (bound_explain_text(key), explain_scalar(value)))
        .collect();
    if object.len() > MAX_EXPLAIN_FIELDS {
        fields.push((
            "…".to_owned(),
            format!("{} more fields", object.len() - MAX_EXPLAIN_FIELDS),
        ));
    }
    fields
}

fn explain_scalar(value: &Value) -> String {
    match value {
        Value::Null => "none".to_owned(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::String(value) => bound_explain_text(value),
        Value::Array(values) => format!("[{} items]", values.len()),
        Value::Object(values) => format!("{{{} fields}}", values.len()),
    }
}

/// Strips control characters and bounds width, matching capture sanitization.
fn bound_explain_text(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.chars().count() <= MAX_EXPLAIN_FIELD_CHARS {
        return trimmed.to_owned();
    }
    let kept: String = trimmed.chars().take(MAX_EXPLAIN_FIELD_CHARS - 1).collect();
    format!("{kept}…")
}

/// Maps Herdr's machine-readable error codes to actionable Tether guidance.
///
/// Returning `None` keeps the generic `Herdr rejected <method>` wording for
/// codes whose raw message is already the most useful thing Tether can say.
fn actionable_error_guidance(code: &str) -> Option<&'static str> {
    match code {
        "server_not_running" => Some("Herdr is not running. Start Herdr, then reopen Tether."),
        "protocol_mismatch" => Some(
            "This Herdr speaks a different socket API protocol than Tether expects. \
             Upgrade Herdr and Tether to compatible releases.",
        ),
        "method_not_found" | "unsupported" => Some(
            "This Herdr does not provide an API Tether requires. \
             Upgrade Herdr to 0.8.0 or newer.",
        ),
        _ => None,
    }
}

fn prompt_error_confirms_no_delivery(code: &str) -> bool {
    matches!(
        code,
        "invalid_request"
            | "invalid_params"
            | "method_not_found"
            | "protocol_mismatch"
            | "permission_denied"
            | "agent_not_found"
            | "agent_not_running"
            | "agent_ambiguous"
            | "unsupported"
    )
}

fn wait_for_stop(stop: &AtomicBool, duration: Duration) -> bool {
    let slices = duration.as_millis().div_ceil(20);
    for _ in 0..slices {
        if stop.load(Ordering::Acquire) {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    stop.load(Ordering::Acquire)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_strings_parse_into_comparable_components() {
        assert_eq!(parse_version("0.8.0").unwrap(), (0, 8, 0));
        assert_eq!(parse_version("v0.8.0-preview").unwrap(), (0, 8, 0));
        assert!(parse_version("0.8").is_err());
    }

    fn snapshot_with_protocol(version: &str, protocol: u32) -> HerdrSessionSnapshot {
        HerdrSessionSnapshot {
            version: version.to_owned(),
            protocol,
            focused_workspace_id: None,
            focused_tab_id: None,
            focused_pane_id: None,
            panes: Vec::new(),
            agents: Vec::new(),
        }
    }

    #[test]
    fn explain_flattens_unknown_shapes_without_guessing_field_names() {
        // Herdr's explain payload is an open object. Whatever it sends must come
        // through as itself, including fields Tether has never heard of.
        let fields = flatten_explain(&json!({
            "state": "blocked",
            "matched_rule": "prompt_wait",
            "future_field_tether_has_not_seen": 7,
            "evidence": ["a", "b", "c"],
            "nested": {"x": 1, "y": 2},
            "absent": null,
        }));
        let map: HashMap<_, _> = fields.into_iter().collect();
        assert_eq!(map.get("state").map(String::as_str), Some("blocked"));
        assert_eq!(
            map.get("matched_rule").map(String::as_str),
            Some("prompt_wait")
        );
        assert_eq!(
            map.get("future_field_tether_has_not_seen")
                .map(String::as_str),
            Some("7")
        );
        // Collections are summarized by shape, never dumped into the surface.
        assert_eq!(map.get("evidence").map(String::as_str), Some("[3 items]"));
        assert_eq!(map.get("nested").map(String::as_str), Some("{2 fields}"));
        assert_eq!(map.get("absent").map(String::as_str), Some("none"));
    }

    #[test]
    fn explain_bounds_field_count_and_width() {
        let wide = json!({ "long": "x".repeat(10_000) });
        let fields = flatten_explain(&wide);
        let (_, value) = &fields[0];
        assert!(value.chars().count() <= MAX_EXPLAIN_FIELD_CHARS, "{value}");
        assert!(value.ends_with('…'), "{value}");

        let many: serde_json::Map<String, Value> = (0..50)
            .map(|index| (format!("k{index}"), json!(index)))
            .collect();
        let fields = flatten_explain(&Value::Object(many));
        assert_eq!(fields.len(), MAX_EXPLAIN_FIELDS + 1);
        // The overflow is reported rather than silently dropped.
        assert!(fields.last().unwrap().1.contains("more fields"));
    }

    #[test]
    fn explain_sanitizes_control_characters_from_agent_supplied_text() {
        let fields = flatten_explain(&json!({"reason": "line\u{1b}[31mone\nline\ttwo"}));
        let (_, value) = &fields[0];
        assert!(!value.contains('\n'), "{value}");
        assert!(!value.contains('\t'), "{value}");
        assert!(!value.contains('\u{1b}'), "{value}");
    }

    #[test]
    fn explain_handles_a_non_object_payload() {
        let fields = flatten_explain(&json!("just a string"));
        assert_eq!(
            fields,
            vec![("explain".to_owned(), "just a string".to_owned())]
        );
    }

    #[test]
    fn protocol_gate_pins_the_wire_contract_not_the_version_string() {
        assert!(snapshot_with_protocol("0.8.0", MIN_HERDR_PROTOCOL).supports_protocol());
        assert!(snapshot_with_protocol("1.2.0", MIN_HERDR_PROTOCOL + 5).supports_protocol());
        assert!(!snapshot_with_protocol("0.7.5", 17).supports_protocol());
        // A build whose version string looks new but whose protocol is old is
        // still rejected: Tether trusts the wire contract, not the label.
        assert!(!snapshot_with_protocol("9.9.9", 18).supports_protocol());
    }

    #[test]
    fn unsupported_protocol_names_both_versions_and_the_upgrade_step() {
        let error = snapshot_with_protocol("0.7.5", 17)
            .require_supported_protocol()
            .unwrap_err()
            .to_string();
        assert!(error.contains("0.8.0"), "{error}");
        assert!(error.contains("17"), "{error}");
        assert!(error.contains("Upgrade Herdr"), "{error}");
        assert!(
            snapshot_with_protocol("0.8.0", MIN_HERDR_PROTOCOL)
                .require_supported_protocol()
                .is_ok()
        );
    }

    #[test]
    fn actionable_codes_replace_raw_transport_wording() {
        assert!(
            actionable_error_guidance("server_not_running")
                .is_some_and(|guidance| guidance.contains("Start Herdr"))
        );
        assert!(
            actionable_error_guidance("protocol_mismatch")
                .is_some_and(|guidance| guidance.contains("protocol"))
        );
        assert!(
            actionable_error_guidance("method_not_found")
                .is_some_and(|guidance| guidance.contains("0.8.0"))
        );
        // Codes whose own message is already the most useful text stay generic.
        assert_eq!(actionable_error_guidance("permission_denied"), None);
        assert_eq!(actionable_error_guidance("agent_not_found"), None);
    }

    #[test]
    fn prompt_error_classification_is_conservative() {
        assert!(prompt_error_confirms_no_delivery("agent_not_running"));
        assert!(!prompt_error_confirms_no_delivery("timeout"));
        assert!(!prompt_error_confirms_no_delivery("agent_prompt_stalled"));
        assert!(!prompt_error_confirms_no_delivery("unknown"));
    }

    #[test]
    fn bounded_line_rejects_oversize_and_partial_records() {
        let mut complete = BufReader::new(&b"ok\nrest"[..]);
        assert_eq!(
            read_bounded_line(&mut complete, 3).unwrap(),
            Some(b"ok\n".to_vec())
        );
        let mut oversized = BufReader::new(&b"toolong\n"[..]);
        assert!(read_bounded_line(&mut oversized, 4).is_err());
        let mut partial = BufReader::new(&b"partial"[..]);
        assert!(read_bounded_line(&mut partial, 16).is_err());
    }
}

#[cfg(test)]
#[cfg(unix)]
mod event_monitor_tests {
    use std::{
        io::{BufRead, BufReader, Write},
        os::unix::net::UnixListener,
        time::{Duration, Instant},
    };

    use serde_json::{Value, json};
    use tempfile::tempdir;

    use super::{EventSignal, HerdrSocketClient};

    #[test]
    fn event_monitor_reconnects_and_requests_a_resnapshot() {
        let temp = tempdir().unwrap();
        let socket = temp.path().join("herdr.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            for connection in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut request)
                    .unwrap();
                let request: Value = serde_json::from_str(&request).unwrap();
                assert_eq!(
                    request.get("method").and_then(Value::as_str),
                    Some("events.subscribe")
                );
                let subscriptions = request["params"]["subscriptions"].as_array().unwrap();
                assert!(subscriptions.iter().any(|entry| {
                    entry.get("type").and_then(Value::as_str) == Some("pane.agent_status_changed")
                }));
                let id = request["id"].as_str().unwrap();
                writeln!(
                    stream,
                    "{}",
                    json!({
                        "id": id,
                        "result": {"type": "subscription_started"}
                    })
                )
                .unwrap();
                if connection == 0 {
                    writeln!(
                        stream,
                        "{}",
                        json!({
                            "event": "pane.agent_status_changed",
                            "data": {"pane_id": "w1:p1", "agent_status": "done"}
                        })
                    )
                    .unwrap();
                }
                stream.flush().unwrap();
            }
        });

        let monitor = HerdrSocketClient::new(socket).subscribe();
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut signals = Vec::new();
        while Instant::now() < deadline
            && !signals
                .iter()
                .any(|signal| matches!(signal, EventSignal::Resnapshot { reconnected: true }))
        {
            match monitor.try_recv() {
                Ok(signal) => signals.push(signal),
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
            }
        }
        assert!(matches!(
            signals.first(),
            Some(EventSignal::Resnapshot { reconnected: false })
        ));
        assert!(signals.contains(&EventSignal::Changed));
        assert!(signals.contains(&EventSignal::Disconnected));
        assert!(
            signals
                .iter()
                .any(|signal| matches!(signal, EventSignal::Resnapshot { reconnected: true }))
        );
        drop(monitor);
        server.join().unwrap();
    }
}
