#![allow(clippy::if_same_then_else)]

use crate::{
    ClientCapabilities, ClientCoreEvent, VideoFrameMetadata,
    logging_backend::{LOG_CHANNEL_SENDER, LogMirrorData},
    sockets::AnnouncerSocket,
    statistics::StatisticsManager,
    storage::Config,
};
use alvr_common::{
    ALVR_VERSION, AnyhowToCon, ConResult, ConnectionError, ConnectionState, LifecycleState,
    dbg_connection, debug, error, info,
    parking_lot::{Condvar, Mutex, RwLock},
    wait_rwlock, warn,
};
use alvr_packets::{
    AUDIO, ClientConnectionResult, ClientControlPacket, ClientStatistics, ConnectionAcceptedInfo,
    HAPTICS, Haptics, STATISTICS, ServerControlPacket, StreamConfigPacket, TRACKING, TrackingData,
    VIDEO, VideoPacketHeader, VideoStreamingCapabilities, VideoStreamingCapabilitiesExt,
};
use alvr_session::{SocketProtocol, settings_schema::Switch};
use alvr_sockets::{
    ControlSocketSender, KEEPALIVE_INTERVAL, KEEPALIVE_TIMEOUT, PeerType, ProtoControlSocket,
    ServerConnectionResult, SocketConnection, StreamSender, StreamSocketConfig,
};
use std::{
    collections::VecDeque,
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

#[cfg(target_os = "android")]
use crate::audio;
#[cfg(not(target_os = "android"))]
use alvr_audio as audio;

const INITIAL_MESSAGE: &str = concat!(
    "Searching for streamer...\n",
    "Open ALVR on your PC then click \"Trust\"\n",
    "next to the device entry",
);
const SUCCESS_CONNECT_MESSAGE: &str = "Successful connection!\nPlease wait...";
const STREAM_STARTING_MESSAGE: &str = "The stream will begin soon\nPlease wait...";
const SERVER_RESTART_MESSAGE: &str = "The streamer is restarting\nPlease wait...";
const SERVER_DISCONNECTED_MESSAGE: &str = "The streamer has disconnected.";
const CONNECTION_TIMEOUT_MESSAGE: &str = "Connection timeout.";

const SOCKET_INIT_RETRY_INTERVAL: Duration = Duration::from_millis(500);
const CONNECTION_RETRY_INTERVAL: Duration = Duration::from_secs(1);
const HANDSHAKE_ACTION_TIMEOUT: Duration = Duration::from_secs(2);
const STREAMING_RECV_TIMEOUT: Duration = Duration::from_millis(500);

const MAX_UNREAD_PACKETS: usize = 10; // Applies per stream
const VIDEO_FRAME_METADATA_HISTORY_SIZE: usize = 128;

pub type DecoderCallback = dyn FnMut(Duration, &[u8]) -> bool + Send;

#[derive(Default)]
pub struct ConnectionContext {
    pub state: RwLock<ConnectionState>,
    pub disconnected_notif: Condvar,
    pub control_sender: Mutex<Option<ControlSocketSender<ClientControlPacket>>>,
    pub tracking_sender: Mutex<Option<StreamSender<TrackingData>>>,
    pub statistics_sender: Mutex<Option<StreamSender<ClientStatistics>>>,
    pub statistics_manager: Mutex<Option<StatisticsManager>>,
    pub decoder_callback: Mutex<Option<Box<DecoderCallback>>>,
    pub video_frame_metadata_queue: Mutex<VecDeque<(Duration, VideoFrameMetadata)>>,
    pub max_prediction: RwLock<Duration>,
    /// Mirrors ExtraConfig::enable_diagnostics from the server settings. Guards every
    /// high-frequency probe on the client (FRAMESPAN, CHAINEND, REPEATGAP, QDEPTH,
    /// DECWAIT, DECPATH, PUREDEC, NALSHAPE, FETCHTIME, FRAMEINT, ACQUIRE, DECINPUT,
    /// PTSVAL, PTSCLOCK, BUFDROP, EMPTYFRAME, ...). Those run on the receive, decode and
    /// render threads, so leaving them on costs CPU on the headset - the device least
    /// able to spare it. Off by default.
    pub diagnostics_enabled: RwLock<bool>,
}

fn set_hud_message(event_queue: &Mutex<VecDeque<ClientCoreEvent>>, message: &str) {
    let message = format!(
        "ALVR v{}\nhostname: {}\nIP: {}\n\n{message}",
        *ALVR_VERSION,
        Config::load().hostname,
        alvr_system_info::local_ip(),
    );

    event_queue
        .lock()
        .push_back(ClientCoreEvent::UpdateHudMessage(message));
}

fn is_streaming(ctx: &ConnectionContext) -> bool {
    *ctx.state.read() == ConnectionState::Streaming
}

pub fn connection_lifecycle_loop(
    capabilities: ClientCapabilities,
    ctx: Arc<ConnectionContext>,
    lifecycle_state: Arc<RwLock<LifecycleState>>,
    event_queue: Arc<Mutex<VecDeque<ClientCoreEvent>>>,
) {
    dbg_connection!("connection_lifecycle_loop: Begin");

    set_hud_message(&event_queue, INITIAL_MESSAGE);

    while *lifecycle_state.read() != LifecycleState::ShuttingDown {
        if *lifecycle_state.read() == LifecycleState::Resumed {
            if let Err(e) = connection_pipeline(
                capabilities.clone(),
                Arc::clone(&ctx),
                Arc::clone(&lifecycle_state),
                Arc::clone(&event_queue),
            ) {
                let message = format!("Connection error:\n{e}\nCheck the PC for more details");
                set_hud_message(&event_queue, &message);
                error!("Connection error: {e}");
            }
        } else {
            debug!("Skip try connection because the device is sleeping");
        }

        *ctx.state.write() = ConnectionState::Disconnected;
        ctx.disconnected_notif.notify_all();

        thread::sleep(CONNECTION_RETRY_INTERVAL);
    }

    dbg_connection!("connection_lifecycle_loop: End");
}

fn connection_pipeline(
    capabilities: ClientCapabilities,
    ctx: Arc<ConnectionContext>,
    lifecycle_state: Arc<RwLock<LifecycleState>>,
    event_queue: Arc<Mutex<VecDeque<ClientCoreEvent>>>,
) -> ConResult {
    dbg_connection!("connection_pipeline: Begin");

    let (mut proto_control_socket, server_ip) = {
        let config = Config::load();
        let announcer_socket = AnnouncerSocket::new(&config.hostname).to_con()?;
        let listener_socket =
            alvr_sockets::get_server_listener(HANDSHAKE_ACTION_TIMEOUT).to_con()?;

        loop {
            if *lifecycle_state.write() != LifecycleState::Resumed {
                return Ok(());
            }

            announcer_socket.announce().ok();

            if let Ok(pair) = ProtoControlSocket::connect_to(
                SOCKET_INIT_RETRY_INTERVAL,
                PeerType::Server(&listener_socket),
            ) {
                set_hud_message(&event_queue, SUCCESS_CONNECT_MESSAGE);
                break pair;
            }
        }
    };

    let mut connection_state_lock = ctx.state.write();
    let disconnect_notif = Arc::new(Condvar::new());

    *connection_state_lock = ConnectionState::Connecting;

    // TODO: Don't fetch cpal sample rate, get directly from AAudio
    let microphone_sample_rate =
        alvr_audio::input_sample_rate(&alvr_audio::new_input(None).to_con()?).to_con()?;

    dbg_connection!("connection_pipeline: Send stream capabilities");
    proto_control_socket
        .send(&ClientConnectionResult::ConnectionAccepted(Box::new(
            ConnectionAcceptedInfo {
                client_protocol_id: alvr_common::protocol_id_u64(),
                platform_string: capabilities.platform.to_string(),
                server_ip,
                streaming_capabilities: Some(
                    VideoStreamingCapabilities {
                        default_view_resolution: capabilities.default_view_resolution,
                        max_view_resolution: capabilities.max_view_resolution,
                        refresh_rates: capabilities.refresh_rates,
                        microphone_sample_rate,
                        foveated_encoding: capabilities.foveated_encoding,
                        encoder_high_profile: capabilities.encoder_high_profile,
                        encoder_10_bits: capabilities.encoder_10_bits,
                        encoder_av1: capabilities.encoder_av1,
                        prefer_10bit: capabilities.prefer_10bit,
                        preferred_encoding_gamma: capabilities.preferred_encoding_gamma,
                        prefer_hdr: capabilities.prefer_hdr,
                        ext_str: String::new(),
                    }
                    .with_ext(VideoStreamingCapabilitiesExt {}),
                ),
            },
        )))
        .to_con()?;
    let config_packet =
        proto_control_socket.recv::<StreamConfigPacket>(HANDSHAKE_ACTION_TIMEOUT)?;
    dbg_connection!("connection_pipeline: stream config received");

    let stream_config = config_packet.to_stream_config().to_con()?;

    let streaming_start_event = ClientCoreEvent::StreamingStarted(Box::new(stream_config.clone()));

    let settings = stream_config.settings;
    let negotiated_config = stream_config.negotiated_config;

    *ctx.max_prediction.write() = Duration::from_millis(settings.headset.max_prediction_ms);
    *ctx.diagnostics_enabled.write() = settings.extra.enable_diagnostics;
    crate::set_diagnostics_enabled(settings.extra.enable_diagnostics);

    *ctx.statistics_manager.lock() = Some(StatisticsManager::new(
        settings.connection.statistics_history_size,
    ));

    let stream_protocol = if negotiated_config.wired {
        SocketProtocol::Tcp
    } else {
        settings.connection.stream_protocol
    };

    dbg_connection!("connection_pipeline: Finishing handshake");
    let mut socket = match SocketConnection::from_server_connection(
        proto_control_socket,
        HANDSHAKE_ACTION_TIMEOUT,
        StreamSocketConfig {
            protocol: stream_protocol,
            port: settings.connection.stream_port,
            buffer_config: settings.connection.client_buffer_config,
            max_packet_size: settings.connection.packet_size as _,
            dscp: settings.connection.dscp,
        },
    ) {
        Ok(ServerConnectionResult::Connected(socket)) => socket,
        Ok(ServerConnectionResult::Restarting) => {
            info!("Server restarting");
            set_hud_message(&event_queue, SERVER_RESTART_MESSAGE);
            return Ok(());
        }
        Err(e) => {
            info!("Server disconnected. Cause: {e}");
            set_hud_message(&event_queue, SERVER_DISCONNECTED_MESSAGE);
            return Ok(());
        }
    };

    info!("Stream starting");
    set_hud_message(&event_queue, STREAM_STARTING_MESSAGE);
    info!("Connected to server");

    let control_sender = socket.request_reliable_stream()?;
    let mut control_receiver = socket.subscribe_to_reliable_stream()?;

    let mut video_receiver =
        socket.subscribe_to_unreliable_stream::<VideoPacketHeader>(VIDEO, MAX_UNREAD_PACKETS);
    let mut game_audio_receiver = socket.subscribe_to_unreliable_stream(AUDIO, MAX_UNREAD_PACKETS);
    let tracking_sender = socket.request_unreliable_stream(TRACKING);
    let mut haptics_receiver =
        socket.subscribe_to_unreliable_stream::<Haptics>(HAPTICS, MAX_UNREAD_PACKETS);
    let statistics_sender = socket.request_unreliable_stream(STATISTICS);

    ctx.video_frame_metadata_queue.lock().clear();

    // New streaming session: let every diagnostic probe zero its counters on its next
    // call. They are block-local statics and live as long as the process, so without
    // this a reported size_max/gap_max mixes this session with earlier ones that may
    // have run with different settings.
    alvr_common::bump_probe_epoch();

    let video_receive_thread = thread::spawn({
        let ctx = Arc::clone(&ctx);
        move || {
            let mut stream_corrupted = true;
            let mut idr_requested = false;
            // REVERTED 2026-11-15: separating "decoder saturation" from stream corruption
            // (waiting for N consecutive drops before requesting an IDR) was measured and
            // did NOT help. It halved forced keyframes (17.5% -> 8.4%) but PUREDEC got
            // worse (18.41 -> 22.89 ms), the 40 ms+ share rose (599 -> 631), total latency
            // stayed at ~139 ms, and corrupted pictures persisted visibly longer because
            // recovery was deferred. The original immediate-IDR behaviour is restored.
            // IDR requests must be rate limited in TIME, not by "consecutive drops".
            //
            // Background: "Decoder saturation" is the decoder refusing a packet because it
            // has no free input buffer - the packet itself is fine. The code below still
            // treats it as stream corruption and requests a keyframe immediately. That
            // closes a feedback loop measured on device at 500 Mbps:
            //   saturation -> RequestIdr -> forced I-frame (much larger) -> slower decode
            //   -> more saturation
            // Forced keyframes reached 17.5% of all frames while GopPicSize asks for 0.3%,
            // and PUREDEC degraded 18.4 -> 22.9 -> 33.9 ms as the loop ran away.
            //
            // A previous attempt gated this on N CONSECUTIVE drops. It halved keyframes but
            // made corrupted pictures persist far longer, because recovery was deferred.
            // Time-based limiting keeps immediate recovery while bounding the request rate:
            // damage is still repaired on the very next drop, but the client cannot ask for
            // more than one keyframe per MIN_IDR_INTERVAL.
            const MIN_IDR_INTERVAL: Duration = Duration::from_millis(500);
            let mut last_idr_request: Option<Instant> = None;

            let request_idr = |requested: &mut bool, last: &mut Option<Instant>| {
                if *requested {
                    return;
                }
                if let Some(prev) = *last
                    && prev.elapsed() < MIN_IDR_INTERVAL
                {
                    return;
                }

                if let Some(sender) = &mut *ctx.control_sender.lock()
                    && sender.send(&ClientControlPacket::RequestIdr).is_ok()
                {
                    *requested = true;
                    *last = Some(Instant::now());
                }
            };

            while is_streaming(&ctx) {
                // TEMPORARY DIAGNOSTIC (actual transfer cost per frame).
                //
                // FRAMESPAN showed each frame arrives as ONE packet (pkts_avg=1.0) of
                // ~626 KB at 450 Mbps, so there is no "last packet of several" to be late -
                // the whole frame lands in a single recv(). The open question is therefore
                // how long that recv() takes. If it is a large fraction of the 11.11 ms
                // frame budget, the transfer itself is what makes frames miss their slot,
                // which is exactly the "holds then jumps" symptom.
                //
                //   recv  - wall time blocked inside video_receiver.recv() until a frame
                //           is fully available (includes socket wait + 626 KB copy)
                //   size  - payload size of that frame
                //   ratio - recv time as a fraction of the frame budget
                let t_recv0: u64 = {
                    use std::time::{SystemTime, UNIX_EPOCH};
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_micros() as u64)
                        .unwrap_or(0)
                };

                let data = match video_receiver.recv(STREAMING_RECV_TIMEOUT) {
                    Ok(data) => data,
                    Err(ConnectionError::TryAgain(_)) => continue,
                    Err(ConnectionError::Other(_)) => return,
                };

                // Third boundary: the whole packet is off the wire. Paired with the server's
                // send timestamp this isolates transport delay from local queueing.
                let trace_pts_ns = data
                    .get_header()
                    .ok()
                    .map(|h| h.timestamp.as_nanos() as u64);
                if crate::diagnostics_enabled()
                    && let Some(pts) = trace_pts_ns
                {
                    alvr_common::record_received(pts);
                }

                {
                    use std::sync::atomic::{AtomicU64, Ordering};
                    static N: AtomicU64 = AtomicU64::new(0);
                    static SUM_US: AtomicU64 = AtomicU64::new(0);
                    static MAX_US: AtomicU64 = AtomicU64::new(0);
                    static MIN_US: AtomicU64 = AtomicU64::new(u64::MAX);
                    static SUM_SZ: AtomicU64 = AtomicU64::new(0);
                    static MAX_SZ: AtomicU64 = AtomicU64::new(0);
                    static OVER_BUDGET: AtomicU64 = AtomicU64::new(0);
                    static HR: [AtomicU64; 6] = [
                        AtomicU64::new(0),
                        AtomicU64::new(0),
                        AtomicU64::new(0),
                        AtomicU64::new(0),
                        AtomicU64::new(0),
                        AtomicU64::new(0),
                    ];
                    static SEEN_EPOCH_0: AtomicU64 = AtomicU64::new(0);
                    if alvr_common::new_probe_session(&SEEN_EPOCH_0) {
                        N.store(0, Ordering::Relaxed);
                        SUM_US.store(0, Ordering::Relaxed);
                        MAX_US.store(0, Ordering::Relaxed);
                        MIN_US.store(u64::MAX, Ordering::Relaxed);
                        SUM_SZ.store(0, Ordering::Relaxed);
                        MAX_SZ.store(0, Ordering::Relaxed);
                        OVER_BUDGET.store(0, Ordering::Relaxed);
                        for slot in HR.iter() {
                            slot.store(0, Ordering::Relaxed);
                        }
                    }

                    let now = {
                        use std::time::{SystemTime, UNIX_EPOCH};
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map(|d| d.as_micros() as u64)
                            .unwrap_or(0)
                    };
                    let recv_us = now.saturating_sub(t_recv0);
                    // size is taken from the payload after parsing (see below); use 0 here
                    // and fill it in via the SZ_ONLY path.
                    let sz: u64 = 0;

                    let n = N.fetch_add(1, Ordering::Relaxed) + 1;
                    SUM_US.fetch_add(recv_us, Ordering::Relaxed);
                    MAX_US.fetch_max(recv_us, Ordering::Relaxed);
                    MIN_US.fetch_min(recv_us, Ordering::Relaxed);
                    SUM_SZ.fetch_add(sz, Ordering::Relaxed);
                    MAX_SZ.fetch_max(sz, Ordering::Relaxed);
                    if recv_us > 11111 {
                        OVER_BUDGET.fetch_add(1, Ordering::Relaxed);
                    }
                    let hb = match recv_us {
                        0..=999 => 0,
                        1000..=2999 => 1,
                        3000..=4999 => 2,
                        5000..=7999 => 3,
                        8000..=11110 => 4,
                        _ => 5,
                    };
                    HR[hb].fetch_add(1, Ordering::Relaxed);

                    if crate::diagnostics_enabled() && n % 180 == 0 {
                        let c = n as f64;
                        alvr_common::warn!(
                            "RECVCOST n={} recv_avg={:.3}ms recv_max={:.2}ms recv_min={:.3}ms | size_avg={:.1}KB size_max={:.1}KB | over11.1ms={} ({:.2}%) | hist(<1,<3,<5,<8,<11.1,11.1+ms)={}/{}/{}/{}/{}/{}",
                            n,
                            SUM_US.load(Ordering::Relaxed) as f64 / c / 1000.0,
                            MAX_US.load(Ordering::Relaxed) as f64 / 1000.0,
                            MIN_US.load(Ordering::Relaxed) as f64 / 1000.0,
                            SUM_SZ.load(Ordering::Relaxed) as f64 / c / 1000.0,
                            MAX_SZ.load(Ordering::Relaxed) as f64 / 1000.0,
                            OVER_BUDGET.load(Ordering::Relaxed),
                            OVER_BUDGET.load(Ordering::Relaxed) as f64 * 100.0 / c,
                            HR[0].load(Ordering::Relaxed),
                            HR[1].load(Ordering::Relaxed),
                            HR[2].load(Ordering::Relaxed),
                            HR[3].load(Ordering::Relaxed),
                            HR[4].load(Ordering::Relaxed),
                            HR[5].load(Ordering::Relaxed),
                        );
                    }
                }

                let Ok((header, nal)) = data.get() else {
                    return;
                };

                {
                    // RECVCOST size: recorded here because ReceiverData.buffer is private.
                    use std::sync::atomic::{AtomicU64, Ordering};
                    static SUM_SZ2: AtomicU64 = AtomicU64::new(0);
                    static MAX_SZ2: AtomicU64 = AtomicU64::new(0);
                    static SZN: AtomicU64 = AtomicU64::new(0);
                    static SEEN_EPOCH_1: AtomicU64 = AtomicU64::new(0);
                    if alvr_common::new_probe_session(&SEEN_EPOCH_1) {
                        SUM_SZ2.store(0, Ordering::Relaxed);
                        MAX_SZ2.store(0, Ordering::Relaxed);
                        SZN.store(0, Ordering::Relaxed);
                    }
                    let sz = nal.len() as u64;
                    SUM_SZ2.fetch_add(sz, Ordering::Relaxed);
                    MAX_SZ2.fetch_max(sz, Ordering::Relaxed);
                    let sn = SZN.fetch_add(1, Ordering::Relaxed) + 1;
                    if crate::diagnostics_enabled() && sn % 180 == 0 {
                        alvr_common::warn!(
                            "RECVSIZE n={} size_avg={:.1}KB size_max={:.1}KB",
                            sn,
                            SUM_SZ2.load(Ordering::Relaxed) as f64 / sn as f64 / 1000.0,
                            MAX_SZ2.load(Ordering::Relaxed) as f64 / 1000.0,
                        );
                    }
                }

                if let Some(stats) = &mut *ctx.statistics_manager.lock() {
                    stats.report_video_packet_received(header.timestamp);
                }

                // TEMPORARY DIAGNOSTIC (per-frame transfer completion).
                //
                // The hypothesis under test: at 450 Mbps a single frame is ~625 KB spread
                // over many packets, and even though the AVERAGE NAL interval stays at
                // 11.14 ms, an individual frame's last packet can arrive after the display
                // deadline. When that happens the client shows the previous image and then
                // jumps - exactly the "holds for a beat then jumps" symptom, and exactly
                // the frequency band where gap>=2f lives.
                //
                // For each frame we track the arrival time of its FIRST packet, then when
                // the timestamp changes we know the previous frame is complete:
                //   span   - first packet -> last packet of the same frame
                //   pkts   - packets per frame
                //   bytes  - payload bytes per frame
                //   tail   - last packet's arrival relative to the frame's nominal deadline
                //            (frame_start + period); positive means it finished LATE
                //
                // The "tail" histogram is the decisive one: if 450 Mbps shows a long right
                // tail and 100 Mbps does not, the bitrate/burst hypothesis is confirmed.
                {
                    use std::sync::atomic::{AtomicU64, Ordering};
                    static N: AtomicU64 = AtomicU64::new(0);
                    static SUM_SPAN_US: AtomicU64 = AtomicU64::new(0);
                    static MAX_SPAN_US: AtomicU64 = AtomicU64::new(0);
                    static SUM_PKTS: AtomicU64 = AtomicU64::new(0);
                    static MAX_PKTS: AtomicU64 = AtomicU64::new(0);
                    static SUM_BYTES: AtomicU64 = AtomicU64::new(0);
                    static MAX_BYTES: AtomicU64 = AtomicU64::new(0);
                    static LATE: AtomicU64 = AtomicU64::new(0);
                    static HS: [AtomicU64; 5] = [
                        AtomicU64::new(0),
                        AtomicU64::new(0),
                        AtomicU64::new(0),
                        AtomicU64::new(0),
                        AtomicU64::new(0),
                    ];
                    static HL: [AtomicU64; 6] = [
                        AtomicU64::new(0),
                        AtomicU64::new(0),
                        AtomicU64::new(0),
                        AtomicU64::new(0),
                        AtomicU64::new(0),
                        AtomicU64::new(0),
                    ];

                    // frame-local state, keyed by the frame timestamp
                    static CUR_TS: AtomicU64 = AtomicU64::new(0);
                    static FIRST_US: AtomicU64 = AtomicU64::new(0);
                    static PKTS: AtomicU64 = AtomicU64::new(0);
                    static BYTES: AtomicU64 = AtomicU64::new(0);
                    static SEEN_EPOCH_2: AtomicU64 = AtomicU64::new(0);
                    if alvr_common::new_probe_session(&SEEN_EPOCH_2) {
                        N.store(0, Ordering::Relaxed);
                        SUM_SPAN_US.store(0, Ordering::Relaxed);
                        MAX_SPAN_US.store(0, Ordering::Relaxed);
                        SUM_PKTS.store(0, Ordering::Relaxed);
                        MAX_PKTS.store(0, Ordering::Relaxed);
                        SUM_BYTES.store(0, Ordering::Relaxed);
                        MAX_BYTES.store(0, Ordering::Relaxed);
                        LATE.store(0, Ordering::Relaxed);
                        for slot in HS.iter() {
                            slot.store(0, Ordering::Relaxed);
                        }
                        for slot in HL.iter() {
                            slot.store(0, Ordering::Relaxed);
                        }
                        CUR_TS.store(0, Ordering::Relaxed);
                        FIRST_US.store(0, Ordering::Relaxed);
                        PKTS.store(0, Ordering::Relaxed);
                        BYTES.store(0, Ordering::Relaxed);
                    }

                    let now_us = {
                        use std::time::{SystemTime, UNIX_EPOCH};
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map(|d| d.as_micros() as u64)
                            .unwrap_or(0)
                    };
                    let ts = header.timestamp.as_nanos() as u64;
                    let cur = CUR_TS.load(Ordering::Relaxed);

                    if cur != ts {
                        // A new frame started: close out the previous one.
                        if cur != 0 {
                            let first = FIRST_US.load(Ordering::Relaxed);
                            let span = now_us.saturating_sub(first);
                            let pk = PKTS.load(Ordering::Relaxed);
                            let by = BYTES.load(Ordering::Relaxed);
                            let n = N.fetch_add(1, Ordering::Relaxed) + 1;
                            SUM_SPAN_US.fetch_add(span, Ordering::Relaxed);
                            MAX_SPAN_US.fetch_max(span, Ordering::Relaxed);
                            SUM_PKTS.fetch_add(pk, Ordering::Relaxed);
                            MAX_PKTS.fetch_max(pk, Ordering::Relaxed);
                            SUM_BYTES.fetch_add(by, Ordering::Relaxed);
                            MAX_BYTES.fetch_max(by, Ordering::Relaxed);

                            let sb = match span {
                                0..=1999 => 0,
                                2000..=3999 => 1,
                                4000..=6999 => 2,
                                7000..=9999 => 3,
                                _ => 4,
                            };
                            HS[sb].fetch_add(1, Ordering::Relaxed);

                            // nominal frame period in us (90 Hz); late = span beyond one period
                            const PERIOD_US: u64 = 11111;
                            let lb = if span <= PERIOD_US {
                                0
                            } else if span <= PERIOD_US + 2000 {
                                1
                            } else if span <= PERIOD_US + 5000 {
                                2
                            } else if span <= PERIOD_US + 11000 {
                                3
                            } else if span <= PERIOD_US + 22000 {
                                4
                            } else {
                                5
                            };
                            HL[lb].fetch_add(1, Ordering::Relaxed);
                            if span > PERIOD_US {
                                LATE.fetch_add(1, Ordering::Relaxed);
                            }

                            if crate::diagnostics_enabled() && n % 180 == 0 {
                                let c = n as f64;
                                alvr_common::warn!(
                                    "FRAMESPAN n={} span_avg={:.2}ms span_max={:.2}ms | pkts_avg={:.1} pkts_max={} | kb_avg={:.1} kb_max={:.1} | late={} ({:.2}%) | span_hist(<2,<4,<7,<10,10+ms)={}/{}/{}/{}/{} | tail_hist(<=11.1,<=13.1,<=16.1,<=22.1,<=33.1,33.1+ms)={}/{}/{}/{}/{}/{}",
                                    n,
                                    SUM_SPAN_US.load(Ordering::Relaxed) as f64 / c / 1000.0,
                                    MAX_SPAN_US.load(Ordering::Relaxed) as f64 / 1000.0,
                                    SUM_PKTS.load(Ordering::Relaxed) as f64 / c,
                                    MAX_PKTS.load(Ordering::Relaxed),
                                    SUM_BYTES.load(Ordering::Relaxed) as f64 / c / 1000.0,
                                    MAX_BYTES.load(Ordering::Relaxed) as f64 / 1000.0,
                                    LATE.load(Ordering::Relaxed),
                                    LATE.load(Ordering::Relaxed) as f64 * 100.0 / c,
                                    HS[0].load(Ordering::Relaxed),
                                    HS[1].load(Ordering::Relaxed),
                                    HS[2].load(Ordering::Relaxed),
                                    HS[3].load(Ordering::Relaxed),
                                    HS[4].load(Ordering::Relaxed),
                                    HL[0].load(Ordering::Relaxed),
                                    HL[1].load(Ordering::Relaxed),
                                    HL[2].load(Ordering::Relaxed),
                                    HL[3].load(Ordering::Relaxed),
                                    HL[4].load(Ordering::Relaxed),
                                    HL[5].load(Ordering::Relaxed),
                                );
                            }
                        }
                        CUR_TS.store(ts, Ordering::Relaxed);
                        FIRST_US.store(now_us, Ordering::Relaxed);
                        PKTS.store(0, Ordering::Relaxed);
                        BYTES.store(0, Ordering::Relaxed);
                    }
                    PKTS.fetch_add(1, Ordering::Relaxed);
                    BYTES.fetch_add(nal.len() as u64, Ordering::Relaxed);
                }

                if header.is_idr {
                    stream_corrupted = false;
                    idr_requested = false;
                } else if data.had_packet_loss() {
                    stream_corrupted = true;
                    request_idr(&mut idr_requested, &mut last_idr_request);
                    warn!("Network dropped video packet");
                }

                if !stream_corrupted || !settings.connection.avoid_video_glitching {
                    // Metadata must be available before the decoder can return this frame.
                    {
                        let queue_mut = &mut *ctx.video_frame_metadata_queue.lock();
                        queue_mut.push_back((
                            header.timestamp,
                            VideoFrameMetadata {
                                view_params: header.global_view_params,
                                foveation_center_shifts: header.foveation_center_shifts,
                            },
                        ));

                        while queue_mut.len() > VIDEO_FRAME_METADATA_HISTORY_SIZE {
                            queue_mut.pop_front();
                        }
                    }

                    // Fourth boundary: the decoder callback is being entered. The gap from
                    // `received` is consumer-thread scheduling; what happens inside the
                    // callback is measured separately by `record_queued`.
                    if crate::diagnostics_enabled() {
                        alvr_common::record_callback(header.timestamp.as_nanos() as u64);
                    }

                    let submitted = ctx
                        .decoder_callback
                        .lock()
                        .as_mut()
                        .is_some_and(|callback| callback(header.timestamp, nal));

                    if !submitted {
                        stream_corrupted = true;
                        request_idr(&mut idr_requested, &mut last_idr_request);
                        warn!("Dropped video packet. Reason: Decoder saturation")
                    }
                } else {
                    request_idr(&mut idr_requested, &mut last_idr_request);
                    warn!("Dropped video packet. Reason: Waiting for IDR frame")
                }
            }
        }
    });

    let game_audio_thread = if let Switch::Enabled(config) = settings.audio.game_audio {
        let device = alvr_audio::new_output(None).to_con()?;
        thread::spawn({
            let ctx = Arc::clone(&ctx);
            move || {
                while is_streaming(&ctx) {
                    alvr_common::show_err(audio::play_audio_loop(
                        || is_streaming(&ctx),
                        &device,
                        2,
                        negotiated_config.game_audio_sample_rate,
                        config.buffering.clone(),
                        &mut game_audio_receiver,
                    ));
                }
            }
        })
    } else {
        thread::spawn(|| ())
    };

    let microphone_thread = if matches!(settings.audio.microphone, Switch::Enabled(_)) {
        let device = alvr_audio::new_input(None).to_con()?;

        let microphone_sender = socket.request_unreliable_stream(AUDIO);

        thread::spawn({
            let ctx = Arc::clone(&ctx);
            move || {
                while is_streaming(&ctx) {
                    let ctx = Arc::clone(&ctx);
                    match audio::record_audio_blocking(
                        Arc::new(move || is_streaming(&ctx)),
                        microphone_sender.clone(),
                        &device,
                        1,
                        false,
                    ) {
                        Ok(()) => break,
                        Err(e) => {
                            error!("Audio record error: {e}");

                            continue;
                        }
                    }
                }
            }
        })
    } else {
        thread::spawn(|| ())
    };

    let haptics_receive_thread = thread::spawn({
        let ctx = Arc::clone(&ctx);
        let event_queue = Arc::clone(&event_queue);
        move || {
            while is_streaming(&ctx) {
                let data = match haptics_receiver.recv(STREAMING_RECV_TIMEOUT) {
                    Ok(packet) => packet,
                    Err(ConnectionError::TryAgain(_)) => continue,
                    Err(ConnectionError::Other(_)) => return,
                };
                let Ok(haptics) = data.get_header() else {
                    return;
                };

                event_queue.lock().push_back(ClientCoreEvent::Haptics {
                    device_id: haptics.device_id,
                    duration: haptics.duration,
                    frequency: haptics.frequency,
                    amplitude: haptics.amplitude,
                });
            }
        }
    });

    let (log_channel_sender, log_channel_receiver) = mpsc::channel();

    let control_send_thread = thread::spawn({
        let ctx = Arc::clone(&ctx);
        let event_queue = Arc::clone(&event_queue);
        let disconnect_notif = Arc::clone(&disconnect_notif);
        move || {
            let mut keepalive_deadline = Instant::now();

            #[cfg(target_os = "android")]
            let mut battery_deadline = Instant::now();

            while is_streaming(&ctx) && *lifecycle_state.read() == LifecycleState::Resumed {
                if let Ok(packet) = log_channel_receiver.recv_timeout(STREAMING_RECV_TIMEOUT)
                    && let Some(sender) = &mut *ctx.control_sender.lock()
                    && let Err(e) = sender.send(&packet)
                {
                    info!("Server disconnected. Cause: {e:?}");
                    set_hud_message(&event_queue, SERVER_DISCONNECTED_MESSAGE);

                    break;
                }

                if Instant::now() > keepalive_deadline
                    && let Some(sender) = &mut *ctx.control_sender.lock()
                {
                    sender.send(&ClientControlPacket::KeepAlive).ok();

                    keepalive_deadline = Instant::now() + KEEPALIVE_INTERVAL;
                }

                #[cfg(target_os = "android")]
                if Instant::now() > battery_deadline {
                    let (gauge_value, is_plugged) = alvr_system_info::get_battery_status();
                    if let Some(sender) = &mut *ctx.control_sender.lock() {
                        sender
                            .send(&ClientControlPacket::Battery(crate::BatteryInfo {
                                device_id: *alvr_common::HEAD_ID,
                                gauge_value,
                                is_plugged,
                            }))
                            .ok();
                    }

                    battery_deadline = Instant::now() + Duration::from_secs(5);
                }
            }

            disconnect_notif.notify_one();
        }
    });

    let control_receive_thread = thread::spawn({
        let ctx = Arc::clone(&ctx);
        let event_queue = Arc::clone(&event_queue);
        let disconnect_notif = Arc::clone(&disconnect_notif);
        move || {
            let mut disconnection_deadline = Instant::now() + KEEPALIVE_TIMEOUT;
            while is_streaming(&ctx) {
                let maybe_packet = control_receiver.recv(STREAMING_RECV_TIMEOUT);

                match maybe_packet {
                    Ok(ServerControlPacket::DecoderConfig(config)) => {
                        event_queue
                            .lock()
                            .push_back(ClientCoreEvent::DecoderConfig {
                                codec: config.codec,
                                config_nal: config.config_buffer,
                            });
                    }
                    Ok(ServerControlPacket::Restarting) => {
                        info!("{SERVER_RESTART_MESSAGE}");
                        set_hud_message(&event_queue, SERVER_RESTART_MESSAGE);
                        disconnect_notif.notify_one();
                    }
                    Ok(ServerControlPacket::RealTimeConfig(config)) => {
                        event_queue
                            .lock()
                            .push_back(ClientCoreEvent::RealTimeConfig(config));
                    }
                    Ok(ServerControlPacket::StartStream) => {
                        error!("Unexpected StartStream paceket");
                    }
                    Ok(ServerControlPacket::KeepAlive) => (),
                    Ok(
                        ServerControlPacket::Reserved(_) | ServerControlPacket::ReservedBuffer(_),
                    ) => {}
                    Err(ConnectionError::TryAgain(_)) => {
                        if Instant::now() > disconnection_deadline {
                            info!("{CONNECTION_TIMEOUT_MESSAGE}");
                            set_hud_message(&event_queue, CONNECTION_TIMEOUT_MESSAGE);
                            disconnect_notif.notify_one();
                        } else {
                            continue;
                        }
                    }
                    Err(e) => {
                        info!("{SERVER_DISCONNECTED_MESSAGE} Cause: {e}");
                        set_hud_message(&event_queue, SERVER_DISCONNECTED_MESSAGE);
                        disconnect_notif.notify_one();
                    }
                }

                disconnection_deadline = Instant::now() + KEEPALIVE_TIMEOUT;
            }
        }
    });

    let stream_receive_thread = thread::spawn({
        let ctx = Arc::clone(&ctx);
        let event_queue = Arc::clone(&event_queue);
        let disconnect_notif = Arc::clone(&disconnect_notif);
        move || {
            while is_streaming(&ctx) {
                match socket.recv_poll() {
                    Ok(()) => (),
                    Err(ConnectionError::TryAgain(_)) => continue,
                    Err(e) => {
                        info!("Client disconnected. Cause: {e}");
                        set_hud_message(&event_queue, SERVER_DISCONNECTED_MESSAGE);
                        disconnect_notif.notify_one();
                    }
                }
            }
        }
    });

    *ctx.control_sender.lock() = Some(control_sender);
    *ctx.tracking_sender.lock() = Some(tracking_sender);
    *ctx.statistics_sender.lock() = Some(statistics_sender);
    if let Switch::Enabled(filter_level) = settings.extra.logging.client_log_report_level {
        *LOG_CHANNEL_SENDER.lock() = Some(LogMirrorData {
            sender: log_channel_sender,
            filter_level,
            debug_groups_config: settings.extra.logging.debug_groups,
        });
    }
    event_queue.lock().push_back(streaming_start_event);

    *connection_state_lock = ConnectionState::Streaming;

    dbg_connection!("connection_pipeline: Unlock streams");

    // Unlock CONNECTION_STATE and block thread
    wait_rwlock(&disconnect_notif, &mut connection_state_lock);

    *connection_state_lock = ConnectionState::Disconnecting;

    // The client's half of the frame trace. Dumped here rather than on the diagnostics
    // switch because the switch lives in the session settings, which the client only reads
    // once at connect time - it never sees a mid-session toggle. Connection teardown is the
    // one moment that always happens, and by then the capture is over.
    if crate::diagnostics_enabled() {
        alvr_common::frame_trace::dump(alvr_common::frame_trace::TraceSide::Client);
    }

    *ctx.control_sender.lock() = None;
    *ctx.tracking_sender.lock() = None;
    *ctx.statistics_sender.lock() = None;
    *LOG_CHANNEL_SENDER.lock() = None;

    event_queue
        .lock()
        .push_back(ClientCoreEvent::StreamingStopped);

    // Remove lock to allow threads to properly exit:
    drop(connection_state_lock);

    dbg_connection!("connection_pipeline: Destroying streams");

    video_receive_thread.join().ok();
    game_audio_thread.join().ok();
    microphone_thread.join().ok();
    haptics_receive_thread.join().ok();
    control_send_thread.join().ok();
    control_receive_thread.join().ok();
    stream_receive_thread.join().ok();

    dbg_connection!("connection_pipeline: End");

    Ok(())
}
