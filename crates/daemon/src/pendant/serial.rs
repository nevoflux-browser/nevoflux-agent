//! The pendant's provisioning console over USB (`prov_nvs` in the pendant
//! repo): one JSON object per line each way, on the same port the firmware
//! logs to, so anything that is not a JSON line is skipped.
//!
//! ```text
//! prov.status                → {"status":"idle|wait_boot|ready","paired":bool,
//!                               "control_channel_id"?, "name"?, "fw"?}
//! prov.offer {device}        → {"status":"wait_boot"}   the pendant asks for a BOOT press
//! prov.write {credentials…}  → {"status":"ok"}          only within 15 s of that press
//! prov.wifi {wifi_ssid, …}   → {"status":"ok"}
//! ```
//!
//! [`Line`] is the transport; the protocol on top of it is plain functions so
//! it is tested with a scripted fake instead of a pendant.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Value};

/// Espressif's USB vendor id: the ESP32-S3's own USB-Serial-JTAG.
pub const ESPRESSIF_VID: u16 = 0x303A;
const BAUD: u32 = 115_200;
/// One reply.
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(5);
/// A pendant still booting answers once its app is up.
pub const BOOT_TIMEOUT: Duration = Duration::from_secs(20);
/// How long the pendant waits for its BOOT press (`PROV_CONFIRM_TIMEOUT_MS`).
pub const CONFIRM_TIMEOUT: Duration = Duration::from_secs(15);

/// A line-oriented link to a pendant.
pub trait Line: Send {
    fn send(&mut self, line: &str) -> std::io::Result<()>;
    /// The next line within `timeout`, without its newline; `None` on timeout.
    fn recv(&mut self, timeout: Duration) -> std::io::Result<Option<String>>;
}

/// What `prov.status` says.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Status {
    pub status: String,
    pub paired: bool,
    pub control_channel_id: Option<String>,
    pub name: Option<String>,
    pub fw: Option<String>,
}

impl Status {
    pub fn parse(v: &Value) -> Option<Status> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).filter(|x| !x.is_empty()).map(String::from);
        Some(Status {
            status: s("status")?,
            paired: v.get("paired").and_then(Value::as_bool).unwrap_or(false),
            control_channel_id: s("control_channel_id"),
            name: s("name"),
            fw: s("fw"),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProvError {
    /// Nothing that answers like a pendant on this port.
    NoAnswer,
    /// The BOOT press did not come in time.
    ConfirmTimeout,
    /// The pendant dropped the offer (its own 15 s ran out, or it rebooted).
    OfferLost,
    /// It answered, but with an error.
    Refused(String),
    Io(String),
}

impl std::fmt::Display for ProvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProvError::NoAnswer => write!(f, "the pendant did not answer; is it running NevoFlux or the Launcher?"),
            ProvError::ConfirmTimeout => write!(f, "BOOT was not pressed on the pendant within 15 seconds"),
            ProvError::OfferLost => write!(f, "the pendant stopped waiting for BOOT; try again"),
            ProvError::Refused(r) => write!(f, "the pendant refused: {r}"),
            ProvError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl From<std::io::Error> for ProvError {
    fn from(e: std::io::Error) -> Self {
        ProvError::Io(e.to_string())
    }
}

/// Send one request and wait for its JSON reply, skipping log lines.
pub fn ask(line: &mut dyn Line, request: &Value, timeout: Duration) -> Result<Option<Value>, ProvError> {
    line.send(&request.to_string())?;
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(None);
        }
        match line.recv(left)? {
            None => return Ok(None),
            Some(l) => {
                let l = l.trim();
                if l.starts_with('{') && l.ends_with('}') {
                    if let Ok(v) = serde_json::from_str::<Value>(l) {
                        if v.get("status").is_some() {
                            return Ok(Some(v));
                        }
                    }
                }
            }
        }
    }
}

/// `prov.status`, knocking for up to `boot` while the pendant boots.
pub fn status(line: &mut dyn Line, boot: Duration) -> Result<Status, ProvError> {
    let deadline = Instant::now() + boot;
    loop {
        let wait = REPLY_TIMEOUT.min(Duration::from_secs(2)).min(deadline.saturating_duration_since(Instant::now()));
        if let Some(v) = ask(line, &json!({"cmd": "prov.status"}), wait.max(Duration::from_millis(50)))? {
            return Status::parse(&v).ok_or(ProvError::NoAnswer);
        }
        if Instant::now() >= deadline {
            return Err(ProvError::NoAnswer);
        }
    }
}

fn expect_ok(v: Option<Value>) -> Result<(), ProvError> {
    let v = v.ok_or(ProvError::NoAnswer)?;
    match v.get("status").and_then(Value::as_str) {
        Some("ok") => Ok(()),
        _ => Err(ProvError::Refused(
            v.get("reason").and_then(Value::as_str).unwrap_or("unknown").to_string(),
        )),
    }
}

/// `prov.offer`, then wait for the BOOT press. `on_waiting` runs once the
/// pendant is showing its prompt.
pub fn offer_and_confirm(
    line: &mut dyn Line,
    host: &str,
    poll: Duration,
    confirm: Duration,
    on_waiting: &mut dyn FnMut(),
) -> Result<(), ProvError> {
    let v = ask(line, &json!({"cmd": "prov.offer", "device": host}), REPLY_TIMEOUT)?.ok_or(ProvError::NoAnswer)?;
    match v.get("status").and_then(Value::as_str) {
        Some("ready") => return Ok(()),
        Some("wait_boot") => {}
        _ => return Err(ProvError::Refused(v.to_string())),
    }
    on_waiting();
    let deadline = Instant::now() + confirm;
    loop {
        if Instant::now() >= deadline {
            return Err(ProvError::ConfirmTimeout);
        }
        std::thread::sleep(poll);
        let st = ask(line, &json!({"cmd": "prov.status"}), REPLY_TIMEOUT)?.and_then(|v| Status::parse(&v));
        match st.as_ref().map(|s| s.status.as_str()) {
            Some("ready") => return Ok(()),
            Some("idle") => return Err(ProvError::OfferLost),
            _ => {}
        }
    }
}

/// What `prov.write` stores on the pendant.
pub struct Credentials<'a> {
    pub account_token: &'a str,
    pub relay_url: &'a str,
    pub control_channel_id: &'a str,
    pub control_key_hex: &'a str,
    pub data_channel_id: &'a str,
    pub data_key_hex: &'a str,
    pub wifi_ssid: &'a str,
    pub wifi_pass: &'a str,
}

pub fn write(line: &mut dyn Line, c: &Credentials<'_>) -> Result<(), ProvError> {
    let req = json!({
        "cmd": "prov.write",
        "wifi_ssid": c.wifi_ssid,
        "wifi_pass": c.wifi_pass,
        "relay_url": c.relay_url,
        "account_token": c.account_token,
        "control_channel_id": c.control_channel_id,
        "control_key_hex": c.control_key_hex,
        "data_channel_id": c.data_channel_id,
        "data_key_hex": c.data_key_hex,
    });
    expect_ok(ask(line, &req, REPLY_TIMEOUT)?)
}

pub fn set_wifi(line: &mut dyn Line, ssid: &str, pass: &str) -> Result<(), ProvError> {
    expect_ok(ask(line, &json!({"cmd": "prov.wifi", "wifi_ssid": ssid, "wifi_pass": pass}), REPLY_TIMEOUT)?)
}

// ── The real port ─────────────────────────────────────────────

/// A serial port that is plausibly a pendant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PortInfo {
    pub port: String,
    /// The USB product string, when the OS gives one.
    pub product: Option<String>,
}

/// Espressif USB ports; on a system that cannot say whose a port is (Linux
/// without udev), every USB-ACM port.
pub fn ports() -> Vec<PortInfo> {
    let Ok(all) = serialport::available_ports() else { return Vec::new() };
    all.into_iter()
        .filter_map(|p| match p.port_type {
            serialport::SerialPortType::UsbPort(u) if u.vid == ESPRESSIF_VID => {
                Some(PortInfo { port: p.port_name, product: u.product })
            }
            serialport::SerialPortType::UsbPort(_) => None,
            _ if p.port_name.contains("ttyACM") || p.port_name.contains("usbmodem") => {
                Some(PortInfo { port: p.port_name, product: None })
            }
            _ => None,
        })
        .collect()
}

pub struct SerialLine {
    port: Box<dyn serialport::SerialPort>,
    buf: Vec<u8>,
}

impl SerialLine {
    /// Open without resetting the pendant: Windows raises DTR and RTS on open
    /// by default, and the ESP32-S3's USB-Serial-JTAG reads that as a reset.
    pub fn open(path: &str) -> Result<Self, ProvError> {
        let mut port = serialport::new(path, BAUD)
            .timeout(Duration::from_millis(200))
            .dtr_on_open(false)
            .open()
            .map_err(|e| ProvError::Io(format!("cannot open {path}: {e}")))?;
        let _ = port.write_request_to_send(false);
        let _ = port.write_data_terminal_ready(false);
        let _ = port.clear(serialport::ClearBuffer::Input);
        Ok(Self { port, buf: Vec::new() })
    }
}

impl Line for SerialLine {
    fn send(&mut self, line: &str) -> std::io::Result<()> {
        self.port.write_all(line.as_bytes())?;
        self.port.write_all(b"\n")?;
        self.port.flush()
    }

    fn recv(&mut self, timeout: Duration) -> std::io::Result<Option<String>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(i) = self.buf.iter().position(|&b| b == b'\n') {
                let rest = self.buf.split_off(i + 1);
                let line = std::mem::replace(&mut self.buf, rest);
                return Ok(Some(String::from_utf8_lossy(&line).trim_end().to_string()));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            let mut chunk = [0u8; 512];
            match self.port.read(&mut chunk) {
                Ok(0) => {}
                Ok(n) => {
                    self.buf.extend_from_slice(&chunk[..n]);
                    // A console that never ends a line is not a pendant.
                    if self.buf.len() > 64 * 1024 {
                        self.buf.clear();
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => return Err(e),
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use std::collections::VecDeque;

    /// Answers each request with the next scripted reply (after any log noise).
    pub struct Scripted {
        pub sent: Vec<Value>,
        pub replies: VecDeque<Vec<String>>,
    }

    impl Scripted {
        pub fn new(replies: &[&[&str]]) -> Self {
            Self {
                sent: Vec::new(),
                replies: replies.iter().map(|r| r.iter().map(|s| s.to_string()).collect()).collect(),
            }
        }
    }

    impl Line for Scripted {
        fn send(&mut self, line: &str) -> std::io::Result<()> {
            self.sent.push(serde_json::from_str(line).unwrap());
            Ok(())
        }
        fn recv(&mut self, _timeout: Duration) -> std::io::Result<Option<String>> {
            loop {
                let Some(front) = self.replies.front_mut() else { return Ok(None) };
                if front.is_empty() {
                    self.replies.pop_front();
                    return Ok(None); // this request goes unanswered
                }
                let l = front.remove(0);
                if front.is_empty() {
                    self.replies.pop_front();
                }
                return Ok(Some(l));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::Scripted;
    use super::*;

    #[test]
    fn status_skips_logs_and_knocks_while_booting() {
        let mut l = Scripted::new(&[
            &[], // still booting: no reply
            &["I (1234) nevoflux: wifi up", r#"{"status":"idle","paired":true,"control_channel_id":"c1","name":"NevoFlux-A1B2","fw":"nevoflux"}"#],
        ]);
        let s = status(&mut l, Duration::from_secs(5)).unwrap();
        assert_eq!(s.name.as_deref(), Some("NevoFlux-A1B2"));
        assert!(s.paired);
        assert_eq!(s.control_channel_id.as_deref(), Some("c1"));
        assert_eq!(l.sent.len(), 2);
    }

    #[test]
    fn an_old_firmware_still_reads() {
        let mut l = Scripted::new(&[&[r#"{"status":"idle"}"#]]);
        let s = status(&mut l, Duration::from_secs(1)).unwrap();
        assert_eq!((s.paired, s.name), (false, None));
    }

    #[test]
    fn confirm_waits_for_boot_and_notices_a_dropped_offer() {
        let mut l = Scripted::new(&[
            &[r#"{"status":"wait_boot","timeout_s":15}"#],
            &[r#"{"status":"wait_boot"}"#],
            &[r#"{"status":"ready"}"#],
        ]);
        let mut prompted = 0;
        offer_and_confirm(&mut l, "PC", Duration::ZERO, Duration::from_secs(5), &mut || prompted += 1).unwrap();
        assert_eq!(prompted, 1);
        assert_eq!(l.sent[0]["cmd"], "prov.offer");
        assert_eq!(l.sent[0]["device"], "PC");

        let mut l = Scripted::new(&[&[r#"{"status":"wait_boot"}"#], &[r#"{"status":"idle"}"#]]);
        assert_eq!(
            offer_and_confirm(&mut l, "PC", Duration::ZERO, Duration::from_secs(5), &mut || {}),
            Err(ProvError::OfferLost)
        );
    }

    #[test]
    fn write_carries_everything_and_reports_a_refusal() {
        let c = Credentials {
            account_token: "tok",
            relay_url: "wss://r",
            control_channel_id: "c",
            control_key_hex: "11",
            data_channel_id: "d",
            data_key_hex: "22",
            wifi_ssid: "家里的WiFi",
            wifi_pass: "p",
        };
        let mut l = Scripted::new(&[&[r#"{"status":"ok"}"#]]);
        write(&mut l, &c).unwrap();
        assert_eq!(l.sent[0]["data_channel_id"], "d");
        assert_eq!(l.sent[0]["wifi_ssid"], "家里的WiFi");
        let mut l = Scripted::new(&[&[r#"{"status":"error","reason":"boot_confirm_required"}"#]]);
        assert_eq!(write(&mut l, &c), Err(ProvError::Refused("boot_confirm_required".into())));
        let mut l = Scripted::new(&[&[]]);
        assert_eq!(set_wifi(&mut l, "x", ""), Err(ProvError::NoAnswer));
    }
}
