//! A `tracing_subscriber::Layer` implementation that ships log events to a
//! remote syslog server over UDP or TCP without any external syslog crate.
//!
//! Protocols supported:
//!   - RFC 3164 (BSD syslog): `<PRI>TIMESTAMP HOSTNAME APP[PID]: MESSAGE`
//!   - RFC 5424 (IETF syslog): `<PRI>1 TIMESTAMP HOSTNAME APP PID - - MESSAGE`
//!
//! TCP framing follows RFC 6587 octet-counting: `{len} {message}\n`.

use std::io::Write;
use std::net::{TcpStream, UdpSocket};
use std::sync::Mutex;

use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

use crate::config::{facility_code, SyslogConfig, SyslogProtocol, SyslogTransport};

// ── transport ──────────────────────────────────────────────────────────────

enum Transport {
    Udp(UdpSocket),
    Tcp(Mutex<TcpStream>),
}

impl Transport {
    fn send(&self, msg: &str) {
        match self {
            Transport::Udp(sock) => {
                let _ = sock.send(msg.as_bytes());
            }
            Transport::Tcp(mutex) => {
                if let Ok(mut stream) = mutex.lock() {
                    // RFC 6587 §3.4.1 octet-counting framing
                    let framed = format!("{} {}\n", msg.len(), msg);
                    let _ = stream.write_all(framed.as_bytes());
                }
            }
        }
    }
}

// ── layer ──────────────────────────────────────────────────────────────────

pub struct SyslogLayer {
    transport: Transport,
    facility:  u8,
    app_name:  String,
    protocol:  SyslogProtocol,
    hostname:  String,
    pid:       u32,
}

impl SyslogLayer {
    pub fn new(config: &SyslogConfig) -> anyhow::Result<Self> {
        let addr = format!("{}:{}", config.host, config.port);
        let transport = match config.transport {
            SyslogTransport::Udp => {
                let sock = UdpSocket::bind("0.0.0.0:0")?;
                sock.connect(&addr)?;
                Transport::Udp(sock)
            }
            SyslogTransport::Tcp => {
                let stream = TcpStream::connect(&addr)?;
                Transport::Tcp(Mutex::new(stream))
            }
        };

        Ok(Self {
            transport,
            facility: facility_code(&config.facility),
            app_name: config.app_name.clone(),
            protocol: config.protocol.clone(),
            hostname: read_hostname(),
            pid:      std::process::id(),
        })
    }

    fn severity(level: &Level) -> u8 {
        match *level {
            Level::ERROR => 3, // ERR
            Level::WARN  => 4, // WARNING
            Level::INFO  => 6, // INFO
            Level::DEBUG => 7, // DEBUG
            Level::TRACE => 7, // DEBUG (syslog has no TRACE)
        }
    }

    fn pri(&self, level: &Level) -> u8 {
        self.facility * 8 + Self::severity(level)
    }

    fn format_rfc3164(&self, level: &Level, target: &str, message: &str) -> String {
        let pri = self.pri(level);
        let ts = chrono::Utc::now().format("%b %e %H:%M:%S");
        format!(
            "<{pri}>{ts} {host} {app}[{pid}]: [{target}] {message}",
            host    = self.hostname,
            app     = self.app_name,
            pid     = self.pid,
        )
    }

    fn format_rfc5424(&self, level: &Level, target: &str, message: &str) -> String {
        let pri = self.pri(level);
        let ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        // Version=1, structured-data="-", msgid="-"
        format!(
            "<{pri}>1 {ts} {host} {app} {pid} - - [{target}] {message}",
            host = self.hostname,
            app  = self.app_name,
            pid  = self.pid,
        )
    }
}

impl<S> Layer<S> for SyslogLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta    = event.metadata();
        let level   = meta.level();
        let target  = meta.target();

        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        let message = visitor.message();

        let formatted = match self.protocol {
            SyslogProtocol::Rfc3164 => self.format_rfc3164(level, target, &message),
            SyslogProtocol::Rfc5424 => self.format_rfc5424(level, target, &message),
        };

        self.transport.send(&formatted);
    }
}

// ── field visitor ──────────────────────────────────────────────────────────

#[derive(Default)]
struct FieldVisitor {
    message: String,
    extras:  Vec<String>,
}

impl tracing::field::Visit for FieldVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.extras.push(format!("{}={}", field.name(), value));
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            // strip surrounding quotes that Debug adds to strings
            let s = format!("{:?}", value);
            self.message = s.trim_matches('"').to_string();
        } else {
            self.extras.push(format!("{}={:?}", field.name(), value));
        }
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.extras.push(format!("{}={}", field.name(), value));
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.extras.push(format!("{}={}", field.name(), value));
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.extras.push(format!("{}={}", field.name(), value));
    }
}

impl FieldVisitor {
    fn message(&self) -> String {
        if self.extras.is_empty() {
            self.message.clone()
        } else {
            format!("{} ({})", self.message, self.extras.join(", "))
        }
    }
}

// ── hostname ───────────────────────────────────────────────────────────────

fn read_hostname() -> String {
    // Try /etc/hostname first (Linux), then HOSTNAME env var, then fallback.
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            std::env::var("HOSTNAME").unwrap_or_else(|_| "localhost".into())
        })
}
