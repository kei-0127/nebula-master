use anyhow::Result;
use chrono::Utc;
use crossbeam_channel::{self, RecvError, Sender};
use nebula_utils::get_hostname;
use serde::Serialize;
use std::{
    collections::{BTreeMap, HashMap},
    io::Write,
    thread,
};
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::{
    field::{Field, Visit},
    level_filters::LevelFilter,
    Level, Subscriber,
};
use tracing_subscriber::{
    layer::SubscriberExt, registry::LookupSpan, util::SubscriberInitExt, Layer,
};

const ENDPOINT: &str = "https://logging.googleapis.com";
const SCOPE: &[&str] = &[
    "https://www.googleapis.com/auth/logging.write",
    "https://www.googleapis.com/auth/logging.admin",
    "https://www.googleapis.com/auth/cloud-platform",
];

pub struct LogWriter {
    sender: Sender<Vec<u8>>,
}

#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
enum LogPayload {
    TextPayload(String),
    JsonPayload(serde_json::Value),
}

#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct LogSourceLocation {
    file: Option<String>,
    line: Option<String>,
    function: Option<String>,
}

#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct LogEntry {
    log_name: String,
    resource: MonitoredResource,
    severity: LogSeverity,
    timestamp: String,
    source_location: Option<LogSourceLocation>,
    #[serde(flatten)]
    payload: LogPayload,
}

#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct MonitoredResource {
    #[serde(rename = "type")]
    typ: String,
    labels: HashMap<String, String>,
}

#[derive(Debug, Default, Serialize, Clone)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum LogSeverity {
    /// Log entry has no assigned severity level
    #[default]
    Default,
    /// Debug or trace information
    Debug,
    /// Routine information, such as ongoing status or performance
    Info,
    /// Normal but significant events, such as start up, shut down, or a configuration change
    Notice,
    /// Warning events might cause problems
    Warning,
    /// Error events are likely to cause problems
    Error,
    /// Critical events cause more severe problems or outages
    Critical,
    /// A person must take an action immediately
    Alert,
    /// One or more systems are unusable
    Emergency,
}

impl From<&Level> for LogSeverity {
    fn from(level: &Level) -> Self {
        match level {
            &Level::DEBUG | &Level::TRACE => Self::Debug,
            &Level::INFO => Self::Info,
            &Level::WARN => Self::Warning,
            &Level::ERROR => Self::Error,
        }
    }
}

impl Default for LogWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl LogWriter {
    pub fn new() -> Self {
        let (sender, receiver) = crossbeam_channel::bounded(1000);
        thread::spawn(move || -> Result<usize, RecvError> {
            let mut stderr = std::io::stderr();
            loop {
                let data: Vec<u8> = receiver.recv()?;
                let _ = stderr.write_all(&data);
            }
        });
        Self { sender }
    }
}

impl std::io::Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = self.sender.try_send(buf.to_vec());
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub fn init() {
    let (non_blocking, guard) = tracing_appender::non_blocking(std::io::stderr());
    std::mem::forget(guard);
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .parse_lossy(""),
        )
        .with(GcpLayer::new())
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(non_blocking)
                .pretty()
                .compact(),
        )
        .init();
}

async fn send_to_gcp(mut rx: UnboundedReceiver<LogEntry>) -> Result<()> {
    let key =
        yup_oauth2::read_service_account_key("/etc/nebula/yayyay.json").await?;
    let authenticator = yup_oauth2::ServiceAccountAuthenticator::builder(key)
        .build()
        .await?;
    let mut token = authenticator.token(SCOPE).await?;
    let mut bearer_token = format!("Bearer {}", token.as_str());
    let client = reqwest::Client::new();

    loop {
        let Some(entry) = rx.recv().await else {
            return Err(anyhow::anyhow!("rx closed"));
        };
        let mut entries = vec![entry];

        while let Ok(entry) = rx.try_recv() {
            entries.push(entry);
            if entries.len() >= 100 {
                break;
            }
        }

        if token.is_expired() {
            token = authenticator.token(SCOPE).await?;
            bearer_token = format!("Bearer {}", token.as_str());
        }

        let _ = client
            .post("https://logging.googleapis.com/v2/entries:write")
            .header("Authorization", &bearer_token)
            .json(&serde_json::json!({
                "entries": entries,
            }))
            .send()
            .await;
    }
}

struct GcpLayer {
    hostname: String,
    tx: tokio::sync::mpsc::UnboundedSender<LogEntry>,
}

impl GcpLayer {
    fn new() -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            if let Err(err) = send_to_gcp(rx).await {
                eprintln!("log send to gcp error {err:?}");
            }
        });
        let hostname = get_hostname().unwrap_or_default();
        Self { tx, hostname }
    }
}

impl<S> Layer<S> for GcpLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let now = Utc::now();
        let now = now.to_rfc3339();
        let meta = event.metadata();
        let source_location = meta.file().map(|file| LogSourceLocation {
            file: Some(file.to_string()),
            line: meta.line().map(|line| line.to_string()),
            function: None,
        });
        let severity = LogSeverity::from(meta.level());
        let mut visitor = Visitor::new(&self.hostname);
        if let Some(module_path) = meta.module_path() {
            visitor
                .values
                .insert("module_path", serde_json::Value::from(module_path));
        }
        event.record(&mut visitor);
        if let Ok(values) = serde_json::to_value(&visitor.values) {
            let entry = LogEntry {
                log_name: "projects/yayyay-143208/logs/nebula".to_string(),
                resource: MonitoredResource {
                    typ: "api".to_string(),
                    labels: HashMap::new(),
                },
                timestamp: now,
                severity,
                source_location,
                payload: LogPayload::JsonPayload(values),
            };
            let _ = self.tx.send(entry);
        }
    }
}

/// Visitor for Stackdriver events that formats custom fields
pub(crate) struct Visitor<'a> {
    values: BTreeMap<&'a str, serde_json::Value>,
}

impl<'a> Visitor<'a> {
    /// Returns a new default visitor using the provided writer
    pub(crate) fn new(hostname: &str) -> Self {
        let mut values = BTreeMap::new();
        values.insert("hostname", serde_json::Value::from(hostname));
        Self { values }
    }
}

impl<'a> Visit for Visitor<'a> {
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.values
            .insert(field.name(), serde_json::Value::from(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.values
            .insert(field.name(), serde_json::Value::from(value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.values
            .insert(field.name(), serde_json::Value::from(value));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.values
            .insert(field.name(), serde_json::Value::from(value));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.values.insert(
            field.name(),
            serde_json::Value::from(format!("{:?}", value)),
        );
    }
}

impl<'a> std::fmt::Debug for Visitor<'a> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter
            .debug_struct("Visitor")
            .field("values", &self.values)
            .finish()
    }
}
