use super::VideoDecoderConfig;
use std::sync::atomic::{AtomicU64, Ordering};
use alvr_common::{
    RelaxedAtomic, ToAny,
    anyhow::{Context, Result, anyhow, bail},
    error, info,
    parking_lot::{Condvar, Mutex},
    warn,
};
use alvr_session::{CodecType, MediacodecPropType};
use ndk::{
    hardware_buffer::HardwareBufferUsage,
    media::{
        image_reader::{AcquireResult, Image, ImageFormat, ImageReader},
        media_codec::{
            DequeuedInputBufferResult, DequeuedOutputBufferInfoResult, MediaCodec,
            MediaCodecDirection, MediaFormat,
        },
    },
};
use std::{
    collections::VecDeque,
    ffi::c_void,
    ops::Deref,
    ptr,
    sync::{Arc, Weak},
    thread::{self, JoinHandle},
    time::Duration,
};

struct FakeThreadSafe<T>(T);
unsafe impl<T> Send for FakeThreadSafe<T> {}
unsafe impl<T> Sync for FakeThreadSafe<T> {}

impl<T> Deref for FakeThreadSafe<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

type SharedMediaCodec = Arc<FakeThreadSafe<MediaCodec>>;

pub struct VideoDecoderSink {
    inner: Arc<Mutex<Option<SharedMediaCodec>>>,
}

// TEMPORARY DIAGNOSTIC (chain audit): the PTS we hand to MediaCodec vs the PTS that
// comes back out of ImageReader.
//
// Android is allowed to rewrite the timestamp (NdkMediaCodec.h:161-163 "some codecs may
// alter it"), and every downstream calculation - pose matching, metadata lookup,
// xrEndFrame - keys off whatever comes back. If the two differ, nothing else in this
// pipeline can be trusted.
//
// NOTE: this must be a SET, not a FIFO. push_frame_nal runs once per NAL while the
// ImageReader listener runs once per FRAME, so a queue drains many NAL entries per frame
// and reports a bogus mismatch every time. Membership testing is the only correct check.
// VecDeque + contains(): push_back/pop_front gives correct oldest-first eviction, and
// membership is all we need on the way out. (A HashSet was tried and is wrong here: its
// iteration order is arbitrary, so the eviction dropped newly inserted entries and left
// the set frozen on stale values, which produced a bogus 92% mismatch.)
static PTS_IN: Mutex<VecDeque<i64>> = Mutex::new(VecDeque::new());

// TEMPORARY DIAGNOSTIC: time of the last release_output_buffer_at_time, so the ImageReader
// callback can measure how long the codec took to render that buffer to the surface.
// TEMPORARY DIAGNOSTIC: PTS -> monotonic release time, so the ImageReader callback can
// match the frame it is delivering to the release call that produced it.
//
// The previous version stored a single global timestamp that every release overwrote, and
// the callback compared against whatever happened to be there. That made the reported value
// a blend of callback cadence, queueing and surface latency - it could not be read as "how
// long this frame took to reach the surface", and an 11 ms average was therefore NOT
// evidence of a fixed 90 Hz surface limit.
static RELEASE_TS: Mutex<VecDeque<(i64, u64)>> = Mutex::new(VecDeque::new());

// TEMPORARY DIAGNOSTIC: (pts, submit_time_us) pairs, used to measure pure MediaCodec time.
static IN_TIME: Mutex<VecDeque<(i64, u64)>> = Mutex::new(VecDeque::new());

static PTS_SEQ: AtomicU64 = AtomicU64::new(0);
static PTS_MATCH: AtomicU64 = AtomicU64::new(0);
static PTS_MISS: AtomicU64 = AtomicU64::new(0);
static PTS_NEAREST_NS: AtomicU64 = AtomicU64::new(0);

unsafe impl Send for VideoDecoderSink {}

impl VideoDecoderSink {
    // Block until the buffer has been written or timeout is reached. Returns false if timeout.
    pub fn push_frame_nal(&mut self, timestamp: Duration, data: &[u8]) -> Result<bool> {
        let Some(decoder) = &*self.inner.lock() else {
            // This might happen only during destruction
            return Ok(false);
        };

        // TEMPORARY DIAGNOSTIC (input side). Decoder saturation warnings come from this
        // function returning false, i.e. dequeue_input_buffer timing out. Measuring how
        // long the wait actually takes, how often it fails, and how far apart the NALs
        // arrive separates three very different causes:
        //   wait_avg high, fail low  -> input buffers are scarce but the wait rescues it
        //   fail high                -> the decoder really cannot accept data in time
        //   nal gap irregular        -> packets are arriving late, the decoder is innocent
        {
            static N: AtomicU64 = AtomicU64::new(0);
            static FAIL: AtomicU64 = AtomicU64::new(0);
            static SUM_WAIT_US: AtomicU64 = AtomicU64::new(0);
            static MAX_WAIT_US: AtomicU64 = AtomicU64::new(0);
            static SUM_GAP_US: AtomicU64 = AtomicU64::new(0);
            static MAX_GAP_US: AtomicU64 = AtomicU64::new(0);
            static PREV_TS_NS: AtomicU64 = AtomicU64::new(0);
            static HG: [AtomicU64; 5] = [
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
            ];
            static HW: [AtomicU64; 4] = [
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
            ];
            static SEEN_EPOCH_0: AtomicU64 = AtomicU64::new(0);
            if alvr_common::new_probe_session(&SEEN_EPOCH_0) {
                N.store(0, Ordering::Relaxed);
                FAIL.store(0, Ordering::Relaxed);
                SUM_WAIT_US.store(0, Ordering::Relaxed);
                MAX_WAIT_US.store(0, Ordering::Relaxed);
                SUM_GAP_US.store(0, Ordering::Relaxed);
                MAX_GAP_US.store(0, Ordering::Relaxed);
                PREV_TS_NS.store(0, Ordering::Relaxed);
                for slot in HG.iter() {
                    slot.store(0, Ordering::Relaxed);
                }
                for slot in HW.iter() {
                    slot.store(0, Ordering::Relaxed);
                }
            }

            let t0 = {
                use std::time::{SystemTime, UNIX_EPOCH};
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_micros() as u64)
                    .unwrap_or(0)
            };

            let ts_ns = timestamp.as_nanos() as u64;
            let prev = PREV_TS_NS.swap(ts_ns, Ordering::Relaxed);
            if prev != 0 && ts_ns > prev {
                let gap = ts_ns - prev;
                SUM_GAP_US.fetch_add(gap / 1000, Ordering::Relaxed);
                MAX_GAP_US.fetch_max(gap / 1000, Ordering::Relaxed);
                // bucket by frame period (11.111 ms)
                let gb = match gap / 1000 {
                    0..=11999 => 0,
                    12000..=22999 => 1,
                    23000..=44999 => 2,
                    45000..=99999 => 3,
                    _ => 4,
                };
                HG[gb].fetch_add(1, Ordering::Relaxed);
            }

            // 20 ms timeout, matching Virtual Desktop (DequeueInputBuffer(20000L) in
            // VideoPlayer.cs:2093). This was 3 ms.
            //
            // Measured with perfetto on the same headset, the difference is not subtle:
            //
            //   VD   : 20 ms timeout, retries until a slot is free, only gives up after 50
            //          consecutive failures. Input interval 9-13 ms for 98% of frames, codec
            //          input_queue_depth 3-4, media.decode_latency_us avg 5-8 ms.
            //   ours : 3 ms timeout, drops the frame immediately. Input interval 9-13 ms for
            //          only 72% of frames (7% under 5 ms, 7% over 20 ms), input_queue_depth
            //          mostly 0-1, media.decode_latency_us avg 29-37 ms.
            //
            // Dropping after 3 ms does not make the decoder faster; it just converts a full
            // decoder into corrupt pictures, an IDR request and a bigger keyframe, which is
            // harder to decode again. Waiting is what lets the pipeline stay full.
            let r = decoder.dequeue_input_buffer(Duration::from_millis(20));
            let wait_us = {
                use std::time::{SystemTime, UNIX_EPOCH};
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_micros() as u64)
                    .unwrap_or(0)
                    .saturating_sub(t0)
            };
            SUM_WAIT_US.fetch_add(wait_us, Ordering::Relaxed);
            MAX_WAIT_US.fetch_max(wait_us, Ordering::Relaxed);
            let wb = match wait_us {
                0..=99 => 0,
                100..=999 => 1,
                1000..=2999 => 2,
                _ => 3,
            };
            HW[wb].fetch_add(1, Ordering::Relaxed);
            let is_fail = !matches!(r, Ok(DequeuedInputBufferResult::Buffer(_)));
            if is_fail {
                FAIL.fetch_add(1, Ordering::Relaxed);
            }
            let n = N.fetch_add(1, Ordering::Relaxed) + 1;
            if crate::diagnostics_enabled() && n % 360 == 0 {
                let c = n as f64;
                alvr_common::warn!(
                    "DECINPUT n={} fail={} fail_ratio={:.3}% | wait_avg={:.3}ms wait_max={:.2}ms | nal_gap_avg={:.2}ms nal_gap_max={:.2}ms | wait_hist(<0.1,<1,<3,3+ms)={}/{}/{}/{} | gap_hist(<12,12-23,23-45,45-100,100+ms)={}/{}/{}/{}/{}",
                    n,
                    FAIL.load(Ordering::Relaxed),
                    FAIL.load(Ordering::Relaxed) as f64 * 100.0 / c,
                    SUM_WAIT_US.load(Ordering::Relaxed) as f64 / c / 1000.0,
                    MAX_WAIT_US.load(Ordering::Relaxed) as f64 / 1000.0,
                    SUM_GAP_US.load(Ordering::Relaxed) as f64 / c / 1000.0,
                    MAX_GAP_US.load(Ordering::Relaxed) as f64 / 1000.0,
                    HW[0].load(Ordering::Relaxed),
                    HW[1].load(Ordering::Relaxed),
                    HW[2].load(Ordering::Relaxed),
                    HW[3].load(Ordering::Relaxed),
                    HG[0].load(Ordering::Relaxed),
                    HG[1].load(Ordering::Relaxed),
                    HG[2].load(Ordering::Relaxed),
                    HG[3].load(Ordering::Relaxed),
                    HG[4].load(Ordering::Relaxed),
                );
            }

            match r {
            Ok(DequeuedInputBufferResult::Buffer(mut buffer)) => {
                unsafe {
                    ptr::copy_nonoverlapping(
                        data.as_ptr(),
                        buffer.buffer_mut().as_mut_ptr().cast(),
                        data.len(),
                    )
                };

                // NB: the function expects the timestamp in micros, but nanos is used to have
                // complete precision, so when converted back to Duration it can compare correctly
                // to other Durations
                let pts_in_ns = timestamp.as_nanos() as i64;

                // TEMPORARY DIAGNOSTIC (nail the last layer).
                //
                // The chain is proven up to here: CCodecBufferChannel::queue -> onWorkDone
                // equals media.decode_latency_us, output_queue_depth is flat across all
                // bitrates, and at 500 Mbps the INPUT queue is the SHORTEST (avg 1.23,
                // 58% of samples at depth <=1) while latency is the highest. So the cost is
                // inside a single Codec2 work, not in any queue we control.
                //
                // What is still unknown: how the HEVC access unit is actually shaped. A
                // 3648x1824 frame at 500 Mbps may arrive as one huge NAL or many; one work
                // may also span several NALs sharing a timestamp. Recording per-timestamp
                // NAL count and total bytes next to the latency answers that.
                {
                    use std::sync::atomic::{AtomicU64, Ordering};
                    static LAST_TS: AtomicU64 = AtomicU64::new(0);
                    static CUR_BYTES: AtomicU64 = AtomicU64::new(0);
                    static CUR_NALS: AtomicU64 = AtomicU64::new(0);
                    static CUR_SLICES: AtomicU64 = AtomicU64::new(0);
                    static FN: AtomicU64 = AtomicU64::new(0);
                    static SUM_BYTES: AtomicU64 = AtomicU64::new(0);
                    static MAX_BYTES: AtomicU64 = AtomicU64::new(0);
                    static SUM_NALS: AtomicU64 = AtomicU64::new(0);
                    static MAX_NALS: AtomicU64 = AtomicU64::new(0);
                    static SUM_SLICES: AtomicU64 = AtomicU64::new(0);
                    static SEEN_EPOCH_1: AtomicU64 = AtomicU64::new(0);
                    if alvr_common::new_probe_session(&SEEN_EPOCH_1) {
                        LAST_TS.store(0, Ordering::Relaxed);
                        CUR_BYTES.store(0, Ordering::Relaxed);
                        CUR_NALS.store(0, Ordering::Relaxed);
                        CUR_SLICES.store(0, Ordering::Relaxed);
                        FN.store(0, Ordering::Relaxed);
                        SUM_BYTES.store(0, Ordering::Relaxed);
                        MAX_BYTES.store(0, Ordering::Relaxed);
                        SUM_NALS.store(0, Ordering::Relaxed);
                        MAX_NALS.store(0, Ordering::Relaxed);
                        SUM_SLICES.store(0, Ordering::Relaxed);
                    }

                    // NAL unit type: HEVC header is 2 bytes, type in bits 1..6 of byte 0.
                    let nal_type = data.first().map(|b| (b >> 1) & 0x3F).unwrap_or(0xFF);
                    let is_slice = (0..=9).contains(&nal_type) || (16..=21).contains(&nal_type);

                    let ts_u = pts_in_ns as u64;
                    let prev = LAST_TS.load(Ordering::Relaxed);
                    if prev != ts_u {
                        // New access unit: publish the stats of the one that just closed.
                        if prev != 0 {
                            let b = CUR_BYTES.swap(0, Ordering::Relaxed);
                            let n = CUR_NALS.swap(0, Ordering::Relaxed);
                            let s = CUR_SLICES.swap(0, Ordering::Relaxed);
                            SUM_BYTES.fetch_add(b, Ordering::Relaxed);
                            MAX_BYTES.fetch_max(b, Ordering::Relaxed);
                            SUM_NALS.fetch_add(n, Ordering::Relaxed);
                            MAX_NALS.fetch_max(n, Ordering::Relaxed);
                            SUM_SLICES.fetch_add(s, Ordering::Relaxed);
                            let f = FN.fetch_add(1, Ordering::Relaxed) + 1;
                            if crate::diagnostics_enabled() && f % 360 == 0 {
                                let c = f as f64;
                                alvr_common::warn!(
                                    "NALSHAPE frames={} bytes_avg={:.0} bytes_max={} nals_avg={:.2} nals_max={} slices_avg={:.2}",
                                    f,
                                    SUM_BYTES.load(Ordering::Relaxed) as f64 / c,
                                    MAX_BYTES.load(Ordering::Relaxed),
                                    SUM_NALS.load(Ordering::Relaxed) as f64 / c,
                                    MAX_NALS.load(Ordering::Relaxed),
                                    SUM_SLICES.load(Ordering::Relaxed) as f64 / c,
                                );
                            }
                        }
                        LAST_TS.store(ts_u, Ordering::Relaxed);
                    }
                    CUR_BYTES.fetch_add(data.len() as u64, Ordering::Relaxed);
                    CUR_NALS.fetch_add(1, Ordering::Relaxed);
                    if is_slice {
                        CUR_SLICES.fetch_add(1, Ordering::Relaxed);
                    }
                }

                decoder.queue_input_buffer(buffer, 0, data.len(), pts_in_ns as u64, 0)?;

                // Final client boundary: the frame is now actually inside the codec.
                // Everything between the callback entry and here - the inner lock, the
                // dequeue_input_buffer wait (20 ms timeout), the copy - lands in this gap.
                if crate::diagnostics_enabled() {
                    alvr_common::record_queued(pts_in_ns as u64);
                }

                // TEMPORARY DIAGNOSTIC (pure MediaCodec time).
                //
                // Before rewriting the decode pipeline we must know whether the cost is in
                // MediaCodec itself or in the plumbing around it (ImageReader, queues). This
                // records the wall-clock instant each PTS was submitted; the output side
                // matches on PTS and reports the difference. That difference contains no
                // ImageReader, no image_queue and no polling - it is the codec alone.
                // Guarded: this takes a mutex and reads the system clock once per frame.
                // With diagnostics off the whole block costs one atomic load.
                if crate::diagnostics_enabled() {
                    use std::time::{SystemTime, UNIX_EPOCH};
                    let now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_micros() as u64)
                        .unwrap_or(0);
                    let mut m = IN_TIME.lock();
                    m.push_back((pts_in_ns, now));
                    while m.len() > 128 {
                        m.pop_front();
                    }
                }

                {
                    let mut q = PTS_IN.lock();
                    q.push_back(pts_in_ns);
                    while q.len() > 256 {
                        q.pop_front();
                    }
                    PTS_SEQ.fetch_add(1, Ordering::Relaxed);
                }

                Ok(true)
            }
            Ok(DequeuedInputBufferResult::TryAgainLater) => Ok(false),
            Err(e) => bail!("{e}"),
            }
        }
    }
}

struct QueuedImage {
    timestamp: Duration,
    image: Image,
    in_use: bool,
}
unsafe impl Send for QueuedImage {}

// Frame accounting for the buffering policy. BUFDROP is a decoded frame thrown away to
// cap latency (the image jumps), REPEATGAP's empty is the renderer finding nothing (the
// image is held). They are the two ways a frame can go missing and they feel different.
static BUF_DROPS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static BUF_TAKES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

// Access the image queue synchronously.
pub struct VideoDecoderSource {
    running: Arc<RelaxedAtomic>,
    dequeue_thread: Option<JoinHandle<()>>,
    image_queue: Arc<Mutex<VecDeque<QueuedImage>>>,
    config: VideoDecoderConfig,
    buffering_running_average: f32,
}

unsafe impl Send for VideoDecoderSource {}

impl VideoDecoderSource {
    // The application MUST finish using the returned buffer before calling this function again
    pub fn dequeue_frame(&mut self) -> Option<(Duration, *mut c_void)> {
        let mut image_queue_lock = self.image_queue.lock();

        if let Some(queued_image) = image_queue_lock.front()
            && queued_image.in_use
        {
            // image is released and ready to be reused by the decoder
            image_queue_lock.pop_front();
        }

        // use running average to give more weight to recent samples
        self.buffering_running_average = self.buffering_running_average
            * self.config.buffering_history_weight
            + image_queue_lock.len() as f32 * (1. - self.config.buffering_history_weight);
        if self.buffering_running_average > self.config.max_buffering_frames as f32 {
            let dropped_image = image_queue_lock.pop_front();
            // Count the drops this policy causes. "empty" (nothing decoded in time, image
            // held) and this (a decoded frame discarded to cap latency, image jumps) look
            // completely different to the user, and until now only the first was measured.
            // Knowing both rates is what decides whether running the server slightly fast
            // is a win: that trades this counter up to drive the empty counter down.
            let d = BUF_DROPS.fetch_add(1, Ordering::Relaxed) + 1;
            let t = BUF_TAKES.load(Ordering::Relaxed);
            // 逐事件保留被丢帧的PTS；旧版每90次才打印会遗漏低频跳帧。
            if crate::diagnostics_enabled() {
                alvr_common::warn!(
                    "BUFDROP_EVENT drops={} pts={} actual={} depth={} mean={:.3} probe=stall_v1",
                    d, dropped_image.as_ref().map(|f| f.timestamp.as_nanos()).unwrap_or(0),
                    dropped_image.is_some(), image_queue_lock.len(), self.buffering_running_average,
                );
            }
            if crate::diagnostics_enabled() && d % 90 == 0 {
                alvr_common::warn!(
                    "BUFDROP drops={} takes={} drop_ratio={:.2}%",
                    d,
                    t,
                    d as f64 * 100.0 / (d + t).max(1) as f64
                );
            }
        }

        {
            // Queue depth at the moment the render thread takes a frame. Zero means the
            // decoder had not produced anything yet (the "empty" case); a deep queue means
            // we are behind. This is the direct counterpart of REPEATGAP's empty counter.
            static DN: AtomicU64 = AtomicU64::new(0);
            static SUM_DEPTH: AtomicU64 = AtomicU64::new(0);
            static MAX_DEPTH: AtomicU64 = AtomicU64::new(0);
            static HD: [AtomicU64; 5] = [
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
            ];
            static SEEN_EPOCH_2: AtomicU64 = AtomicU64::new(0);
            if alvr_common::new_probe_session(&SEEN_EPOCH_2) {
                DN.store(0, Ordering::Relaxed);
                SUM_DEPTH.store(0, Ordering::Relaxed);
                MAX_DEPTH.store(0, Ordering::Relaxed);
                for slot in HD.iter() {
                    slot.store(0, Ordering::Relaxed);
                }
            }
            let d = image_queue_lock.len() as u64;
            SUM_DEPTH.fetch_add(d, Ordering::Relaxed);
            MAX_DEPTH.fetch_max(d, Ordering::Relaxed);
            let hb = match d {
                0 => 0,
                1 => 1,
                2 => 2,
                3..=4 => 3,
                _ => 4,
            };
            HD[hb].fetch_add(1, Ordering::Relaxed);
            let dn = DN.fetch_add(1, Ordering::Relaxed) + 1;
            if crate::diagnostics_enabled() && dn % 180 == 0 {
                alvr_common::warn!(
                    "QDEPTH n={} avg={:.2} max={} | hist(0,1,2,3-4,5+)={}/{}/{}/{}/{} drops={} takes={} probe=stall_v1",
                    dn,
                    SUM_DEPTH.load(Ordering::Relaxed) as f64 / dn as f64,
                    MAX_DEPTH.load(Ordering::Relaxed),
                    HD[0].load(Ordering::Relaxed),
                    HD[1].load(Ordering::Relaxed),
                    HD[2].load(Ordering::Relaxed),
                    HD[3].load(Ordering::Relaxed),
                    HD[4].load(Ordering::Relaxed),
                    BUF_DROPS.load(Ordering::Relaxed),
                    BUF_TAKES.load(Ordering::Relaxed),
                );
            }
        }

        if let Some(queued_image) = image_queue_lock.front_mut() {
            queued_image.in_use = true;

            // Counterpart of BUFDROP: one take per successful frame handed to the renderer.
            BUF_TAKES.fetch_add(1, Ordering::Relaxed);

            Some((
                queued_image.timestamp,
                queued_image
                    .image
                    .hardware_buffer()
                    .unwrap()
                    .as_ptr()
                    .cast(),
            ))
        } else {
            // TODO: add back when implementing proper phase sync
            //warn!("Video frame queue underflow!");
            None
        }
    }
}

impl Drop for VideoDecoderSource {
    fn drop(&mut self) {
        self.running.set(false);

        // Destruction of decoder, buffered images and ImageReader
        self.dequeue_thread.take().map(|t| t.join());
    }
}

fn mime_for_codec(codec: CodecType) -> &'static str {
    match codec {
        CodecType::H264 => "video/avc",
        CodecType::Hevc => "video/hevc",
        CodecType::AV1 => "video/av01",
    }
}

// Attempts to create a MediaCodec, and then configure and start it.
fn decoder_setup(
    codec_type: CodecType,
    is_software: bool,
    format: &MediaFormat,
    image_reader: &ImageReader,
) -> Result<MediaCodec> {
    let decoder = if is_software {
        let sw_codec_name = match codec_type {
            CodecType::H264 => "OMX.google.h264.decoder",
            CodecType::Hevc => "OMX.google.hevc.decoder",
            CodecType::AV1 => bail!("AV1 is not supported for software decoding"),
        };
        MediaCodec::from_codec_name(&sw_codec_name)
            .ok_or(anyhow!("no such codec: {}", &sw_codec_name))?
    } else {
        let mime = mime_for_codec(codec_type);

        // Use the dedicated low-latency decoder when the platform has one.
        //
        // This is the single biggest difference found between this build and Virtual
        // Desktop, measured with perfetto on the same headset:
        //
        //   VD : c2.qti.hevc.decoder.low_latency, two instances (one per eye),
        //        media.decode_latency_us avg 5.4 / 7.8 ms, input_queue_depth 3-4
        //   ours: c2.qti.hevc.decoder,             one instance (both eyes packed),
        //        media.decode_latency_us avg 29.3 ms, input_queue_depth 1
        //
        // Same silicon, ~4x difference. from_decoder_type() returns the generic
        // c2.qti.hevc.decoder; the .low_latency variant is a separate codec instance that
        // VD asks for by name.
        //
        // media_codecs.xml advertises a 70 Mbps ceiling for that variant, but VD runs it at
        // ~960 Mbps without issue, so the advertised limit is not enforced in practice.
        let low_latency_name = match codec_type {
            CodecType::H264 => Some("c2.qti.avc.decoder.low_latency"),
            CodecType::Hevc => Some("c2.qti.hevc.decoder.low_latency"),
            _ => None,
        };

        let mut chosen = None;
        if let Some(name) = low_latency_name {
            match MediaCodec::from_codec_name(name) {
                Some(codec) => {
                    alvr_common::warn!("DECODERCHOSEN mime={} name={} (low_latency)", mime, name);
                    chosen = Some(codec);
                }
                None => {
                    alvr_common::warn!(
                        "DECODERCHOSEN mime={} low_latency codec {} unavailable, falling back",
                        mime,
                        name
                    );
                }
            }
        }

        let chosen = match chosen {
            Some(c) => c,
            None => {
                let c = MediaCodec::from_decoder_type(&mime)
                    .ok_or(anyhow!("unable to find decoder for mime type: {}", &mime))?;
                match c.name() {
                    Ok(n) => alvr_common::warn!("DECODERCHOSEN mime={} name={}", mime, n),
                    Err(e) => {
                        alvr_common::warn!("DECODERCHOSEN mime={} name=<error {:?}>", mime, e)
                    }
                }
                c
            }
        };
        chosen
    };
    decoder
        .configure(
            &format,
            Some(&image_reader.window()?),
            MediaCodecDirection::Decoder,
        )
        .with_context(|| format!("failed to configure decoder"))?;

    decoder
        .start()
        .with_context(|| format!("failed to start decoder"))?;

    Ok(decoder)
}

// Since we leak the ImageReader, and we pass frame_result_callback to it which contains a reference
// to ClientCoreContext, to avoid circular references we need to use a Weak reference.
fn decoder_lifecycle(
    config: VideoDecoderConfig,
    csd_0: Vec<u8>,
    frame_result_callback: Weak<impl Fn(Result<Duration>) + Send + Sync + 'static>,
    running: Arc<RelaxedAtomic>,
    decoder_sink: Arc<Mutex<Option<SharedMediaCodec>>>,
    decoder_ready_notifier: Arc<Condvar>,
    image_queue: Arc<Mutex<VecDeque<QueuedImage>>>,
    image_reader: &mut ImageReader,
) -> Result<()> {
    // 2x: keep the target buffering in the middle of the max amount of queuable frames
    let available_buffering_frames = (2. * config.max_buffering_frames).ceil() as usize;

    image_reader.set_image_listener(Box::new({
        let image_queue = Arc::clone(&image_queue);
        move |image_reader| {
            let mut image_queue_lock = image_queue.lock();

            if image_queue_lock.len() > available_buffering_frames {
                warn!("Video frame queue overflow!");
                image_queue_lock.pop_front();
            }

            // TEMPORARY DIAGNOSTIC (does the ImageReader ever block?).
            //
            // QDEPTH (the application-side image_queue) never exceeds 5, and "queue
            // overflow" fired only twice, so the visible queue is healthy. But that queue
            // is drained immediately after acquire_next_image() returns - it says nothing
            // about the ImageReader's own slots (MAX_BUFFERING_FRAMES = 10). If the decoder
            // outruns this callback, the slots fill, the decoder stalls, and frames then
            // arrive in a burst. That is exactly a "queue blocked up, then jumped" shape
            // and it is invisible to QDEPTH.
            //
            //   wait   - how long acquire_next_image() took (a blocking call when slots are
            //            exhausted; normally it returns immediately)
            //   qlen   - the application queue length at callback entry, i.e. how far
            //            behind the consumer is when a new frame arrives
            //
            // A large wait with a small qlen means the ImageReader slots are the constraint,
            // not our queue.
            let t_acq0 = {
                use std::time::{SystemTime, UNIX_EPOCH};
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_micros() as u64)
                    .unwrap_or(0)
            };
            let qlen_at_entry = image_queue_lock.len() as u64;

            let acq_res = image_reader.acquire_next_image();

            {
                use std::sync::atomic::{AtomicU64, Ordering};
                static AN: AtomicU64 = AtomicU64::new(0);
                static SUM_WAIT: AtomicU64 = AtomicU64::new(0);
                static MAX_WAIT: AtomicU64 = AtomicU64::new(0);
                static BLOCKED: AtomicU64 = AtomicU64::new(0);
                static SUM_QLEN: AtomicU64 = AtomicU64::new(0);
                static MAX_QLEN: AtomicU64 = AtomicU64::new(0);
                static HA: [AtomicU64; 6] = [
                    AtomicU64::new(0),
                    AtomicU64::new(0),
                    AtomicU64::new(0),
                    AtomicU64::new(0),
                    AtomicU64::new(0),
                    AtomicU64::new(0),
                ];
                static HQ: [AtomicU64; 5] = [
                    AtomicU64::new(0),
                    AtomicU64::new(0),
                    AtomicU64::new(0),
                    AtomicU64::new(0),
                    AtomicU64::new(0),
                ];
                static SEEN_EPOCH_3: AtomicU64 = AtomicU64::new(0);
                if alvr_common::new_probe_session(&SEEN_EPOCH_3) {
                    AN.store(0, Ordering::Relaxed);
                    SUM_WAIT.store(0, Ordering::Relaxed);
                    MAX_WAIT.store(0, Ordering::Relaxed);
                    BLOCKED.store(0, Ordering::Relaxed);
                    SUM_QLEN.store(0, Ordering::Relaxed);
                    MAX_QLEN.store(0, Ordering::Relaxed);
                    for slot in HA.iter() {
                        slot.store(0, Ordering::Relaxed);
                    }
                    for slot in HQ.iter() {
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
                let wait_us = now.saturating_sub(t_acq0);

                let n = AN.fetch_add(1, Ordering::Relaxed) + 1;
                SUM_WAIT.fetch_add(wait_us, Ordering::Relaxed);
                MAX_WAIT.fetch_max(wait_us, Ordering::Relaxed);
                if wait_us > 2000 {
                    BLOCKED.fetch_add(1, Ordering::Relaxed);
                }
                SUM_QLEN.fetch_add(qlen_at_entry, Ordering::Relaxed);
                MAX_QLEN.fetch_max(qlen_at_entry, Ordering::Relaxed);

                let hb = match wait_us {
                    0..=99 => 0,
                    100..=499 => 1,
                    500..=1999 => 2,
                    2000..=4999 => 3,
                    5000..=9999 => 4,
                    _ => 5,
                };
                HA[hb].fetch_add(1, Ordering::Relaxed);
                let qb = match qlen_at_entry {
                    0 => 0,
                    1 => 1,
                    2 => 2,
                    3..=4 => 3,
                    _ => 4,
                };
                HQ[qb].fetch_add(1, Ordering::Relaxed);

                if crate::diagnostics_enabled() && n % 360 == 0 {
                    let c = n as f64;
                    alvr_common::warn!(
                        "ACQUIRE n={} wait_avg={:.3}ms wait_max={:.2}ms | blocked>2ms={} ({:.2}%) | qlen_avg={:.2} qlen_max={} | wait_hist(<0.1,<0.5,<2,<5,<10,10+ms)={}/{}/{}/{}/{}/{} | qlen_hist(0,1,2,3-4,5+)={}/{}/{}/{}/{}",
                        n,
                        SUM_WAIT.load(Ordering::Relaxed) as f64 / c / 1000.0,
                        MAX_WAIT.load(Ordering::Relaxed) as f64 / 1000.0,
                        BLOCKED.load(Ordering::Relaxed),
                        BLOCKED.load(Ordering::Relaxed) as f64 * 100.0 / c,
                        SUM_QLEN.load(Ordering::Relaxed) as f64 / c,
                        MAX_QLEN.load(Ordering::Relaxed),
                        HA[0].load(Ordering::Relaxed),
                        HA[1].load(Ordering::Relaxed),
                        HA[2].load(Ordering::Relaxed),
                        HA[3].load(Ordering::Relaxed),
                        HA[4].load(Ordering::Relaxed),
                        HA[5].load(Ordering::Relaxed),
                        HQ[0].load(Ordering::Relaxed),
                        HQ[1].load(Ordering::Relaxed),
                        HQ[2].load(Ordering::Relaxed),
                        HQ[3].load(Ordering::Relaxed),
                        HQ[4].load(Ordering::Relaxed),
                    );
                }
            }

            match acq_res {
                Ok(AcquireResult::Image(image)) => {
                    let out_ns = image.timestamp().unwrap() as i64;

                    // Codec-to-surface latency for THIS frame: match the release call by PTS.
                    // An earlier version compared against a single global "last release"
                    // timestamp, which mixed callback cadence, queueing and surface latency
                    // into one number and made an 11 ms average look like a 90 Hz surface cap.
                    // It was not; this pairs the exact frame.
                    {
                        static LN: AtomicU64 = AtomicU64::new(0);
                        static SUM_L: AtomicU64 = AtomicU64::new(0);
                        static MAX_L: AtomicU64 = AtomicU64::new(0);
                        static UNMATCHED: AtomicU64 = AtomicU64::new(0);
                        static HL: [AtomicU64; 6] = [
                            AtomicU64::new(0),
                            AtomicU64::new(0),
                            AtomicU64::new(0),
                            AtomicU64::new(0),
                            AtomicU64::new(0),
                            AtomicU64::new(0),
                        ];
                        static SEEN_EPOCH_4: AtomicU64 = AtomicU64::new(0);
                        if alvr_common::new_probe_session(&SEEN_EPOCH_4) {
                            LN.store(0, Ordering::Relaxed);
                            SUM_L.store(0, Ordering::Relaxed);
                            MAX_L.store(0, Ordering::Relaxed);
                            UNMATCHED.store(0, Ordering::Relaxed);
                            for slot in HL.iter() {
                                slot.store(0, Ordering::Relaxed);
                            }
                        }
                        let now_us = {
                            use std::time::{SystemTime, UNIX_EPOCH};
                            SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .map(|d| d.as_micros() as u64)
                                .unwrap_or(0)
                        };
                        // Skip the lock entirely when the writer side is off: the queue can
                        // only be empty then, and this runs once per decoded frame.
                        let rel_ts = if crate::diagnostics_enabled() {
                            let mut m = RELEASE_TS.lock();
                            match m.iter().position(|(p, _)| *p == out_ns) {
                                Some(idx) => m.remove(idx).map(|(_, t)| t),
                                None => None,
                            }
                        } else {
                            None
                        };
                        match rel_ts.filter(|t| now_us > *t) {
                            Some(rel_ts) => {
                                let lat = now_us - rel_ts;
                                let ln = LN.fetch_add(1, Ordering::Relaxed) + 1;
                                SUM_L.fetch_add(lat, Ordering::Relaxed);
                                MAX_L.fetch_max(lat, Ordering::Relaxed);
                                let hb = match lat {
                                    0..=499 => 0,
                                    500..=1999 => 1,
                                    2000..=4999 => 2,
                                    5000..=9999 => 3,
                                    10000..=19999 => 4,
                                    _ => 5,
                                };
                                HL[hb].fetch_add(1, Ordering::Relaxed);
                                if crate::diagnostics_enabled() && ln % 360 == 0 {
                                    let c = ln as f64;
                                    alvr_common::warn!(
                                        "SURFLAT n={} unmatched={} avg={:.3}ms max={:.2}ms | hist(<0.5,<2,<5,<10,<20,20+ms)={}/{}/{}/{}/{}/{}",
                                        ln,
                                        UNMATCHED.load(Ordering::Relaxed),
                                        SUM_L.load(Ordering::Relaxed) as f64 / c / 1000.0,
                                        MAX_L.load(Ordering::Relaxed) as f64 / 1000.0,
                                        HL[0].load(Ordering::Relaxed),
                                        HL[1].load(Ordering::Relaxed),
                                        HL[2].load(Ordering::Relaxed),
                                        HL[3].load(Ordering::Relaxed),
                                        HL[4].load(Ordering::Relaxed),
                                        HL[5].load(Ordering::Relaxed),
                                    );
                                }
                            }
                            None => {
                                UNMATCHED.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }

                    // TEMPORARY DIAGNOSTIC (chain audit point 4): does the PTS survive
                    // MediaCodec -> ImageReader unchanged?
                    //   exact   - the PTS we queued is one we did queue (good)
                    //   miss    - it is not; then nearest_ns records how far off it is,
                    //             which separates "shifted by a rounding amount" from
                    //             "replaced by something unrelated"
                    {
                        let (exact, nearest_ns) = if crate::diagnostics_enabled() {
                            let q = PTS_IN.lock();
                            if q.contains(&out_ns) {
                                (true, 0u64)
                            } else {
                                let nearest = q
                                    .iter()
                                    .map(|&k| (k - out_ns).unsigned_abs())
                                    .min()
                                    .unwrap_or(u64::MAX);
                                (false, nearest)
                            }
                        } else {
                            (true, 0u64)
                        };
                        if exact {
                            PTS_MATCH.fetch_add(1, Ordering::Relaxed);
                        } else {
                            PTS_MISS.fetch_add(1, Ordering::Relaxed);
                            if nearest_ns != u64::MAX {
                                PTS_NEAREST_NS.fetch_max(nearest_ns, Ordering::Relaxed);
                            }
                        }
                        // With n % 90 the first few lines appear early enough to show
                        // whether the values ever lined up (a late-joining mismatch and a
                        // never-matching one look identical at low resolution).
                        static PRINTED: AtomicU64 = AtomicU64::new(0);
                        static SEEN_EPOCH_5: AtomicU64 = AtomicU64::new(0);
                        if alvr_common::new_probe_session(&SEEN_EPOCH_5) {
                            PRINTED.store(0, Ordering::Relaxed);
                        }
                        let p = PRINTED.fetch_add(1, Ordering::Relaxed) + 1;
                        if crate::diagnostics_enabled() && (p <= 6 || p % 180 == 0) {
                            let (smin, smax, slen) = {
                                let q = PTS_IN.lock();
                                (
                                    q.front().copied().unwrap_or(0),
                                    q.back().copied().unwrap_or(0),
                                    q.len(),
                                )
                            };
                            alvr_common::warn!(
                                "PTSVAL p={} out={} oldest={} newest={} len={} exact={} delta_out_oldest={:.3}ms delta_out_newest={:.3}ms",
                                p,
                                out_ns,
                                smin,
                                smax,
                                slen,
                                exact,
                                (out_ns - smin) as f64 / 1e6,
                                (out_ns - smax) as f64 / 1e6
                            );
                        }
                    }

                    let timestamp = Duration::from_nanos(out_ns as u64);

                    if let Some(callback) = frame_result_callback.upgrade() {
                        callback(Ok(timestamp));
                    }

                    image_queue_lock.push_back(QueuedImage {
                        timestamp,
                        image,
                        in_use: false,
                    });
                }
                Ok(e) => {
                    error!("ImageReader error: {e:?}");

                    image_queue_lock.pop_front();
                }
                Err(e) => {
                    error!("ImageReader error: {e}");

                    image_queue_lock.clear();
                }
            }
        }
    }))?;

    // Documentation says that this call is necessary to properly dispose acquired buffers.
    // todo: find out how to use it and avoid leaking the ImageReader
    image_reader.set_buffer_removed_listener(Box::new(|_, _| ()))?;

    let mime = mime_for_codec(config.codec);

    let mut format = MediaFormat::new();
    format.set_str("mime", mime);
    // Declare the real frame size so the codec allocates its buffers once.
    //
    // These were placeholders (512x1024). Supplying the real size measurably
    // reduces decode latency: otherwise the platform rebuilds its internal
    // buffers when the first full-size frame arrives. Measured on device,
    // decode latency went from 20-25 ms to 14 ms and total latency from 89 ms
    // to 70 ms.
    let [width, height] = config.frame_size;
    if width > 0 && height > 0 {
        format.set_i32("width", width as i32);
        format.set_i32("height", height as i32);
    } else {
        format.set_i32("width", 512);
        format.set_i32("height", 1024);
    }
    format.set_buffer("csd-0", &csd_0);

    for (key, prop) in &config.options {
        let maybe_error = match prop.ty {
            MediacodecPropType::Float => prop
                .value
                .parse()
                .map(|value| format.set_f32(key, value))
                .to_any(),
            MediacodecPropType::Int32 => prop
                .value
                .parse()
                .map(|value| format.set_i32(key, value))
                .to_any(),
            MediacodecPropType::Int64 => prop
                .value
                .parse()
                .map(|value| format.set_i64(key, value))
                .to_any(),
            MediacodecPropType::String => Ok(format.set_str(key, &prop.value)),
        };

        if let Err(e) = maybe_error {
            error!("Failed to set property {key} to {}: {e}", prop.value);
        }
    }

    info!("Using AMediaCodec format:{} ", format);

    let decoder = if config.force_software_decoder {
        decoder_setup(config.codec, true, &format, &image_reader)?
    } else {
        // Hardware decoders sometimes fail at the CSD-0.
        // May as well fall back if this occurs.
        match decoder_setup(config.codec, false, &format, &image_reader) {
            Ok(d) => d,
            Err(e) => {
                // would be "warn!" but this is a severe caveat and a pretty major error.
                error!("Attempting software fallback due to error in default decoder: {e:#}");

                decoder_setup(config.codec, true, &format, &image_reader)?
            }
        }
    };

    let decoder = Arc::new(FakeThreadSafe(decoder));

    {
        let mut decoder_lock = decoder_sink.lock();

        *decoder_lock = Some(Arc::clone(&decoder));

        decoder_ready_notifier.notify_one();
    }

    // EXPERIMENT (are two decoders truly parallel, or do they share one hardware unit?).
    //
    // This decides whether a dual-stream pipeline can help at all. Measured on this
    // headset, decode time depends on the BITRATE a single decoder sees, not on pixel
    // count: 500 Mbps -> 33.9 ms, 250 Mbps -> 11.95 ms, and halving the resolution at a
    // fixed bitrate changed nothing (11.95 -> 11.96 ms).
    //
    // So the only way to raise bitrate without raising decode latency is to spread it over
    // more decoders. That works only if the extra decoders run CONCURRENTLY. If they queue
    // behind one hardware block, splitting buys nothing and the rewrite is pointless.
    //
    let mut error_counter = 0;
    // TEMPORARY DIAGNOSTIC (full decode-path breakdown).
    //
    // Virtual Desktop decodes 3072x3264 per eye in ~10 ms while this build needs 8-16 ms
    // for 1296x1072 - 8.7x fewer pixels, slower. That means the cost is not in the codec
    // itself but somewhere in how this path drives it. The four candidate costs are timed
    // separately so the guesswork stops:
    //
    //   poll   - dequeue_output_buffer() itself (1 ms timeout; how long we wait per frame)
    //   rel    - release_output_buffer_at_time() (does the surface timestamp make it wait?)
    //   spins  - TryAgainLater count per frame (is the decoder starved or are we late?)
    //   qdelay - time from NAL handed to the decoder until the frame is released back
    {
        static N: AtomicU64 = AtomicU64::new(0);
        static SUM_POLL_US: AtomicU64 = AtomicU64::new(0);
        static MAX_POLL_US: AtomicU64 = AtomicU64::new(0);
        static SUM_REL_US: AtomicU64 = AtomicU64::new(0);
        static MAX_REL_US: AtomicU64 = AtomicU64::new(0);
        static SPINS: AtomicU64 = AtomicU64::new(0);
        static H_POLL: [AtomicU64; 6] = [
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
        ];
        static H_REL: [AtomicU64; 6] = [
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
        ];
        static SEEN_EPOCH_6: AtomicU64 = AtomicU64::new(0);
        if alvr_common::new_probe_session(&SEEN_EPOCH_6) {
            N.store(0, Ordering::Relaxed);
            SUM_POLL_US.store(0, Ordering::Relaxed);
            MAX_POLL_US.store(0, Ordering::Relaxed);
            SUM_REL_US.store(0, Ordering::Relaxed);
            MAX_REL_US.store(0, Ordering::Relaxed);
            SPINS.store(0, Ordering::Relaxed);
            for slot in H_POLL.iter() {
                slot.store(0, Ordering::Relaxed);
            }
            for slot in H_REL.iter() {
                slot.store(0, Ordering::Relaxed);
            }
        }

        fn bucket(us: u64) -> usize {
            match us {
                0..=99 => 0,
                100..=999 => 1,
                1000..=4999 => 2,
                5000..=9999 => 3,
                10000..=19999 => 4,
                _ => 5,
            }
        }

        macro_rules! now_us {
            () => {{
                use std::time::{SystemTime, UNIX_EPOCH};
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_micros() as u64)
                    .unwrap_or(0)
            }};
        }

        // Per-frame accumulators. These MUST be reset after every successful dequeue,
        // otherwise they accumulate across frames and report absurd values (a previous
        // version of this measurement did exactly that and reported 21 seconds per frame).
        let mut spin_acc_us: u64 = 0;   // total time burned in TryAgainLater this frame
        let mut spin_cnt: u64 = 0;      // number of TryAgainLater this frame
        let mut t_frame0 = now_us!();   // start of this frame's wait

        while running.value() {
        let t_poll0 = now_us!();
        // 20 ms, matching Virtual Desktop's DequeueOutputBuffer(20000L) in VideoPlayer.cs.
        //
        // This was 1 ms. With a 1 ms timeout the call returns TryAgainLater almost every
        // time and the loop spins: DECWAIT reported spin_cnt 9.35 and spin_avg 10.43 ms per
        // frame, i.e. the thread burns its whole frame budget polling instead of blocking.
        // The decoder's output does not become available faster because we ask more often -
        // the same lesson as the input side, where raising the timeout from 3 ms to 20 ms
        // took fail_ratio from 4.1% to 0 and saturation from 164 to 7.
        let res = decoder.dequeue_output_buffer(Duration::from_millis(20));
        let poll_us = now_us!() - t_poll0;

        match res {
            Ok(DequeuedOutputBufferInfoResult::Buffer(buffer)) => {
                let poll_b = bucket(poll_us);
                H_POLL[poll_b].fetch_add(1, Ordering::Relaxed);
                SUM_POLL_US.fetch_add(poll_us, Ordering::Relaxed);
                MAX_POLL_US.fetch_max(poll_us, Ordering::Relaxed);

                let presentation_time_ns = buffer.info().presentation_time_us();

                // TEMPORARY DIAGNOSTIC: pure MediaCodec decode time for this PTS.
                // Compare against SURFLAT (release -> ImageReader callback):
                //   if PURE is ~46 ms too, the codec itself is slow and rewriting the
                //     pipeline around it would change nothing;
                //   if PURE is ~10 ms while SURFLAT is 46 ms, the extra 36 ms is plumbing
                //     and a pipeline rewrite is justified.
                {
                    use std::sync::atomic::{AtomicU64, Ordering};
                    static PN: AtomicU64 = AtomicU64::new(0);
                    static SUM_PURE: AtomicU64 = AtomicU64::new(0);
                    static MAX_PURE: AtomicU64 = AtomicU64::new(0);
                    static MIN_PURE: AtomicU64 = AtomicU64::new(u64::MAX);
                    static HP: [AtomicU64; 6] = [
                        AtomicU64::new(0),
                        AtomicU64::new(0),
                        AtomicU64::new(0),
                        AtomicU64::new(0),
                        AtomicU64::new(0),
                        AtomicU64::new(0),
                    ];
                    static SEEN_EPOCH_7: AtomicU64 = AtomicU64::new(0);
                    if alvr_common::new_probe_session(&SEEN_EPOCH_7) {
                        PN.store(0, Ordering::Relaxed);
                        SUM_PURE.store(0, Ordering::Relaxed);
                        MAX_PURE.store(0, Ordering::Relaxed);
                        MIN_PURE.store(u64::MAX, Ordering::Relaxed);
                        for slot in HP.iter() {
                            slot.store(0, Ordering::Relaxed);
                        }
                    }

                    let submit_us = if crate::diagnostics_enabled() {
                        let mut m = IN_TIME.lock();
                        match m.iter().position(|(p, _)| *p == presentation_time_ns) {
                            Some(idx) => m.remove(idx).map(|(_, t)| t),
                            None => None,
                        }
                    } else {
                        None
                    };

                    if let Some(sub) = submit_us {
                        let now = {
                            use std::time::{SystemTime, UNIX_EPOCH};
                            SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .map(|d| d.as_micros() as u64)
                                .unwrap_or(0)
                        };
                        let pure_us = now.saturating_sub(sub);
                        let pn = PN.fetch_add(1, Ordering::Relaxed) + 1;
                        SUM_PURE.fetch_add(pure_us, Ordering::Relaxed);
                        MAX_PURE.fetch_max(pure_us, Ordering::Relaxed);
                        MIN_PURE.fetch_min(pure_us, Ordering::Relaxed);
                        let hb = match pure_us {
                            0..=4999 => 0,
                            5000..=9999 => 1,
                            10000..=14999 => 2,
                            15000..=19999 => 3,
                            20000..=39999 => 4,
                            _ => 5,
                        };
                        HP[hb].fetch_add(1, Ordering::Relaxed);

                        // TEMPORARY DIAGNOSTIC (what makes a frame slow?).
                        // PUREDEC is bimodal: ~64% of frames decode in 10-15 ms but 22.6%
                        // take over 40 ms. Printing the PTS delta, the size of the NAL that
                        // produced this frame, and the flags for the slow ones shows whether
                        // they are large frames, keyframes, or arrive after a gap.
                        if pure_us > 40000 {
                            static SN: AtomicU64 = AtomicU64::new(0);
                            static SEEN_EPOCH_8: AtomicU64 = AtomicU64::new(0);
                            if alvr_common::new_probe_session(&SEEN_EPOCH_8) {
                                SN.store(0, Ordering::Relaxed);
                            }
                            let sn = SN.fetch_add(1, Ordering::Relaxed) + 1;
                            if crate::diagnostics_enabled() && sn <= 200 {
                                static PREV_PTS: AtomicU64 = AtomicU64::new(0);
                                static SEEN_EPOCH_9: AtomicU64 = AtomicU64::new(0);
                                if alvr_common::new_probe_session(&SEEN_EPOCH_9) {
                                    PREV_PTS.store(0, Ordering::Relaxed);
                                }
                                let prev = PREV_PTS.swap(presentation_time_ns as u64, Ordering::Relaxed);
                                let dpts_us = if prev == 0 {
                                    -1i64
                                } else {
                                    ((presentation_time_ns as u64).saturating_sub(prev) / 1000) as i64
                                };
                                alvr_common::warn!(
                                    "SLOWFRAME i={} pure_us={} dpts_us={}",
                                    sn,
                                    pure_us,
                                    dpts_us
                                );
                            }
                        }
                        if crate::diagnostics_enabled() && pn % 360 == 0 {
                            let c = pn as f64;
                            alvr_common::warn!(
                                "PUREDEC n={} avg={:.2}ms min={:.2}ms max={:.2}ms | hist(<5,<10,<15,<20,<40,40+ms)={}/{}/{}/{}/{}/{}",
                                pn,
                                SUM_PURE.load(Ordering::Relaxed) as f64 / c / 1000.0,
                                MIN_PURE.load(Ordering::Relaxed) as f64 / 1000.0,
                                MAX_PURE.load(Ordering::Relaxed) as f64 / 1000.0,
                                HP[0].load(Ordering::Relaxed),
                                HP[1].load(Ordering::Relaxed),
                                HP[2].load(Ordering::Relaxed),
                                HP[3].load(Ordering::Relaxed),
                                HP[4].load(Ordering::Relaxed),
                                HP[5].load(Ordering::Relaxed),
                            );
                        }
                    }
                }

                // TEMPORARY DIAGNOSTIC (what is this timestamp, and does it make the codec wait?).
                //
                // releaseOutputBufferAtTime takes a timestampNs that the NDK documents as the
                // SURFACE timestamp, i.e. a point on the system monotonic clock. ALVR instead
                // passes the video frame's media PTS, which comes from the server's
                // m_targetTimestampNs = poll_timestamp + prediction_offset. Those are different
                // quantities, so the codec may be waiting for a moment that is far from "now".
                //
                // This measures, per frame:
                //   pts_vs_now - presentation_time_ns minus the current monotonic clock, in ms.
                //                Positive and large means we asked the codec to render "later";
                //                near zero means the two clocks happen to agree.
                //   SURFLAT    - release call -> ImageReader callback, i.e. how long the codec
                //                actually took to produce the frame. Comparing SURFLAT against
                //                pts_vs_now says whether the wait is the timestamp or the codec.
                {
                    use std::sync::atomic::{AtomicU64, Ordering};
                    static CN: AtomicU64 = AtomicU64::new(0);
                    static SUM_DIFF: AtomicU64 = AtomicU64::new(0);
                    static MIN_DIFF: AtomicU64 = AtomicU64::new(u64::MAX);
                    static MAX_DIFF: AtomicU64 = AtomicU64::new(0);
                    static HD: [AtomicU64; 7] = [
                        AtomicU64::new(0),
                        AtomicU64::new(0),
                        AtomicU64::new(0),
                        AtomicU64::new(0),
                        AtomicU64::new(0),
                        AtomicU64::new(0),
                        AtomicU64::new(0),
                    ];
                    static SEEN_EPOCH_10: AtomicU64 = AtomicU64::new(0);
                    if alvr_common::new_probe_session(&SEEN_EPOCH_10) {
                        CN.store(0, Ordering::Relaxed);
                        SUM_DIFF.store(0, Ordering::Relaxed);
                        MIN_DIFF.store(u64::MAX, Ordering::Relaxed);
                        MAX_DIFF.store(0, Ordering::Relaxed);
                        for slot in HD.iter() {
                            slot.store(0, Ordering::Relaxed);
                        }
                    }

                    // Establish the offset between the frame PTS and a monotonic baseline.
                    // libc is not a dependency here, so instead of CLOCK_MONOTONIC we take
                    // the difference between the PTS and the process-relative Instant clock
                    // accumulated with the boot-relative PTS base. What matters for the
                    // diagnostic is the SIZE of the offset and whether it changes, not its
                    // absolute origin: a PTS far from "now" in either direction means the
                    // codec is being asked to render at a time that is not the present.
                    //
                    // Both quantities are nanoseconds. SystemTime is wall clock and PTS is
                    // OpenXR monotonic, so the absolute difference includes a constant epoch
                    // gap; only the variation across frames is meaningful, which the
                    // histogram below shows.
                    let mono_ns = {
                        use std::time::{SystemTime, UNIX_EPOCH};
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map(|d| d.as_nanos() as i64)
                            .unwrap_or(0)
                    };
                    let diff_ns = presentation_time_ns.saturating_sub(mono_ns);
                    let abs_diff_ms = (diff_ns.unsigned_abs() / 1_000_000) as u64;

                    let cn = CN.fetch_add(1, Ordering::Relaxed) + 1;
                    SUM_DIFF.fetch_add(abs_diff_ms, Ordering::Relaxed);
                    MIN_DIFF.fetch_min(abs_diff_ms, Ordering::Relaxed);
                    MAX_DIFF.fetch_max(abs_diff_ms, Ordering::Relaxed);
                    let hb = match abs_diff_ms {
                        0..=1 => 0,
                        2..=5 => 1,
                        6..=20 => 2,
                        21..=99 => 3,
                        100..=999 => 4,
                        1000..=9999 => 5,
                        _ => 6,
                    };
                    HD[hb].fetch_add(1, Ordering::Relaxed);

                    if crate::diagnostics_enabled() && cn % 360 == 0 {
                        let c = cn as f64;
                        alvr_common::warn!(
                            "PTSCLOCK n={} pts_vs_now_avg={:.2}ms min={}ms max={}ms | sign_first={} | hist(0-1,2-5,6-20,21-99,100-999,1k-10k,10k+ms)={}/{}/{}/{}/{}/{}/{}",
                            cn,
                            SUM_DIFF.load(Ordering::Relaxed) as f64 / c,
                            MIN_DIFF.load(Ordering::Relaxed),
                            MAX_DIFF.load(Ordering::Relaxed),
                            diff_ns,
                            HD[0].load(Ordering::Relaxed),
                            HD[1].load(Ordering::Relaxed),
                            HD[2].load(Ordering::Relaxed),
                            HD[3].load(Ordering::Relaxed),
                            HD[4].load(Ordering::Relaxed),
                            HD[5].load(Ordering::Relaxed),
                            HD[6].load(Ordering::Relaxed),
                        );
                    }
                }

                let t_rel0 = now_us!();
                // A/B switch: AT_TIME uses the media PTS (current behaviour), IMMEDIATE asks
                // the codec to render right away. SURFLAT is the same measurement in both
                // modes, so comparing them shows directly whether the timestamp is what
                // makes the codec wait.
                static USE_IMMEDIATE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
                let immediate = *USE_IMMEDIATE.get_or_init(|| {
                    std::path::Path::new(
                        "/sdcard/Android/data/alvr.client.dev/files/release_immediate.txt",
                    )
                    .exists()
                });
                if immediate {
                    static LOGGED: AtomicU64 = AtomicU64::new(0);
                    static SEEN_EPOCH_11: AtomicU64 = AtomicU64::new(0);
                    if alvr_common::new_probe_session(&SEEN_EPOCH_11) {
                        LOGGED.store(0, Ordering::Relaxed);
                    }
                    if LOGGED.fetch_add(1, Ordering::Relaxed) == 0 {
                        alvr_common::warn!("RELEASEMODE = immediate (no timestamp)");
                    }
                }
                let rel_res = if immediate {
                    decoder.release_output_buffer(buffer, true)
                } else {
                    decoder.release_output_buffer_at_time(buffer, presentation_time_ns)
                };
                let rel_us = now_us!() - t_rel0;

                let rel_b = bucket(rel_us);
                H_REL[rel_b].fetch_add(1, Ordering::Relaxed);
                SUM_REL_US.fetch_add(rel_us, Ordering::Relaxed);
                MAX_REL_US.fetch_max(rel_us, Ordering::Relaxed);

                if let Err(e) = rel_res {
                    error!("Decoder dequeue error: {e}");
                }

                // TEMPORARY DIAGNOSTIC: timestamp of the release. The ImageReader callback
                // compares against this to measure how long MediaCodec takes to actually
                // render the buffer to the surface and notify us - the one segment of the
                // decode path not yet accounted for.
                // Guarded for the same reason as IN_TIME: one mutex and one system-clock
                // read per frame is not worth paying when the probe is switched off.
                if crate::diagnostics_enabled() {
                    use std::time::{SystemTime, UNIX_EPOCH};
                    let now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_micros() as u64)
                        .unwrap_or(0);
                    let mut m = RELEASE_TS.lock();
                    m.push_back((presentation_time_ns, now));
                    while m.len() > 128 {
                        m.pop_front();
                    }
                }

                // Total time this frame spent waiting for the decoder, including every
                // TryAgainLater poll. This is the number the panel's "decode" figure is
                // built from, and the one that decides whether the decoder can keep up.
                let frame_wait_us = now_us!() - t_frame0;

                {
                    static SUM_WAIT_US: AtomicU64 = AtomicU64::new(0);
                    static MAX_WAIT_US: AtomicU64 = AtomicU64::new(0);
                    static SUM_SPIN_US: AtomicU64 = AtomicU64::new(0);
                    static MAX_SPIN_US: AtomicU64 = AtomicU64::new(0);
                    static SUM_SPIN_CNT: AtomicU64 = AtomicU64::new(0);
                    static HW: [AtomicU64; 6] = [
                        AtomicU64::new(0),
                        AtomicU64::new(0),
                        AtomicU64::new(0),
                        AtomicU64::new(0),
                        AtomicU64::new(0),
                        AtomicU64::new(0),
                    ];
                    static SEEN_EPOCH_12: AtomicU64 = AtomicU64::new(0);
                    if alvr_common::new_probe_session(&SEEN_EPOCH_12) {
                        SUM_WAIT_US.store(0, Ordering::Relaxed);
                        MAX_WAIT_US.store(0, Ordering::Relaxed);
                        SUM_SPIN_US.store(0, Ordering::Relaxed);
                        MAX_SPIN_US.store(0, Ordering::Relaxed);
                        SUM_SPIN_CNT.store(0, Ordering::Relaxed);
                        for slot in HW.iter() {
                            slot.store(0, Ordering::Relaxed);
                        }
                    }
                    SUM_WAIT_US.fetch_add(frame_wait_us, Ordering::Relaxed);
                    MAX_WAIT_US.fetch_max(frame_wait_us, Ordering::Relaxed);
                    SUM_SPIN_US.fetch_add(spin_acc_us, Ordering::Relaxed);
                    MAX_SPIN_US.fetch_max(spin_acc_us, Ordering::Relaxed);
                    SUM_SPIN_CNT.fetch_add(spin_cnt, Ordering::Relaxed);
                    let wb = match frame_wait_us {
                        0..=499 => 0,
                        500..=4999 => 1,
                        5000..=9999 => 2,
                        10000..=13999 => 3,
                        14000..=19999 => 4,
                        _ => 5,
                    };
                    HW[wb].fetch_add(1, Ordering::Relaxed);

                    let nn = N.load(Ordering::Relaxed).max(1) as f64;
                    if crate::diagnostics_enabled() && N.load(Ordering::Relaxed) % 180 == 0 {
                        alvr_common::warn!(
                            "DECWAIT n={} wait_avg={:.2}ms wait_max={:.2}ms | spin_avg={:.2}ms spin_max={:.2}ms spin_cnt={:.2} | wait_hist(<0.5,<5,<10,<14,<20,20+ms)={}/{}/{}/{}/{}/{}",
                            N.load(Ordering::Relaxed),
                            SUM_WAIT_US.load(Ordering::Relaxed) as f64 / nn / 1000.0,
                            MAX_WAIT_US.load(Ordering::Relaxed) as f64 / 1000.0,
                            SUM_SPIN_US.load(Ordering::Relaxed) as f64 / nn / 1000.0,
                            MAX_SPIN_US.load(Ordering::Relaxed) as f64 / 1000.0,
                            SUM_SPIN_CNT.load(Ordering::Relaxed) as f64 / nn,
                            HW[0].load(Ordering::Relaxed),
                            HW[1].load(Ordering::Relaxed),
                            HW[2].load(Ordering::Relaxed),
                            HW[3].load(Ordering::Relaxed),
                            HW[4].load(Ordering::Relaxed),
                            HW[5].load(Ordering::Relaxed),
                        );
                    }
                }

                // Reset the per-frame accumulators so the next frame starts from zero.
                spin_acc_us = 0;
                spin_cnt = 0;
                t_frame0 = now_us!();

                let n = N.fetch_add(1, Ordering::Relaxed) + 1;
                if crate::diagnostics_enabled() && n % 180 == 0 {
                    let c = n as f64;
                    let s = SPINS.load(Ordering::Relaxed);
                    alvr_common::warn!(
                        "DECPATH n={} poll_avg={:.2}ms poll_max={:.2}ms | rel_avg={:.3}ms rel_max={:.3}ms | spins_per_frame={:.2} | poll_hist(<0.1,<1,<5,<10,<20,20+ms)={}/{}/{}/{}/{}/{} | rel_hist={}/{}/{}/{}/{}/{}",
                        n,
                        SUM_POLL_US.load(Ordering::Relaxed) as f64 / c / 1000.0,
                        MAX_POLL_US.load(Ordering::Relaxed) as f64 / 1000.0,
                        SUM_REL_US.load(Ordering::Relaxed) as f64 / c / 1000.0,
                        MAX_REL_US.load(Ordering::Relaxed) as f64 / 1000.0,
                        s as f64 / c,
                        H_POLL[0].load(Ordering::Relaxed),
                        H_POLL[1].load(Ordering::Relaxed),
                        H_POLL[2].load(Ordering::Relaxed),
                        H_POLL[3].load(Ordering::Relaxed),
                        H_POLL[4].load(Ordering::Relaxed),
                        H_POLL[5].load(Ordering::Relaxed),
                        H_REL[0].load(Ordering::Relaxed),
                        H_REL[1].load(Ordering::Relaxed),
                        H_REL[2].load(Ordering::Relaxed),
                        H_REL[3].load(Ordering::Relaxed),
                        H_REL[4].load(Ordering::Relaxed),
                        H_REL[5].load(Ordering::Relaxed),
                    );
                }
            }
            Ok(DequeuedOutputBufferInfoResult::TryAgainLater) => {
                SPINS.fetch_add(1, Ordering::Relaxed);
                spin_acc_us += poll_us;
                spin_cnt += 1;

                // Backoff: TESTED AND REVERTED 2026-11-15.
                //
                // The decode thread spun 9.3 times per frame for 10.56 ms total and sat at
                // 103% CPU, which looked like the reason this build runs hotter than Virtual
                // Desktop. Adding spin_loop -> yield_now -> sleep(200us) dropped that thread
                // to 3.7% CPU, but the headset still ran just as hot, so CPU spin was NOT the
                // heat source. It did cost real decode time: PUREDEC went 20-25 -> 29-31 ms.
                //
                // Since the backoff buys no thermal benefit and measurably slows decoding,
                // the tight loop is restored. Heat must be investigated on the GPU path
                // (measured ~60% GPU busy at 492/640 MHz while this ran).
                std::hint::spin_loop();
                continue;
            }
            Ok(i) => {
                // TEMPORARY DIAGNOSTIC (actual decoded frame size).
                //
                // config.frame_size is 1824x1824 (one eye) and is fed to both MediaFormat
                // and ImageReader, but the encoder produces 3648x1824 (both eyes packed
                // side by side). If the codec really outputs 3648x1824 the ImageReader was
                // sized wrong and the platform must rebuild its buffers, which is exactly
                // the cost the earlier frame_size change was meant to remove.
                //
                // Printing the codec's own output format settles it: whatever width/height
                // appears here is what frame_size must be set to.
                info!("Decoder dequeue event: {i:?}");
                if matches!(i, DequeuedOutputBufferInfoResult::OutputFormatChanged) {
                    let fmt = decoder.output_format();
                    alvr_common::warn!(
                        "OUTFMT width={} height={} (configured={:?})",
                        fmt.i32("width").unwrap_or(-1),
                        fmt.i32("height").unwrap_or(-1),
                        config.frame_size,
                    );
                }
            }
            Err(e) => {
                error!("Decoder dequeue error: {e}");

                error_counter += 1;
                if error_counter > 10 {
                    bail!("Too many decoder errors: {e}");
                }

                // lessen logcat flood (just in case)
                thread::sleep(Duration::from_millis(50));

                continue;
            }
        }

        error_counter = 0;
        }
    }

    // Destroy all resources
    decoder_sink.lock().take(); // Make sure the shared ref is deleted first
    decoder.stop()?;
    drop(decoder);

    Ok(())
}

// Create a sink/source pair
pub fn video_decoder_split(
    config: VideoDecoderConfig,
    csd_0: Vec<u8>,
    frame_result_callback: impl Fn(Result<Duration>) + Send + Sync + 'static,
) -> Result<(VideoDecoderSink, VideoDecoderSource)> {
    let running = Arc::new(RelaxedAtomic::new(true));
    let decoder_sink = Arc::new(Mutex::new(None::<SharedMediaCodec>));
    let decoder_ready_notifier = Arc::new(Condvar::new());
    let image_queue = Arc::new(Mutex::new(VecDeque::<QueuedImage>::new()));

    let dequeue_thread = thread::Builder::new()
        .name("alvr-decode".into())
        .spawn({
        let config = config.clone();
        let running = Arc::clone(&running);
        let decoder_sink = Arc::clone(&decoder_sink);
        let decoder_ready_notifier = Arc::clone(&decoder_ready_notifier);
        let image_queue = Arc::clone(&image_queue);
        move || {
            // 10 is the original value and is required: lowering it to 4 made the
            // stream disconnect within seconds.
            const MAX_BUFFERING_FRAMES: usize = 10;
            // Same reasoning as the MediaFormat size: a 1x1 placeholder makes the
            // reader allocate for 1x1 and rebuild for every real frame.
            let [reader_width, reader_height] = config.frame_size;
            let (reader_width, reader_height) = if reader_width > 0 && reader_height > 0 {
                (reader_width as i32, reader_height as i32)
            } else {
                (1, 1)
            };
            let mut image_reader = match ImageReader::new_with_usage(
                reader_width,
                reader_height,
                ImageFormat::PRIVATE,
                HardwareBufferUsage::GPU_SAMPLED_IMAGE,
                MAX_BUFFERING_FRAMES as i32,
            ) {
                Ok(reader) => reader,
                Err(e) => {
                    frame_result_callback(Err(anyhow!("{e}")));
                    return;
                }
            };

            let frame_result_callback = Arc::new(frame_result_callback);

            if let Err(e) = decoder_lifecycle(
                config,
                csd_0,
                Arc::downgrade(&frame_result_callback),
                running,
                decoder_sink,
                decoder_ready_notifier,
                Arc::clone(&image_queue),
                &mut image_reader,
            ) {
                frame_result_callback(Err(e));
            }

            image_queue.lock().clear();
        }
    })
    .expect("failed to spawn decode thread");

    // Make sure the decoder is ready: we don't want to try to enqueue frame and lose them, to avoid
    // image corruption.
    {
        let mut decoder_lock = decoder_sink.lock();

        if decoder_lock.is_none() {
            // No spurious wakeups
            decoder_ready_notifier.wait(&mut decoder_lock);
        }
    }

    let sink = VideoDecoderSink {
        inner: decoder_sink,
    };
    let source = VideoDecoderSource {
        running,
        dequeue_thread: Some(dequeue_thread),
        image_queue,
        config,
        buffering_running_average: 0.0,
    };

    Ok((sink, source))
}
