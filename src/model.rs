use std::collections::{HashMap, VecDeque};

use chrono::{DateTime, Local, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ams::AmsState;

pub const HISTORY_LIMIT: usize = 500;
pub const TELEMETRY_LIMIT: usize = 240;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrinterConfig {
    pub host: String,
    pub access_code: String,
    /// Identity from discovery; empty for printers entered by hand.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub serial: String,
}

impl PrinterConfig {
    pub fn is_complete(&self) -> bool {
        !self.host.trim().is_empty() && !self.access_code.trim().is_empty()
    }

    pub fn normalized_host(&self) -> String {
        self.host
            .trim()
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_end_matches('/')
            .to_owned()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Online,
    Reconnecting,
    Error,
}

impl ConnectionState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Disconnected => "OFFLINE",
            Self::Connecting => "CONNECTING",
            Self::Online => "LIVE",
            Self::Reconnecting => "RECONNECTING",
            Self::Error => "CONNECTION ERROR",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceState {
    Unknown,
    Checking,
    Available,
    Unavailable,
}

#[derive(Debug, Clone)]
pub struct ServiceHealth {
    pub port: u16,
    pub name: &'static str,
    pub detail: &'static str,
    pub state: ServiceState,
    pub latency_ms: Option<u128>,
}

impl ServiceHealth {
    pub fn defaults() -> Vec<Self> {
        vec![
            Self::new(8883, "MQTT / TLS", "live telemetry"),
            Self::new(990, "FTPS", "files & jobs"),
            Self::new(3000, "BAMBU", "device service"),
            Self::new(6000, "BAMBU", "device service"),
        ]
    }

    fn new(port: u16, name: &'static str, detail: &'static str) -> Self {
        Self {
            port,
            name,
            detail,
            state: ServiceState::Unknown,
            latency_ms: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct PrinterStatus {
    pub serial: Option<String>,
    pub state: String,
    pub job_name: String,
    pub gcode_file: String,
    pub progress: f32,
    pub remaining_minutes: Option<u32>,
    pub current_layer: Option<u32>,
    pub total_layers: Option<u32>,
    pub nozzle_temp: Option<f32>,
    pub nozzle_target: Option<f32>,
    pub bed_temp: Option<f32>,
    pub bed_target: Option<f32>,
    pub chamber_temp: Option<f32>,
    pub speed_level: Option<u32>,
    pub wifi_signal: Option<String>,
    pub last_update: Option<DateTime<Utc>>,
    pub ams: AmsState,
    pub raw: Value,
}

impl PrinterStatus {
    pub fn state_label(&self) -> &str {
        match self.state.as_str() {
            "RUNNING" => "PRINTING",
            "PAUSE" => "PAUSED",
            "FINISH" => "COMPLETE",
            "FAILED" => "FAILED",
            "PREPARE" => "PREPARING",
            "SLICING" => "SLICING",
            "IDLE" | "" => "IDLE",
            value => value,
        }
    }

    pub fn remaining_label(&self) -> String {
        match self.remaining_minutes {
            Some(minutes) if minutes >= 60 => {
                format!("{}h {:02}m", minutes / 60, minutes % 60)
            }
            Some(minutes) => format!("{minutes} min"),
            None => "—".into(),
        }
    }

    pub fn layer_label(&self) -> String {
        match (self.current_layer, self.total_layers) {
            (Some(current), Some(total)) => format!("{current} / {total}"),
            (Some(current), None) => current.to_string(),
            _ => "—".into(),
        }
    }

    pub fn apply_report(&mut self, topic: &str, report: Value) -> Option<HistoryEntry> {
        let old_state = self.state.clone();
        let old_progress = self.progress;
        let old_layer = self.current_layer;
        let old_job = self.job_name.clone();

        self.ams.apply(report.get("print").unwrap_or(&report));
        if self.raw.is_null() {
            self.raw = Value::Object(Default::default());
        }
        merge_json(&mut self.raw, report);

        if self.serial.is_none() {
            self.serial = serial_from_topic(topic).map(str::to_owned);
        }

        let print = self.raw.get("print").unwrap_or(&self.raw);
        self.state = string_at(print, "gcode_state")
            .or_else(|| string_at(print, "print_status"))
            .unwrap_or_else(|| self.state.clone());
        self.job_name = string_at(print, "subtask_name")
            .or_else(|| string_at(print, "gcode_file"))
            .unwrap_or_else(|| self.job_name.clone());
        self.gcode_file = string_at(print, "gcode_file")
            .or_else(|| string_at(print, "file"))
            .unwrap_or_else(|| self.gcode_file.clone());
        self.progress = number_at(print, "mc_percent")
            .map(|value| (value as f32).clamp(0.0, 100.0))
            .unwrap_or(self.progress);
        self.remaining_minutes = u32_at(print, "mc_remaining_time").or(self.remaining_minutes);
        self.current_layer = u32_at(print, "layer_num").or(self.current_layer);
        self.total_layers = u32_at(print, "total_layer_num").or(self.total_layers);
        self.nozzle_temp = f32_at(print, "nozzle_temper").or(self.nozzle_temp);
        self.nozzle_target = f32_at(print, "nozzle_target_temper").or(self.nozzle_target);
        self.bed_temp = f32_at(print, "bed_temper").or(self.bed_temp);
        self.bed_target = f32_at(print, "bed_target_temper").or(self.bed_target);
        self.chamber_temp = f32_at(print, "chamber_temper").or(self.chamber_temp);
        self.speed_level = u32_at(print, "spd_lvl").or(self.speed_level);
        self.wifi_signal = string_at(print, "wifi_signal").or_else(|| self.wifi_signal.clone());
        self.last_update = Some(Utc::now());

        let detail = if self.job_name != old_job && !self.job_name.is_empty() {
            Some(format!("Job loaded · {}", self.job_name))
        } else if self.state != old_state && !self.state.is_empty() {
            Some(format!("State changed · {}", self.state_label()))
        } else if self.current_layer != old_layer {
            Some(format!("Layer advanced · {}", self.layer_label()))
        } else if (self.progress - old_progress).abs() >= 1.0 {
            Some(format!("Progress · {:.0}%", self.progress))
        } else {
            None
        };

        detail.map(HistoryEntry::mqtt)
    }
}

#[derive(Debug, Clone)]
pub struct HistoryEntry {
    pub at: DateTime<Local>,
    pub title: String,
    pub source: &'static str,
}

impl HistoryEntry {
    pub fn mqtt(title: impl Into<String>) -> Self {
        Self {
            at: Local::now(),
            title: title.into(),
            source: "MQTT",
        }
    }

    pub fn system(title: impl Into<String>) -> Self {
        Self {
            at: Local::now(),
            title: title.into(),
            source: "SYSTEM",
        }
    }
}

#[derive(Debug, Clone)]
pub struct TelemetryPoint {
    pub at: DateTime<Utc>,
    pub nozzle: Option<f32>,
    pub bed: Option<f32>,
    pub progress: f32,
    pub ams: Vec<AmsEnvironment>,
}

#[derive(Debug, Clone, Copy)]
pub struct AmsEnvironment {
    pub unit: u32,
    pub humidity: Option<f32>,
    pub temperature: Option<f32>,
}

#[derive(Default)]
pub struct SessionHistory {
    pub events: VecDeque<HistoryEntry>,
    pub telemetry: VecDeque<TelemetryPoint>,
    /// Each spool's remaining percent when first seen this session.
    spool_baseline: HashMap<String, u8>,
}

impl SessionHistory {
    pub fn push_event(&mut self, entry: HistoryEntry) {
        self.events.push_front(entry);
        self.events.truncate(HISTORY_LIMIT);
    }

    pub fn sample(&mut self, status: &PrinterStatus) {
        self.telemetry.push_back(TelemetryPoint {
            at: Utc::now(),
            nozzle: status.nozzle_temp,
            bed: status.bed_temp,
            progress: status.progress,
            ams: status
                .ams
                .units
                .iter()
                .map(|unit| AmsEnvironment {
                    unit: unit.id,
                    humidity: unit.humidity,
                    temperature: unit.temperature,
                })
                .collect(),
        });
        for unit in &status.ams.units {
            for (slot, spool) in unit.slots.iter().enumerate() {
                if let (true, Some(remain)) = (spool.loaded, spool.remain) {
                    self.spool_baseline
                        .entry(spool_key(unit.id, slot as u32, spool))
                        .or_insert(remain);
                }
            }
        }
        while self.telemetry.len() > TELEMETRY_LIMIT {
            self.telemetry.pop_front();
        }
    }
}

impl SessionHistory {
    /// Percentage points of a spool used since it was first seen this session.
    pub fn spool_used(&self, unit: u32, slot: u32, spool: &crate::ams::Spool) -> Option<u8> {
        let start = *self.spool_baseline.get(&spool_key(unit, slot, spool))?;
        Some(start.saturating_sub(spool.remain?))
    }
}

/// Track a spool by its RFID identity so swapping spools resets the baseline.
fn spool_key(unit: u32, slot: u32, spool: &crate::ams::Spool) -> String {
    spool
        .uuid
        .clone()
        .unwrap_or_else(|| format!("{unit}-{slot}-{}-{:?}", spool.kind, spool.color))
}

fn merge_json(target: &mut Value, update: Value) {
    match (target, update) {
        (Value::Object(target), Value::Object(update)) => {
            for (key, value) in update {
                merge_json(target.entry(key).or_insert(Value::Null), value);
            }
        }
        (target, update) => *target = update,
    }
}

fn serial_from_topic(topic: &str) -> Option<&str> {
    let mut parts = topic.split('/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some("device"), Some(serial), Some("report")) => Some(serial),
        _ => None,
    }
}

fn string_at(value: &Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().map(str::to_owned)
}

fn number_at(value: &Value, key: &str) -> Option<f64> {
    let value = value.get(key)?;
    value
        .as_f64()
        .or_else(|| value.as_str()?.parse::<f64>().ok())
}

fn f32_at(value: &Value, key: &str) -> Option<f32> {
    number_at(value, key).map(|value| value as f32)
}

fn u32_at(value: &Value, key: &str) -> Option<u32> {
    number_at(value, key).and_then(|value| {
        if value.is_finite() && value >= 0.0 && value <= u32::MAX as f64 {
            Some(value as u32)
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn partial_mqtt_reports_are_merged() {
        let mut status = PrinterStatus::default();
        status.apply_report(
            "device/01P00A/report",
            json!({"print": {"gcode_state": "RUNNING", "mc_percent": 12, "nozzle_temper": 219.5}}),
        );
        status.apply_report(
            "device/01P00A/report",
            json!({"print": {"mc_percent": 13, "bed_temper": 60}}),
        );

        assert_eq!(status.serial.as_deref(), Some("01P00A"));
        assert_eq!(status.state, "RUNNING");
        assert_eq!(status.progress, 13.0);
        assert_eq!(status.nozzle_temp, Some(219.5));
        assert_eq!(status.bed_temp, Some(60.0));
    }

    #[test]
    fn host_is_normalized() {
        let config = PrinterConfig {
            host: "https://192.168.1.20/".into(),
            access_code: "code".into(),
            ..Default::default()
        };
        assert_eq!(config.normalized_host(), "192.168.1.20");
    }
}
