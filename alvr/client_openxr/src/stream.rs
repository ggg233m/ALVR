use crate::{
    graphics::{self, ProjectionLayerAlphaConfig, ProjectionLayerBuilder},
    interaction::{self, InteractionContext, InteractionSourcesConfig},
};
use alvr_client_core::{
    ClientCoreContext, VideoFrameMetadata,
    video_decoder::{self, VideoDecoderConfig, VideoDecoderSource},
};
use alvr_common::{
    AlvrFoveatedEncodingParams, DETACHED_CONTROLLER_LEFT_ID, DETACHED_CONTROLLER_RIGHT_ID,
    HAND_LEFT_ID, HAND_RIGHT_ID, HEAD_ID, Pose, RelaxedAtomic, ViewParams,
    anyhow::Result,
    error,
    glam::{UVec2, Vec2},
    parking_lot::RwLock,
};
use alvr_graphics::{GraphicsContext, StreamRenderer, StreamViewParams};
use alvr_packets::{ClientStreamConfig, RealTimeConfig, TrackingData};
use alvr_session::{
    ClientsideFoveationConfig, ClientsideFoveationMode, ClientsidePostProcessingConfig, CodecType,
    MediacodecProperty, PassthroughMode, UpscalingConfig,
};
use alvr_system_info::Platform;
use openxr as xr;
use std::{
    ptr,
    rc::Rc,
    sync::Arc,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

// Fraction of the frame period that render() will spend waiting for a decoded frame.
//
// WAS 0.8 (= 8.889 ms at 90 Hz). Measured with FRAMEINT: predicted_display_period is a
// rock-steady 11.111 ms (99.92% of frames; the only exceptions are 3 frames at 13.889 ms
// when the runtime briefly drops to 75 Hz), so the deadline is a fixed 8.889 ms. Every
// observed empty frame had fetch_us between 8.92 and 9.38 ms - i.e. the decoded frame
// arrived just AFTER the 8.889 ms cutoff, the poll gave up, and the client re-submitted
// the previous image. That is the single-frame repeat behind the periodic stutter.
//
// 0.9 gives a 10.000 ms cutoff, which covers every measured empty (max 9.38 ms) while
// still leaving ~1.1 ms for submit+present (measured ~0.8 ms). 0.95 would leave only
// 0.55 ms and start crowding the render path.
const DECODER_MAX_TIMEOUT_MULTIPLIER: f32 = 0.9;

pub struct ParsedStreamConfig {
    pub view_resolution: UVec2,
    pub refresh_rate_hint: f32,
    pub encoding_gamma: f32,
    pub enable_hdr: bool,
    pub passthrough: Option<PassthroughMode>,
    pub foveated_encoding_config: Option<AlvrFoveatedEncodingParams>,
    pub clientside_foveation_config: Option<ClientsideFoveationConfig>,
    pub clientside_post_processing: Option<ClientsidePostProcessingConfig>,
    pub upscaling: Option<UpscalingConfig>,
    pub force_software_decoder: bool,
    pub max_buffering_frames: f32,
    pub buffering_history_weight: f32,
    pub decoder_options: Vec<(String, MediacodecProperty)>,
    pub interaction_sources: InteractionSourcesConfig,
}

impl ParsedStreamConfig {
    pub fn new(config: &ClientStreamConfig) -> Self {
        Self {
            view_resolution: config.negotiated_config.view_resolution,
            refresh_rate_hint: config.negotiated_config.refresh_rate_hint,
            encoding_gamma: config.negotiated_config.encoding_gamma,
            enable_hdr: config.negotiated_config.enable_hdr,
            passthrough: config.settings.video.passthrough.as_option().cloned(),
            foveated_encoding_config: config.negotiated_config.foveated_encoding,
            clientside_foveation_config: config
                .settings
                .video
                .clientside_foveation
                .as_option()
                .cloned(),
            clientside_post_processing: config
                .settings
                .video
                .clientside_post_processing
                .as_option()
                .cloned(),
            upscaling: config.settings.video.upscaling.as_option().cloned(),
            force_software_decoder: config.settings.video.force_software_decoder,
            max_buffering_frames: config.settings.video.max_buffering_frames,
            buffering_history_weight: config.settings.video.buffering_history_weight,
            decoder_options: config.settings.video.mediacodec_extra_options.clone(),
            interaction_sources: InteractionSourcesConfig::new(config),
        }
    }
}

pub struct StreamContext {
    core_context: Arc<ClientCoreContext>,
    xr_session: xr::Session<xr::OpenGlEs>,
    interaction_context: Arc<RwLock<InteractionContext>>,
    stage_reference_space: Arc<xr::Space>,
    view_reference_space: Arc<xr::Space>,
    swapchains: [xr::Swapchain<xr::OpenGlEs>; 2],
    last_good_video_frame_metadata: VideoFrameMetadata,
    input_thread: Option<JoinHandle<()>>,
    input_thread_running: Arc<RelaxedAtomic>,
    config: ParsedStreamConfig,
    target_view_resolution: UVec2,
    renderer: StreamRenderer,
    decoder: Option<(VideoDecoderConfig, VideoDecoderSource)>,
    use_custom_reprojection: bool,
}

impl StreamContext {
    pub fn new(
        core_ctx: Arc<ClientCoreContext>,
        xr_session: xr::Session<xr::OpenGlEs>,
        gfx_ctx: Rc<GraphicsContext>,
        interaction_ctx: Arc<RwLock<InteractionContext>>,
        config: ParsedStreamConfig,
    ) -> StreamContext {
        interaction_ctx
            .write()
            .select_sources(&config.interaction_sources);

        let xr_exts = xr_session.instance().exts();

        if xr_exts.fb_display_refresh_rate.is_some() {
            xr_session
                .request_display_refresh_rate(config.refresh_rate_hint)
                .unwrap();
        }

        let foveation_profile = if let Some(config) = &config.clientside_foveation_config
            && xr_exts.fb_swapchain_update_state.is_some()
            && xr_exts.fb_foveation.is_some()
            && xr_exts.fb_foveation_configuration.is_some()
        {
            let level;
            let dynamic;
            match config.mode {
                ClientsideFoveationMode::Static { level: lvl } => {
                    level = lvl;
                    dynamic = false;
                }
                ClientsideFoveationMode::Dynamic { max_level } => {
                    level = max_level;
                    dynamic = true;
                }
            };

            xr_session
                .create_foveation_profile(Some(xr::FoveationLevelProfile {
                    level: xr::FoveationLevelFB::from_raw(level as i32),
                    vertical_offset: config.vertical_offset_deg,
                    dynamic: xr::FoveationDynamicFB::from_raw(dynamic as i32),
                }))
                .ok()
        } else {
            None
        };

        let target_view_resolution = alvr_graphics::compute_target_view_resolution(
            config.view_resolution,
            &config.upscaling,
        );
        let format = graphics::swapchain_format(&gfx_ctx, &xr_session, config.enable_hdr);

        let swapchains = [
            graphics::create_swapchain(
                &xr_session,
                &gfx_ctx,
                target_view_resolution,
                format,
                foveation_profile.as_ref(),
            ),
            graphics::create_swapchain(
                &xr_session,
                &gfx_ctx,
                target_view_resolution,
                format,
                foveation_profile.as_ref(),
            ),
        ];

        let renderer = StreamRenderer::new(
            gfx_ctx,
            config.view_resolution,
            target_view_resolution,
            [
                swapchains[0]
                    .enumerate_images()
                    .unwrap()
                    .iter()
                    .map(|i| *i as _)
                    .collect(),
                swapchains[1]
                    .enumerate_images()
                    .unwrap()
                    .iter()
                    .map(|i| *i as _)
                    .collect(),
            ],
            format,
            config.foveated_encoding_config,
            !((core_ctx.platform().is_pico()
                || (core_ctx.platform() == Platform::SamsungGalaxyXR))
                && config.enable_hdr),
            // TODO: Find a driver heuristic for the limited range bug instead?
            core_ctx.platform() != Platform::SamsungGalaxyXR && !config.enable_hdr,
            config.encoding_gamma,
            config.upscaling.clone(),
        );

        {
            let int_ctx = interaction_ctx.read();
            core_ctx.send_active_interaction_profile(
                *HAND_LEFT_ID,
                int_ctx.hands_interaction[0].controllers_profile_id,
                int_ctx.hands_interaction[0].input_ids.clone(),
            );
            core_ctx.send_active_interaction_profile(
                *HAND_RIGHT_ID,
                int_ctx.hands_interaction[1].controllers_profile_id,
                int_ctx.hands_interaction[1].input_ids.clone(),
            );
        }

        let input_thread_running = Arc::new(RelaxedAtomic::new(false));

        let stage_reference_space = Arc::new(interaction::get_reference_space(
            &xr_session,
            xr::ReferenceSpaceType::STAGE,
        ));
        let view_reference_space = Arc::new(interaction::get_reference_space(
            &xr_session,
            xr::ReferenceSpaceType::VIEW,
        ));

        let mut this = StreamContext {
            use_custom_reprojection: core_ctx.platform().is_yvr(),
            core_context: core_ctx,
            xr_session,
            interaction_context: interaction_ctx,
            stage_reference_space,
            view_reference_space,
            swapchains,
            last_good_video_frame_metadata: VideoFrameMetadata {
                view_params: [ViewParams::DUMMY; 2],
                foveation_center_shifts: None,
            },
            input_thread: None,
            input_thread_running,
            config,
            target_view_resolution,
            renderer,
            decoder: None,
        };

        this.update_reference_space();

        this
    }

    pub fn uses_passthrough(&self) -> bool {
        self.config.passthrough.is_some()
    }

    pub fn update_reference_space(&mut self) {
        self.input_thread_running.set(false);

        self.stage_reference_space = Arc::new(interaction::get_reference_space(
            &self.xr_session,
            xr::ReferenceSpaceType::STAGE,
        ));
        self.view_reference_space = Arc::new(interaction::get_reference_space(
            &self.xr_session,
            xr::ReferenceSpaceType::VIEW,
        ));

        self.core_context.send_playspace(
            self.xr_session
                .reference_space_bounds_rect(xr::ReferenceSpaceType::STAGE)
                .unwrap()
                .map(|a| Vec2::new(a.width, a.height)),
        );

        if let Some(running) = self.input_thread.take() {
            running.join().ok();
        }

        self.input_thread_running.set(true);

        self.input_thread = Some(thread::spawn({
            let core_ctx = Arc::clone(&self.core_context);
            let xr_session = self.xr_session.clone();
            let interaction_ctx = Arc::clone(&self.interaction_context);
            let stage_reference_space = Arc::clone(&self.stage_reference_space);
            let view_reference_space = Arc::clone(&self.view_reference_space);
            let refresh_rate = self.config.refresh_rate_hint;
            let running = Arc::clone(&self.input_thread_running);
            move || {
                stream_input_loop(
                    &core_ctx,
                    xr_session,
                    &interaction_ctx,
                    &stage_reference_space,
                    &view_reference_space,
                    refresh_rate,
                    running,
                )
            }
        }));
    }

    /// Size of ONE decoded frame as the codec emits it, i.e. both eyes packed side by
    /// side - NOT the size of a single eye view.
    ///
    /// The encoder packs both eyes into one frame, so the decoder's output is twice as
    /// wide as one eye. Measured on device (OUTFMT): with the previous single-eye value
    /// the codec reported width=3648 height=1824 while MediaFormat and ImageReader had
    /// both been configured for 1824x1824. The ImageReader therefore had to rebuild its
    /// buffers once the first real frame arrived, which is precisely the cost the earlier
    /// frame_size change was introduced to avoid.
    ///
    /// The width is doubled here rather than taken from the foveation config because
    /// encoded_view_resolution describes one eye.
    fn decoded_frame_size(&self) -> [u32; 2] {
        let eye = if let Some(config) = &self.config.foveated_encoding_config {
            [
                config.encoded_view_resolution[0],
                config.encoded_view_resolution[1],
            ]
        } else {
            [self.config.view_resolution.x, self.config.view_resolution.y]
        };


        [eye[0] * 2, eye[1]]
    }

    pub fn maybe_initialize_decoder(&mut self, codec: CodecType, config_nal: Vec<u8>) {
        let new_config = VideoDecoderConfig {
            codec,
            force_software_decoder: self.config.force_software_decoder,
            max_buffering_frames: self.config.max_buffering_frames,
            buffering_history_weight: self.config.buffering_history_weight,
            options: self.config.decoder_options.clone(),
            config_buffer: config_nal,
            frame_size: self.decoded_frame_size(),
        };

        let maybe_config = if let Some((config, _)) = &self.decoder {
            (new_config != *config).then_some(new_config)
        } else {
            Some(new_config)
        };

        if let Some(config) = maybe_config {
            let (mut sink, source) = video_decoder::create_decoder(config.clone(), {
                let ctx = Arc::clone(&self.core_context);
                move |maybe_timestamp: Result<Duration>| match maybe_timestamp {
                    Ok(timestamp) => ctx.report_frame_decoded(timestamp),
                    Err(e) => ctx.report_fatal_decoder_error(&e.to_string()),
                }
            });
            self.decoder = Some((config, source));

            self.core_context.set_decoder_input_callback(Box::new(
                move |timestamp, buffer| -> bool { sink.push_nal(timestamp, buffer) },
            ));
        }
    }

    pub fn update_real_time_config(&mut self, config: &RealTimeConfig) {
        self.config.passthrough = config.passthrough.clone();
        self.config.clientside_post_processing = config.clientside_post_processing.clone();
    }

    pub fn render(
        &mut self,
        frame_interval: Duration,
        vsync_time: Duration,
    ) -> (ProjectionLayerBuilder<'_>, Duration) {
        let xr_vsync_time = xr::Time::from_nanos(vsync_time.as_nanos() as _);
        let frame_poll_deadline = Instant::now()
            + Duration::from_secs_f32(
                frame_interval.as_secs_f32() * DECODER_MAX_TIMEOUT_MULTIPLIER,
            );

        // TEMPORARY DIAGNOSTIC (the deadline itself).
        //
        // frame_interval comes from xrWaitFrame's predicted_display_period EVERY frame
        // (lib.rs:589), so the poll deadline moves with it. Before changing
        // DECODER_MAX_TIMEOUT_MULTIPLIER we must know whether that period is a rock-steady
        // 11.111 ms or whether the runtime varies it - because if it varies, the deadline
        // varies too and a fixed multiplier is only ever a partial fix.
        //
        //   period     - predicted_display_period for this frame
        //   deadline   - period * DECODER_MAX_TIMEOUT_MULTIPLIER (the value that matters)
        //   vsync_gap  - spacing between consecutive predicted_display_time values; this is
        //                the runtime's own frame grid and should track the period
        //
        // A tight, single-valued period means the multiplier can be tuned safely.
        // A bimodal or wandering period means the deadline itself is the instability.
        {
            use std::sync::atomic::{AtomicU64, Ordering};
            static N: AtomicU64 = AtomicU64::new(0);
            static SUM_P: AtomicU64 = AtomicU64::new(0);
            static MIN_P: AtomicU64 = AtomicU64::new(u64::MAX);
            static MAX_P: AtomicU64 = AtomicU64::new(0);
            static SUM_D: AtomicU64 = AtomicU64::new(0);
            static MAX_D: AtomicU64 = AtomicU64::new(0);
            static PREV_VSYNC: AtomicU64 = AtomicU64::new(0);
            static SUM_VG: AtomicU64 = AtomicU64::new(0);
            static MAX_VG: AtomicU64 = AtomicU64::new(0);
            static MIN_VG: AtomicU64 = AtomicU64::new(u64::MAX);
            static HP: [AtomicU64; 6] = [
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
            ];
            static HV: [AtomicU64; 6] = [
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
            ];

            let period_ns = frame_interval.as_nanos() as u64;
            let deadline_ns =
                (frame_interval.as_secs_f64() * DECODER_MAX_TIMEOUT_MULTIPLIER as f64 * 1e9)
                    as u64;
            let vsync_ns = vsync_time.as_nanos() as u64;

            let n = N.fetch_add(1, Ordering::Relaxed) + 1;
            SUM_P.fetch_add(period_ns, Ordering::Relaxed);
            MIN_P.fetch_min(period_ns, Ordering::Relaxed);
            MAX_P.fetch_max(period_ns, Ordering::Relaxed);
            SUM_D.fetch_add(deadline_ns, Ordering::Relaxed);
            MAX_D.fetch_max(deadline_ns, Ordering::Relaxed);

            // bucket by 0.5 ms around 11.111 ms
            let pb = match period_ns / 1_000_000 {
                0..=8 => 0,
                9 => 1,
                10 => 2,
                11 => 3,
                12 => 4,
                _ => 5,
            };
            HP[pb].fetch_add(1, Ordering::Relaxed);

            let prev_v = PREV_VSYNC.swap(vsync_ns, Ordering::Relaxed);
            if prev_v != 0 && vsync_ns > prev_v {
                let vg = vsync_ns - prev_v;
                SUM_VG.fetch_add(vg, Ordering::Relaxed);
                MAX_VG.fetch_max(vg, Ordering::Relaxed);
                MIN_VG.fetch_min(vg, Ordering::Relaxed);
                let vb = match vg / 1_000_000 {
                    0..=8 => 0,
                    9 => 1,
                    10 => 2,
                    11 => 3,
                    12 => 4,
                    _ => 5,
                };
                HV[vb].fetch_add(1, Ordering::Relaxed);
            }

            if alvr_client_core::diagnostics_enabled() && n % 180 == 0 {
                let c = n as f64;
                alvr_common::warn!(
                    "FRAMEINT n={} period_avg={:.3}ms min={:.3}ms max={:.3}ms | deadline_avg={:.3}ms max={:.3}ms | vsync_gap_avg={:.3}ms min={:.3}ms max={:.3}ms | period_hist(<9,9-10,10-11,11-12,12-13,13+ms)={}/{}/{}/{}/{}/{} | vsync_hist={}/{}/{}/{}/{}/{}",
                    n,
                    SUM_P.load(Ordering::Relaxed) as f64 / c / 1e6,
                    MIN_P.load(Ordering::Relaxed) as f64 / 1e6,
                    MAX_P.load(Ordering::Relaxed) as f64 / 1e6,
                    SUM_D.load(Ordering::Relaxed) as f64 / c / 1e6,
                    MAX_D.load(Ordering::Relaxed) as f64 / 1e6,
                    SUM_VG.load(Ordering::Relaxed) as f64 / c / 1e6,
                    MIN_VG.load(Ordering::Relaxed) as f64 / 1e6,
                    MAX_VG.load(Ordering::Relaxed) as f64 / 1e6,
                    HP[0].load(Ordering::Relaxed),
                    HP[1].load(Ordering::Relaxed),
                    HP[2].load(Ordering::Relaxed),
                    HP[3].load(Ordering::Relaxed),
                    HP[4].load(Ordering::Relaxed),
                    HP[5].load(Ordering::Relaxed),
                    HV[0].load(Ordering::Relaxed),
                    HV[1].load(Ordering::Relaxed),
                    HV[2].load(Ordering::Relaxed),
                    HV[3].load(Ordering::Relaxed),
                    HV[4].load(Ordering::Relaxed),
                    HV[5].load(Ordering::Relaxed),
                );
            }
        }
        // TEMPORARY DIAGNOSTIC (client render path timing). Measures the three segments of
        // render() that can each stall a frame, so the total budget is accounted for:
        //   fetch  - polling the decoder until a frame appears (or the deadline expires)
        //   views  - locate_views(), which asks the runtime for the current head pose
        //   draw   - renderer.render(), the actual GL submission
        // Guarded: with diagnostics off this is one atomic load instead of two clock reads.
        let diag_on = alvr_client_core::diagnostics_enabled();
        let t_fetch0 = diag_on.then(Instant::now);
        let mut frame_result = None;
        let mut poll_count: u32 = 0;
        if let Some((_, source)) = &mut self.decoder {
            while frame_result.is_none() && Instant::now() < frame_poll_deadline {
                frame_result = source.get_frame();
                poll_count += 1;
                thread::sleep(Duration::from_micros(500));
            }
        }
        let fetch_us = t_fetch0.map_or(0, |t| t.elapsed().as_micros() as u64);

        // TEMPORARY DIAGNOSTIC (repeat vs gap). A slow-motion capture shows the image
        // HOLDING for several frames, then jumping a long way. Two causes, and they need
        // to be separated:
        //
        //   GOT / gap == 0   : a fresh decoded frame every render()  - healthy
        //   GOT / gap  > 0   : decoded frames arrived further apart than one frame period
        //                      -> content starves; the hold is upstream of this code
        //   EMPTY            : render() found NO frame within its poll deadline
        //                      -> the hold happens HERE (this is the literal "image held")
        //
        // Only real decodes are compared against each other. Mixing them with the
        // vsync_time fallback would compare two different clocks and produce nonsense,
        // and an "identical timestamp" counter would be structurally always zero because
        // neither branch can repeat the previous value.
        {
            use std::sync::atomic::{AtomicU64, Ordering};
            static N: AtomicU64 = AtomicU64::new(0);
            static EMPTY: AtomicU64 = AtomicU64::new(0);
            static GOT: AtomicU64 = AtomicU64::new(0);
            static GAP1: AtomicU64 = AtomicU64::new(0);
            static GAP2: AtomicU64 = AtomicU64::new(0);
            static GAP3: AtomicU64 = AtomicU64::new(0);
            static PREV_DECODED: AtomicU64 = AtomicU64::new(u64::MAX);
            static MAX_GAP_US: AtomicU64 = AtomicU64::new(0);
            static EMPTY_RUN: AtomicU64 = AtomicU64::new(0);

            let n = N.fetch_add(1, Ordering::Relaxed) + 1;
            let period_ns = frame_interval.as_nanos().max(1) as u64;

            if let Some((ts, _)) = frame_result {
                let ts = ts.as_nanos() as u64;
                GOT.fetch_add(1, Ordering::Relaxed);
                let prev = PREV_DECODED.swap(ts, Ordering::Relaxed);
                let held = EMPTY_RUN.swap(0, Ordering::Relaxed);
                if held > 0 && alvr_client_core::diagnostics_enabled() {
                    alvr_common::warn!(
                        "FRAME_RESUME n={} pts={} prev={} held={} vsync={} probe=stall_v1",
                        n, ts, prev, held, vsync_time.as_nanos(),
                    );
                }
                if prev != u64::MAX && ts > prev {
                    let gap = ts - prev;
                    MAX_GAP_US.fetch_max(gap / 1000, Ordering::Relaxed);
                    // Compare against the nominal period with a little slack: perfectly
                    // paced 90 Hz frames still land at 11.1x ms, not exactly 11.111.
                    let slack = period_ns + period_ns / 4;
                    if gap > slack {
                        if gap < 2 * period_ns {
                            GAP1.fetch_add(1, Ordering::Relaxed);
                        } else if gap < 3 * period_ns {
                            GAP2.fetch_add(1, Ordering::Relaxed);
                        } else {
                            GAP3.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            } else {
                EMPTY.fetch_add(1, Ordering::Relaxed);
                let held = EMPTY_RUN.fetch_add(1, Ordering::Relaxed) + 1;
                // 首次缺帧和每第30次持续缺帧记事件，恢复时再记录新PTS。
                if alvr_client_core::diagnostics_enabled() && (held == 1 || held % 30 == 0) {
                    alvr_common::warn!(
                        "FRAME_HOLD n={} prev={} held={} vsync={} fetch_us={} probe=stall_v1",
                        n, PREV_DECODED.load(Ordering::Relaxed), held,
                        vsync_time.as_nanos(), fetch_us,
                    );
                }
            }

            if alvr_client_core::diagnostics_enabled() && n % 90 == 0 {
                let got = GOT.load(Ordering::Relaxed);
                alvr_common::warn!(
                    "REPEATGAP n={} got={} empty={} empty_ratio={:.2}% | gap>1.25f={} gap>=2f={} gap>=3f={} | gap_max={:.2}ms (period={:.2}ms)",
                    n,
                    got,
                    EMPTY.load(Ordering::Relaxed),
                    EMPTY.load(Ordering::Relaxed) as f64 * 100.0 / n as f64,
                    GAP1.load(Ordering::Relaxed),
                    GAP2.load(Ordering::Relaxed),
                    GAP3.load(Ordering::Relaxed),
                    MAX_GAP_US.load(Ordering::Relaxed) as f64 / 1000.0,
                    period_ns as f64 / 1e6,
                );
            }

            // fetch timing: how long render() spends waiting for a decoded frame. If this
            // regularly approaches the 8.9 ms deadline, the poll window is too tight for
            // the decoder's actual output interval.
            {
                static FN: AtomicU64 = AtomicU64::new(0);
                static SUM_FETCH_US: AtomicU64 = AtomicU64::new(0);
                static MAX_FETCH_US: AtomicU64 = AtomicU64::new(0);
                static SUM_POLLS: AtomicU64 = AtomicU64::new(0);
                static MAX_POLLS: AtomicU64 = AtomicU64::new(0);
                static HF: [AtomicU64; 5] = [
                    AtomicU64::new(0),
                    AtomicU64::new(0),
                    AtomicU64::new(0),
                    AtomicU64::new(0),
                    AtomicU64::new(0),
                ];
                let fi = FN.fetch_add(1, Ordering::Relaxed) + 1;
                SUM_FETCH_US.fetch_add(fetch_us, Ordering::Relaxed);
                MAX_FETCH_US.fetch_max(fetch_us, Ordering::Relaxed);
                SUM_POLLS.fetch_add(poll_count as u64, Ordering::Relaxed);
                MAX_POLLS.fetch_max(poll_count as u64, Ordering::Relaxed);
                let hb = match fetch_us {
                    0..=499 => 0,
                    500..=1999 => 1,
                    2000..=4999 => 2,
                    5000..=8499 => 3,
                    _ => 4,
                };
                HF[hb].fetch_add(1, Ordering::Relaxed);
                if alvr_client_core::diagnostics_enabled() && fi % 90 == 0 {
                    let c = fi as f64;
                    alvr_common::warn!(
                        "FETCHTIME n={} avg={:.2}ms max={:.2}ms | polls_avg={:.2} polls_max={} | hist(<0.5,<2,<5,<8.5,8.5+ms)={}/{}/{}/{}/{}",
                        fi,
                        SUM_FETCH_US.load(Ordering::Relaxed) as f64 / c / 1000.0,
                        MAX_FETCH_US.load(Ordering::Relaxed) as f64 / 1000.0,
                        SUM_POLLS.load(Ordering::Relaxed) as f64 / c,
                        MAX_POLLS.load(Ordering::Relaxed),
                        HF[0].load(Ordering::Relaxed),
                        HF[1].load(Ordering::Relaxed),
                        HF[2].load(Ordering::Relaxed),
                        HF[3].load(Ordering::Relaxed),
                        HF[4].load(Ordering::Relaxed),
                    );
                }
            }
        }

        // TEMPORARY DIAGNOSTIC (per-frame empty evidence).
        //
        // The remaining hypothesis is that the 8.8% of long SteamVR cycles (11.5-13 ms)
        // push the output phase back, and the client's 8.889 ms poll window then misses
        // the decoded frame - producing exactly one repeated frame.
        //
        // To test it frame by frame, every empty must be printed with the quantities that
        // distinguish the causes:
        //   fetch_us   - how long render() actually waited (>8.5 ms means it hit the
        //                deadline; a short wait means the queue was drained early)
        //   polls      - how many 500 us probes it made
        //   vsync      - the runtime's display time for this frame, so the entry can be
        //                matched against the server's OVERRUN lines by wall-clock order
        //
        // If empties coincide with fetch_us near 8.889 ms, the deadline is the cause.
        // If they coincide with short fetches, the queue was empty for another reason.
        if frame_result.is_none() {
            static EN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let en = EN.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if alvr_client_core::diagnostics_enabled() && en <= 600 {
                alvr_common::warn!(
                    "EMPTYFRAME i={} fetch_us={} polls={} vsync_ns={}",
                    en,
                    fetch_us,
                    poll_count,
                    vsync_time.as_nanos() as u64
                );
            }
        }

        let (timestamp, frame_metadata, buffer_ptr) =
            if let Some((timestamp, buffer_ptr)) = frame_result {
                if let Some(metadata) = self.core_context.report_compositor_start(timestamp) {
                    self.last_good_video_frame_metadata = VideoFrameMetadata {
                        foveation_center_shifts: metadata
                            .foveation_center_shifts
                            .or(self.last_good_video_frame_metadata.foveation_center_shifts),
                        ..metadata
                    };
                }

                // Keep displaying new images even when their metadata is unavailable.
                (timestamp, self.last_good_video_frame_metadata, buffer_ptr)
            } else {
                (
                    vsync_time,
                    self.last_good_video_frame_metadata,
                    ptr::null_mut(),
                )
            };
        // TEMPORARY DIAGNOSTIC (chain audit, final point): the freshness of the frame we
        // are about to submit, expressed in frame periods. `timestamp` is the remote
        // content time (already carrying the accumulated prediction offsets);
        // `vsync_time` is what the runtime says this frame is for. Their difference is
        // therefore how far ahead of the display instant the content is. A stable offset
        // means one fixed latency; a wobbling one means the image and the pose disagree by
        // a varying amount, which is exactly what a translation judder would look like.
        {
            use std::sync::atomic::{AtomicU64, Ordering};
            static N: AtomicU64 = AtomicU64::new(0);
            static SUM: AtomicU64 = AtomicU64::new(0);
            static MIN: AtomicU64 = AtomicU64::new(u64::MAX);
            static MAX: AtomicU64 = AtomicU64::new(0);
            static PREV: AtomicU64 = AtomicU64::new(u64::MAX);
            static SUM_D: AtomicU64 = AtomicU64::new(0);
            static MAX_D: AtomicU64 = AtomicU64::new(0);
            static NULLF: AtomicU64 = AtomicU64::new(0);
            static H: [AtomicU64; 6] = [
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
            ];

            // signed: positive = content is ahead of the display instant
            let diff_ns = timestamp.as_nanos() as i64 - vsync_time.as_nanos() as i64;
            let mag = diff_ns.unsigned_abs();
            if buffer_ptr.is_null() {
                NULLF.fetch_add(1, Ordering::Relaxed);
            }

            let n = N.fetch_add(1, Ordering::Relaxed) + 1;
            SUM.fetch_add(mag, Ordering::Relaxed);
            MAX.fetch_max(mag, Ordering::Relaxed);
            MIN.fetch_min(mag, Ordering::Relaxed);
            let prev = PREV.swap(mag, Ordering::Relaxed);
            if prev != u64::MAX {
                let d = mag.abs_diff(prev);
                SUM_D.fetch_add(d, Ordering::Relaxed);
                MAX_D.fetch_max(d, Ordering::Relaxed);
            }
            let b = match mag / 1_000_000 {
                0..=49 => 0,
                50..=99 => 1,
                100..=129 => 2,
                130..=160 => 3,
                161..=250 => 4,
                _ => 5,
            };
            H[b].fetch_add(1, Ordering::Relaxed);

            if alvr_client_core::diagnostics_enabled() && n % 90 == 0 {
                let c = n as f64;
                alvr_common::warn!(
                    "CHAINEND n={} diff_avg={:.2}ms min={:.2}ms max={:.2}ms | delta_avg={:.2}ms delta_max={:.2}ms | null={} | hist(<50,50-99,100-129,130-160,161-250,250+ms)={}/{}/{}/{}/{}/{}",
                    n,
                    SUM.load(Ordering::Relaxed) as f64 / c / 1e6,
                    MIN.load(Ordering::Relaxed) as f64 / 1e6,
                    MAX.load(Ordering::Relaxed) as f64 / 1e6,
                    SUM_D.load(Ordering::Relaxed) as f64 / (c - 1.0) / 1e6,
                    MAX_D.load(Ordering::Relaxed) as f64 / 1e6,
                    NULLF.load(Ordering::Relaxed),
                    H[0].load(Ordering::Relaxed),
                    H[1].load(Ordering::Relaxed),
                    H[2].load(Ordering::Relaxed),
                    H[3].load(Ordering::Relaxed),
                    H[4].load(Ordering::Relaxed),
                    H[5].load(Ordering::Relaxed),
                );
            }
        }

        let view_params = frame_metadata.view_params;

        let left_swapchain_idx = self.swapchains[0].acquire_image().unwrap();
        let right_swapchain_idx = self.swapchains[1].acquire_image().unwrap();

        self.swapchains[0]
            .wait_image(xr::Duration::INFINITE)
            .unwrap();
        self.swapchains[1]
            .wait_image(xr::Duration::INFINITE)
            .unwrap();

        let (flags, maybe_views) = self
            .xr_session
            .locate_views(
                xr::ViewConfigurationType::PRIMARY_STEREO,
                xr_vsync_time,
                &self.stage_reference_space,
            )
            .unwrap();

        let current_headset_views = if flags.contains(xr::ViewStateFlags::ORIENTATION_VALID) {
            maybe_views
        } else {
            vec![crate::default_view(), crate::default_view()]
        };

        // The poses and FoVs we received from the PC runtime, which may differ and/or include
        // altered FoVs based on settings and view conversions done for canting.
        let input_view_params = view_params;
        let mut output_view_params = input_view_params;
        // Avoid passing invalid timestamp to runtime.
        // `timestamp` is generally a current vsync time, but may be repeated if frames are
        // dropped. Some runtimes dislike it if the timestamp is repeated for too long, so after
        // one second we begin presenting a lagged vsync time instead.
        let mut openxr_display_time =
            Duration::max(timestamp, vsync_time.saturating_sub(Duration::from_secs(1)));

        // RESULT (2026-11-14): displayTime is NOT the cause of the judder.
        //
        // An A/B run was done with this line forced to `vsync_time`. The switch was
        // confirmed active (the client logged "DISPLAYTIME mode=B"), and the result was:
        //   - "Rendered frame is earlier than the next expected frame": 1699 -> 1117
        //   - disagreement p50: 133.36 ms -> 144.44 ms  (worse, not better)
        //   - subjective judder: unchanged
        // So xrEndFrame.displayTime only affects Quest-side bookkeeping. Do not redo it.

        // (shinyquagsire23) I don't entirely trust runtimes to implement CompositionLayerProjectionView
        // correctly, but if we do trust them, avoid doing rotation ourselves. Otherwise, rerender.
        // Ex: YVR/PFDMR has issues with aspect ratio mismatches and passthrough compositing.
        if self.use_custom_reprojection {
            output_view_params = [
                ViewParams {
                    pose: crate::from_xr_pose(current_headset_views[0].pose),
                    fov: crate::from_xr_fov(current_headset_views[0].fov),
                },
                ViewParams {
                    pose: crate::from_xr_pose(current_headset_views[1].pose),
                    fov: crate::from_xr_fov(current_headset_views[1].fov),
                },
            ];

            openxr_display_time = vsync_time;
        }

        self.renderer.render(
            buffer_ptr,
            [
                StreamViewParams {
                    swapchain_index: left_swapchain_idx,
                    input_view_params: input_view_params[0],
                    output_view_params: output_view_params[0],
                },
                StreamViewParams {
                    swapchain_index: right_swapchain_idx,
                    input_view_params: input_view_params[1],
                    output_view_params: output_view_params[1],
                },
            ],
            self.config.passthrough.as_ref(),
            frame_metadata
                .foveation_center_shifts
                .map(|centers| centers.map(Vec2::from_array)),
        );

        self.swapchains[0].release_image().unwrap();
        self.swapchains[1].release_image().unwrap();

        if !buffer_ptr.is_null()
            && let Some(xr_now) = crate::xr_runtime_now(self.xr_session.instance())
        {
            self.core_context.report_submit(
                timestamp,
                vsync_time.saturating_sub(Duration::from_nanos(xr_now.as_nanos() as u64)),
            );
        }

        let rect = xr::Rect2Di {
            offset: xr::Offset2Di { x: 0, y: 0 },
            extent: xr::Extent2Di {
                width: self.target_view_resolution.x as _,
                height: self.target_view_resolution.y as _,
            },
        };

        let clientside_post_processing = self
            .xr_session
            .instance()
            .exts()
            .fb_composition_layer_settings
            .and(self.config.clientside_post_processing.clone());

        let layer = ProjectionLayerBuilder::new(
            &self.stage_reference_space,
            [
                xr::CompositionLayerProjectionView::new()
                    .pose(crate::to_xr_pose(output_view_params[0].pose))
                    .fov(crate::to_xr_fov(output_view_params[0].fov))
                    .sub_image(
                        xr::SwapchainSubImage::new()
                            .swapchain(&self.swapchains[0])
                            .image_array_index(0)
                            .image_rect(rect),
                    ),
                xr::CompositionLayerProjectionView::new()
                    .pose(crate::to_xr_pose(output_view_params[1].pose))
                    .fov(crate::to_xr_fov(output_view_params[1].fov))
                    .sub_image(
                        xr::SwapchainSubImage::new()
                            .swapchain(&self.swapchains[1])
                            .image_array_index(0)
                            .image_rect(rect),
                    ),
            ],
            self.config
                .passthrough
                .clone()
                .map(|mode| ProjectionLayerAlphaConfig {
                    premultiplied: matches!(
                        mode,
                        PassthroughMode::Blend {
                            premultiplied_alpha: true,
                            ..
                        } | PassthroughMode::RgbChromaKey(_)
                            | PassthroughMode::HsvChromaKey(_)
                    ),
                }),
            clientside_post_processing,
        );

        (layer, openxr_display_time)
    }
}

impl Drop for StreamContext {
    fn drop(&mut self) {
        self.input_thread_running.set(false);
        self.input_thread.take().unwrap().join().ok();
    }
}

fn stream_input_loop(
    core_ctx: &ClientCoreContext,
    xr_session: xr::Session<xr::OpenGlEs>,
    interaction_ctx: &RwLock<InteractionContext>,
    stage_reference_space: &xr::Space,
    view_reference_space: &xr::Space,
    refresh_rate: f32,
    running: Arc<RelaxedAtomic>,
) {
    let mut last_controller_poses = [Pose::IDENTITY; 2];
    let mut last_palm_poses = [Pose::IDENTITY; 2];
    let mut last_view_params = [ViewParams::DUMMY; 2];

    let mut deadline = Instant::now();
    let frame_interval = Duration::from_secs_f32(1.0 / refresh_rate);
    while running.value() {
        let int_ctx_lock = interaction_ctx.read();
        let int_ctx = &*int_ctx_lock;
        // Streaming related inputs are updated here. Make sure every input poll is done in this
        // thread
        if let Err(e) = xr_session.sync_actions(&[(&int_ctx.action_set).into()]) {
            error!("{e}");
            return;
        }

        let Some(now) = crate::xr_runtime_now(xr_session.instance()).map(crate::from_xr_time)
        else {
            error!("Cannot poll tracking: invalid time");
            return;
        };

        let target_time = now + core_ctx.get_total_prediction_offset();

        let Some((head_motion, local_views)) = interaction::get_head_data(
            &xr_session,
            core_ctx.platform(),
            stage_reference_space,
            view_reference_space,
            now,
            target_time,
            &last_view_params,
        ) else {
            continue;
        };

        if let Some(views) = local_views {
            core_ctx.send_view_params(views);
            last_view_params = views;
        }

        let mut device_motions = Vec::with_capacity(3);

        device_motions.push((*HEAD_ID, head_motion));

        let left_hand_data = crate::interaction::get_hand_data(
            &xr_session,
            core_ctx.platform(),
            stage_reference_space,
            now,
            target_time,
            &int_ctx.hands_interaction[0],
            &mut last_controller_poses[0],
            &mut last_palm_poses[0],
        );
        let right_hand_data = crate::interaction::get_hand_data(
            &xr_session,
            core_ctx.platform(),
            stage_reference_space,
            now,
            target_time,
            &int_ctx.hands_interaction[1],
            &mut last_controller_poses[1],
            &mut last_palm_poses[1],
        );

        // Note: When multimodal input is enabled, we are sure that when free hands are used
        // (not holding controllers) the controller data is None.
        if (int_ctx.multimodal_hands_enabled || left_hand_data.skeleton_joints.is_none())
            && let Some(motion) = left_hand_data.grip_motion
        {
            device_motions.push((*HAND_LEFT_ID, motion));
        }
        if (int_ctx.multimodal_hands_enabled || right_hand_data.skeleton_joints.is_none())
            && let Some(motion) = right_hand_data.grip_motion
        {
            device_motions.push((*HAND_RIGHT_ID, motion));
        }

        if int_ctx.multimodal_hands_enabled
            && let Some(detached_controller) = left_hand_data.detached_grip_motion
        {
            device_motions.push((*DETACHED_CONTROLLER_LEFT_ID, detached_controller));
        }
        if int_ctx.multimodal_hands_enabled
            && let Some(detached_controller) = right_hand_data.detached_grip_motion
        {
            device_motions.push((*DETACHED_CONTROLLER_RIGHT_ID, detached_controller));
        }

        let face = interaction::get_face_data(
            &xr_session,
            &int_ctx.face_sources,
            view_reference_space,
            now,
        );

        let body = int_ctx
            .body_source
            .as_ref()
            .and_then(|source| interaction::get_body_skeleton(source, stage_reference_space, now));

        if let Some(source) = &int_ctx.body_source {
            device_motions.append(&mut interaction::get_bd_motion_trackers(source, now));
        }

        drop(int_ctx_lock);

        // Even though the server is already adding the motion-to-photon latency, here we use
        // target_time as the poll_timestamp to compensate for the fact that video frames are sent
        // with the poll timestamp instead of the vsync time. This is to ensure correctness when
        // submitting frames to OpenXR. This won't cause any desync with the server because no time
        // sync step is performed between client and server.
        core_ctx.send_tracking(TrackingData {
            poll_timestamp: target_time,
            device_motions,
            hand_skeletons: [
                left_hand_data.skeleton_joints,
                right_hand_data.skeleton_joints,
            ],
            face,
            body,
        });

        let button_entries =
            interaction::update_buttons(&xr_session, &interaction_ctx.read().button_actions);
        if !button_entries.is_empty() {
            core_ctx.send_buttons(button_entries);
        }

        // Tracking is sent at 3x the frame rate (frame_interval / 3, ~263 Hz against a
        // 90 Hz display), so PoseHistory receives ~2.93 entries per submitted frame and
        // GetBestPoseMatch samples 2, 3 or 4 of them per frame.
        //
        // TESTED 2026-11-14: sending at 1x (frame_interval) instead made the POSERAW step
        // perfectly uniform (all ~11.1 ms, no more 7.4/14.8 ms excursions) and cut the
        // client's gap>1.25f from 9.16% to 0.17% - a 54x reduction. The user reported NO
        // perceptible difference, and empty_ratio got worse (0.81% -> 5.75%). So the
        // 2.93 sampling ratio is NOT the cause of the visible stutter; reverted. Do not
        // retry this without a new reason.
        deadline += frame_interval / 3;
        thread::sleep(deadline.saturating_duration_since(Instant::now()));
    }
}
