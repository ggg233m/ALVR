//! Per-frame latency breakdown keyed by presentation timestamp.
//!
//! The remaining stutter happens *before* the decoder sees a frame, and the aggregate
//! counters cannot say which stage lost the time: encoder output, socket write, socket read,
//! or the hand-off into the decoder queue. This records the four boundaries for each frame
//! and matches them on PTS, so the delay can be attributed to one segment instead of guessed.
//!
//! Design constraints, both learned the hard way:
//!
//!   * Bounded. An earlier version pushed every frame into an unbounded structure and the
//!     probe itself became the bottleneck. This keeps a fixed ring and overwrites.
//!   * No formatting or IO on the recording path. Everything is a fixed-size array write
//!     under one short mutex; nothing is formatted until `dump` is called after the session.
//!
//! Everything here is behind `diagnostics_enabled()`, so with the switch off each call site
//! costs a single atomic load and never reaches this module.

use crate::warn;
use parking_lot::Mutex;
use std::time::Duration;

/// One frame's four boundaries, all in microseconds on the same monotonic clock.
///
/// Zero means "not recorded yet", which is how a partial chain is detected.
#[derive(Clone, Copy, Default)]
pub struct FrameTrace {
    pub pts_ns: u64,
    /// When the frame was handed to the hardware encoder (server, Transmit entry).
    ///
    /// The gap between consecutive values here is the true output cadence of the submit
    /// path. `encoded_us` is later and includes the encoder's own callbacks and logging, so
    /// without this point a long `encoded` gap cannot be attributed to hardware or to the
    /// plumbing in between.
    pub encoder_input_us: u64,
    /// When the encoder finished this frame (server, at VideoSend entry).
    ///
    /// Note: this is NOT the hardware completion instant. VideoSend is entered after the
    /// encoder's own callback and logging, so the output spacing measured here is already
    /// the result of whatever happened upstream. Treat it as "the frame is available to the
    /// server", not "the GPU finished".
    pub encoded_us: u64,
    /// When SteamVR entered `SubmitLayer` for this frame (server).
    ///
    /// The interval between consecutive values is the callback arrival cadence. If it is
    /// already long, the delay is upstream of the driver - including the previous round's
    /// `PostPresent`/vsync wait. If it is steady, the driver itself is holding things up.
    pub submit_entry_us: u64,
    /// When `Present` was entered (server).
    pub present_entry_us: u64,
    /// When `Present` returned (server).
    pub present_exit_us: u64,
    /// When `PostPresent` was entered (server).
    ///
    /// Belongs to THIS frame but runs after it; the gap from `present_exit` is the driver's
    /// own bookkeeping, and the gap from here to the NEXT frame's `submit_entry` is the
    /// vsync sleep plus SteamVR's scheduling.
    pub post_entry_us: u64,
    /// When `PostPresent` returned (server).
    pub post_exit_us: u64,
    /// Accumulated thread CPU time (kernel + user, 100 ns units) at `PostPresent` exit.
    ///
    /// Paired with `submit_cpu` on the NEXT frame this says whether the submitting thread
    /// executed anything during the callback gap: a zero delta means it was off-CPU (waiting
    /// or simply not scheduled), which is the one thing the timestamps alone cannot tell.
    pub post_exit_cpu: u64,
    /// Thread CPU time at `SubmitLayer` entry.
    pub submit_cpu: u64,
    /// Why `Present` exited early, if it did.
    ///
    /// 0 = completed normally. Anything else marks a frame that never reached
    /// `NewFrameReady`, which the earlier statistics silently dropped: the dump required
    /// present_exit and ready to exist, so every aborted frame was filtered out of the
    /// report and the failure rate looked like zero.
    pub abort_reason: u32,
    /// `HRESULT` from the failing call when `abort_reason` is set.
    pub abort_hr: i32,
    /// When `NewFrameReady` was called (server, Present side).
    ///
    /// This is the instant the submitting thread told the encoder thread a frame is ready.
    /// The gap from here to `transmit_return` includes the encoder thread being scheduled,
    /// so it separates "handed over late" from "encoder started late".
    pub frame_ready_us: u64,
    /// When `Transmit` returned (server, encoder thread).
    ///
    /// `encoded_us` (VideoSend) is inside Transmit; this is after it, so the gap between the
    /// two is the VideoSend tail, callbacks and locks that run before Transmit unwinds.
    pub transmit_return_us: u64,
    /// When `m_encodeFinished.Set()` ran (server, encoder thread).
    ///
    /// The submitting thread blocks in WaitForEncode until this fires. `encoded_us` is NOT
    /// this point: everything between VideoSend and here (statistics, logging, scheduling)
    /// lands in between, which is why "encoding is fast" and "WaitForEncode is long" can
    /// both be true at once.
    pub encode_finished_us: u64,
    /// When the socket write call returned (server).
    ///
    /// This marks the write_all return, not bytes on the wire: the kernel may still hold
    /// them. Recorded only when the send reported success.
    pub sent_us: u64,
    /// When the video consumer thread pulled the complete packet out of its queue (client).
    ///
    /// This already includes waiting in the client's own packet queue, so it is later than
    /// the kernel finished reassembly. Kept because it is the first instant the client can
    /// act on the frame.
    pub received_us: u64,
    /// When the decoder callback was entered (client, before taking the decoder lock).
    pub callback_us: u64,
    /// When queue_input_buffer returned successfully (client).
    ///
    /// This is the boundary that actually covers the lock, the input-buffer wait
    /// (dequeue_input_buffer, 20 ms timeout), the payload copy and the queue call. Without
    /// it, "recv -> enq" looked like 9 us simply because it stopped before all of that.
    pub queued_us: u64,
}

/// Number of frames kept. 600 is about 6.7 s at 90 Hz: long enough to cover a stall and the
/// frames around it, small enough to stay in cache.
const CAPACITY: usize = 600;

struct Ring {
    frames: [FrameTrace; CAPACITY],
    next: usize,
    count: usize,
}

impl Ring {
    const fn new() -> Self {
        Self {
            frames: [FrameTrace {
                pts_ns: 0,
                encoder_input_us: 0,
                submit_entry_us: 0,
                present_entry_us: 0,
                present_exit_us: 0,
                post_entry_us: 0,
                post_exit_us: 0,
                post_exit_cpu: 0,
                submit_cpu: 0,
                abort_reason: 0,
                abort_hr: 0,
                frame_ready_us: 0,
                transmit_return_us: 0,
                encode_finished_us: 0,
                encoded_us: 0,
                sent_us: 0,
                received_us: 0,
                callback_us: 0,
                queued_us: 0,
            }; CAPACITY],
            next: 0,
            count: 0,
        }
    }

    fn slot(&mut self, pts_ns: u64) -> &mut FrameTrace {
        // Prefer an existing entry for this PTS so the four stages land in one slot
        // regardless of the order they arrive in.
        for i in 0..self.count {
            let idx = (self.next + CAPACITY - 1 - i) % CAPACITY;
            if self.frames[idx].pts_ns == pts_ns {
                return &mut self.frames[idx];
            }
        }

        let idx = self.next;
        self.frames[idx] = FrameTrace {
            pts_ns,
            ..Default::default()
        };
        self.next = (self.next + 1) % CAPACITY;
        if self.count < CAPACITY {
            self.count += 1;
        }

        &mut self.frames[idx]
    }
}

static RING: Mutex<Ring> = Mutex::new(Ring::new());

/// Microseconds since the Unix epoch, matching `GetTimestampUs()` on the C++ side.
///
/// This used to be `Instant::now()` relative to a stored origin, which is a monotonic clock
/// with an arbitrary epoch. The C++ call sites pass `GetTimestampUs()`, i.e. `system_clock`
/// since 1970, so the two produced values roughly 1.8e15 us apart and every derived segment
/// was garbage (all-zero or saturating). The whole module now uses one domain.
fn now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

/// Reason codes for `FrameTrace::abort_reason`.
pub mod abort {
    /// `Present` completed and the frame reached the encoder.
    pub const NONE: u32 = 0;
    /// `GetSharedTexture` returned null.
    pub const NO_SYNC_TEXTURE: u32 = 1;
    /// `IDXGIKeyedMutex::AcquireSync` timed out or failed.
    pub const ACQUIRE_SYNC_FAILED: u32 = 2;
}

/// Server: `Present` exited before handing the frame to the encoder.
pub fn record_present_abort(pts_ns: u64, reason: u32, hr: i32) {
    let mut ring = RING.lock();
    let slot = ring.slot(pts_ns);
    slot.abort_reason = reason;
    slot.abort_hr = hr;
}

/// Server: SteamVR called `SubmitLayer`.
///
/// Takes the instant explicitly. These callbacks are entered before the PTS is known, so the
/// value is captured into a thread-local first and written here later; letting the recorder
/// read the clock itself would store the flush time instead of the entry time.
pub fn record_submit_entry(pts_ns: u64, at_us: u64) {
    RING.lock().slot(pts_ns).submit_entry_us = at_us;
}

/// Server: `Present` entered. See `record_submit_entry` for why the instant is passed in.
pub fn record_present_entry(pts_ns: u64, at_us: u64) {
    RING.lock().slot(pts_ns).present_entry_us = at_us;
}

/// Server: `Present` returned.
pub fn record_present_exit(pts_ns: u64) {
    let t = now_us();
    RING.lock().slot(pts_ns).present_exit_us = t;
}

/// Server: `PostPresent` entered.
pub fn record_post_entry(pts_ns: u64) {
    let t = now_us();
    RING.lock().slot(pts_ns).post_entry_us = t;
}

/// Server: `PostPresent` returned, with the thread's accumulated CPU time.
pub fn record_post_exit(pts_ns: u64, cpu_100ns: u64) {
    let t = now_us();
    let mut ring = RING.lock();
    let slot = ring.slot(pts_ns);
    slot.post_exit_us = t;
    slot.post_exit_cpu = cpu_100ns;
}

/// Server: `SubmitLayer` entered, with the thread's accumulated CPU time.
pub fn record_submit_cpu(pts_ns: u64, cpu_100ns: u64) {
    RING.lock().slot(pts_ns).submit_cpu = cpu_100ns;
}

/// Server: the submitting thread signalled that a frame is ready.
pub fn record_frame_ready(pts_ns: u64) {
    let t = now_us();
    RING.lock().slot(pts_ns).frame_ready_us = t;
}

/// Server: `Transmit` returned on the encoder thread.
pub fn record_transmit_return(pts_ns: u64) {
    let t = now_us();
    RING.lock().slot(pts_ns).transmit_return_us = t;
}

/// Server: `m_encodeFinished.Set()` ran, releasing the submitting thread.
pub fn record_encode_finished(pts_ns: u64) {
    let t = now_us();
    RING.lock().slot(pts_ns).encode_finished_us = t;
}

/// Server: the frame was handed to the hardware encoder.
pub fn record_encoder_input(pts_ns: u64) {
    let t = now_us();
    RING.lock().slot(pts_ns).encoder_input_us = t;
}

/// Server: the encoder finished this frame.
pub fn record_encoded(pts_ns: u64) {
    let t = now_us();
    RING.lock().slot(pts_ns).encoded_us = t;
}

/// Server: the socket accepted the packet.
pub fn record_sent(pts_ns: u64) {
    let t = now_us();
    RING.lock().slot(pts_ns).sent_us = t;
}

/// Client: the complete packet was read off the wire.
pub fn record_received(pts_ns: u64) {
    let t = now_us();
    RING.lock().slot(pts_ns).received_us = t;
}

/// Client: the decoder callback was entered (before taking any decoder lock).
pub fn record_callback(pts_ns: u64) {
    let t = now_us();
    RING.lock().slot(pts_ns).callback_us = t;
}

/// Client: `queue_input_buffer` returned successfully.
///
/// Covers the decoder lock, the input-buffer wait, the payload copy and the queue call -
/// everything the earlier "enqueued" boundary skipped.
pub fn record_queued(pts_ns: u64) {
    let t = now_us();
    RING.lock().slot(pts_ns).queued_us = t;
}

/// Which process is dumping. The ring is a per-process static, so the server only ever
/// holds `encoded`/`sent` and the client only `received`/`enqueued`. Each side prints what it
/// actually has, tagged so the two logs can be joined on PTS afterwards.
#[derive(Clone, Copy, PartialEq)]
pub enum TraceSide {
    Server,
    Client,
}

/// Format the ring for the log. Call once, after the session, never on the frame path.
///
/// Only the boundaries this process owns are printed. The cross-process segment
/// (`send -> recv`) is deliberately not computed here: the two sides run on unrelated clocks
/// (steady_clock vs CLOCK_MONOTONIC), so subtracting them directly would be meaningless.
/// Join the two dumps on PTS instead, and compare the *interval* sequences - a frame whose
/// client-side gap grew while the server-side gap did not is a transport delay, and vice versa.
pub fn dump(side: TraceSide) {
    let ring = RING.lock();
    if ring.count == 0 {
        warn!("FRAMETRACE no frames recorded");
        return;
    }

    let mut frames = Vec::with_capacity(ring.count);
    for i in 0..ring.count {
        let idx = (ring.next + CAPACITY - 1 - i) % CAPACITY;
        frames.push(ring.frames[idx]);
    }
    drop(ring);
    frames.reverse();

    let tag = match side {
        TraceSide::Server => "SRV",
        TraceSide::Client => "CLI",
    };

    let mut printed = 0;
    let mut partial = 0;
    let mut first_seg = Vec::new();
    let mut second_seg = Vec::new();
    let mut third_seg = Vec::new();
    let mut fourth_seg = Vec::new();
    let mut fifth_seg = Vec::new();
    let mut sixth_seg = Vec::new();
    let mut seventh_seg = Vec::new();
    let mut pre_present_seg = Vec::new();
    let mut in_present_seg = Vec::new();
    let mut to_ready_seg = Vec::new();

    for f in &frames {
        match side {
            TraceSide::Server => {
                if f.submit_entry_us == 0
                    || f.present_entry_us == 0
                    || f.present_exit_us == 0
                    || f.frame_ready_us == 0
                {
                    partial += 1;
                    continue;
                }

                // The full per-frame chain, in execution order:
                //   submit -> present_enter   driver bookkeeping before Present
                //   present_enter -> present_exit   AcquireSync, WaitForEncode, render
                //   present_exit -> ready      NewFrameReady
                //   ready -> in                encoder thread scheduling
                //   in -> enc                  encode work
                //   enc -> txret -> set        VideoSend tail + CEncoder post
                //   enc -> send                socket write
                let pre_present = f.present_entry_us.saturating_sub(f.submit_entry_us);
                let in_present = f.present_exit_us.saturating_sub(f.present_entry_us);
                let to_ready = f.frame_ready_us.saturating_sub(f.present_exit_us);

                pre_present_seg.push(pre_present);
                in_present_seg.push(in_present);
                to_ready_seg.push(to_ready);

                if f.encoder_input_us != 0
                    && f.encoded_us != 0
                    && f.transmit_return_us != 0
                    && f.encode_finished_us != 0
                    && f.sent_us != 0
                {
                    fifth_seg.push(f.encoder_input_us.saturating_sub(f.frame_ready_us));
                    fourth_seg.push(f.encoded_us.saturating_sub(f.encoder_input_us));
                    sixth_seg.push(f.transmit_return_us.saturating_sub(f.encoded_us));
                    seventh_seg.push(f.encode_finished_us.saturating_sub(f.transmit_return_us));
                    first_seg.push(f.sent_us.saturating_sub(f.encoded_us));
                }

                printed += 1;
                warn!(
                    "FRAMETRACE_{tag} pts={} submit={} prenter={} prexit={} ready={} in={} enc={} txret={} set={} send={} | pre_present={}us in_present={}us to_ready={}us | ready={} in={} enc={} txret={} set={} send={} post_enter={} post_exit={}",
                    f.pts_ns,
                    f.submit_entry_us, f.present_entry_us, f.present_exit_us,
                    f.frame_ready_us, f.encoder_input_us, f.encoded_us,
                    f.transmit_return_us, f.encode_finished_us, f.sent_us,
                    pre_present, in_present, to_ready,
                    f.frame_ready_us, f.encoder_input_us, f.encoded_us,
                    f.transmit_return_us, f.encode_finished_us, f.sent_us,
                    f.post_entry_us, f.post_exit_us
                );
                let _ = (f.post_exit_cpu, f.submit_cpu);
            }
            TraceSide::Client => {
                if f.received_us == 0 || f.callback_us == 0 || f.queued_us == 0 {
                    partial += 1;
                    continue;
                }
                // Three client-side segments, each with a different owner:
                //   recv -> callback : consumer thread scheduling / queue handoff
                //   callback -> queued : the decoder lock, input-buffer wait and copy
                let a = f.callback_us.saturating_sub(f.received_us);
                let b = f.queued_us.saturating_sub(f.callback_us);
                second_seg.push(b);
                third_seg.push(a);
                printed += 1;
                warn!(
                    "FRAMETRACE_{tag} pts={} recv={} cb={} queued={} | recv->cb={}us cb->queued={}us",
                    f.pts_ns, f.received_us, f.callback_us, f.queued_us, a, b
                );
            }
        }
    }

    fn summary(tag: &str, name: &str, mut v: Vec<u64>) {
        if v.is_empty() {
            return;
        }
        v.sort_unstable();
        let p = |q: f64| v[((v.len() as f64 * q) as usize).min(v.len() - 1)];
        let sum: u64 = v.iter().sum();
        warn!(
            "FRAMETRACE_{tag} {name}: n={} avg={}us p50={}us p95={}us p99={}us max={}us",
            v.len(),
            sum / v.len() as u64,
            p(0.5),
            p(0.95),
            p(0.99),
            v[v.len() - 1]
        );
    }

    // Aborted frames: Present exited before NewFrameReady. These were previously invisible
    // because the per-frame report skipped any entry without present_exit/ready.
    if side == TraceSide::Server {
        let mut counts = std::collections::BTreeMap::new();
        for f in &frames {
            if f.abort_reason != 0 {
                *counts.entry((f.abort_reason, f.abort_hr)).or_insert(0usize) += 1;
            }
        }
        let total = counts.values().sum::<usize>();
        warn!(
            "FRAMETRACE_{tag} aborts: total={} of {} frames",
            total,
            frames.len()
        );
        for ((reason, hr), count) in counts {
            let name = match reason {
                abort::NO_SYNC_TEXTURE => "NO_SYNC_TEXTURE",
                abort::ACQUIRE_SYNC_FAILED => "ACQUIRE_SYNC_FAILED",
                _ => "UNKNOWN",
            };
            warn!(
                "FRAMETRACE_{tag} abort: reason={name} hr=0x{:08X} count={count}",
                hr as u32
            );
        }
    }

    // Callback-gap CPU usage: for each consecutive pair, how much CPU the submitting
    // thread accumulated between the previous PostPresent exit and this SubmitLayer entry.
    // A zero delta means the thread was off-CPU for the whole gap.
    if side == TraceSide::Server {
        let mut zero = 0usize;
        let mut nonzero = 0usize;
        let mut samples = Vec::new();
        for w in frames.windows(2) {
            let (prev, cur) = (w[0], w[1]);
            if prev.post_exit_cpu == 0 || cur.submit_cpu == 0 || cur.submit_cpu < prev.post_exit_cpu
            {
                continue;
            }
            let delta_100ns = cur.submit_cpu - prev.post_exit_cpu;
            if delta_100ns == 0 {
                zero += 1;
            } else {
                nonzero += 1;
            }
            samples.push(delta_100ns / 10); // to microseconds
        }
        if !samples.is_empty() {
            samples.sort_unstable();
            let p = |q: f64| samples[((samples.len() as f64 * q) as usize).min(samples.len() - 1)];
            warn!(
                "FRAMETRACE_{tag} gap_cpu: n={} zero_cpu={} nonzero_cpu={} p50={}us p95={}us p99={}us max={}us",
                samples.len(),
                zero,
                nonzero,
                p(0.5),
                p(0.95),
                p(0.99),
                samples[samples.len() - 1]
            );
        }
    }

    match side {
        TraceSide::Server => {
            // Inter-frame gaps are the interesting part for the server, and they need the
            // previous frame, so they are computed by the analysis script from the per-frame
            // lines. Only the intra-frame segments are summarised here.
            summary(tag, "pre_present", pre_present_seg);
            summary(tag, "in_present", in_present_seg);
            summary(tag, "to_ready", to_ready_seg);
            summary(tag, "ready->in (sched)", fifth_seg);
            summary(tag, "in->enc (encode)", fourth_seg);
            summary(tag, "enc->txret (tail)", sixth_seg);
            summary(tag, "txret->set (post)", seventh_seg);
            summary(tag, "enc->send", first_seg);
        }
        TraceSide::Client => {
            summary(tag, "recv->callback", third_seg);
            summary(tag, "callback->queued", second_seg);
        }
    }
    let _ = second_seg;
    let _ = first_seg;
    let _ = third_seg;
    let _ = fourth_seg;
    let _ = fifth_seg;
    let _ = sixth_seg;
    let _ = seventh_seg;
    let _ = pre_present_seg;
    let _ = in_present_seg;
    let _ = to_ready_seg;

    warn!(
        "FRAMETRACE_{tag} total: printed={} partial={} capacity={}",
        printed, partial, CAPACITY
    );
}

/// Kept for symmetry with the other probes; the ring never needs an explicit reset because
/// it is bounded, but a new session should start from a clean slate.
pub fn reset() {
    *RING.lock() = Ring::new();
}

#[allow(dead_code)]
pub fn _duration_marker(_: Duration) {}
