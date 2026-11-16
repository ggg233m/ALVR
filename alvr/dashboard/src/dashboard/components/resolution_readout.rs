//! Derived resolution readout for the presets tab.
//!
//! The user sees four different resolutions that are easy to confuse, because they come
//! from three independent sources and are then transformed twice:
//!
//!   game      = emulated_headset_view_resolution   (handed to SteamVR; the game renders this)
//!   hmd       = transcoding_view_resolution * upscale_factor
//!                                                  (what the client actually requests for
//!                                                   its swapchain; upscaling stretches the
//!                                                   decoded image up to this)
//!   encoded   = transcoding_view_resolution after foveated-encoding compression
//!   transport = encoded * 2, side by side       (what actually crosses the link)
//!
//! The arithmetic here is a deliberate duplicate of server_core/src/connection.rs:579-718.
//! It has to be a duplicate: the dashboard has no access to the negotiation code, and the
//! numbers must update live while the user drags a slider, before anything is applied.
//! If that code changes, this must change with it - there is a test at the bottom that
//! pins the two known-good values from real session logs.
//!
//! Two details are easy to get wrong and both are handled explicitly:
//!
//!   1. `center_size` is NOT used verbatim. connection.rs snaps it up to a multiple of
//!      `edge_ratio * 2` in pixel space, so a slider value of 0.60 can behave as 0.5967.
//!      The readout reports the snapped value, because that is what the encoder sees.
//!
//!   2. The snapping runs through `ceil`, so values a hair apart can land one 32-pixel
//!      step apart. That is why this module mirrors the f32/f64 split exactly instead of
//!      doing everything in f64: at center_size = 0.60 the f32 value stored by the slider
//!      (0.59999996) yields 1760 while the f64 literal 0.6 yields 1792.

use alvr_session::{FrameSize, Settings, SteamvrHmdInitConfig};

/// Round DOWN to a multiple of 32, matching connection.rs:58 `align32`.
///
/// Used for the configured view resolutions. It is a floor: 2600 becomes 2592.
fn align32_down(value: f32) -> u32 {
    ((value / 32.0).floor() * 32.0) as u32
}

/// Round UP to a multiple of 32, matching the foveation block at connection.rs:718.
///
/// The two directions are NOT interchangeable and both appear in the real pipeline:
/// the configured resolution is floored first, then the foveation-optimised resolution
/// is ceiled. Using one for the other shifts the reported encoded size by one step.
fn align32_up(value: f32) -> u32 {
    (value / 32.0).ceil() as u32 * 32
}

/// One axis of the foveation transform, mirroring connection.rs:685-718.
///
/// Returns the snapped center size as well, because it differs from the input.
fn foveate_axis(resolution: u32, center_size: f32, edge_ratio: f32) -> (u32, f32) {
    let resolution = resolution as f32;

    // f32, exactly as in the original.
    let edge_size = resolution - center_size * resolution;

    // The ceil expression is evaluated in f64 in the original, so reproduce that.
    let edge_ratio_2 = edge_ratio as f64 * 2.0;
    let snapped_center_size =
        (1.0 - (edge_size as f64 / edge_ratio_2).ceil() * edge_ratio_2 / resolution as f64) as f32;

    let scale = snapped_center_size + (1.0 - snapped_center_size) / edge_ratio;
    let optimized = scale * resolution;

    (align32_up(optimized), snapped_center_size)
}

/// Resolve an absolute `FrameSize` to pixels. Mirrors connection.rs `get_view_res`.
///
/// The height is `None` when the user did not pin it. In that case the server derives it
/// from the headset's own default aspect ratio, which the dashboard never receives - so
/// there is no honest number to show. Returning `None` lets the caller say "by headset
/// aspect" instead of inventing a 1:1 square, which is what an earlier version did.
///
/// `Scale` is deliberately unsupported: it is relative to the same unknown default.
fn view_res(config: &FrameSize) -> ([u32; 2], bool) {
    match config {
        FrameSize::Scale(_) => ([0, 0], true),
        FrameSize::Absolute { width, height } => {
            let width_f = *width as f32;
            match height {
                Some(h) => ([align32_down(width_f), align32_down(*h as f32)], true),
                // Width is known; height depends on the headset aspect.
                None => ([align32_down(width_f), 0], false),
            }
        }
    }
}

/// Everything the readout shows, for one point in time.
pub struct ResolutionBreakdown {
    pub game: [u32; 2],
    pub hmd: [u32; 2],
    pub encoded: [u32; 2],
    pub transport: [u32; 2],
    /// Encoded pixels as a fraction of the transcoding (pre-foveation) pixels.
    pub pixel_ratio: f32,
    /// Foveation center size actually applied, per axis.
    pub center_size_effective: Option<[f32; 2]>,
    /// Foveation center size the user asked for, per axis.
    pub center_size_configured: Option<[f32; 2]>,
    /// False when a height was left unset, so the server derives it from the headset's own
    /// aspect ratio. Everything that depends on the height is then unknown here.
    pub height_known: bool,
    /// Center/edge split, per axis, when foveation is on and the height is known.
    pub regions: Option<[AxisRegions; 2]>,
}

impl ResolutionBreakdown {
    /// True when foveation snapped the center size enough to be worth mentioning.
    ///
    /// The snap step is `edge_ratio * 2 / resolution`, so a difference smaller than that
    /// is just rounding noise and showing it would only add clutter.
    pub fn center_size_was_adjusted(&self) -> bool {
        match (self.center_size_effective, self.center_size_configured) {
            (Some(effective), Some(configured)) => {
                let tolerance = 0.0001;
                (effective[0] - configured[0]).abs() > tolerance
                    || (effective[1] - configured[1]).abs() > tolerance
            }
            _ => false,
        }
    }
}

/// Compute the readout from current settings.
///
/// Every dimension whose height is not pinned is reported as `None` in
/// `ResolutionBreakdown::height_known`, and the corresponding second element is 0.
pub fn compute(
    settings: &Settings,
    negotiated: Option<&SteamvrHmdInitConfig>,
) -> ResolutionBreakdown {
    let video = &settings.video;

    let (mut transcoding, transcode_height_known) = view_res(&video.transcoding_view_resolution);
    let (mut emulated, emulated_height_known) = view_res(&video.emulated_headset_view_resolution);

    // A height left unset means "derive it from the headset's own aspect ratio". The server
    // resolves that during connection negotiation and stores the result in
    // steamvr_hmd_init_config, which reaches the dashboard with the session. Use it when
    // available: it is the real number, not an estimate.
    if let Some(negotiated) = negotiated {
        if !transcode_height_known && transcoding[0] == negotiated.eye_resolution_width {
            transcoding[1] = negotiated.eye_resolution_height;
        }
        if !emulated_height_known
            && emulated[0] == negotiated.target_eye_resolution_width
        {
            emulated[1] = negotiated.target_eye_resolution_height;
        }
    }

    // Fall back to the negotiated pair itself when the widths match what the server used.
    let (transcoding, transcode_height_known) = (
        transcoding,
        transcode_height_known || transcoding[1] > 0,
    );
    let (emulated, emulated_height_known) = (
        emulated,
        emulated_height_known || emulated[1] > 0,
    );

    let upscale_factor = video
        .upscaling
        .as_option()
        .map_or(1.0, |u| u.upscale_factor);

    let foveation = video.foveated_encoding.as_option();

    let (encoded, center_size_effective, center_size_configured) = match foveation {
        Some(config) if transcode_height_known => {
            let (enc_x, snapped_x) =
                foveate_axis(transcoding[0], config.center_size[0], config.edge_ratio[0]);
            let (enc_y, snapped_y) =
                foveate_axis(transcoding[1], config.center_size[1], config.edge_ratio[1]);
            (
                [enc_x, enc_y],
                Some([snapped_x, snapped_y]),
                Some(config.center_size),
            )
        }
        // Without a known height the encoded size cannot be derived at all.
        Some(_) => ([align32_up(transcoding[0] as f32), 0], None, None),
        // No foveation: the encoder still has to respect the 32-pixel alignment.
        None => (
            [align32_up(transcoding[0] as f32), align32_up(transcoding[1] as f32)],
            None,
            None,
        ),
    };

    let height_known = transcode_height_known && emulated_height_known;

    ResolutionBreakdown {
        game: emulated,
        hmd: [
            (transcoding[0] as f32 * upscale_factor) as u32,
            (transcoding[1] as f32 * upscale_factor) as u32,
        ],
        encoded,
        transport: [encoded[0] * 2, encoded[1]],
        pixel_ratio: if height_known && transcoding[0] > 0 && transcoding[1] > 0 {
            (encoded[0] * encoded[1]) as f32 / (transcoding[0] * transcoding[1]) as f32
        } else {
            0.0
        },
        center_size_effective,
        center_size_configured,
        height_known,
        regions: if height_known {
            foveation.map(|config| {
                let hmd = [
                    (transcoding[0] as f32 * upscale_factor) as u32,
                    (transcoding[1] as f32 * upscale_factor) as u32,
                ];
                [
                    axis_regions(
                        transcoding[0],
                        encoded[0],
                        hmd[0],
                        center_size_effective.map_or(config.center_size[0], |c| c[0]),
                        config.center_shift[0],
                        config.edge_ratio[0],
                    ),
                    axis_regions(
                        transcoding[1],
                        encoded[1],
                        hmd[1],
                        center_size_effective.map_or(config.center_size[1], |c| c[1]),
                        config.center_shift[1],
                        config.edge_ratio[1],
                    ),
                ]
            })
        } else {
            None
        },
    }
}

/// Draw the readout as a grid of `label : value` rows.
///
/// `default_view_resolution` is the headset's own per-eye default. It is only needed for
/// `FrameSize::Scale`; when it is unknown and the setting uses Scale, the readout says so
/// instead of inventing numbers.
/// How one axis of the foveation transform splits into a sharp center and a compressed
/// edge.
///
/// The transform is piecewise: the center maps 1:1, the edges are squeezed by `edge_ratio`.
/// A single "encoded width" number hides that entirely. These are the two spans, expressed
/// three ways:
///
///   * in SOURCE pixels - how much real image detail the region represents
///   * in ENCODED pixels - how much of that detail actually survives the encoder
///   * in HMD pixels - how much screen area the region has to fill
///
/// The ratio source/encoded is what the display must stretch, and it is the only number
/// that corresponds to perceived softness. It does not change with upscaling, because
/// upscaling scales the screen area and the pixel budget is unchanged.
pub struct AxisRegions {
    /// Center width in the source (pre-foveation) image.
    pub center_src: u32,
    /// Center width in the encoded image.
    pub center_encoded: u32,
    /// Center width on the headset's swapchain.
    pub center_hmd: u32,
    /// Edge width in the source image (both sides combined).
    pub edge_src: u32,
    /// Edge width in the encoded image (both sides combined).
    pub edge_encoded: u32,
    /// Edge width on the headset's swapchain.
    pub edge_hmd: u32,
}

impl AxisRegions {
    /// How far the display has to stretch the center. 1.0 means one encoded pixel per
    /// screen pixel, i.e. no softening introduced by this stage.
    pub fn center_stretch(&self) -> f32 {
        if self.center_encoded == 0 {
            return 0.0;
        }
        self.center_src as f32 / self.center_encoded as f32
    }

    pub fn edge_stretch(&self) -> f32 {
        if self.edge_encoded == 0 {
            return 0.0;
        }
        self.edge_src as f32 / self.edge_encoded as f32
    }

    /// Fraction of the headset's swapchain width taken by the center.
    pub fn center_share(&self) -> f32 {
        let total = self.center_hmd + self.edge_hmd;
        if total == 0 {
            return 0.0;
        }
        self.center_hmd as f32 / total as f32
    }

    pub fn edge_share(&self) -> f32 {
        let total = self.center_hmd + self.edge_hmd;
        if total == 0 {
            return 0.0;
        }
        self.edge_hmd as f32 / total as f32
    }
}

/// Mirror of the shader's `CompressAxis` (CompressAxisAlignedPixelShader.hlsl).
///
/// Maps a position in the ENCODED image (0..1) to the SOURCE position it samples (0..1).
/// Used to find where the center region starts and ends in source coordinates, because
/// `center_size` is stated as a fraction of the SOURCE image.
fn compress_axis(eye_uv: f32, center_size: f32, center_shift: f32, edge_ratio: f32) -> f32 {
    let c0 = (1.0 - center_size) / 2.0;
    let c1 = (edge_ratio - 1.0) * c0 * (center_shift + 1.0) / edge_ratio;
    let c2 = (edge_ratio - 1.0) * center_size + 1.0;

    let lo_bound = c0 * (center_shift + 1.0) / c2;
    let hi_bound = c0 * (center_shift - 1.0) / c2 + 1.0;
    let center = eye_uv * c2 / edge_ratio + c1;

    if eye_uv < lo_bound {
        let d2 = eye_uv * c2;
        let g1 = eye_uv / lo_bound.max(1e-6);
        g1 * center + (1.0 - g1) * d2
    } else if eye_uv > hi_bound {
        let d3 = (eye_uv - 1.0) * c2 + 1.0;
        let g2 = (1.0 - eye_uv) / (1.0 - hi_bound).max(1e-6);
        g2 * center + (1.0 - g2) * d3
    } else {
        center
    }
}

/// Split one axis into its center and edge spans, in source, encoded and HMD pixels.
fn axis_regions(
    src_width: u32,
    encoded_width: u32,
    hmd_width: u32,
    center_size: f32,
    center_shift: f32,
    edge_ratio: f32,
) -> AxisRegions {
    let c0 = (1.0 - center_size) / 2.0;
    let c2 = (edge_ratio - 1.0) * center_size + 1.0;

    // Center bounds in ENCODED space.
    let lo = c0 * (center_shift + 1.0) / c2;
    let hi = c0 * (center_shift - 1.0) / c2 + 1.0;

    // The same bounds expressed in SOURCE space.
    let src_lo = compress_axis(lo, center_size, center_shift, edge_ratio);
    let src_hi = compress_axis(hi, center_size, center_shift, edge_ratio);

    let encoded_span = (hi - lo).clamp(0.0, 1.0);
    let src_span = (src_hi - src_lo).clamp(0.0, 1.0);

    let center_src = (src_width as f32 * src_span).round() as u32;
    let center_encoded = (encoded_width as f32 * encoded_span).round() as u32;
    // The swapchain shows the SOURCE framing, so the center occupies the same fraction of
    // it that it occupies of the source image.
    let center_hmd = (hmd_width as f32 * src_span).round() as u32;

    AxisRegions {
        center_src,
        center_encoded,
        center_hmd,
        edge_src: src_width.saturating_sub(center_src),
        edge_encoded: encoded_width.saturating_sub(center_encoded),
        edge_hmd: hmd_width.saturating_sub(center_hmd),
    }
}

/// Emit the readout as rows of the CALLER's grid.
///
/// This must not create its own `Grid`: the presets page already renders a two-column
/// grid, and nesting another one inside it produces four columns, which shears every row
/// off the shared left edge.
pub fn ui(
    ui: &mut eframe::egui::Ui,
    settings: &Settings,
    negotiated: Option<&SteamvrHmdInitConfig>,
) {
    use alvr_gui_common::tr;
    use eframe::egui::RichText;

    let uses_scale = matches!(
        settings.video.transcoding_view_resolution,
        FrameSize::Scale(_)
    ) || matches!(
        settings.video.emulated_headset_view_resolution,
        FrameSize::Scale(_)
    );

    if uses_scale {
        ui.label(tr("Resolution breakdown"));
        ui.label(RichText::new(tr("Not available in Scale mode")).weak());
        ui.end_row();
        return;
    }

    let breakdown = compute(settings, negotiated);
    let aspect = tr("by headset aspect");

    let row = |ui: &mut eframe::egui::Ui, label: &str, value: String| {
        ui.label(tr(label));
        ui.label(RichText::new(value).monospace());
        ui.end_row();
    };

    // A zero height means "not pinned by the user", so the server derives it from the
    // headset's own aspect ratio. Say that instead of showing a made-up number.
    let size = |w: u32, h: u32| {
        if h == 0 {
            format!("{w} × {aspect}")
        } else {
            format!("{w}×{h}")
        }
    };

    row(
        ui,
        "Game rendering (per eye)",
        size(breakdown.game[0], breakdown.game[1]),
    );
    row(
        ui,
        "Headset rendering (per eye)",
        size(breakdown.hmd[0], breakdown.hmd[1]),
    );
    row(
        ui,
        "Encoded (per eye)",
        size(breakdown.encoded[0], breakdown.encoded[1]),
    );

    // The encoded size alone hides the whole point of foveation: the center keeps every
    // pixel while the edges lose most of theirs. Report each span three ways - how much
    // real detail it holds (source), how much survives encoding, and how much screen it
    // has to fill (HMD) - plus the stretch the display applies, which is what actually
    // softens the image.
    if let Some([x, y]) = &breakdown.regions {
        // tr() only matches whole strings, so localize the fragments and assemble them
        // here rather than trying to translate a format! result.
        let of_headset = tr("of headset");
        let stretch = tr("stretch");

        row(
            ui,
            "Center region",
            format!(
                "{}×{} px  ·  {:.0}% {}  ·  {} ×{:.2}/{:.2}",
                x.center_src,
                y.center_src,
                x.center_share() * 100.0,
                of_headset,
                stretch,
                x.center_stretch(),
                y.center_stretch()
            ),
        );
        row(
            ui,
            "Edge region",
            format!(
                "{}×{} px  ·  {:.0}% {}  ·  {} ×{:.1}/{:.1}",
                x.edge_src,
                y.edge_src,
                x.edge_share() * 100.0,
                of_headset,
                stretch,
                x.edge_stretch(),
                y.edge_stretch()
            ),
        );
    }

    row(
        ui,
        "Transmitted (both eyes)",
        size(breakdown.transport[0], breakdown.transport[1]),
    );

    if breakdown.height_known {
        row(
            ui,
            "Pixel ratio",
            format!("{:.1}%", breakdown.pixel_ratio * 100.0),
        );
    } else {
        ui.label(tr("Pixel ratio"));
        ui.label(RichText::new(tr("Pin the height to compute")).weak());
        ui.end_row();
    }

    if breakdown.center_size_was_adjusted()
        && let (Some(effective), Some(configured)) = (
            breakdown.center_size_effective,
            breakdown.center_size_configured,
        )
    {
        row(
            ui,
            "Foveation center (effective)",
            format!(
                "{:.4} / {:.4}   ({:.2} / {:.2})",
                effective[0], effective[1], configured[0], configured[1]
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alvr_session::SessionConfig;
    use settings_schema::OptionalDefault;

    /// Build a concrete `Settings` from session-style values, the same way the dashboard
    /// does when it reads the live session.
    fn settings_from(
        transcoding: (u32, u32),
        emulated: (u32, u32),
        foveation: Option<(f32, f32, f32, f32)>,
    ) -> Settings {
        let mut config = SessionConfig::default();

        config.session_settings.video.transcoding_view_resolution.variant =
            alvr_session::FrameSizeDefaultVariant::Absolute;
        config
            .session_settings
            .video
            .transcoding_view_resolution
            .Absolute
            .width = transcoding.0;
        config
            .session_settings
            .video
            .transcoding_view_resolution
            .Absolute
            .height = OptionalDefault {
            set: true,
            content: transcoding.1,
        };

        config.session_settings.video.emulated_headset_view_resolution.variant =
            alvr_session::FrameSizeDefaultVariant::Absolute;
        config
            .session_settings
            .video
            .emulated_headset_view_resolution
            .Absolute
            .width = emulated.0;
        config
            .session_settings
            .video
            .emulated_headset_view_resolution
            .Absolute
            .height = OptionalDefault {
            set: true,
            content: emulated.1,
        };

        // Enable upscaling so the upscale_factor is reachable; default is 1.0 (no-op).
        config.session_settings.video.upscaling.enabled = true;

        match foveation {
            Some((cs_x, cs_y, er_x, er_y)) => {
                config.session_settings.video.foveated_encoding.enabled = true;
                config.session_settings.video.foveated_encoding.content.center_size.content =
                    [cs_x, cs_y];
                config.session_settings.video.foveated_encoding.content.edge_ratio.content =
                    [er_x, er_y];
            }
            None => {
                config.session_settings.video.foveated_encoding.enabled = false;
            }
        }

        config.to_settings()
    }

    /// Pins the values read out of a real ALVR session log, so the duplicated arithmetic
    /// in this module cannot silently drift away from server_core.
    ///
    /// Note the center size: the slider stores 0.59999996, NOT `0.6_f32` (which is
    /// 0.6000000238). `step_by(0.01)` accumulates from 0.0, so the f32 the user lands on
    /// depends on how they got there. The two values are 6.4e-8 apart and yet straddle a
    /// quantisation boundary, producing 1760 versus 1792 - which is exactly why this
    /// module reads the stored f32 instead of the displayed number.
    #[test]
    fn matches_logged_session() {
        let stored_center_size = 0.59999996_f32;

        let settings = settings_from(
            (2400, 2600),
            (2400, 2600),
            Some((stored_center_size, stored_center_size, 4.0, 5.0)),
        );

        let breakdown = compute(&settings, None);

        // "EFFECTIVE PARAMS: 3392x1760" from the session log.
        assert_eq!(breakdown.transport, [3392, 1760]);
        assert_eq!(breakdown.encoded, [1696, 1760]);
    }

    /// The session -> settings conversion goes through JSON. Confirm the f32 survives.
    #[test]
    fn center_size_survives_round_trip() {
        let probe = 0.59999996_f32;
        let settings = settings_from((2400, 2600), (2400, 2600), Some((probe, probe, 4.0, 5.0)));
        let after = settings.video.foveated_encoding.as_option().unwrap().center_size[0];
        println!("before={:?} after={:?} equal={}", probe, after, probe == after);
        assert_eq!(probe, after, "f32 changed during session->settings conversion");
    }

    /// When the height is left unpinned, the server derives it from the headset aspect and
    /// records the result. The readout must use that instead of guessing.
    #[test]
    fn negotiated_config_fills_unpinned_height() {
        let mut config = SessionConfig::default();

        // Width pinned, height not.
        config.session_settings.video.transcoding_view_resolution.variant =
            alvr_session::FrameSizeDefaultVariant::Absolute;
        config
            .session_settings
            .video
            .transcoding_view_resolution
            .Absolute
            .width = 2144;
        config
            .session_settings
            .video
            .transcoding_view_resolution
            .Absolute
            .height = OptionalDefault {
            set: false,
            content: 0,
        };

        config.session_settings.video.emulated_headset_view_resolution =
            config.session_settings.video.transcoding_view_resolution.clone();

        let settings = config.to_settings();

        // Without the negotiated data the height is unknown.
        let unknown = compute(&settings, None);
        assert!(!unknown.height_known);
        assert_eq!(unknown.game[0], 2144);
        assert_eq!(unknown.game[1], 0);

        // With it, the real height appears.
        let negotiated = SteamvrHmdInitConfig {
            eye_resolution_width: 2144,
            eye_resolution_height: 2294,
            target_eye_resolution_width: 2144,
            target_eye_resolution_height: 2294,
            refresh_rate: 90,
        };
        let known = compute(&settings, Some(&negotiated));
        assert!(known.height_known);
        assert_eq!(known.game, [2144, 2294]);
        assert_eq!(known.hmd, [2144, 2294]);
    }

    /// The center must stay at 1:1 while the edges carry all the compression.
    #[test]
    fn center_is_lossless_and_edges_are_squeezed() {
        let regions = axis_regions(2272, 1600, 2272, 0.6, 0.4, 4.0);

        // Center survives untouched (stretch ~1.0).
        let stretch = regions.center_stretch();
        assert!(
            (stretch - 1.0).abs() < 0.02,
            "center stretch should be ~1.0, got {stretch}"
        );

        // Edges lose most of their pixels.
        assert!(
            regions.edge_stretch() > 3.0,
            "edges should be stretched several times, got {}",
            regions.edge_stretch()
        );

        // The two shares must cover the whole headset width.
        assert!((regions.center_share() + regions.edge_share() - 1.0).abs() < 1e-6);

        // The two spans must add up to the whole axis.
        assert_eq!(regions.center_src + regions.edge_src, 2272);
        assert_eq!(regions.center_encoded + regions.edge_encoded, 1600);
    }

    #[test]
    fn no_foveation_is_aligned_only() {
        let settings = settings_from((2400, 2600), (2400, 2600), None);

        let breakdown = compute(&settings, None);

        // 2600 is floored to 2592 by get_view_res, then ceiled to 2592 by the encoder.
        assert_eq!(breakdown.encoded, [2400, 2592]);
        assert_eq!(breakdown.transport, [4800, 2592]);
        assert!(!breakdown.center_size_was_adjusted());
    }

    #[test]
    fn upscaling_only_moves_the_hmd_column() {
        let base_settings = settings_from(
            (2400, 2600),
            (2400, 2600),
            Some((0.6_f32, 0.6_f32, 4.0, 5.0)),
        );
        let base = compute(&base_settings, None);

        let mut upscaled_settings = base_settings.clone();
        if let settings_schema::Switch::Enabled(upscaling) =
            &mut upscaled_settings.video.upscaling
        {
            upscaling.upscale_factor = 1.5;
        }
        let upscaled = compute(&upscaled_settings, None);

        // hmd = transcoding AFTER the floor alignment (2400x2592), times 1.5.
        // There is no further alignment on this value.
        assert_eq!(upscaled.hmd, [3600, 3888]);
        // Encoding and transport must not react to upscaling.
        assert_eq!(base.encoded, upscaled.encoded);
        assert_eq!(base.transport, upscaled.transport);
    }
}
