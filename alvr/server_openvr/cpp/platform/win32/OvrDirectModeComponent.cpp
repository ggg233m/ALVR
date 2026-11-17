#include "OvrDirectModeComponent.h"

OvrDirectModeComponent::OvrDirectModeComponent(
    std::shared_ptr<CD3DRender> pD3DRender, std::shared_ptr<PoseHistory> poseHistory
)
    : m_pD3DRender(pD3DRender)
    , m_poseHistory(poseHistory)
    , m_submitLayer(0) { }

void OvrDirectModeComponent::SetEncoder(std::shared_ptr<CEncoder> pEncoder) {
    m_pEncoder = pEncoder;
}

/** Specific to Oculus compositor support, textures supplied must be created using this method. */
void OvrDirectModeComponent::CreateSwapTextureSet(
    uint32_t unPid,
    const SwapTextureSetDesc_t* pSwapTextureSetDesc,
    SwapTextureSet_t* pOutSwapTextureSet
) {
    Debug(
        "OvrDirectModeComponent::CreateSwapTextureSet pid=%d Format=%d %dx%d SampleCount=%d",
        unPid,
        pSwapTextureSetDesc->nFormat,
        pSwapTextureSetDesc->nWidth,
        pSwapTextureSetDesc->nHeight,
        pSwapTextureSetDesc->nSampleCount
    );

    // HRESULT hr = D3D11CreateDevice(pAdapter, D3D_DRIVER_TYPE_HARDWARE, NULL, creationFlags, NULL,
    // 0, D3D11_SDK_VERSION, &pDevice, &eFeatureLevel, &pContext);

    D3D11_TEXTURE2D_DESC SharedTextureDesc = {};
    DXGI_FORMAT format = (DXGI_FORMAT)pSwapTextureSetDesc->nFormat;
    SharedTextureDesc.BindFlags = D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET;
    if (format == DXGI_FORMAT_R32G8X24_TYPELESS || format == DXGI_FORMAT_R32_TYPELESS) {
        SharedTextureDesc.BindFlags = D3D11_BIND_DEPTH_STENCIL;
    }
    SharedTextureDesc.ArraySize = 1;
    SharedTextureDesc.MipLevels = 1;
    SharedTextureDesc.SampleDesc.Count
        = pSwapTextureSetDesc->nSampleCount == 0 ? 1 : pSwapTextureSetDesc->nSampleCount;
    SharedTextureDesc.SampleDesc.Quality = 0;
    SharedTextureDesc.Usage = D3D11_USAGE_DEFAULT;
    SharedTextureDesc.Format = format;

    // Some(or all?) applications request larger texture than we specified in
    // GetRecommendedRenderTargetSize. But, we must create textures in requested size to prevent
    // cropped output. And then we must shrink texture to H.264 movie size.
    SharedTextureDesc.Width = pSwapTextureSetDesc->nWidth;
    SharedTextureDesc.Height = pSwapTextureSetDesc->nHeight;

    // SharedTextureDesc.MiscFlags = D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX |
    // D3D11_RESOURCE_MISC_SHARED_NTHANDLE;
    SharedTextureDesc.MiscFlags = D3D11_RESOURCE_MISC_SHARED;

    ProcessResource* processResource = new ProcessResource();
    processResource->pid = unPid;
    m_swapchainIndices[processResource] = 0;

    for (int i = 0; i < 3; i++) {
        HRESULT hr = m_pD3DRender->GetDevice()->CreateTexture2D(
            &SharedTextureDesc, NULL, &processResource->textures[i]
        );
        // LogDriver("texture%d %p res:%d %s", i, texture[i], hr, GetDxErrorStr(hr).c_str());
        if (FAILED(hr)) {
            Error("CreateSwapTextureSet CreateTexture2D %p %ls", hr, GetErrorStr(hr).c_str());
            m_swapchainIndices.erase(processResource);
            delete processResource;
            break;
        }

        IDXGIResource* pResource;
        hr = processResource->textures[i]->QueryInterface(
            __uuidof(IDXGIResource), (void**)&pResource
        );
        if (FAILED(hr)) {
            Error("CreateSwapTextureSet QueryInterface %p %ls", hr, GetErrorStr(hr).c_str());
            m_swapchainIndices.erase(processResource);
            delete processResource;
            break;
        }
        // LogDriver("QueryInterface %p res:%d %s", pResource, hr, GetDxErrorStr(hr).c_str());

        hr = pResource->GetSharedHandle(&processResource->sharedHandles[i]);
        if (FAILED(hr)) {
            Error("CreateSwapTextureSet GetSharedHandle %p %ls", hr, GetErrorStr(hr).c_str());
            m_swapchainIndices.erase(processResource);
            delete processResource;
            pResource->Release();
            break;
        }
        // LogDriver("GetSharedHandle %p res:%d %s", processResource->sharedHandles[i], hr,
        // GetDxErrorStr(hr).c_str());

        m_handleMap.insert(
            std::make_pair(processResource->sharedHandles[i], std::make_pair(processResource, i))
        );

        pOutSwapTextureSet->rSharedTextureHandles[i]
            = (vr::SharedTextureHandle_t)processResource->sharedHandles[i];

        pResource->Release();

        Debug("Created Texture %d %p", i, processResource->sharedHandles[i]);
    }
    // m_processMap.insert(std::pair<uint32_t, ProcessResource *>(unPid, processResource));
}

/** Used to textures created using CreateSwapTextureSet.  Only one of the set's handles needs to be
 * used to destroy the entire set. */
void OvrDirectModeComponent::DestroySwapTextureSet(vr::SharedTextureHandle_t sharedTextureHandle) {
    Debug("OvrDirectModeComponent::DestroySwapTextureSet %p", sharedTextureHandle);

    auto it = m_handleMap.find((HANDLE)sharedTextureHandle);
    if (it != m_handleMap.end()) {
        // Release all reference (a bit forcible)
        ProcessResource* p = it->second.first;
        m_handleMap.erase(p->sharedHandles[0]);
        m_handleMap.erase(p->sharedHandles[1]);
        m_handleMap.erase(p->sharedHandles[2]);
        m_swapchainIndices.erase(p);
        delete p;
    } else {
        Debug("Requested to destroy not managing texture. handle:%p", sharedTextureHandle);
    }
}

/** Used to purge all texture sets for a given process. */
void OvrDirectModeComponent::DestroyAllSwapTextureSets(uint32_t unPid) {
    Debug("OvrDirectModeComponent::DestroyAllSwapTextureSets pid=%d", unPid);

    for (auto it = m_handleMap.begin(); it != m_handleMap.end();) {
        if (it->second.first->pid == unPid) {
            if (it->second.second == 0) {
                m_swapchainIndices.erase(it->second.first);
                delete it->second.first;
            }
            m_handleMap.erase(it++);
        } else {
            ++it;
        }
    }
}

/** After Present returns, calls this to get the next index to use for rendering. */
void OvrDirectModeComponent::GetNextSwapTextureSetIndex(
    vr::SharedTextureHandle_t sharedTextureHandles[2], uint32_t (*pIndices)[2]
) {
    Debug("OvrDirectModeComponent::GetNextSwapTextureSetIndex");

    for (int eye = 0; eye < 2; eye++) {
        auto it = m_handleMap.find((HANDLE)sharedTextureHandles[eye]);
        if (it == m_handleMap.end()) {
            continue;
        }
        auto& idx = m_swapchainIndices[it->second.first];
        idx = (idx + 1) % 3;
        if (pIndices) {
            (*pIndices)[eye] = idx;
        }
    }
}

/** Call once per layer to draw for this frame.  One shared texture handle per eye.  Textures must
 * be created using CreateSwapTextureSet and should be alternated per frame.  Call Present once all
 * layers have been submitted. */
// TEMPORARY DIAGNOSTIC (complete frame loop account).
//
// Present() costs only 0.66 ms and WaitForVSync sleeps ~8.2 ms, which together is 8.9 ms -
// yet the measured output interval is 11.59 ms (86 fps). Roughly 2.2 ms per frame is
// unaccounted for, and PRESENTINT shows 11.5% of intervals landing in 11.5-13 ms while a
// few are as short as 3.5 ms. That pattern is the pacing grid skipping a step.
//
// To find the missing time, the three driver entry points that SteamVR calls per frame are
// timed against a common frame boundary:
//
//   submit - SubmitLayer()      (pose matching + layer bookkeeping)
//   present- Present()          (already known: 0.66 ms)
//   post   - PostPresent()      (WaitForVSync sleep)
//   cycle  - SubmitLayer(n) -> SubmitLayer(n+1), the interval SteamVR actually sees
//
// If cycle is 11.59 ms while submit+present+post sums to 8.9 ms, the remaining 2.7 ms is
// spent in SteamVR itself between our callbacks - which would mean the shortfall is not
// ours to fix.
static std::atomic<uint64_t> g_frame_start_us(0);
static std::atomic<uint64_t> g_sum_submit_us(0), g_sum_present_us(0), g_sum_post_us(0);
static std::atomic<uint64_t> g_sum_cycle_us(0), g_max_cycle_us(0);
static std::atomic<uint64_t> g_cycle_n(0);
static std::atomic<uint64_t> g_cycle_hist[6] = {};
static std::atomic<uint64_t> g_present_start_us(0), g_post_start_us(0);
// The cycle measured in SubmitLayer for the frame currently being presented. Present()
// cannot recompute it: g_frame_start_us already holds THIS frame's SubmitLayer time by the
// time Present runs, so subtracting would yield only the submit->present gap (~0.65 ms).
static std::atomic<uint64_t> g_last_cycle_us(0);

// TEMPORARY DIAGNOSTIC (SteamVR callback gap).
//
// The frame cycle decomposes as:
//   cycle = (SubmitLayer -> Present -> PostPresent) + gap
// where gap is the time from PostPresent returning to the NEXT SubmitLayer entering.
// Measured gap_avg is 1.78 ms. That is NOT a cost that can simply be subtracted to reach
// 90 Hz: WaitForVSync recomputes its sleep from the current time, so a larger gap is
// normally absorbed by a shorter sleep. What matters is the per-frame breakdown:
//
//   sleep_over - how much longer thread::sleep took than requested (SLEEPACC, in lib.rs)
//   cb_gap     - PostPresent return -> next SubmitLayer entry (measured here)
//
// If cb_gap is a steady ~1.7 ms and uncorrelated with sleep overshoot, it is SteamVR's own
// callback scheduling and a normal OpenVR driver cannot remove it. If cb_gap spikes
// together with long cycles, it is thread preemption and can potentially be mitigated by
// raising this thread's priority.
static std::atomic<uint64_t> g_post_return_us(0);
static std::atomic<uint64_t> g_sum_cbgap_us(0), g_max_cbgap_us(0), g_cbgap_n(0);
static std::atomic<uint64_t> g_cbgap_hist[6] = {};

// Per-cycle diagnostics use a monotonic clock and record only long cycles; each
// callback's thread id is kept alongside.
static uint64_t StallNowUs() {
    return std::chrono::duration_cast<std::chrono::microseconds>(
        std::chrono::steady_clock::now().time_since_epoch()).count();
}
struct StallCycleProbe {
    uint64_t start = 0, submitEnd = 0, presentStart = 0, presentEnd = 0;
    uint64_t postStart = 0, postEnd = 0, pts = 0;
    uint64_t presentLock = 0, acquire = 0, copy = 0, encodeWait = 0;
    DWORD submitTid = 0, presentTid = 0, postTid = 0;
};
static thread_local StallCycleProbe g_stallProbe;

// Entry instants captured before the PTS is known. SubmitLayer and Present run on the same
// thread, and m_targetTimestampNs is only resolved partway through SubmitLayer, so the
// timestamps are parked here and flushed once the key exists.
// Accumulated CPU time of the calling thread, in 100 ns units.
//
// Used to tell "the thread was off-CPU" from "the thread was executing" during the callback
// gap. GetThreadTimes is cheap (a kernel read of the thread object) and adds no trace volume,
// unlike an ETW CSwitch capture, which produced ~90 MB/s here and stalled the machine.
static uint64_t TraceThreadCpu100ns() {
    FILETIME creation = {};
    FILETIME exit = {};
    FILETIME kernel = {};
    FILETIME user = {};
    if (!GetThreadTimes(GetCurrentThread(), &creation, &exit, &kernel, &user)) {
        return 0;
    }
    ULARGE_INTEGER k = {};
    k.LowPart = kernel.dwLowDateTime;
    k.HighPart = kernel.dwHighDateTime;
    ULARGE_INTEGER u = {};
    u.LowPart = user.dwLowDateTime;
    u.HighPart = user.dwHighDateTime;
    return k.QuadPart + u.QuadPart;
}

static thread_local uint64_t g_traceSubmitEntryUs = 0;
static thread_local uint64_t g_tracePresentEntryUs = 0;
static thread_local uint64_t g_tracePresentExitUs = 0;

void OvrDirectModeComponent::SubmitLayer(const SubmitLayerPerEye_t (&perEye)[2]) {
    Debug("OvrDirectModeComponent::SubmitLayer");

    // Frame-trace boundary: SteamVR called us. The spacing of consecutive values is the
    // callback arrival cadence - the earliest point that distinguishes "the callback came
    // late" from "the driver was slow inside it".
    if (DiagEnabled()) {
        g_traceSubmitEntryUs = GetTimestampUs();
    }

    // ENTRY CLOCK SAMPLE (must stay the FIRST thing in this function).
    //
    // flHmdPosePredictionTimeInSecondsFromNow is expressed relative to the instant
    // SteamVR calls SubmitLayer, i.e. the function entry. Sampling QueryPerformanceCounter
    // later - after the CBGAP bookkeeping, the present mutex and especially after
    // GetBestPoseMatch (which walks up to ~1080 pose entries) - adds that work to the
    // derived timestamp. FRAMENOISE measured the resulting residual at 2.16 ms average,
    // which is 19% of a frame period and enough to make the derived timestamp unusable.
    //
    // This value is only read by the FRAMENOISE probe for now; nothing else changes.
    uint64_t g_entryNowNs = 0;
    {
        LARGE_INTEGER ef, ec;
        QueryPerformanceFrequency(&ef);
        QueryPerformanceCounter(&ec);
        g_entryNowNs = (uint64_t)((double)ec.QuadPart * 1e9 / (double)ef.QuadPart);
    }

    const bool stallDiag = DiagEnabled();
    if (stallDiag) {
        const uint64_t now = StallNowUs();
        const auto p = g_stallProbe;
        if (p.start && now - p.start >= 20000) {
            // Emit the difference only when both boundaries were recorded; cross-thread or missing
// callbacks are told apart by the timestamps and thread ids.
            const auto span = [](uint64_t end, uint64_t begin) {
                return end >= begin && begin ? end - begin : uint64_t(0);
            };
            Warn("STALLCYCLE pts=%llu cycle_us=%llu submit_us=%llu sp_gap_us=%llu "
                 "present_us=%llu lock_us=%llu acq_us=%llu copy_us=%llu encwait_us=%llu "
                 "pp_gap_us=%llu post_us=%llu next_gap_us=%llu tids=%lu/%lu/%lu "
                 "complete=%d end_us=%llu wall_us=%llu probe=stall_v1\n",
                 (unsigned long long)p.pts, (unsigned long long)(now-p.start),
                 (unsigned long long)span(p.submitEnd,p.start),
                 (unsigned long long)span(p.presentStart,p.submitEnd),
                 (unsigned long long)span(p.presentEnd,p.presentStart),
                 (unsigned long long)p.presentLock, (unsigned long long)p.acquire,
                 (unsigned long long)p.copy, (unsigned long long)p.encodeWait,
                 (unsigned long long)span(p.postStart,p.presentEnd),
                 (unsigned long long)span(p.postEnd,p.postStart),
                 (unsigned long long)span(now,p.postEnd),
                 p.submitTid,p.presentTid,p.postTid,
                 int(p.submitEnd && p.presentEnd && p.postEnd),
                 (unsigned long long)now, (unsigned long long)GetTimestampUs());
        }
        g_stallProbe = {};
        g_stallProbe.start = now;
        g_stallProbe.submitTid = GetCurrentThreadId();
    }

    // THREAD PRIORITY: TESTED AND REVERTED 2026-11-15.
    //
    // The comment above PostPresent suggests raising this thread's priority to mitigate
    // CBGAP spikes, so THREAD_PRIORITY_TIME_CRITICAL was tried here (set once on first
    // entry). It was accepted by Windows (logged "THREADPRIO set to TIME_CRITICAL") but
    // made things worse:
    //
    //   CBGAP avg        1.438 -> 1.755 ms
    //   CBGAP <1ms       45.6% -> 17.8%
    //   CBGAP 4+ms       55    -> 73  (1.2% -> 2.3%)
    //   gap_avg          1.46  -> 1.84 ms
    //   cycle_avg        11.20 -> 11.31 ms
    //
    // The fast (<1 ms) portion of the gap collapsed and shifted into the 1.5-2 ms bucket,
    // i.e. the loop was scheduled more often but SteamVR's own work got slower, most likely
    // because a TIME_CRITICAL thread starves the compositor threads it hands off to.
    // Leave the default priority alone.

    const uint64_t t_submit0 = GetTimestampUs();

    // TEMPORARY DIAGNOSTIC: gap from the previous PostPresent return to this SubmitLayer.
    if (DiagEnabled())
    {
        const uint64_t pr = g_post_return_us.load();
        if (pr != 0 && t_submit0 > pr) {
            const uint64_t cbg = t_submit0 - pr;
            g_sum_cbgap_us.fetch_add(cbg);
            uint64_t pm = g_max_cbgap_us.load();
            while (cbg > pm && !g_max_cbgap_us.compare_exchange_weak(pm, cbg)) { }
            const size_t hb = cbg < 500 ? 0
                : cbg < 1000 ? 1
                : cbg < 1500 ? 2
                : cbg < 2000 ? 3
                : cbg < 4000 ? 4
                             : 5;
            g_cbgap_hist[hb].fetch_add(1);
            const uint64_t n = g_cbgap_n.fetch_add(1) + 1;
            if (n % 180 == 0) {
                const double c = (double)n;
                Warn(
                    "CBGAP n=%llu avg=%.3fms max=%.3fms | hist(<0.5,<1,<1.5,<2,<4,4+ms)=%llu/%llu/%llu/%llu/%llu/%llu\n",
                    (unsigned long long)n,
                    (double)g_sum_cbgap_us.load() / c / 1000.0,
                    (double)g_max_cbgap_us.load() / 1000.0,
                    (unsigned long long)g_cbgap_hist[0].load(),
                    (unsigned long long)g_cbgap_hist[1].load(),
                    (unsigned long long)g_cbgap_hist[2].load(),
                    (unsigned long long)g_cbgap_hist[3].load(),
                    (unsigned long long)g_cbgap_hist[4].load(),
                    (unsigned long long)g_cbgap_hist[5].load()
                );
            }
        }
    }

    if (DiagEnabled())
    {
        const uint64_t prev_start = g_frame_start_us.exchange(t_submit0);
        if (prev_start != 0) {
            const uint64_t cycle = t_submit0 - prev_start;
            g_last_cycle_us.store(cycle);
            g_sum_cycle_us.fetch_add(cycle);
            uint64_t pm = g_max_cycle_us.load();
            while (cycle > pm && !g_max_cycle_us.compare_exchange_weak(pm, cycle)) { }
            const size_t hb = cycle < 10000 ? 0
                : cycle < 11500 ? 1
                : cycle < 13000 ? 2
                : cycle < 23000 ? 3
                : cycle < 45000 ? 4
                                : 5;
            g_cycle_hist[hb].fetch_add(1);
            const uint64_t n = g_cycle_n.fetch_add(1) + 1;

            if (n % 180 == 0) {
                const double c = (double)n;
                Warn(
                    "FRAMELOOP n=%llu cycle_avg=%.2fms cycle_max=%.2fms | submit_avg=%.2fms present_avg=%.2fms post_avg=%.2fms | gap_avg=%.2fms | hist(<10,<11.5,<13,<23,<45,45+ms)=%llu/%llu/%llu/%llu/%llu/%llu\n",
                    (unsigned long long)n,
                    (double)g_sum_cycle_us.load() / c / 1000.0,
                    (double)g_max_cycle_us.load() / 1000.0,
                    (double)g_sum_submit_us.load() / c / 1000.0,
                    (double)g_sum_present_us.load() / c / 1000.0,
                    (double)g_sum_post_us.load() / c / 1000.0,
                    ((double)g_sum_cycle_us.load()
                     - (double)(g_sum_submit_us.load() + g_sum_present_us.load()
                                + g_sum_post_us.load())) / c / 1000.0,
                    (unsigned long long)g_cycle_hist[0].load(),
                    (unsigned long long)g_cycle_hist[1].load(),
                    (unsigned long long)g_cycle_hist[2].load(),
                    (unsigned long long)g_cycle_hist[3].load(),
                    (unsigned long long)g_cycle_hist[4].load(),
                    (unsigned long long)g_cycle_hist[5].load()
                );
            }
        }
    }

    m_presentMutex.lock();

    // TEMPORARY DIAGNOSTIC (call-rate baseline). POSESEL reports ~8.3% of selected poses
    // spaced more than 1.25 frame periods apart. Before blaming the selection, we must
    // know how often SteamVR asks us to submit a layer at all: if SubmitLayer itself
    // arrives irregularly, the gaps are inherited and GetBestPoseMatch is innocent.
    // Compare this gap rate against POSESEL: equal means "inherited", much lower means
    // "GetBestPoseMatch invented the gaps".
    if (DiagEnabled())
    {
        static std::atomic<uint64_t> n(0), gap1(0), gap2(0), gap3(0);
        static std::atomic<uint64_t> prevCallUs(0), maxGapUs(0);
        LARGE_INTEGER f, t;
        QueryPerformanceFrequency(&f);
        QueryPerformanceCounter(&t);
        const uint64_t nowUs = (uint64_t)(t.QuadPart * 1000000ull / f.QuadPart);
        const uint64_t prev = prevCallUs.exchange(nowUs);
        const uint64_t periodUs = 11111ull; // 90 Hz nominal
        if (prev != 0 && nowUs > prev) {
            const uint64_t gap = nowUs - prev;
            uint64_t pm = maxGapUs.load();
            while (gap > pm && !maxGapUs.compare_exchange_weak(pm, gap)) { }
            if (gap > periodUs + periodUs / 4) {
                if (gap < 2 * periodUs) {
                    gap1.fetch_add(1);
                } else if (gap < 3 * periodUs) {
                    gap2.fetch_add(1);
                } else {
                    gap3.fetch_add(1);
                }
            }
        }
        const uint64_t i = n.fetch_add(1) + 1;
        if (DiagEnabled() && (i % 180 == 0)) {
            Warn(
                "SUBMITCALL n=%llu gap>1.25f=%llu gap>=2f=%llu gap>=3f=%llu gap_max=%.2fms (period=11.11ms)\n",
                (unsigned long long)i,
                (unsigned long long)gap1.load(),
                (unsigned long long)gap2.load(),
                (unsigned long long)gap3.load(),
                (double)maxGapUs.load() / 1000.0
            );
        }
    }

    // mHmdPose is the same pose for both eyes, getting the eye view pose
    //  requires some records keeping, unfortunately (m_eyeToHead)
    auto pPose = &perEye[0].mHmdPose;

    if (m_submitLayer == 0) {
        // Detect FrameIndex of submitted frame by pPose.
        // This is important part to achieve smooth headtracking.
        // We search for history of TrackingInfo and find the TrackingInfo which have nearest matrix
        // value.

        // REVERTED 2026-11-13: replacing this with GetNewest() made the jitter WORSE.
        // matched_age went 84.8% -> 100% zero as intended, but TIMESTAT late_avg doubled
        // (0.44 -> 0.97 ms) and gap_avg rose (1.34 -> 2.08 ms). Reason: this timestamp is
        // not only the pose key, it is also forwarded as the client's openxr_display_time,
        // so moving it forward moved the presentation time forward too.
        auto pose = m_poseHistory->GetBestPoseMatch(*pPose);
        if (pose) {
            // found the frameIndex
            m_prevTargetTimestampNs = m_targetTimestampNs;

            // DIAGNOSTIC ONLY (no behaviour change).
            //
            // Question: is flHmdPosePredictionTimeInSecondsFromNow precise enough to be used
            // as the frame timestamp? Earlier the FRAMETIME probe showed the timestamp derived
            // from it has a 0.48% gap rate versus 10.99% for the matcher-selected one, i.e. 23x
            // smoother. But the ABSOLUTE value of that derived timestamp wobbled +-2 ms across
            // samples, and using it directly would inject that wobble into openxr_display_time.
            //
            // This block changes NOTHING. It decomposes the wobble into its two possible
            // sources so the next step can be decided on evidence:
            //
            //   nowNs     = QueryPerformanceCounter, sampled at SubmitLayer entry
            //   predSec   = flHmdPosePredictionTimeInSecondsFromNow, SteamVR supplied
            //   derived   = nowNs + predSec*1e9
            //   d         = matched - derived  (constant clock offset plus noise)
            //
            //   dResid    = d minus its own low-pass average, i.e. the noise in d
            //
            // If dResid tracks predSec variation, SteamVR is varying the horizon and the
            // field cannot be used as-is. If dResid is small while predSec varies, the two
            // cancel (nowNs moves opposite) and derived is actually stable in the client
            // clock domain, which would make the direct replacement viable.
            if (DiagEnabled())
            {
                static std::atomic<uint64_t> pn(0);
                static std::atomic<uint64_t> sumAbsResid(0), maxAbsResid(0);
                static std::atomic<int64_t> dLp(0);
                static std::atomic<uint64_t> lpInit(0);
                static std::atomic<uint64_t> sumAbsPredDelta(0), maxAbsPredDelta(0);
                static std::atomic<uint64_t> sumAbsNowDelta(0), maxAbsNowDelta(0);
                static std::atomic<uint64_t> prevNow(0);
                static std::atomic<int64_t> prevPredNs(0);
                static std::atomic<uint64_t> sumAbsDerivedDelta(0), maxAbsDerivedDelta(0);
                static std::atomic<uint64_t> prevDerivedAbs(0);

                // Use the sample taken at function entry, not a fresh one here.
                const uint64_t nowNs = g_entryNowNs;
                const double predSec = perEye[0].flHmdPosePredictionTimeInSecondsFromNow;
                const int64_t predNs = (int64_t)(predSec * 1e9);
                const int64_t derivedAbs = (int64_t)nowNs + predNs;
                const uint64_t matched = pose->targetTimestampNs;
                const int64_t d = (int64_t)matched - derivedAbs;

                // Low-pass the offset so its slow component is not counted as noise.
                int64_t lp = dLp.load();
                if (lpInit.load() == 0) {
                    lp = d;
                    dLp.store(d);
                    lpInit.store(1);
                } else {
                    lp = lp + (d - lp) / 64;
                    dLp.store(lp);
                }
                const int64_t resid = d - lp;
                const uint64_t ar = (uint64_t)(resid < 0 ? -resid : resid);
                sumAbsResid.fetch_add(ar);
                {
                    uint64_t m = maxAbsResid.load();
                    while (ar > m && !maxAbsResid.compare_exchange_weak(m, ar)) { }
                }

                // Per-frame changes of each ingredient, so their contributions can be
                // compared directly.
                const uint64_t pnow = prevNow.exchange(nowNs);
                if (pnow != 0 && nowNs > pnow) {
                    const uint64_t dn = nowNs - pnow;
                    sumAbsNowDelta.fetch_add(dn);
                    uint64_t m = maxAbsNowDelta.load();
                    while (dn > m && !maxAbsNowDelta.compare_exchange_weak(m, dn)) { }
                }
                const int64_t ppred = prevPredNs.exchange(predNs);
                if (ppred != 0) {
                    const int64_t dp = predNs - ppred;
                    const uint64_t adp = (uint64_t)(dp < 0 ? -dp : dp);
                    sumAbsPredDelta.fetch_add(adp);
                    uint64_t m = maxAbsPredDelta.load();
                    while (adp > m && !maxAbsPredDelta.compare_exchange_weak(m, adp)) { }
                }
                const uint64_t pdv = prevDerivedAbs.exchange((uint64_t)derivedAbs);
                if (pdv != 0 && (uint64_t)derivedAbs > pdv) {
                    const uint64_t dd = (uint64_t)derivedAbs - pdv;
                    sumAbsDerivedDelta.fetch_add(dd);
                    uint64_t m = maxAbsDerivedDelta.load();
                    while (dd > m && !maxAbsDerivedDelta.compare_exchange_weak(m, dd)) { }
                }

                const uint64_t k = pn.fetch_add(1) + 1;
                if (DiagEnabled() && (k % 180 == 0)) {
                    const double c = (double)k;
                    Warn(
                        "FRAMENOISE n=%llu | resid_avg=%.3fms resid_max=%.3fms | "
                        "nowDelta_avg=%.3fms max=%.3fms | predDelta_avg=%.3fms max=%.3fms | "
                        "derivedDelta_avg=%.3fms max=%.3fms\n",
                        (unsigned long long)k,
                        (double)sumAbsResid.load() / c / 1e6,
                        (double)maxAbsResid.load() / 1e6,
                        (double)sumAbsNowDelta.load() / c / 1e6,
                        (double)maxAbsNowDelta.load() / 1e6,
                        (double)sumAbsPredDelta.load() / c / 1e6,
                        (double)maxAbsPredDelta.load() / 1e6,
                        (double)sumAbsDerivedDelta.load() / c / 1e6,
                        (double)maxAbsDerivedDelta.load() / 1e6
                    );
                }
            }

            m_targetTimestampNs = pose->targetTimestampNs;

            // Now that the key exists, flush the SubmitLayer entry instant.
            if (DiagEnabled() && g_traceSubmitEntryUs != 0) {
                RecordSubmitEntry(m_targetTimestampNs, g_traceSubmitEntryUs);
                RecordSubmitCpu(m_targetTimestampNs, TraceThreadCpu100ns());
            }

            // TEMPORARY DIAGNOSTIC (pose-selection vs encoder). ENCINPUT showed 6.46% of
            // encoder inputs spaced more than 1.25 frame periods apart, matching the client
            // side almost exactly. That number is measured AFTER this assignment, so it
            // cannot distinguish "GetBestPoseMatch selected a non-consecutive history frame"
            // from "the encoder path perturbed the sequence". This counter measures the
            // same spacing right here, before anything else touches the value:
            //   if this is ~6.46% too, the selection is the source and the encoder is innocent;
            //   if this is ~0%, something downstream reorders the frames.
            if (DiagEnabled())
            {
                static std::atomic<uint64_t> n(0), gap1(0), gap2(0), gap3(0);
                static std::atomic<uint64_t> prevSel(0), maxGapNs(0), backwards(0);
                const uint64_t periodNs = 11111111ull; // 90 Hz nominal
                const uint64_t cur = pose->targetTimestampNs;
                const uint64_t prev = prevSel.exchange(cur);
                if (prev != 0) {
                    if (cur > prev) {
                        const uint64_t gap = cur - prev;
                        uint64_t pm = maxGapNs.load();
                        while (gap > pm && !maxGapNs.compare_exchange_weak(pm, gap)) { }
                        if (gap > periodNs + periodNs / 4) {
                            if (gap < 2 * periodNs) {
                                gap1.fetch_add(1);
                            } else if (gap < 3 * periodNs) {
                                gap2.fetch_add(1);
                            } else {
                                gap3.fetch_add(1);
                            }
                        }
                    } else if (cur < prev) {
                        backwards.fetch_add(1);
                    }
                }
                const uint64_t i = n.fetch_add(1) + 1;

                // Raw value trace. Three independent counters disagree: the buffer receives
                // perfectly spaced timestamps (TRACKARRIVE gap=0), nearly every match takes
                // the newest entry (MATCHIDX=0 at 99.9%), yet the SELECTED sequence still
                // shows ~5% gaps. Those cannot all be true unless the measured quantity is
                // not what it appears to be, so print the actual numbers and the actual
                // delta instead of another derived statistic.
                if (DiagEnabled() && (i <= 40 || i % 90 == 0)) {
                    Warn(
                        "POSERAW i=%llu cur=%llu prev=%llu delta_us=%lld\n",
                        (unsigned long long)i,
                        (unsigned long long)cur,
                        (unsigned long long)prev,
                        (long long)((int64_t)(cur - prev)) / 1000
                    );
                }

                if (i % 180 == 0) {
                    Warn(
                        "POSESEL n=%llu gap>1.25f=%llu gap>=2f=%llu gap>=3f=%llu backwards=%llu gap_max=%.2fms (period=11.11ms)\n",
                        (unsigned long long)i,
                        (unsigned long long)gap1.load(),
                        (unsigned long long)gap2.load(),
                        (unsigned long long)gap3.load(),
                        (unsigned long long)backwards.load(),
                        (double)maxGapNs.load() / 1e6
                    );
                }
            }

            // OBSERVE-ONLY PROBE (does OpenVR already tell us the frame time?).
            //
            // SubmitLayerPerEye_t carries flHmdPosePredictionTimeInSecondsFromNow, which
            // OpenVR documents as "Time in seconds from now that mHmdPose was predicted
            // to" (openvr_driver.h:3123). If that field is populated, the frame timestamp
            // can be derived directly instead of being reverse-looked-up in the pose
            // buffer. That lookup is the suspected source of the 5.2% POSESEL gaps: it
            // scores candidates by 3x3 rotation only, so during pure translation every
            // candidate ties and the winner is decided by floating point noise.
            //
            // This block changes NOTHING. It records, per frame:
            //   derived = now_ns + flHmdPosePredictionTimeInSecondsFromNow
            //   matched = pose->targetTimestampNs  (what the code uses today)
            //   d       = matched - derived, in us
            // If d is a stable constant, the field is usable and the replacement is a
            // one-line change. If d is noisy or the field is always 0, it is not usable
            // and this direction is dead. The gap histograms below measure whether the
            // DERIVED sequence is smoother than the MATCHED sequence, which is the whole
            // question: replacing the lookup only helps if derived has fewer gaps.
            {
                static std::atomic<uint64_t> pn(0);
                static std::atomic<uint64_t> dg1(0), dg2(0), dg3(0);
                static std::atomic<uint64_t> mg1(0), mg2(0), mg3(0);
                static std::atomic<uint64_t> zeroPred(0);
                static std::atomic<uint64_t> sumD(0), maxDAbs(0), nD(0);
                static std::atomic<uint64_t> prevDerived(0), prevMatched(0);
                static std::atomic<uint64_t> sumAbsDDerived(0), sumAbsDMatched(0);
                static std::atomic<uint64_t> maxAbsDDerived(0), maxAbsDMatched(0);
                static std::atomic<uint64_t> nDeltas(0);

                LARGE_INTEGER freq, cnt;
                QueryPerformanceFrequency(&freq);
                QueryPerformanceCounter(&cnt);
                const uint64_t nowNs
                    = (uint64_t)((double)cnt.QuadPart * 1e9 / (double)freq.QuadPart);

                const double predSec = perEye[0].flHmdPosePredictionTimeInSecondsFromNow;
                const int64_t derivedSigned
                    = (int64_t)nowNs + (int64_t)(predSec * 1e9);
                const uint64_t derived = (uint64_t)(derivedSigned > 0 ? derivedSigned : 0);
                const uint64_t matched = pose->targetTimestampNs;


                if (predSec == 0.0f) {
                    zeroPred.fetch_add(1);
                }

                const int64_t d = (int64_t)matched - (int64_t)derived;
                sumD.fetch_add((uint64_t)(d < 0 ? -d : d));
                nD.fetch_add(1);
                const uint64_t dAbs = (uint64_t)(d < 0 ? -d : d);
                uint64_t pm = maxDAbs.load();
                while (dAbs > pm && !maxDAbs.compare_exchange_weak(pm, dAbs)) { }

                // CLOCK DOMAIN: `matched` is the client OpenXR monotonic timestamp forwarded
                // through poll_timestamp (boot-relative), while `derived` is built from the
                // PC QPC. Different clocks, so their raw difference is a constant offset, not
                // an error. What matters is whether the offset is STABLE: if it is, one
                // calibration constant converts `derived` into the client clock domain.
                if (DiagEnabled())
                {
                    static std::atomic<int64_t> firstD(0);
                    static std::atomic<uint64_t> firstSet(0);
                    if (firstSet.load() == 0) {
                        firstD.store(d);
                        firstSet.store(1);
                    }
                    static std::atomic<int64_t> maxDrift(0);
                    const int64_t drift = d - firstD.load();
                    const uint64_t ad = (uint64_t)(drift < 0 ? -drift : drift);
                    int64_t md = maxDrift.load();
                    while ((int64_t)ad > md && !maxDrift.compare_exchange_weak(md, (int64_t)ad)) { }
                    static std::atomic<int64_t> curD(0);
                    curD.store(d);
                    static std::atomic<uint64_t> clkPrinted(0);
                    if (clkPrinted.fetch_add(1) % 180 == 0) {
                        Warn(
                            "FRAMECLK offset_us=%lld drift_max_us=%lld\n",
                            (long long)curD.load(),
                            (long long)maxDrift.load()
                        );
                    }
                }

                // Gap histograms for both candidate sequences, so their smoothness can be
                // compared on identical code paths.
                {
                    const uint64_t periodNs = 11111111ull;
                    const uint64_t pd = prevDerived.exchange(derived);
                    if (pd != 0 && derived > pd) {
                        const uint64_t g = derived - pd;
                        if (g > periodNs + periodNs / 4) {
                            if (g < 2 * periodNs) {
                                dg1.fetch_add(1);
                            } else if (g < 3 * periodNs) {
                                dg2.fetch_add(1);
                            } else {
                                dg3.fetch_add(1);
                            }
                        }
                        sumAbsDDerived.fetch_add(g);
                        uint64_t m = maxAbsDDerived.load();
                        while (g > m && !maxAbsDDerived.compare_exchange_weak(m, g)) { }
                        nDeltas.fetch_add(1);
                    }
                    const uint64_t pmv = prevMatched.exchange(matched);
                    if (pmv != 0 && matched > pmv) {
                        const uint64_t g = matched - pmv;
                        if (g > periodNs + periodNs / 4) {
                            if (g < 2 * periodNs) {
                                mg1.fetch_add(1);
                            } else if (g < 3 * periodNs) {
                                mg2.fetch_add(1);
                            } else {
                                mg3.fetch_add(1);
                            }
                        }
                        sumAbsDMatched.fetch_add(g);
                        uint64_t m = maxAbsDMatched.load();
                        while (g > m && !maxAbsDMatched.compare_exchange_weak(m, g)) { }
                    }
                }

                const uint64_t k = pn.fetch_add(1) + 1;
                if (DiagEnabled() && (k % 180 == 0)) {
                    const double c = (double)k;
                    const double nd = (double)nDeltas.load();
                    Warn(
                        "FRAMETIME n=%llu predZero=%llu | predSec=%.6f | d_avg=%.3fms d_max=%.3fms | "
                        "DERIVED gap>1.25f=%llu/%llu/%llu avg=%.3fms max=%.3fms | "
                        "MATCHED gap>1.25f=%llu/%llu/%llu avg=%.3fms max=%.3fms\n",
                        (unsigned long long)k,
                        (unsigned long long)zeroPred.load(),
                        predSec,
                        (double)sumD.load() / c / 1e6,
                        (double)maxDAbs.load() / 1e6,
                        (unsigned long long)dg1.load(),
                        (unsigned long long)dg2.load(),
                        (unsigned long long)dg3.load(),
                        nd > 0 ? (double)sumAbsDDerived.load() / nd / 1e6 : 0.0,
                        (double)maxAbsDDerived.load() / 1e6,
                        (unsigned long long)mg1.load(),
                        (unsigned long long)mg2.load(),
                        (unsigned long long)mg3.load(),
                        nd > 0 ? (double)sumAbsDMatched.load() / nd / 1e6 : 0.0,
                        (double)maxAbsDMatched.load() / 1e6
                    );
                }
            }

            m_prevFramePoseRotation = m_framePoseRotation;
            m_framePoseRotation.x = pose->motion.pose.orientation.x;
            m_framePoseRotation.y = pose->motion.pose.orientation.y;
            m_framePoseRotation.z = pose->motion.pose.orientation.z;
            m_framePoseRotation.w = pose->motion.pose.orientation.w;

            // DECISIVE MEASUREMENT (pose pairing error, in millimetres).
            //
            // PoseHistory already computes the distance between the translation SteamVR
            // submitted and the translation of the buffer entry we matched to it, but
            // nothing ever printed it - the getters existed and had zero call sites. That
            // left every earlier conclusion about "stale pose" resting on a timestamp age
            // instead of an actual position error.
            //
            // This prints the real thing. Reading it:
            //   posDelta ~= speed * matchedAge   -> the matched entry really is old, and the
            //                                       image carries a wrong head position
            //   posDelta ~= 0 with rawSubmit ~= 0 -> SteamVR is submitting a pose whose
            //                                       translation column is zero, so there is
            //                                       nothing to compare (different bug)
            //   posDelta large and unrelated to age -> matcher is picking wrong entries
            //
            // rawSubmit and rawMatched are printed alongside so a zero delta cannot be
            // confused with "both sides are zero".
            if (DiagEnabled())
            {
                static std::atomic<uint64_t> n(0), sumUs(0), maxUs(0), over1mm(0), over2mm(0),
                    sumAgeUs(0), maxAgeUs(0);
                static std::atomic<uint64_t> hist[6] = {};
                const float deltaMm = m_poseHistory->MatchPosDeltaMm();
                const float ageMs = m_poseHistory->AgeOfMatchMs();
                const float* rawSubmit = m_poseHistory->LastSubmitPos();
                const float* rawMatched = m_poseHistory->LastMatchedPos();

                const uint64_t deltaUm = (uint64_t)(deltaMm * 1000.0f);
                const uint64_t ageUs = (uint64_t)(ageMs * 1000.0f);

                const uint64_t i = n.fetch_add(1) + 1;
                sumUs.fetch_add(deltaUm);
                uint64_t pm = maxUs.load();
                while (deltaUm > pm && !maxUs.compare_exchange_weak(pm, deltaUm)) { }
                sumAgeUs.fetch_add(ageUs);
                pm = maxAgeUs.load();
                while (ageUs > pm && !maxAgeUs.compare_exchange_weak(pm, ageUs)) { }
                if (deltaMm > 1.0f) {
                    over1mm.fetch_add(1);
                }
                if (deltaMm > 2.0f) {
                    over2mm.fetch_add(1);
                }
                const size_t hb = deltaMm < 0.05f ? 0
                    : deltaMm < 0.2f            ? 1
                    : deltaMm < 0.5f            ? 2
                    : deltaMm < 1.0f            ? 3
                    : deltaMm < 2.0f            ? 4
                                                : 5;
                hist[hb].fetch_add(1);

                if (DiagEnabled() && (i % 180 == 0)) {
                    const double c = (double)i;
                    Warn(
                        "POSEDELTA n=%llu delta_avg=%.3fmm delta_max=%.3fmm | over1mm=%llu "
                        "(%.2f%%) over2mm=%llu | age_avg=%.3fms age_max=%.3fms | "
                        "hist(<0.05,<0.2,<0.5,<1,<2,2+mm)=%llu/%llu/%llu/%llu/%llu/%llu | "
                        "rawSubmit=(%.4f,%.4f,%.4f) rawMatched=(%.4f,%.4f,%.4f)\n",
                        (unsigned long long)i,
                        (double)sumUs.load() / c / 1000.0,
                        (double)maxUs.load() / 1000.0,
                        (unsigned long long)over1mm.load(),
                        (double)over1mm.load() * 100.0 / c,
                        (unsigned long long)over2mm.load(),
                        (double)sumAgeUs.load() / c / 1000.0,
                        (double)maxAgeUs.load() / 1000.0,
                        (unsigned long long)hist[0].load(),
                        (unsigned long long)hist[1].load(),
                        (unsigned long long)hist[2].load(),
                        (unsigned long long)hist[3].load(),
                        (unsigned long long)hist[4].load(),
                        (unsigned long long)hist[5].load(),
                        (double)rawSubmit[0],
                        (double)rawSubmit[1],
                        (double)rawSubmit[2],
                        (double)rawMatched[0],
                        (double)rawMatched[1],
                        (double)rawMatched[2]
                    );
                }
            }

        } else {
            m_targetTimestampNs = 0;
            m_framePoseRotation = HmdQuaternion_Init(0.0, 0.0, 0.0, 0.0);
        }
    }
    if (m_submitLayer < MAX_LAYERS) {
        m_submitLayers[m_submitLayer][0] = perEye[0];
        m_submitLayers[m_submitLayer][1] = perEye[1];
        m_submitLayer++;
    } else {
        Warn("Too many layers submitted!");
    }

    // CopyTexture();

    m_presentMutex.unlock();

    g_sum_submit_us.fetch_add(GetTimestampUs() - t_submit0);
    if (stallDiag) {
        g_stallProbe.pts = m_targetTimestampNs;
        g_stallProbe.submitEnd = StallNowUs();
    }
}

/** Submits queued layers for display. */
void OvrDirectModeComponent::Present(vr::SharedTextureHandle_t syncTexture) {
    Debug("OvrDirectModeComponent::Present");

    const bool stallDiag = DiagEnabled();
    const uint64_t stallPresentStart = stallDiag ? StallNowUs() : 0;
    // Every normal and early-return path records an exit; thread-local state avoids
// introducing cross-thread contention.
    struct PresentProbeExit {
        bool enabled;
        ~PresentProbeExit() { if (enabled) g_stallProbe.presentEnd = StallNowUs(); }
    } stallExit { stallDiag };
    if (stallDiag) {
        g_stallProbe.presentStart = stallPresentStart;
        g_stallProbe.presentTid = GetCurrentThreadId();
    }

    // TEMPORARY DIAGNOSTIC (full Present cost account).
    //
    // PRESENTINT shows the server producing ~88.8 fps instead of 90, and DECINPUT confirms
    // the client receives NALs at exactly that rate - so the shortfall is created here.
    // The pacing grid (statistics.rs duration_until_next_vsync) advances in fixed 11.111 ms
    // steps and SKIPS a step whenever a frame overruns, which permanently loses that frame.
    // To know whether overruns are the cause we need the cost of each stage:
    //
    //   acq    - AcquireSync(0,10): waiting for the compositor to release the shared texture
    //   copy   - CopyTexture(): WaitForEncode + GPU copy to staging
    //   enc    - the WaitForEncode portion inside CopyTexture (subset of copy)
    //   total  - the whole Present() call
    //
    // A total that sits just under 11.111 ms explains the shortfall: there is no margin, so
    // any scheduling hiccup overruns and the grid drops a step.
    const uint64_t t_present0 = GetTimestampUs();

    // Frame-trace boundary: Present entered.
    if (DiagEnabled()) {
        g_tracePresentEntryUs = t_present0;
    }

    m_presentMutex.lock();
    if (stallDiag) g_stallProbe.presentLock = StallNowUs() - stallPresentStart;

    // TEMPORARY DIAGNOSTIC (per-frame alignment with the client).
    //
    // m_targetTimestampNs is only valid HERE - inside SubmitLayer it is still the previous
    // frame's value (a first attempt printed ts=0 for every frame because of that).
    // This value is what travels to the client as the video PTS, and it is exactly what
    // the client's REPEATGAP counter measures. Printing it for every frame whose SteamVR
    // cycle overran one period lets the two logs be lined up frame by frame:
    //
    //   server OVERRUN ts=T  ->  client should show gap>=2f around T if a pacing tick was
    //                            lost and the client repeated a frame.
    //
    // If the client shows no gap near those timestamps, the overruns are absorbed and the
    // judder originates downstream (client display queue or OpenXR runtime).
    if (DiagEnabled())
    {
        const uint64_t cyc = g_last_cycle_us.load();
        const uint64_t ts = m_targetTimestampNs;
        // Only report cycles beyond 11.5 ms. Those are the tail the client hypothesis is
        // about; 11.11-11.5 ms is ordinary rounding and would flood the log.
        // ts == 0 is the startup window before GetBestPoseMatch has anything to return.
        if (ts != 0 && cyc > 11500) {
            static std::atomic<uint64_t> overN(0);
            const uint64_t on = overN.fetch_add(1) + 1;
            // No hard cap any more: a cap made an earlier run look like "no overruns" when
            // it had simply exhausted its budget during startup. Rate-limit instead so a
            // long session still produces analysable output.
            if (on % 1 == 0 && on <= 5000) {
                Warn(
                    "OVERRUN i=%llu ts=%llu cycle_us=%llu\n",
                    (unsigned long long)on,
                    (unsigned long long)ts,
                    (unsigned long long)cyc
                );
            }
        }
    }

    ReportPresent(m_targetTimestampNs, 0);

    bool useMutex = true;

    IDXGIKeyedMutex* pKeyedMutex = NULL;

    uint32_t layerCount = m_submitLayer;
    m_submitLayer = 0;

    if (m_prevTargetTimestampNs == m_targetTimestampNs) {
        Debug("Discard duplicated frame. FrameIndex=%llu (Ignoring)", m_targetTimestampNs);
        // return;
    }

    ID3D11Texture2D* pSyncTexture = m_pD3DRender->GetSharedTexture((HANDLE)syncTexture);
    if (!pSyncTexture) {
        Warn("[VDispDvr] SyncTexture is NULL!");
        if (DiagEnabled()) {
            RecordPresentAbort(m_targetTimestampNs, 1, 0);
        }
        m_presentMutex.unlock();
        return;
    }

    uint64_t acq_us = 0;
    if (useMutex) {
        // Access to shared texture must be wrapped in AcquireSync/ReleaseSync
        // to ensure the compositor has finished rendering to it before it gets used.
        // This enforces scheduling of work on the gpu between processes.
        if (SUCCEEDED(
                pSyncTexture->QueryInterface(__uuidof(IDXGIKeyedMutex), (void**)&pKeyedMutex)
            )) {
            // TODO: Reasonable timeout and timeout handling
            const uint64_t t_acq0 = GetTimestampUs();
            HRESULT hr = pKeyedMutex->AcquireSync(0, 10);
            acq_us = GetTimestampUs() - t_acq0;
            if (hr != S_OK) {
                Debug(
                    "[VDispDvr] ACQUIRESYNC FAILED!!! hr=%d %p %ls", hr, hr, GetErrorStr(hr).c_str()
                );
                // This frame never reaches NewFrameReady. Record it: the per-frame dump used
                // to skip entries without present_exit/ready, so these failures were counted
                // as "not a frame" and the abort rate looked like zero.
                if (DiagEnabled()) {
                    RecordPresentAbort(m_targetTimestampNs, 2, (int)hr);
                }
                pKeyedMutex->Release();
                m_presentMutex.unlock();
                return;
            }
        }
    }

    const uint64_t t_copy0 = GetTimestampUs();
    CopyTexture(layerCount);
    const uint64_t copy_us = GetTimestampUs() - t_copy0;
    if (stallDiag) {
        g_stallProbe.acquire = acq_us;
        g_stallProbe.copy = copy_us;
    }

    if (useMutex) {
        if (pKeyedMutex) {
            pKeyedMutex->ReleaseSync(0);
            pKeyedMutex->Release();
        }
    }

    ReportComposed(m_targetTimestampNs, 0);

    if (m_pEncoder) {
        // Frame-trace boundary: the submitting thread has published this frame. The gap to
        // Transmit entry is how long the encoder thread took to get scheduled.
        // Frame-trace boundaries, in the order they actually happen:
        //   present entry  (captured at Present() start, flushed here because the PTS is
        //                   only known after SubmitLayer)
        //   present exit   (before handing the frame to the encoder thread)
        //   frame ready    (NewFrameReady, the moment the encoder thread is signalled)
        //
        // Order matters: an earlier version recorded `ready` before `present exit`, which
        // made to_ready evaluate to zero for every frame.
        if (DiagEnabled()) {
            RecordPresentEntry(m_targetTimestampNs, g_tracePresentEntryUs);
            g_tracePresentExitUs = GetTimestampUs();
            RecordPresentExit(m_targetTimestampNs);
        }
        m_pEncoder->NewFrameReady();
        if (DiagEnabled()) {
            RecordFrameReady(m_targetTimestampNs);
        }
    }

    m_presentMutex.unlock();

    g_sum_present_us.fetch_add(GetTimestampUs() - t_present0);

    // TEMPORARY DIAGNOSTIC: stage costs inside Present(). Budget is 11.111 ms.
    if (DiagEnabled())
    {
        static std::atomic<uint64_t> n(0), sumTotal(0), maxTotal(0);
        static std::atomic<uint64_t> sumAcq(0), maxAcq(0);
        static std::atomic<uint64_t> sumCopy(0), maxCopy(0);
        static std::atomic<uint64_t> over11(0), over10(0);
        static std::atomic<uint64_t> hist[6] = {};

        const uint64_t total_us = GetTimestampUs() - t_present0;
        const uint64_t i = n.fetch_add(1) + 1;
        sumTotal.fetch_add(total_us);
        sumAcq.fetch_add(acq_us);
        sumCopy.fetch_add(copy_us);
        if (total_us > 11111) {
            over11.fetch_add(1);
        }
        if (total_us > 10000) {
            over10.fetch_add(1);
        }
        uint64_t pm = maxTotal.load();
        while (total_us > pm && !maxTotal.compare_exchange_weak(pm, total_us)) { }
        pm = maxAcq.load();
        while (acq_us > pm && !maxAcq.compare_exchange_weak(pm, acq_us)) { }
        pm = maxCopy.load();
        while (copy_us > pm && !maxCopy.compare_exchange_weak(pm, copy_us)) { }
        const size_t hb = total_us < 2000 ? 0
            : total_us < 5000 ? 1
            : total_us < 8000 ? 2
            : total_us < 11111 ? 3
            : total_us < 15000 ? 4
                               : 5;
        hist[hb].fetch_add(1);

        if (DiagEnabled() && (i % 180 == 0)) {
            Warn(
                "PRESENTCOST n=%llu total_avg=%.2fms total_max=%.2fms | acq_avg=%.2fms acq_max=%.2fms | copy_avg=%.2fms copy_max=%.2fms | over10ms=%llu (%.2f%%) over11.1ms=%llu (%.2f%%) | hist(<2,<5,<8,<11.1,<15,15+ms)=%llu/%llu/%llu/%llu/%llu/%llu\n",
                (unsigned long long)i,
                (double)sumTotal.load() / (double)i / 1000.0,
                (double)maxTotal.load() / 1000.0,
                (double)sumAcq.load() / (double)i / 1000.0,
                (double)maxAcq.load() / 1000.0,
                (double)sumCopy.load() / (double)i / 1000.0,
                (double)maxCopy.load() / 1000.0,
                (unsigned long long)over10.load(),
                (double)over10.load() * 100.0 / (double)i,
                (unsigned long long)over11.load(),
                (double)over11.load() * 100.0 / (double)i,
                (unsigned long long)hist[0].load(),
                (unsigned long long)hist[1].load(),
                (unsigned long long)hist[2].load(),
                (unsigned long long)hist[3].load(),
                (unsigned long long)hist[4].load(),
                (unsigned long long)hist[5].load()
            );
        }
    }
}

void OvrDirectModeComponent::PostPresent(const vr::IVRDriverDirectModeComponent::Throttling_t*) {
    Debug("OvrDirectModeComponent::PostPresent");

    const bool stallDiag = DiagEnabled();
    if (stallDiag) {
        g_stallProbe.postStart = StallNowUs();
        g_stallProbe.postTid = GetCurrentThreadId();
    }

    const uint64_t t_post0 = GetTimestampUs();
    if (DiagEnabled()) {
        RecordPostEntry(m_targetTimestampNs);
    }
    WaitForVSync();
    g_sum_post_us.fetch_add(GetTimestampUs() - t_post0);
    g_post_return_us.store(GetTimestampUs());
    if (DiagEnabled()) {
        RecordPostExit(m_targetTimestampNs, TraceThreadCpu100ns());
    }
    if (stallDiag) g_stallProbe.postEnd = StallNowUs();
}

void OvrDirectModeComponent::CopyTexture(uint32_t layerCount) {

    uint64_t presentationTime = GetTimestampUs();

    ID3D11Texture2D* pTexture[MAX_LAYERS][2];
    ComPtr<ID3D11Texture2D> Texture[MAX_LAYERS][2];
    vr::VRTextureBounds_t bounds[MAX_LAYERS][2];
    vr::HmdMatrix34_t poses[MAX_LAYERS];

    for (uint32_t i = 0; i < layerCount; i++) {
        // Find left eye texture.
        HANDLE leftEyeTexture = (HANDLE)m_submitLayers[i][0].hTexture;
        auto it = m_handleMap.find(leftEyeTexture);
        if (it == m_handleMap.end()) {
            // Ignore this layer.
            Debug(
                "Submitted texture is not found on HandleMap. eye=right layer=%d/%d Texture "
                "Handle=%p",
                i,
                layerCount,
                leftEyeTexture
            );
        } else {
            Texture[i][0] = it->second.first->textures[it->second.second];
            D3D11_TEXTURE2D_DESC desc;
            Texture[i][0]->GetDesc(&desc);

            // Find right eye texture.
            HANDLE rightEyeTexture = (HANDLE)m_submitLayers[i][1].hTexture;
            it = m_handleMap.find(rightEyeTexture);
            if (it == m_handleMap.end()) {
                // Ignore this layer
                Debug(
                    "Submitted texture is not found on HandleMap. eye=left layer=%d/%d Texture "
                    "Handle=%p",
                    i,
                    layerCount,
                    rightEyeTexture
                );
                Texture[i][0].Reset();
            } else {
                Texture[i][1] = it->second.first->textures[it->second.second];
            }
        }

        pTexture[i][0] = Texture[i][0].Get();
        pTexture[i][1] = Texture[i][1].Get();
        bounds[i][0] = m_submitLayers[i][0].bounds;
        bounds[i][1] = m_submitLayers[i][1].bounds;
        poses[i] = m_submitLayers[i][0].mHmdPose;
    }

    // This can go away, but is useful to see it as a separate packet on the gpu in traces.
    m_pD3DRender->GetContext()->Flush();

    if (m_pEncoder) {
        // Wait for the encoder to be ready.  This is important because the encoder thread
        // blocks on transmit which uses our shared d3d context (which is not thread safe).
        // One atomic read decides everything below, so with diagnostics off this costs a
        // single relaxed load and nothing else.
        const bool diagOn = DiagEnabled();
        const uint64_t stallEncodeStart = diagOn ? StallNowUs() : 0;
        m_pEncoder->WaitForEncode();
        if (stallEncodeStart) {
            const uint64_t waited = StallNowUs() - stallEncodeStart;
            g_stallProbe.encodeWait = waited;

            // Per-event record for the long waits only.
            //
            // The aggregate STALLCYCLE line proves the tail exists (p99 ~71 ms, max ~256 ms)
            // but cannot say what the encoder thread was doing during it. An ETW/WPR trace
            // can, but only if there is an exact timestamp to line it up with. Printing just
            // these few events keeps the log quiet: measured 4 waits above 100 ms in 689
            // frames, so this fires a handful of times per session.
            if (waited > 30000) {
                static std::atomic<uint64_t> longN(0);
                const uint64_t n = longN.fetch_add(1) + 1;
                Warn(
                    "ENCWAIT_LONG n=%llu start_us=%llu waited_us=%llu pts=%llu\n",
                    (unsigned long long)n,
                    (unsigned long long)stallEncodeStart,
                    (unsigned long long)waited,
                    (unsigned long long)m_targetTimestampNs
                );
            }
        }

        std::string debugText;

        uint64_t submitFrameIndex = m_targetTimestampNs;

        // Copy entire texture to staging so we can read the pixels to send to remote device.
        m_pEncoder->CopyToStaging(
            pTexture,
            bounds,
            poses,
            layerCount,
            false,
            presentationTime,
            submitFrameIndex,
            "",
            debugText
        );

        m_pD3DRender->GetContext()->Flush();
    }
}
