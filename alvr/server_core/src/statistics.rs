use alvr_common::{HEAD_ID, SlidingWindowAverage};
use alvr_events::{BitrateDirectives, EventType, GraphStatistics, StatisticsSummary};
use alvr_packets::ClientStatistics;
use std::{
    collections::{HashMap, VecDeque},
    time::{Duration, Instant},
};

const FULL_REPORT_INTERVAL: Duration = Duration::from_millis(500);
const EPS_INTERVAL: Duration = Duration::from_micros(1);

pub struct HistoryFrame {
    target_timestamp: Duration,
    tracking_received: Instant,
    frame_present: Instant,
    frame_composed: Instant,
    frame_encoded: Instant,
    video_packet_bytes: usize,
    total_pipeline_latency: Duration,
}

impl Default for HistoryFrame {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            target_timestamp: Duration::ZERO,
            tracking_received: now,
            frame_present: now,
            frame_composed: now,
            frame_encoded: now,
            video_packet_bytes: 0,
            total_pipeline_latency: Duration::ZERO,
        }
    }
}

#[derive(Default, Clone)]
struct BatteryData {
    gauge_value: f32,
    is_plugged: bool,
}

pub struct StatisticsManager {
    history_buffer: VecDeque<HistoryFrame>,
    max_history_size: usize,
    last_full_report_instant: Instant,
    last_frame_present_instant: Instant,
    last_frame_present_interval: Duration,
    video_packets_total: usize,
    video_packets_partial_sum: usize,
    video_bytes_total: usize,
    video_bytes_partial_sum: usize,
    battery_gauges: HashMap<u64, BatteryData>,
    steamvr_pipeline_latency: Duration,
    motion_to_photon_latency_average: SlidingWindowAverage<Duration>,
    last_vsync_time: Instant,
    frame_interval: Duration,
    last_throughput_directives: BitrateDirectives,
    /// Diagnostics produced while this manager was locked.
    ///
    /// Writing a log line is a synchronous disk write: fern chains straight to the file
    /// handle. Doing that under the statistics lock blocks the frame-submission thread,
    /// which needs the same lock. Measured with ETW: the statistics thread spent 40.62 ms
    /// inside the log write, the submitting thread then waited 32.45 ms for the lock, and
    /// the frame interval stretched to 44.86 ms, which the headset showed as repeated
    /// frames.
    ///
    /// So the text is collected here and emitted after the caller drops the lock.
    pending_logs: Vec<String>,
}

impl StatisticsManager {
    // history size used to calculate average total pipeline latency
    pub fn new(
        max_history_size: usize,
        nominal_server_frame_interval: Duration,
        steamvr_pipeline_frames: f32,
    ) -> Self {
        Self {
            history_buffer: VecDeque::new(),
            max_history_size,
            last_full_report_instant: Instant::now(),
            last_frame_present_instant: Instant::now(),
            last_frame_present_interval: Duration::ZERO,
            video_packets_total: 0,
            video_packets_partial_sum: 0,
            video_bytes_total: 0,
            video_bytes_partial_sum: 0,
            battery_gauges: HashMap::new(),
            steamvr_pipeline_latency: Duration::from_secs_f32(
                steamvr_pipeline_frames * nominal_server_frame_interval.as_secs_f32(),
            ),
            motion_to_photon_latency_average: SlidingWindowAverage::new(
                Duration::ZERO,
                max_history_size,
            ),
            last_vsync_time: Instant::now(),
            frame_interval: nominal_server_frame_interval,
            last_throughput_directives: BitrateDirectives::default(),
            pending_logs: Vec::new(),
        }
    }

    /// Hand the queued diagnostics to the caller so it can log them after dropping the lock.
    pub fn take_pending_logs(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending_logs)
    }

        pub fn report_tracking_received(&mut self, target_timestamp: Duration) {
        if !self
            .history_buffer
            .iter()
            .any(|frame| frame.target_timestamp == target_timestamp)
        {
            self.history_buffer.push_front(HistoryFrame {
                target_timestamp,
                tracking_received: Instant::now(),
                ..Default::default()
            });
        }

        if self.history_buffer.len() > self.max_history_size {
            self.history_buffer.pop_back();
        }
    }

    pub fn report_frame_present(&mut self, target_timestamp: Duration, offset: Duration) {
        // MEASUREMENT NOTE: the lookup below is an exact timestamp match, and a miss leaves
        // last_frame_present_interval untouched while still counting towards PRESENTINT's n.
        // That silently inflates the reported interval (a stale value from several frames ago
        // gets attributed to this sample). Count hits and misses so the series can be trusted
        // only while misses stay negligible.
        {
            use std::sync::atomic::{AtomicU64, Ordering};
            static HIT: AtomicU64 = AtomicU64::new(0);
            static MISS: AtomicU64 = AtomicU64::new(0);
            let found = self
                .history_buffer
                .iter()
                .any(|frame| frame.target_timestamp == target_timestamp);
            if found {
                HIT.fetch_add(1, Ordering::Relaxed);
            } else {
                let m = MISS.fetch_add(1, Ordering::Relaxed) + 1;
                if m % 180 == 0 && crate::diagnostics_enabled() {
                    // Deferred: see `pending_logs`. Writing here would block the submitting
                    // thread, which needs this same lock.
                    self.pending_logs.push(format!(
                        "PRESENTMATCH hits={} misses={} miss_ratio={:.2}%",
                        HIT.load(Ordering::Relaxed),
                        m,
                        m as f64 * 100.0 / (HIT.load(Ordering::Relaxed) + m) as f64
                    ));
                }
            }
        }

        if let Some(frame) = self
            .history_buffer
            .iter_mut()
            .find(|frame| frame.target_timestamp == target_timestamp)
        {
            let now = Instant::now() - offset;

            self.last_frame_present_interval =
                now.saturating_duration_since(self.last_frame_present_instant);
            self.last_frame_present_instant = now;

            frame.frame_present = now;

            // TEMPORARY DIAGNOSTIC (server output rate, full account).
            //
            // The client measures NALs arriving every 11.23 ms on average, i.e. 89.0 fps
            // rather than 90, with 18.6% of gaps over 12 ms. That irregularity propagates
            // straight through the decoder (DECWAIT shows it waits one frame per NAL) into
            // an empty client queue 11% of the time. This counter measures the same
            // interval on the producing side, plus the two sub-intervals that can cause it:
            //
            //   present  - wall-clock spacing of Present() calls (the real output rate)
            //   gpu->enc - frame_composed -> frame_encoded (CopyToStaging + encode handoff)
            //   trk->gpu - tracking_received -> frame_composed (waiting for a fresh pose)
            //
            // If "present" matches the client's 11.23 ms, the server is the origin and the
            // fix belongs here. If "present" is a clean 11.11 ms, the irregularity is
            // introduced downstream (network) and the server is innocent.
            {
                use std::sync::atomic::{AtomicU64, Ordering};
                static N: AtomicU64 = AtomicU64::new(0);
                static SUM: AtomicU64 = AtomicU64::new(0);
                static MIN: AtomicU64 = AtomicU64::new(u64::MAX);
                static MAX: AtomicU64 = AtomicU64::new(0);
                static SUM_D: AtomicU64 = AtomicU64::new(0);
                static MAX_D: AtomicU64 = AtomicU64::new(0);
                static PREV: AtomicU64 = AtomicU64::new(u64::MAX);
                static HIST: [AtomicU64; 6] = [
                    AtomicU64::new(0),
                    AtomicU64::new(0),
                    AtomicU64::new(0),
                    AtomicU64::new(0),
                    AtomicU64::new(0),
                    AtomicU64::new(0),
                ];

                let us = self.last_frame_present_interval.as_micros() as u64;
                let n = N.fetch_add(1, Ordering::Relaxed) + 1;
                SUM.fetch_add(us, Ordering::Relaxed);
                MIN.fetch_min(us, Ordering::Relaxed);
                MAX.fetch_max(us, Ordering::Relaxed);
                let prev = PREV.swap(us, Ordering::Relaxed);
                if prev != u64::MAX {
                    let d = us.abs_diff(prev);
                    SUM_D.fetch_add(d, Ordering::Relaxed);
                    MAX_D.fetch_max(d, Ordering::Relaxed);
                }
                let hb = match us {
                    0..=9999 => 0,
                    10000..=11499 => 1,
                    11500..=12999 => 2,
                    13000..=22999 => 3,
                    23000..=44999 => 4,
                    _ => 5,
                };
                HIST[hb].fetch_add(1, Ordering::Relaxed);

                if n % 180 == 0 && crate::diagnostics_enabled() {
                    let c = n as f64;
                    // Deferred: see `pending_logs`.
                    self.pending_logs.push(format!(
                        "PRESENTINT n={} avg={:.2}ms fps={:.2} min={:.2}ms max={:.2}ms | jitter_avg={:.3}ms jitter_max={:.3}ms | hist(<10,10-11.5,11.5-13,13-23,23-45,45+ms)={}/{}/{}/{}/{}/{}",
                        n,
                        SUM.load(Ordering::Relaxed) as f64 / c / 1000.0,
                        1e6 / (SUM.load(Ordering::Relaxed) as f64 / c),
                        MIN.load(Ordering::Relaxed) as f64 / 1000.0,
                        MAX.load(Ordering::Relaxed) as f64 / 1000.0,
                        SUM_D.load(Ordering::Relaxed) as f64 / (c - 1.0) / 1000.0,
                        MAX_D.load(Ordering::Relaxed) as f64 / 1000.0,
                        HIST[0].load(Ordering::Relaxed),
                        HIST[1].load(Ordering::Relaxed),
                        HIST[2].load(Ordering::Relaxed),
                        HIST[3].load(Ordering::Relaxed),
                        HIST[4].load(Ordering::Relaxed),
                        HIST[5].load(Ordering::Relaxed),
                    ));
                }
            }
        }
    }

    pub fn report_frame_composed(&mut self, target_timestamp: Duration, offset: Duration) {
        if let Some(frame) = self
            .history_buffer
            .iter_mut()
            .find(|frame| frame.target_timestamp == target_timestamp)
        {
            frame.frame_composed = Instant::now() - offset;
        }
    }

    // returns encoding interval
    pub fn report_frame_encoded(
        &mut self,
        target_timestamp: Duration,
        bytes_count: usize,
    ) -> Duration {
        self.video_packets_total += 1;
        self.video_packets_partial_sum += 1;
        self.video_bytes_total += bytes_count;
        self.video_bytes_partial_sum += bytes_count;

        if let Some(frame) = self
            .history_buffer
            .iter_mut()
            .find(|frame| frame.target_timestamp == target_timestamp)
        {
            frame.frame_encoded = Instant::now();

            frame.video_packet_bytes = bytes_count;

            frame
                .frame_encoded
                .saturating_duration_since(frame.frame_composed)
        } else {
            Duration::ZERO
        }
    }

    pub fn report_battery(&mut self, device_id: u64, gauge_value: f32, is_plugged: bool) {
        *self.battery_gauges.entry(device_id).or_default() = BatteryData {
            gauge_value,
            is_plugged,
        };
    }

    pub fn report_throughput_stats(&mut self, stats: BitrateDirectives) {
        self.last_throughput_directives = stats;
    }

    // Called every frame. Some statistics are reported once every frame
    // Returns (network latency, game time latency)
    pub fn report_statistics(&mut self, client_stats: ClientStatistics) -> (Duration, Duration) {
        self.motion_to_photon_latency_average
            .submit_sample(client_stats.total_pipeline_latency);

        if let Some(frame) = self
            .history_buffer
            .iter_mut()
            .find(|frame| frame.target_timestamp == client_stats.target_timestamp)
        {
            frame.total_pipeline_latency = client_stats.total_pipeline_latency;

            let game_time_latency = frame
                .frame_present
                .saturating_duration_since(frame.tracking_received);

            let server_compositor_latency = frame
                .frame_composed
                .saturating_duration_since(frame.frame_present);

            let encoder_latency = frame
                .frame_encoded
                .saturating_duration_since(frame.frame_composed);

            // The network latency cannot be estiamed directly. It is what's left of the total
            // latency after subtracting all other latency intervals. In particular it contains the
            // transport latency of the tracking packet and the interval between the first video
            // packet is sent and the last video packet is received for a specific frame.
            // For safety, use saturating_sub to avoid a crash if for some reason the network
            // latency is miscalculated as negative.
            let network_latency = frame.total_pipeline_latency.saturating_sub(
                game_time_latency
                    + server_compositor_latency
                    + encoder_latency
                    + client_stats.video_decode
                    + client_stats.video_decoder_queue
                    + client_stats.rendering
                    + client_stats.vsync_queue,
            );

            let client_fps =
                1.0 / Duration::max(client_stats.frame_interval, EPS_INTERVAL).as_secs_f32();
            let server_fps =
                1.0 / Duration::max(self.last_frame_present_interval, EPS_INTERVAL).as_secs_f32();

            if self.last_full_report_instant + FULL_REPORT_INTERVAL < Instant::now() {
                self.last_full_report_instant += FULL_REPORT_INTERVAL;

                let interval_secs = FULL_REPORT_INTERVAL.as_secs_f32();

                alvr_events::send_event(EventType::StatisticsSummary(StatisticsSummary {
                    video_packets_total: self.video_packets_total,
                    video_packets_per_sec: (self.video_packets_partial_sum as f32 / interval_secs)
                        as _,
                    video_mbytes_total: (self.video_bytes_total as f32 / 1e6) as usize,
                    video_mbits_per_sec: self.video_bytes_partial_sum as f32 * 8.
                        / 1e6
                        / interval_secs,
                    total_latency_ms: client_stats.total_pipeline_latency.as_secs_f32() * 1000.,
                    network_latency_ms: network_latency.as_secs_f32() * 1000.,
                    encode_latency_ms: encoder_latency.as_secs_f32() * 1000.,
                    decode_latency_ms: client_stats.video_decode.as_secs_f32() * 1000.,
                    client_fps: client_fps as _,
                    server_fps: server_fps as _,
                    battery_hmd: (self
                        .battery_gauges
                        .get(&HEAD_ID)
                        .cloned()
                        .unwrap_or_default()
                        .gauge_value
                        * 100.) as u32,
                    hmd_plugged: self
                        .battery_gauges
                        .get(&HEAD_ID)
                        .cloned()
                        .unwrap_or_default()
                        .is_plugged,
                }));

                self.video_packets_partial_sum = 0;
                self.video_bytes_partial_sum = 0;
            }

            let packet_bits = frame.video_packet_bytes as f32 * 8.0;
            let throughput_bps =
                packet_bits / Duration::max(network_latency, EPS_INTERVAL).as_secs_f32();
            let bitrate_bps = packet_bits
                / Duration::max(self.last_frame_present_interval, EPS_INTERVAL).as_secs_f32();

            // todo: use target timestamp in nanoseconds. the dashboard needs to use the first
            // timestamp as the graph time origin.
            alvr_events::send_event(EventType::GraphStatistics(GraphStatistics {
                total_pipeline_latency_s: client_stats.total_pipeline_latency.as_secs_f32(),
                game_time_s: game_time_latency.as_secs_f32(),
                server_compositor_s: server_compositor_latency.as_secs_f32(),
                encoder_s: encoder_latency.as_secs_f32(),
                network_s: network_latency.as_secs_f32(),
                decoder_s: client_stats.video_decode.as_secs_f32(),
                decoder_queue_s: client_stats.video_decoder_queue.as_secs_f32(),
                client_compositor_s: client_stats.rendering.as_secs_f32(),
                vsync_queue_s: client_stats.vsync_queue.as_secs_f32(),
                client_fps,
                server_fps,
                bitrate_directives: self.last_throughput_directives.clone(),
                throughput_bps,
                bitrate_bps,
            }));

            (network_latency, game_time_latency)
        } else {
            (Duration::ZERO, Duration::ZERO)
        }
    }

    pub fn motion_to_photon_latency_average(&self) -> Duration {
        self.motion_to_photon_latency_average.get_average()
    }

    pub fn tracker_pose_time_offset(&self) -> Duration {
        // This is the opposite of the client's StatisticsManager::tracker_prediction_offset().
        self.steamvr_pipeline_latency
    }

    // NB: this call is non-blocking, waiting should be done externally
    pub fn duration_until_next_vsync(&mut self) -> Duration {
        let now = Instant::now();

        // PACING LEAD: TESTED AND REVERTED 2026-11-16.
        //
        // Running the grid 0.5% fast to keep the client queue non-empty was tried here.
        // The intent was to convert "renderer finds nothing, image held" into "surplus
        // frame discarded", since a held image is a visible stall and a dropped frame is
        // not. It does not work, because ALVR does not own the frame clock: SteamVR calls
        // SubmitLayer on its own schedule and duration_until_next_vsync only decides how
        // long PostPresent sleeps inside that call.
        //
        // Measured with ALVR_PACING_LEAD=1.005:
        //   server fps      89.82 -> 88.52   (it got SLOWER, not faster)
        //   empty_ratio      0.19% -> 1.45%
        //   CHAINEND delta   0.78 ms -> 0.78 ms (unchanged - the judder metric did not move)
        //   decoder in_sd    2.84 ms -> 3.58 ms
        //   QDEPTH avg       1.10 -> 1.09
        //
        // Shortening the grid makes the sleep end earlier relative to SteamVR's own call,
        // so the loop drifts out of phase with the real pacing and loses more often.

        // update the last vsync if it's too old
        while self.last_vsync_time + self.frame_interval < now {
            self.last_vsync_time += self.frame_interval;
        }

        // SLEEP OVERSHOOT COMPENSATION: TESTED AND REVERTED 2026-11-16.
        //
        // thread::sleep on Windows overshoots by a stable ~0.33 ms (SLEEPACC over_avg
        // 0.331-0.337 ms, bulk in the 0.3-0.5 ms bucket). Subtracting that from the requested
        // sleep looks like an obvious way to close the gap between cycle 11.29 ms and the
        // 11.111 ms target.
        //
        // It makes things worse. Measured with a 330 us subtraction:
        //   req_avg   9.606 -> 7.841 ms   (fell 1.77 ms, not the 0.33 that was subtracted)
        //   post_avg  9.95  -> 8.18 ms
        //   gap_avg   0.93  -> 2.43 ms    (SteamVR pushed further away instead)
        //   cycle_avg 11.29 -> 11.60 ms   (worse, not better)
        //
        // The reason is that to_grid is recomputed from Instant::now() on every call, so it
        // already absorbs the previous overshoot; shortening the request moves the phase
        // against SteamVR's SubmitFrameThread instead of removing the overshoot. This is the
        // same trap as the earlier ALVR_PACING_LEAD and "return 1.6 ms early" experiments:
        // anything that perturbs the requested sleep desynchronises the loop.
        (self.last_vsync_time + self.frame_interval).saturating_duration_since(now)
    }
}
