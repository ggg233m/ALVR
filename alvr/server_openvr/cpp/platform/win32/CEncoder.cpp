#include "CEncoder.h"

#include "alvr_server/Settings.h"

CEncoder::CEncoder()
    : m_bExiting(false)
    , m_targetTimestampNs(0) {
    m_encodeFinished.Set();
}

CEncoder::~CEncoder() {
    if (m_videoEncoder) {
        m_videoEncoder->Shutdown();
        m_videoEncoder.reset();
    }
}

void CEncoder::Initialize(std::shared_ptr<CD3DRender> d3dRender) {
    m_FrameRender = std::make_shared<FrameRender>(d3dRender);
    m_FrameRender->Startup();
    uint32_t encoderWidth, encoderHeight;
    m_FrameRender->GetEncodingResolution(&encoderWidth, &encoderHeight);

    Exception vplException;
    Exception vceException;
    Exception nvencException;
#ifdef ALVR_GPL
    Exception swException;

    if (Settings_Instance()->m_forceSwEncoding) {
        try {
            Debug("Try to use VideoEncoderSW.\n");
            m_videoEncoder
                = std::make_shared<VideoEncoderSW>(d3dRender, encoderWidth, encoderHeight);
            m_videoEncoder->Initialize();
            return;
        } catch (Exception e) {
            swException = e;
        }
    }
#endif

    try {
        Debug("Try to use VideoEncoderAMF.\n");
        m_videoEncoder = std::make_shared<VideoEncoderAMF>(d3dRender, encoderWidth, encoderHeight);
        m_videoEncoder->Initialize();
        return;
    } catch (Exception e) {
        vceException = e;
    }
    try {
        Debug("Try to use VideoEncoderNVENC.\n");
        m_videoEncoder
            = std::make_shared<VideoEncoderNVENC>(d3dRender, encoderWidth, encoderHeight);
        m_videoEncoder->Initialize();
        return;
    } catch (Exception e) {
        nvencException = e;
    }
    try {
        Debug("Try to use VideoEncoderVPL.\n");
        m_videoEncoder = std::make_shared<VideoEncoderVPL>(d3dRender, encoderWidth, encoderHeight);
        m_videoEncoder->Initialize();
        return;
    } catch (Exception e) {
        vplException = e;
    }
#ifdef ALVR_GPL
    try {
        Debug("Try to use VideoEncoderSW.\n");
        m_videoEncoder = std::make_shared<VideoEncoderSW>(d3dRender, encoderWidth, encoderHeight);
        m_videoEncoder->Initialize();
        return;
    } catch (Exception e) {
        swException = e;
    }
    throw MakeException(
        "All VideoEncoder are not available. VCE: %s, NVENC: %s, VPL: %s, SW: %s",
        vceException.what(),
        nvencException.what(),
        vplException.what(),
        swException.what()
    );
#else
    throw MakeException(
        "All VideoEncoder are not available. VCE: %s, NVENC: %s, VPL: %s",
        vceException.what(),
        nvencException.what(),
        vplException.what()
    );
#endif
}

void CEncoder::SetViewParams(
    vr::HmdRect2_t projLeft,
    vr::HmdMatrix34_t eyeToHeadLeft,
    vr::HmdRect2_t projRight,
    vr::HmdMatrix34_t eyeToHeadRight
) {
    m_FrameRender->SetViewParams(projLeft, eyeToHeadLeft, projRight, eyeToHeadRight);
}

bool CEncoder::CopyToStaging(
    ID3D11Texture2D* pTexture[][2],
    vr::VRTextureBounds_t bounds[][2],
    vr::HmdMatrix34_t poses[],
    int layerCount,
    bool recentering,
    uint64_t presentationTime,
    uint64_t targetTimestampNs,
    const std::string& message,
    const std::string& debugText
) {
    m_presentationTime = presentationTime;
    m_targetTimestampNs = targetTimestampNs;
    m_FrameRender->Startup();

    m_FrameRender->RenderFrame(
        pTexture, bounds, poses, layerCount, recentering, targetTimestampNs, message, debugText
    );
    return true;
}

void CEncoder::Run() {
    Debug("CEncoder: Start thread. Id=%d\n", GetCurrentThreadId());
    SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_MOST_URGENT);

    while (!m_bExiting) {
        m_newFrameReady.Wait();
        if (m_bExiting)
            break;

        if (m_FrameRender->GetTexture()) {
            m_videoEncoder->Transmit(
                m_FrameRender->GetTexture().Get(),
                m_presentationTime,
                m_targetTimestampNs,
                m_scheduler.CheckIDRInsertion()
            );

            // Frame-trace boundary: Transmit has unwound. Everything from VideoSend to here
            // (callbacks, locks) is the tail the earlier probe did not cover.
            if (DiagEnabled()) {
                RecordTransmitReturn(m_targetTimestampNs);
            }

            // TEMPORARY DIAGNOSTIC (server half of the gap comparison). The client reports
            // that ~9.9% of decoded frame timestamps are further apart than 1.25 frame
            // periods, with a worst case of 16 frame periods. This measures the same
            // quantity on the producing side: the spacing of the target timestamps we
            // hand to the encoder. Comparing the two histograms says whether the gaps are
            // created upstream (encoder input is irregular) or downstream (network and
            // decode). The buckets are counted in units of the 90 Hz frame period so the
            // two sides can be compared directly.
            {
                static std::atomic<uint64_t> n(0), gap1(0), gap2(0), gap3(0);
                static std::atomic<uint64_t> prevTs(0), maxGapNs(0);
                const uint64_t periodNs = 11111111ull; // 90 Hz nominal

                const uint64_t cur = m_targetTimestampNs;
                const uint64_t prev = prevTs.exchange(cur);
                if (prev != 0 && cur > prev) {
                    const uint64_t gap = cur - prev;
                    uint64_t prevMax = maxGapNs.load();
                    while (gap > prevMax && !maxGapNs.compare_exchange_weak(prevMax, gap)) { }
                    if (gap > periodNs + periodNs / 4) {
                        if (gap < 2 * periodNs) {
                            gap1.fetch_add(1);
                        } else if (gap < 3 * periodNs) {
                            gap2.fetch_add(1);
                        } else {
                            gap3.fetch_add(1);
                        }
                    }
                }

                const uint64_t i = n.fetch_add(1) + 1;
                if (DiagEnabled() && i % 180 == 0) {
                    Warn(
                        "ENCINPUT n=%llu gap>1.25f=%llu gap>=2f=%llu gap>=3f=%llu gap_max=%.2fms (period=11.11ms)\n",
                        (unsigned long long)i,
                        (unsigned long long)gap1.load(),
                        (unsigned long long)gap2.load(),
                        (unsigned long long)gap3.load(),
                        (double)maxGapNs.load() / 1e6
                    );
                }
            }
        }

        // Frame-trace boundary: this is what releases the submitting thread's
        // WaitForEncode. It is NOT the VideoSend instant - the statistics block above sits
        // between them, which is why a fast encode and a long WaitForEncode can coexist.
        if (DiagEnabled()) {
            RecordEncodeFinished(m_targetTimestampNs);
        }
        m_encodeFinished.Set();
    }
}

void CEncoder::Stop() {
    m_bExiting = true;
    m_newFrameReady.Set();
    Join();
    m_FrameRender.reset();
}

void CEncoder::NewFrameReady() {
    m_encodeFinished.Reset();
    m_newFrameReady.Set();
}

void CEncoder::WaitForEncode() { m_encodeFinished.Wait(); }

void CEncoder::OnStreamStart() { m_scheduler.OnStreamStart(); }

void CEncoder::InsertIDR() { m_scheduler.InsertIDR(); }

void CEncoder::CaptureFrame() { }
