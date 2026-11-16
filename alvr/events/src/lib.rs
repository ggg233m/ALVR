use alvr_common::{DeviceMotion, LogEntry, LogSeverity, Pose, info};
use alvr_packets::{ButtonValue, FaceData};
use alvr_session::SessionConfig;
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, time::Duration};

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct StatisticsSummary {
    pub video_packets_total: usize,
    pub video_packets_per_sec: usize,
    pub video_mbytes_total: usize,
    pub video_mbits_per_sec: f32,
    pub total_latency_ms: f32,
    pub network_latency_ms: f32,
    pub encode_latency_ms: f32,
    pub decode_latency_ms: f32,
    pub client_fps: u32,
    pub server_fps: u32,
    pub battery_hmd: u32,
    pub hmd_plugged: bool,
}

// Bitrate statistics minus the empirical output value
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct BitrateDirectives {
    pub scaled_calculated_throughput_bps: Option<f32>,
    pub decoder_latency_limiter_bps: Option<f32>,
    pub network_latency_limiter_bps: Option<f32>,
    pub encoder_latency_limiter_bps: Option<f32>,
    pub manual_max_throughput_bps: Option<f32>,
    pub manual_min_throughput_bps: Option<f32>,
    pub requested_bitrate_bps: f32,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct GraphStatistics {
    pub total_pipeline_latency_s: f32,
    pub game_time_s: f32,
    pub server_compositor_s: f32,
    pub encoder_s: f32,
    pub network_s: f32,
    pub decoder_s: f32,
    pub decoder_queue_s: f32,
    pub client_compositor_s: f32,
    pub vsync_queue_s: f32,
    pub client_fps: f32,
    pub server_fps: f32,
    pub bitrate_directives: BitrateDirectives,
    pub throughput_bps: f32,
    pub bitrate_bps: f32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TrackingEvent {
    pub device_motions: Vec<(String, DeviceMotion)>,
    pub hand_skeletons: [Option<[Pose; 26]>; 2],
    pub face: FaceData,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ButtonEvent {
    pub path: String,
    pub value: ButtonValue,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct HapticsEvent {
    pub path: String,
    pub duration: Duration,
    pub frequency: f32,
    pub amplitude: f32,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct AdbEvent {
    pub download_progress: f32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "id", content = "data")]
pub enum EventType {
    Log(LogEntry),
    DebugGroup { group: String, message: String },
    Session(Box<SessionConfig>),
    StatisticsSummary(StatisticsSummary),
    GraphStatistics(GraphStatistics),
    Tracking(Box<TrackingEvent>),
    Buttons(Vec<ButtonEvent>),
    Haptics(HapticsEvent),
    DriversList(Vec<PathBuf>),
    ServerRequestsSelfRestart,
    Adb(AdbEvent),
    NewVersionFound { version: String, message: String },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Event {
    pub timestamp: String,
    pub event_type: EventType,
}

impl Event {
    pub fn event_type_string(&self) -> String {
        match &self.event_type {
            EventType::Log(entry) => match entry.severity {
                LogSeverity::Error => "ERROR".into(),
                LogSeverity::Warning => "WARNING".into(),
                LogSeverity::Info => "INFO".into(),
                LogSeverity::Debug => "DEBUG".into(),
            },
            EventType::DebugGroup { group, .. } => group.clone(),
            EventType::Session(_) => "SESSION".to_string(),
            EventType::StatisticsSummary(_) => "STATS".to_string(),
            EventType::GraphStatistics(_) => "GRAPH".to_string(),
            EventType::Tracking(_) => "TRACKING".to_string(),
            EventType::Buttons(_) => "BUTTONS".to_string(),
            EventType::Haptics(_) => "HAPTICS".to_string(),
            EventType::DriversList(_) => "DRV LIST".to_string(),
            EventType::ServerRequestsSelfRestart => "RESTART".to_string(),
            EventType::Adb(_) => "ADB".to_string(),
            EventType::NewVersionFound { .. } => "NEW VER".to_string(),
        }
    }

    pub fn message(&self) -> String {
        match &self.event_type {
            EventType::Log(log_entry) => log_entry.content.clone(),
            EventType::DebugGroup { message, .. } => message.clone(),
            EventType::Session(_) => "Updated".into(),
            EventType::StatisticsSummary(_) | EventType::GraphStatistics(_) => "".into(),
            EventType::Tracking(tracking) => serde_json::to_string(tracking).unwrap(),
            EventType::Buttons(buttons) => serde_json::to_string(buttons).unwrap(),
            EventType::Haptics(haptics) => serde_json::to_string(haptics).unwrap(),
            EventType::DriversList(drivers) => serde_json::to_string(drivers).unwrap(),
            EventType::ServerRequestsSelfRestart => "Request for server restart".into(),
            EventType::Adb(adb) => serde_json::to_string(adb).unwrap(),
            EventType::NewVersionFound { version, .. } => version.clone(),
        }
    }
}

/// Log target used only by telemetry events.
///
/// `send_event` historically routed everything through `info!`, which meant a
/// GraphStatistics event was serialised to JSON, parsed back by the logging backend,
/// re-formatted into a line with an empty message ("04:00:47.256 [GRAPH] ") and written to
/// disk 72 times a second, discarding all 14 fields in the process. fern flushes on every
/// record, so each of those was a synchronous write; on a mechanical disk ETW measured
/// 77.8 ms inside one of them, which held up Present() and starved the headset.
///
/// Telemetry only feeds the dashboard graphs, which receive it over the event broadcast, so
/// the file sink now filters this target out while the broadcast still happens.
pub const TELEMETRY_TARGET: &str = "alvr::telemetry";

pub fn send_event(event_type: EventType) {
    info!(target: TELEMETRY_TARGET, "{}", serde_json::to_string(&event_type).unwrap());
}
