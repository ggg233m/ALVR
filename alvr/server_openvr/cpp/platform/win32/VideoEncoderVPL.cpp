#include "VideoEncoderVPL.h"
#include "alvr_server/Logger.h"
#include "alvr_server/Utils.h"
#include "alvr_server/Settings.h"
#include "alvr_server/bindings.h"

#include <algorithm>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <vector>

#define VPLVERSION(major, minor) (major << 16 | minor)
#define MAJOR_API_VERSION_REQUIRED 2
#define MINOR_API_VERSION_REQUIRED 9
#define WAIT_100_MILLISECONDS 100
#define ALIGN16(value) (((value + 15) >> 4) << 4)
#define ERROR_THROW(msg, ...)                                                                      \
    {                                                                                              \
        Error("VPL: " msg "\n", __VA_ARGS__);                                                      \
        throw MakeException("VPL: " msg, __VA_ARGS__);                                             \
    }

#define VPL_LOG(fn, msg, ...) fn("VPL: " msg "\n", __VA_ARGS__)
#define VPL_DEBUG(msg, ...) VPL_LOG(Debug, msg, __VA_ARGS__)
#define VPL_WARN(msg, ...) VPL_LOG(Warn, msg, __VA_ARGS__)
#define VPL_INFO(msg, ...) VPL_LOG(Info, msg, __VA_ARGS__)

#define VERIFY(expr, msg)                                                                          \
    if (!(expr))                                                                                   \
    ERROR_THROW("%s. %s", msg, #expr)
// MFX_WRN_* (>0) are warnings: Query/Init may auto-correct parameters and still
// succeed. Only negative status codes are fatal. The old macro threw on +5
// (MFX_WRN_INCOMPATIBLE_VIDEO_PARAM), which is what the "Init failed with 5"
// crash in crash_log.txt actually was.
#define VPL_VERIFY(expr)                                                                           \
    {                                                                                              \
        mfxStatus res = expr;                                                                      \
        if (res < MFX_ERR_NONE)                                                                    \
            ERROR_THROW("\"%s\" failed with %d", #expr, res);                                      \
    }

namespace {
constexpr uint64_t VPL_MAX_TARGET_KBPS = 65535;

// OneVPL scales ALL BRC fields (TargetKbps, MaxKbps, BufferSizeInKB,
// InitialDelayInKB, WinBRCMaxAvgKbps) by BRCParamMultiplier. Effective bitrate
// = TargetKbps * BRCParamMultiplier * 1000. Verified on the real D3D11
// video-memory / BGR4 path: Query=0, Init=0, GetVideoParam preserves all
// fields, peak bitrate hits 100/200/500/1000 Mbps (see BITRATE_RESEARCH.md).
struct VplBrcParams {
    mfxU16 multiplier;
    mfxU16 targetKbps;
    mfxU16 bufferSizeInKB;   // 0 = leave driver default (mult == 1 only)
    mfxU16 initialDelayInKB;
};

VplBrcParams MakeVplBrcParams(uint64_t bitrate_bps, unsigned int vbv_frames, int refresh_rate) {
    const uint64_t kbps = (std::max)(uint64_t(1), (bitrate_bps + 999) / 1000);
    uint64_t mult = (kbps + VPL_MAX_TARGET_KBPS - 1) / VPL_MAX_TARGET_KBPS;
    if (mult < 1) mult = 1;
    if (mult > 65535) mult = 65535;
    const uint64_t tgt = (std::min)(VPL_MAX_TARGET_KBPS, (kbps + mult - 1) / mult);

    uint64_t buf = 0;
    if (vbv_frames > 0) {
        // Size the VBV from the frame budget instead of a fixed byte count:
        //   bytes per frame = bitrate / fps / 8
        // BufferSizeInKB is scaled by BRCParamMultiplier, so the value written
        // here must be the unscaled one.
        //
        // Every other backend already does this (NVENC and AMF use 1.1 frames,
        // FFmpeg/VAAPI the same), so this path only needs the knob to match them.
        const uint64_t fps = uint64_t(refresh_rate > 0 ? refresh_rate : 90);
        const uint64_t frame_bits = bitrate_bps / fps;
        const uint64_t buf_kb = frame_bits * uint64_t(vbv_frames) / 8 / 1000;
        buf = (std::max)(uint64_t(1), buf_kb / mult);
        // BufferSizeInKB is mfxU16; clamp before the cast rather than after.
        buf = (std::min)(buf, uint64_t(65535));
    } else if (mult > 1) {
        // Historical behaviour: a fixed 8 MB buffer, independent of bitrate.
        // Keep BufferSizeInKB * mult * 1000 small enough for the existing
        // bitstream (width*height*8) and small enough that BRC actually spends
        // the budget. 8192 KB / mult was measured to hit the exact target peak
        // rate, but it is ~25 frames at 230 Mbps.
        buf = (std::max)(uint64_t(256), 8192 / mult);
    }

    // InitialDelay is the VBV fill level assumed at the first frame. Half the
    // buffer is the conventional choice and stops the first frames from being
    // starved.
    return { static_cast<mfxU16>(mult), static_cast<mfxU16>(tgt),
             static_cast<mfxU16>(buf), static_cast<mfxU16>(buf / 2) };
}
} // namespace

VideoEncoderVPL::VideoEncoderVPL(std::shared_ptr<CD3DRender> pD3DRender, int width, int height)
    : m_pD3DRender(pD3DRender)
    , m_renderWidth(width)
    , m_renderHeight(height) {
    VPL_DEBUG("constructed");
}

VideoEncoderVPL::~VideoEncoderVPL() { VPL_DEBUG("destructed"); }

void VideoEncoderVPL::Initialize() {
    VPL_DEBUG("initialize");

    ChooseParams();

    // Whether the VPP stage is needed is decided by ChooseParams; record it now
    // so InitTransferTex knows to keep the staging texture in RGBA.
    m_useNv12 = m_vplColorFormat == MFX_FOURCC_NV12 && !Settings_Instance()->m_enableHdr;

    InitTransferTex();
    InitVpl();

    // NV12 conversion is done by a full-screen shader pass, not by the oneVPL
    // VPP stage. Measured on an Arc A770 at 3648x1824: VPP = 2.8 ms/frame,
    // equivalent shader draw = 0.016 ms/frame. Both are synchronous work on the
    // encode thread, so this difference went straight into the frame budget and
    // is what kept the encoder at ~81 fps instead of 90.
    if (m_useNv12 && !InitNv12ShaderConverter()) {
        // Without the conversion the encoder would run 4:4:4 while declaring a
        // 4:2:0-only profile. For AV1 that yields an undecodable stream; better
        // to refuse than to emit something that silently fails.
        ERROR_THROW("encoder requires the NV12 conversion stage, but it failed to init");
    }

    InitVplEncode();

    // Size the output bitstream from the driver FINAL BRC parameters: the
    // required space is BufferSizeInKB * BRCParamMultiplier * 1000 bytes.
    // This also fixes the generic -5 that occurs when the encoded resolution
    // shrinks (FFR) while the driver keeps a large buffer.
    mfxVideoParam actual = {};
    const mfxStatus gvp = MFXVideoENCODE_GetVideoParam(m_vplSession, &actual);
    uint64_t required = uint64_t(m_renderWidth) * m_renderHeight * 8;
    if (gvp >= MFX_ERR_NONE) {
        const uint64_t mult = actual.mfx.BRCParamMultiplier ? actual.mfx.BRCParamMultiplier : 1;
        const uint64_t need = uint64_t(actual.mfx.BufferSizeInKB) * mult * 1000 * 2;
        required = (std::max)(required, need);
    }

    LogEffectiveParams(gvp, actual);
    m_vplBitstream.MaxLength
        = static_cast<mfxU32>((std::min)(required, uint64_t(0xFFFFFFFFull)));
    m_vplBitstream.Data = (mfxU8*)calloc(m_vplBitstream.MaxLength, sizeof(mfxU8));
}

void VideoEncoderVPL::Shutdown() {
    VPL_DEBUG("shutdown");

    ReleaseNv12ShaderResources();

    MFXVideoENCODE_Close(m_vplSession);
    MFXClose(m_vplSession);

    if (m_vplBitstream.Data)
        free(m_vplBitstream.Data);

    if (m_vplLoader)
        MFXUnload(m_vplLoader);
}

void VideoEncoderVPL::Transmit(
    ID3D11Texture2D* pTexture, uint64_t presentationTime, uint64_t targetTimestampNs, bool insertIDR
) {
    // VPL_DEBUG("transmit");

    // Diagnostic timing covers only this encode call; kept separate from the video PTS so
    // clocks from different devices are never mixed.
    const bool traceEncode = DiagEnabled();
    const uint64_t traceEncodeStartUs = traceEncode ? GetTimestampUs() : 0;

    // Frame-trace boundary: the frame has reached the encoder. The spacing of consecutive
    // values here is the submit cadence, before any encode work or callback logging, which
    // is what separates "the pipeline feeds the encoder unevenly" from "the encoder is slow".
    if (traceEncode) {
        RecordEncoderInput(targetTimestampNs);
    }

    auto dynParams = GetDynamicEncoderParams();
    // BRCParamMultiplier is init-only and MFXVideoENCODE_Reset returns -14
    // (MFX_ERR_INCOMPATIBLE_VIDEO_PARAM) for every CBR bitrate change on this
    // driver, even with multiplier=1. The original code ignored the return
    // value, so runtime bitrate updates never worked. Bitrate is fixed at Init
    // from the configured value; do not attempt Reset here.
    (void)dynParams;

    {
        // Diagnostic: confirm Transmit is actually reached and how the encoder
        // responds. AV1 can initialize successfully yet still never emit a
        // bitstream, which leaves the client with an empty frame stream (black
        // screen) and ParseFrameNals never called.
        if (DiagEnabled()) {
            static int transmitCalls = 0;
            transmitCalls++;
            if (transmitCalls <= 5 || transmitCalls % 120 == 0) {
                VPL_WARN("TRANSMIT call #%d (codec=0x%x)", transmitCalls, m_codec);
            }
        }
    }

    // Optional server-side keyframe cadence ("IDR frame interval", default 0 = off).
    //
    // Normally unnecessary: the client requests a keyframe through IDRScheduler when the
    // decoder refuses a frame, or when a packet was lost (UDP only - the TCP receive path
    // never reports loss). Other backends rely on exactly that, with an infinite GOP.
    //
    // It exists for one case: the encoder was given a GOP it does not execute. That was
    // observed with GopPicSize = 0, which this driver rewrites to 248 and then ignores
    // (measured spacing 44 s, then 30 s, instead of the declared 2.8 s). With a value the
    // driver accepts the cadence is exact - GopPicSize = 900 produced eleven consecutive
    // 900-frame gaps at 10.0 s each on AV1 - so the workaround is only needed when
    // gop_frames = 0.
    //
    // Kept AV1-only on purpose: HEVC has always honoured GopPicSize here, and a forced
    // IDR costs 16.2 ms against an 11.11 ms budget, i.e. every forced keyframe drops a
    // frame.
    //
    // Deliberately NOT tied to gop_frames: coupling them meant gop_frames = 0 silently
    // installed a 1000 ms (90-frame) cadence, three times denser than the 300-frame
    // default it was meant to relax.
    const bool needsForcedKeyframes = (m_codec == ALVR_CODEC_AV1);
    if (needsForcedKeyframes) {
        static std::chrono::steady_clock::time_point lastForcedIdr
            = std::chrono::steady_clock::now();
        const auto now = std::chrono::steady_clock::now();
        const auto interval
            = std::chrono::milliseconds(Settings_Instance()->m_idrFrameIntervalMs);
        if (interval.count() > 0 && now - lastForcedIdr >= interval) {
            insertIDR = true;
            lastForcedIdr = now;
        }
    }

    mfxFrameSurface1* encSurface = nullptr;

    if (m_useNv12) {
        // BGR4/4:4:4 -> NV12/4:2:0, because Main-profile HEVC/AV1 are 4:2:0-only
        // and this encoder cannot convert the colour space itself.
        //
        // Both the source copy and the conversion are done by one shader pass
        // writing the Y and UV planes directly into m_nv12Tex, which is the
        // shareable texture the encoder imports. That replaces the previous
        // CopyResource + VPP pair, whose VPP stage alone cost 2.8 ms/frame.
        encSurface = ConvertToNv12Shader(pTexture);
        if (!encSurface) {
            // Conversion failed this round; skip the frame rather than encode
            // a stale surface.
            return;
        }
    } else {
        m_pD3DRender->GetContext()->CopyResource(m_transferTex.Get(), pTexture);
        encSurface = VplImportTexture(m_transferTex.Get());
    }

    // FrameType is a bit-OR of mfxFrameType: an IDR is
    // "I | IDR | REF", never the bare IDR bit. Passing 0x0080 alone was measured
    // here to be accepted without any error while producing a normal inter
    // frame, so the periodic "keyframe" cadence silently did nothing and the
    // stream could only recover at the driver's own GOP boundary (~91 s).
    mfxEncodeCtrl encodeCtrl = {};
    if (insertIDR) {
        encodeCtrl.FrameType = MFX_FRAMETYPE_I | MFX_FRAMETYPE_IDR | MFX_FRAMETYPE_REF;
    }

    mfxStatus sts = MFX_ERR_NONE;
    mfxSyncPoint syncp = {};
    bool isEncGoing = true;
    bool isDraining = false;

    while (isEncGoing) {
        isDraining = encSurface == nullptr;
        sts = MFXVideoENCODE_EncodeFrameAsync(
            m_vplSession, &encodeCtrl, encSurface, &m_vplBitstream, &syncp
        );

        if (encSurface) {
            VPL_VERIFY(encSurface->FrameInterface->Release(encSurface));
            encSurface = nullptr;
        }

        switch (sts) {
        case MFX_ERR_NONE:
            // MFX_ERR_NONE and syncp indicate output is available
            if (syncp) {
                // Encode output is not available on CPU until sync operation completes
                //
                // Timestamped so an ENCWAIT_LONG on the submitting thread can be matched to
                // what this thread was doing. Guarded by DiagEnabled(), which is one atomic
                // load when diagnostics are off.
                const bool diagOn = DiagEnabled();
                const uint64_t syncStartUs = diagOn ? GetTimestampUs() : 0;
                do {
                    sts = MFXVideoCORE_SyncOperation(m_vplSession, syncp, WAIT_100_MILLISECONDS);
                    if (diagOn && (sts == MFX_ERR_NONE || sts == MFX_ERR_MORE_DATA)) {
                        const uint64_t syncUs = GetTimestampUs() - syncStartUs;
                        if (syncUs > 30000) {
                            static std::atomic<uint64_t> syncLongN(0);
                            const uint64_t n = syncLongN.fetch_add(1) + 1;
                            VPL_WARN(
                                "ENCSYNC_LONG n=%llu start_us=%llu sync_us=%llu sts=%d\n",
                                (unsigned long long)n,
                                (unsigned long long)syncStartUs,
                                (unsigned long long)syncUs,
                                int(sts)
                            );
                        }
                    }
                    if (MFX_ERR_NONE == sts) {
                        {
                            static int outFrames = 0;
                            outFrames++;
                            // Random-access flag from the encoder's own frame type, not
                            // from the bitstream. mfxFrameType bits: 0x01 I, 0x02 P, 0x04 B,
                            // 0x40 REF, 0x80 IDR, so 0x42 = P|REF and 0xC1 = I|REF|IDR.
                            //
                            // This replaced a HEVC NAL-header scan that ran for every codec.
                            // AV1 has OBUs, not NAL start codes, so the scan matched 19/20/21
                            // by chance: 157 of 222 frames in a 75 s AV1 session were reported
                            // as keyframes while being P|REF, and the resulting 2.08 "keyframes
                            // per second" sent an investigation after the wrong signal.
                            int realKey = 0;
                            if (DiagEnabled()) {
                                realKey = (m_vplBitstream.FrameType & MFX_FRAMETYPE_IDR) ? 1 : 0;
                            }
                            if (DiagEnabled()
                                && (outFrames <= 5 || outFrames % 120 == 0 || realKey
                                    || insertIDR)) {
                                VPL_WARN(
                                    "OUTPUT frame #%d len=%u forced=%d realkey=%d "
                                    "pts=%llu vpltype=%u encode_us=%llu probe=pts_v1",
                                    outFrames,
                                    unsigned(m_vplBitstream.DataLength),
                                    int(insertIDR),
                                    realKey,
                                    static_cast<unsigned long long>(targetTimestampNs),
                                    unsigned(m_vplBitstream.FrameType),
                                    static_cast<unsigned long long>(GetTimestampUs() - traceEncodeStartUs)
                                );
                            }
                        }
                        ParseFrameNals(
                            m_codec,
                            reinterpret_cast<uint8_t*>(
                                m_vplBitstream.Data + m_vplBitstream.DataOffset
                            ),
                            m_vplBitstream.DataLength,
                            targetTimestampNs,
                            insertIDR
                        );
                        m_vplBitstream.DataLength = 0;
                    }
                } while (sts == MFX_WRN_IN_EXECUTION);
            }
            break;

        case MFX_ERR_NOT_ENOUGH_BUFFER:
            ERROR_THROW("not enough buffer");

        case MFX_ERR_MORE_DATA:
            if (isDraining == true)
                isEncGoing = false;
            break;

        case MFX_ERR_DEVICE_LOST:
            ERROR_THROW("device lost");

        case MFX_WRN_DEVICE_BUSY:
            VPL_DEBUG("device busy");
            break;

        default:
            ERROR_THROW("unknown encoding status %d", sts);
        }
    }
}

// ---- Shader-based NV12 conversion -------------------------------------------
//
// Main-profile HEVC and AV1 are 4:2:0-only, so a 4:4:4 surface declared against
// those profiles is rejected or silently mistreated by the driver. The surface
// has to be converted before it reaches the encoder.
//
// This used to be a oneVPL MFXVideoVPP stage. Measured on an Arc A770 at
// 3648x1824 with a standalone harness, the two implementations cost:
//
//     D3D11 VideoProcessor Blt (RGBA -> NV12)   2.82 ms/frame
//     full-screen shader pass (RGBA -> NV12)    0.016 ms/frame
//
// The VPP is submitted and then synchronously waited for on the encode thread,
// so every one of those milliseconds landed inside the per-frame budget. With a
// 11.111 ms budget at 90 Hz that alone was the difference between 90 fps and
// the ~81 fps this encoder was actually producing.
//
// Implementation notes, all verified with standalone probes on the A770:
//   - An NV12 texture CAN be a render target in D3D11, but only through
//     per-plane views: Y = DXGI_FORMAT_R8_UNORM, UV = DXGI_FORMAT_R8G8_UNORM.
//     Passing nullptr as the view desc fails with E_INVALIDARG.
//   - D3D11_RESOURCE_MISC_SHARED is compatible with NV12 + render target, which
//     matters because the encoder imports this texture with
//     MFX_SURFACE_FLAG_IMPORT_SHARED.
//   - A full-screen quad is culled by the default rasterizer state. CullMode
//     MUST be NONE or nothing is written.
//   - One draw per plane, each with its own viewport. Binding both planes as
//     two render targets writes Y only over the top-left corner on the A770
//     driver (RenderPipelineYUV works around the same defect with an extra
//     draw), and it forces a shared viewport even though chroma is half height.
//   - The conversion uses its own shaders (rgbtoyuv420nv12.hlsl). The HDR
//     pipeline's rgbtoyuv420.hlsl computes chroma as
//     "(x * 2) % width / width", which folds the chroma plane horizontally and
//     produced mirrored colours when reused here.
bool VideoEncoderVPL::InitNv12ShaderConverter() {
    ID3D11Device* device = m_pD3DRender->GetDevice();

    // Output: a shareable NV12 texture the encoder imports directly.
    D3D11_TEXTURE2D_DESC nv12Desc = {};
    nv12Desc.Width = UINT(m_renderWidth);
    nv12Desc.Height = UINT(m_renderHeight);
    nv12Desc.MipLevels = 1;
    nv12Desc.ArraySize = 1;
    nv12Desc.Format = DXGI_FORMAT_NV12;
    nv12Desc.SampleDesc.Count = 1;
    nv12Desc.Usage = D3D11_USAGE_DEFAULT;
    nv12Desc.BindFlags = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;
    nv12Desc.MiscFlags = D3D11_RESOURCE_MISC_SHARED;

    HRESULT hr = device->CreateTexture2D(&nv12Desc, nullptr, &m_nv12Tex);
    if (FAILED(hr)) {
        VPL_WARN("NV12 texture creation failed HR=%p %ls", hr, GetErrorStr(hr).c_str());
        return false;
    }

    // Per-plane render targets. The null-desc overload does not work for NV12.
    D3D11_RENDER_TARGET_VIEW_DESC rtvDesc = {};
    rtvDesc.ViewDimension = D3D11_RTV_DIMENSION_TEXTURE2D;
    rtvDesc.Texture2D.MipSlice = 0;

    rtvDesc.Format = DXGI_FORMAT_R8_UNORM;
    hr = device->CreateRenderTargetView(m_nv12Tex.Get(), &rtvDesc, &m_nv12RtvY);
    if (FAILED(hr)) {
        VPL_WARN("NV12 Y RTV failed HR=%p %ls", hr, GetErrorStr(hr).c_str());
        return false;
    }

    rtvDesc.Format = DXGI_FORMAT_R8G8_UNORM;
    hr = device->CreateRenderTargetView(m_nv12Tex.Get(), &rtvDesc, &m_nv12RtvUV);
    if (FAILED(hr)) {
        VPL_WARN("NV12 UV RTV failed HR=%p %ls", hr, GetErrorStr(hr).c_str());
        return false;
    }

    // Source view over the staging texture. Created once: an SRV per frame is
    // pure overhead at 90 Hz.
    hr = device->CreateShaderResourceView(m_transferTex.Get(), nullptr, &m_nv12SrcSrv);
    if (FAILED(hr)) {
        VPL_WARN("NV12 input SRV failed HR=%p %ls", hr, GetErrorStr(hr).c_str());
        return false;
    }

    // Shaders are compiled into this DLL as CSO blobs (see rgbtoyuv420nv12.hlsl).
    // The quad VS is shared; each plane gets its own pixel shader because they
    // need different sampling coordinates.
    std::vector<uint8_t> vsCso(QUAD_SHADER_CSO_PTR, QUAD_SHADER_CSO_PTR + QUAD_SHADER_CSO_LEN);
    std::vector<uint8_t> psYCso(NV12_Y_CSO_PTR, NV12_Y_CSO_PTR + NV12_Y_CSO_LEN);
    std::vector<uint8_t> psUVCso(NV12_UV_CSO_PTR, NV12_UV_CSO_PTR + NV12_UV_CSO_LEN);

    hr = device->CreateVertexShader(vsCso.data(), vsCso.size(), nullptr, &m_nv12Vs);
    if (FAILED(hr)) {
        VPL_WARN("NV12 VS creation failed HR=%p %ls", hr, GetErrorStr(hr).c_str());
        return false;
    }
    hr = device->CreatePixelShader(psYCso.data(), psYCso.size(), nullptr, &m_nv12PsY);
    if (FAILED(hr)) {
        VPL_WARN("NV12 PS(Y) creation failed HR=%p %ls", hr, GetErrorStr(hr).c_str());
        return false;
    }
    hr = device->CreatePixelShader(psUVCso.data(), psUVCso.size(), nullptr, &m_nv12PsUV);
    if (FAILED(hr)) {
        VPL_WARN("NV12 PS(UV) creation failed HR=%p %ls", hr, GetErrorStr(hr).c_str());
        return false;
    }

    // Colour conversion coefficients, matching the shader's cbuffer layout.
    // BT.709 full range, which is what SDR content on a Quest expects. (The
    // FrameRender HDR path uses BT.2020 instead; that path never runs here.)
    struct Nv12ColorParams {
        float offset[4];
        float yCoeff[4];
        float uCoeff[4];
        float vCoeff[4];
        float renderWidth;
        float renderHeight;
        float padding0;
        float padding1;
    };

    Nv12ColorParams params = {};
    params.offset[0] = 0.0f;
    params.offset[1] = 0.5019608f;
    params.offset[2] = 0.5019608f;

    // Kr = 0.2126, Kb = 0.0722 (BT.709)
    params.yCoeff[0] = 0.2126f;
    params.yCoeff[1] = 0.7152f;
    params.yCoeff[2] = 0.0722f;
    params.uCoeff[0] = -0.1146f;
    params.uCoeff[1] = -0.3854f;
    params.uCoeff[2] = 0.5f;
    params.vCoeff[0] = 0.5f;
    params.vCoeff[1] = -0.4542f;
    params.vCoeff[2] = -0.0458f;

    // Two buffers, identical coefficients but different renderSize: each plane's
    // pixel shader normalises sampling against the viewport it is drawing into,
    // and the chroma plane is half height.
    D3D11_BUFFER_DESC cbufDesc = {};
    cbufDesc.ByteWidth = sizeof(Nv12ColorParams);
    cbufDesc.Usage = D3D11_USAGE_IMMUTABLE;
    cbufDesc.BindFlags = D3D11_BIND_CONSTANT_BUFFER;

    Nv12ColorParams yParams = params;
    yParams.renderWidth = float(m_renderWidth);
    yParams.renderHeight = float(m_renderHeight);
    D3D11_SUBRESOURCE_DATA yData = {};
    yData.pSysMem = &yParams;
    hr = device->CreateBuffer(&cbufDesc, &yData, &m_nv12CbufY);
    if (FAILED(hr)) {
        VPL_WARN("NV12 Y constant buffer failed HR=%p %ls", hr, GetErrorStr(hr).c_str());
        return false;
    }

    // renderSize must describe the plane being written, i.e. the viewport size.
    Nv12ColorParams uvParams = params;
    uvParams.renderWidth = float(m_renderWidth) / 2.0f;
    uvParams.renderHeight = float(m_renderHeight) / 2.0f;
    D3D11_SUBRESOURCE_DATA uvData = {};
    uvData.pSysMem = &uvParams;
    hr = device->CreateBuffer(&cbufDesc, &uvData, &m_nv12CbufUV);
    if (FAILED(hr)) {
        VPL_WARN("NV12 UV constant buffer failed HR=%p %ls", hr, GetErrorStr(hr).c_str());
        return false;
    }

    D3D11_SAMPLER_DESC samplerDesc = {};
    samplerDesc.Filter = D3D11_FILTER_MIN_MAG_LINEAR_MIP_POINT;
    samplerDesc.AddressU = D3D11_TEXTURE_ADDRESS_CLAMP;
    samplerDesc.AddressV = D3D11_TEXTURE_ADDRESS_CLAMP;
    samplerDesc.AddressW = D3D11_TEXTURE_ADDRESS_CLAMP;
    samplerDesc.MaxLOD = D3D11_FLOAT32_MAX;
    hr = device->CreateSamplerState(&samplerDesc, &m_nv12Sampler);
    if (FAILED(hr)) {
        VPL_WARN("NV12 sampler failed HR=%p %ls", hr, GetErrorStr(hr).c_str());
        return false;
    }

    // Load-bearing: without CULL_NONE the full-screen quad is discarded and the
    // conversion silently writes nothing (measured: entire frame stays at 0).
    D3D11_RASTERIZER_DESC rasterDesc = {};
    rasterDesc.FillMode = D3D11_FILL_SOLID;
    rasterDesc.CullMode = D3D11_CULL_NONE;
    rasterDesc.DepthClipEnable = TRUE;
    hr = device->CreateRasterizerState(&rasterDesc, &m_nv12Raster);
    if (FAILED(hr)) {
        VPL_WARN("NV12 rasterizer state failed HR=%p %ls", hr, GetErrorStr(hr).c_str());
        return false;
    }

    VPL_INFO(
        "NV12 shader conversion enabled (%ux%u RGBA -> NV12/4:2:0, BT.709)",
        m_renderWidth,
        m_renderHeight
    );
    return true;
}

mfxFrameSurface1* VideoEncoderVPL::ConvertToNv12Shader(ID3D11Texture2D* rgbaTexture) {
    ID3D11DeviceContext* context = m_pD3DRender->GetContext();

    // Stage into the shareable RGBA texture. pTexture comes from FrameRender and
    // is not shared, so it cannot be imported (see VplImportTexture).
    context->CopyResource(m_transferTex.Get(), rgbaTexture);

    ID3D11ShaderResourceView* srv = m_nv12SrcSrv.Get();

    D3D11_VIEWPORT viewportY = {};
    viewportY.Width = float(m_renderWidth);
    viewportY.Height = float(m_renderHeight);
    viewportY.MinDepth = 0.0f;
    viewportY.MaxDepth = 1.0f;

    // The R8G8_UNORM view of an NV12 texture is W/2 texels wide: each texel holds
    // the UV pair for a 2x2 pixel block. Using the full width here only wrote the
    // left half of the chroma plane and squeezed it 2x, which misaligned colour
    // against luma and showed up as a colour ghost over a sharp greyscale image.
    D3D11_VIEWPORT viewportUV = viewportY;
    viewportUV.Width = float(m_renderWidth) / 2.0f;
    viewportUV.Height = float(m_renderHeight) / 2.0f;

    context->RSSetState(m_nv12Raster.Get());
    context->PSSetShaderResources(0, 1, &srv);
    context->PSSetSamplers(0, 1, m_nv12Sampler.GetAddressOf());
    context->VSSetShader(m_nv12Vs.Get(), nullptr, 0);
    context->IASetInputLayout(nullptr);
    context->IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);

    // One draw per plane. Binding both planes at once writes Y only over the
    // top-left corner on this driver, and forces both planes to share a
    // viewport even though chroma is half height.
    context->OMSetRenderTargets(1, m_nv12RtvY.GetAddressOf(), nullptr);
    context->RSSetViewports(1, &viewportY);
    context->PSSetConstantBuffers(0, 1, m_nv12CbufY.GetAddressOf());
    context->PSSetShader(m_nv12PsY.Get(), nullptr, 0);
    context->Draw(4, 0);

    context->OMSetRenderTargets(1, m_nv12RtvUV.GetAddressOf(), nullptr);
    context->RSSetViewports(1, &viewportUV);
    context->PSSetConstantBuffers(0, 1, m_nv12CbufUV.GetAddressOf());
    context->PSSetShader(m_nv12PsUV.Get(), nullptr, 0);
    context->Draw(4, 0);

    // Unbind so the texture is not held as an input while the encoder reads it.
    ID3D11ShaderResourceView* nullSrv = nullptr;
    context->PSSetShaderResources(0, 1, &nullSrv);
    context->OMSetRenderTargets(0, nullptr, nullptr);

    // Import the freshly written NV12 texture for the encoder.
    mfxSurfaceD3D11Tex2D extSurface = {};
    extSurface.SurfaceInterface.Header.SurfaceType = MFX_SURFACE_TYPE_D3D11_TEX2D;
    extSurface.SurfaceInterface.Header.SurfaceFlags
        = MFX_SURFACE_FLAG_IMPORT_SHARED | MFX_SURFACE_FLAG_IMPORT_COPY;
    extSurface.SurfaceInterface.Header.StructSize = sizeof(mfxSurfaceD3D11Tex2D);
    extSurface.texture2D = m_nv12Tex.Get();

    mfxFrameSurface1* encSurface = nullptr;
    const mfxStatus importStatus = m_vplMemoryInterface->ImportFrameSurface(
        m_vplMemoryInterface,
        MFX_SURFACE_COMPONENT_ENCODE,
        &extSurface.SurfaceInterface.Header,
        &encSurface
    );
    if (importStatus < MFX_ERR_NONE) {
        ERROR_THROW("NV12 encode surface import failed with %d", int(importStatus));
    }

    return encSurface;
}

void VideoEncoderVPL::ReleaseNv12ShaderResources() {
    m_nv12Raster.Reset();
    m_nv12Sampler.Reset();
    m_nv12CbufUV.Reset();
    m_nv12CbufY.Reset();
    m_nv12PsUV.Reset();
    m_nv12PsY.Reset();
    m_nv12Vs.Reset();
    m_nv12SrcSrv.Reset();
    m_nv12RtvUV.Reset();
    m_nv12RtvY.Reset();
    m_nv12Tex.Reset();
}

void VideoEncoderVPL::InitTransferTex() {
    // m_transferTex is the shareable staging copy of the composition texture and
    // the input of the NV12 conversion pass, so it must stay in the composition
    // format (R8G8B8A8). The conversion writes into m_nv12Tex, which is what the
    // encoder imports, so m_dxColorFormat (NV12 on that path) does not apply.
    const DXGI_FORMAT transferFormat
        = m_useNv12 ? DXGI_FORMAT_R8G8B8A8_UNORM : m_dxColorFormat;

    D3D11_TEXTURE2D_DESC transferTexDesc = { UINT(m_renderWidth),
                                             UINT(m_renderHeight),
                                             1,
                                             1,
                                             transferFormat,
                                             { 1, 0 },
                                             D3D11_USAGE_DEFAULT,
                                             D3D11_BIND_SHADER_RESOURCE,
                                             0,
                                             D3D11_RESOURCE_MISC_SHARED };

    HRESULT hr
        = m_pD3DRender->GetDevice()->CreateTexture2D(
            &transferTexDesc, nullptr, &m_transferTex
        );
    if (FAILED(hr))
        ERROR_THROW("failed to create transfer texture HR=%p %ls", hr, GetErrorStr(hr).c_str());
}

void VideoEncoderVPL::InitVpl() {
    m_vplLoader = MFXLoad();
    VERIFY(m_vplLoader != nullptr, "MFXLoad failed -- is implementation in path?");

    CheckVPLConfig();

    VPL_VERIFY(MFXCreateSession(m_vplLoader, 0, &m_vplSession));
    VPL_VERIFY(
        MFXVideoCORE_SetHandle(m_vplSession, MFX_HANDLE_D3D11_DEVICE, m_pD3DRender->GetDevice())
    );

    LogImplementationInfo();

    // Get interface for ImportFrameSurface
    VPL_VERIFY(MFXGetMemoryInterface(m_vplSession, &m_vplMemoryInterface));
    VERIFY(m_vplMemoryInterface != nullptr, "MFXGetMemoryInterface failed");
}

void VideoEncoderVPL::InitVplEncode() {
    m_vplEncodeParams.IOPattern = MFX_IOPATTERN_IN_VIDEO_MEMORY;
    // Previously hardcoded to MFX_CODINGOPTION_ON (VDENC low-power path), which is
    // limited on ARC and forces a lower-quality engine. Now user-selectable.
    m_vplEncodeParams.mfx.LowPower = m_vplLowPower;
    // Sync encode loop below submits one task and waits for it, so the depth stays 1.
    m_vplEncodeParams.AsyncDepth = 1;
    m_vplEncodeParams.mfx.CodecId = m_vplCodec;
    m_vplEncodeParams.mfx.CodecProfile = m_vplCodecProfile;
    // Only meaningful for AV1 (see ChooseParams); MFX_LEVEL_UNKNOWN leaves the
    // driver default in place for the other codecs.
    if (m_vplCodecLevel != MFX_LEVEL_UNKNOWN) {
        m_vplEncodeParams.mfx.CodecLevel = m_vplCodecLevel;
    }
    m_vplEncodeParams.mfx.TargetUsage = m_vplQualityPreset;
    m_vplEncodeParams.mfx.RateControlMethod = m_vplRateControlMode;
    m_vplEncodeParams.mfx.FrameInfo.FrameRateExtN = m_refreshRate;
    m_vplEncodeParams.mfx.FrameInfo.FrameRateExtD = 1;
    m_vplEncodeParams.mfx.FrameInfo.FourCC = m_vplColorFormat;
    m_vplEncodeParams.mfx.FrameInfo.ChromaFormat = m_vplChromaFormat;
    m_vplEncodeParams.mfx.FrameInfo.CropW = m_renderWidth;
    m_vplEncodeParams.mfx.FrameInfo.CropH = m_renderHeight;
    m_vplEncodeParams.mfx.FrameInfo.Width = ALIGN16(m_renderWidth);
    m_vplEncodeParams.mfx.FrameInfo.Height = ALIGN16(m_renderHeight);

    // "Enable VBAQ/CAQ" is deliberately not implemented for VPL.
    //
    // oneVPL has no content-adaptive quantisation option. The closest relative,
    // mfxExtCodingOption3::AdaptiveCQM, was tried and measured on this Arc A770:
    // the driver accepted the extended buffer (NumExtParam stayed 1) but
    // MFXVideoENCODE_Query answered MFX_WRN_INCOMPATIBLE_VIDEO_PARAM and reset
    // AdaptiveCQM from MFX_CODINGOPTION_ON (16) to MFX_CODINGOPTION_UNKNOWN (0),
    // i.e. "do not know this option". DG2 only ships VDEnc encoders and that
    // path is a feature subset, so the setting cannot be honoured here and is
    // documented as AMD/NVIDIA-only instead of being silently ignored.

    // The driver defaults to GopRefDist = 8 (7 consecutive B-frames) because we
    // never specified a GOP. B-frames must be reordered before display, so the
    // decoder has to hold a whole group before it can output anything - pure
    // added latency in a real-time stream. NVENC in this codebase explicitly
    // sets frameIntervalP = 1 (I/P only); do the same here. GopRefDist = 1 means
    // "no regular B-frames" per the oneVPL docs.
    m_vplEncodeParams.mfx.GopRefDist = 1;

    // Reference frame count: TESTED AND REVERTED 2026-11-15.
    //
    // Setting NumRefFrame = 2 was accepted by the driver (EFFECTIVE PARAMS reported
    // numRefFrame=2) but did not help: PUREDEC stayed at 18.3-18.7 ms and the 40 ms+ share
    // actually rose (1623-2037 -> 2123). Reference-frame pressure is therefore not what
    // makes this decoder slow, and the setting only costs compression efficiency.
    // Leave NumRefFrame unset.

    // Slice count: TESTED AND REVERTED 2026-11-14.
    //
    // Setting NumSlice = 4 was accepted by the driver (EFFECTIVE PARAMS reported
    // numSlice=4), and it did reduce "Decoder saturation" drops from 941 to 131. But it
    // made decoding WORSE: PUREDEC average rose 19.69 -> 20.73 ms and the share of frames
    // taking over 40 ms went from ~23% to 48-96%. Total latency climbed to 133 ms with the
    // server warning about it, and the user reported the corruption was unchanged.
    //
    // Conclusion: this decoder does not gain from multiple slices, and the extra slice
    // boundaries cost more than the parallelism buys. Leave NumSlice unset.

    // IdrInterval = 1 makes every keyframe a true IDR. The client only resynchronises on
    // an IDR; with 0 the driver emits CRA frames after the first keyframe. The meaning is
    // per-codec per the oneVPL docs (H.264: 1 = every other I-frame is an IDR, HEVC:
    // 1 = every I-frame is an IDR).
    //
    // GopPicSize is a setting (gop_frames). 0 is the documented "unspecified" value,
    // which this driver resolves to its own default (measured: 248 on AV1, then not
    // executed). Any positive value is honoured exactly - 900 gave eleven consecutive
    // 10.0 s gaps. Do not use 0xFFFF: not a documented value, and it makes
    // MFXVideoENCODE_Query return MFX_WRN_INCOMPATIBLE_VIDEO_PARAM.
    //
    // Recovery does not depend on the GOP. A requested keyframe is produced correctly by
    // the "I | IDR | REF" mask below; passing MFX_FRAMETYPE_IDR alone was accepted
    // without error but silently produced a P frame.
    //
    // Other backends never schedule keyframes at all (NVENC infinite GOP, AMF GOP 0,
    // VAAPI gop_size = INT_MAX), so a periodic keyframe is an Intel-only cost.
    m_vplEncodeParams.mfx.GopPicSize
        = m_vplGopFrames > 0 ? mfxU16((std::min)(m_vplGopFrames, 65535u)) : 0;
    m_vplEncodeParams.mfx.IdrInterval = 1;

    auto dynParams = GetDynamicEncoderParams();
    const uint64_t initialBitrateBps = (dynParams.updated && dynParams.bitrate_bps > 0)
        ? dynParams.bitrate_bps
        : 30'000'000ULL;
    const VplBrcParams brc
        = MakeVplBrcParams(initialBitrateBps, m_vplVbvFrames, m_refreshRate);
    m_vplEncodeParams.mfx.BRCParamMultiplier = brc.multiplier;
    m_vplEncodeParams.mfx.TargetKbps = brc.targetKbps;
    // MaxKbps is "the maximum bitrate at which the encoded data enters the VBV
    // buffer" (oneVPL docs). It used to be left at 0 for CBR on the assumption
    // that CBR implies no ceiling, but a zero here means "not explicitly
    // specified", so the driver falls back to its own default and the stream is
    // free to exceed the target. Measured at a 300 Mbps target: delivered mean
    // 443 Mbps (+48%) and p90 565 Mbps (+88%), with peaks of 968 KB per frame.
    // Setting it to the target keeps the VBV fed at the requested rate.
    m_vplEncodeParams.mfx.MaxKbps = brc.targetKbps;
    if (brc.bufferSizeInKB > 0) {
        m_vplEncodeParams.mfx.BufferSizeInKB = brc.bufferSizeInKB;
        m_vplEncodeParams.mfx.InitialDelayInKB = brc.initialDelayInKB;
    }
    VPL_INFO(
        "VPL BRC: request %.1f Mbps -> TargetKbps=%u x mult=%u = %.1f Mbps, "
        "BufferSizeInKB=%u, InitialDelayInKB=%u",
        initialBitrateBps / 1e6, brc.targetKbps, brc.multiplier,
        double(brc.targetKbps) * brc.multiplier / 1000.0,
        brc.bufferSizeInKB, brc.initialDelayInKB
    );

    VPL_WARN(
        "REQUESTED PARAMS: codec=0x%x profile=0x%x fourCC=0x%x chroma=0x%x %ux%u "
        "rateControl=%u targetUsage=%u lowPower=%u asyncDepth=%u",
        unsigned(m_vplCodec), unsigned(m_vplCodecProfile), unsigned(m_vplColorFormat),
        unsigned(m_vplChromaFormat), m_renderWidth, m_renderHeight,
        unsigned(m_vplRateControlMode), unsigned(m_vplQualityPreset), unsigned(m_vplLowPower),
        unsigned(m_vplEncodeParams.AsyncDepth)
    );

    const mfxStatus sts
        = MFXVideoENCODE_Query(m_vplSession, &m_vplEncodeParams, &m_vplEncodeParams);
    switch (sts) {
    case MFX_ERR_NONE:
    case MFX_WRN_INCOMPATIBLE_VIDEO_PARAM:
        if (sts == MFX_WRN_INCOMPATIBLE_VIDEO_PARAM)
            VPL_WARN("incompatible video params, auto-correcting");
        break;

    case MFX_WRN_PARTIAL_ACCELERATION:
        VPL_WARN("partial acceleration");
        break;

    case MFX_ERR_UNSUPPORTED:
        ERROR_THROW("query unsupported");

    default:
        ERROR_THROW("query failed with %d", sts);
    }

    // Initialize ENCODE
    VPL_VERIFY(MFXVideoENCODE_Init(m_vplSession, &m_vplEncodeParams));
}

mfxFrameSurface1* VideoEncoderVPL::VplImportTexture(ID3D11Texture2D* texture) {
    // Callers pass m_transferTex itself in the common path, in which case the
    // copy has already been done by Transmit; skip it.
    if (texture != m_transferTex.Get()) {
        m_pD3DRender->GetContext()->CopyResource(m_transferTex.Get(), texture);
    }

    mfxSurfaceD3D11Tex2D extSurfD3D11 = {};
    extSurfD3D11.SurfaceInterface.Header.SurfaceType = MFX_SURFACE_TYPE_D3D11_TEX2D;
    extSurfD3D11.SurfaceInterface.Header.SurfaceFlags
        = MFX_SURFACE_FLAG_IMPORT_SHARED | MFX_SURFACE_FLAG_IMPORT_COPY;
    extSurfD3D11.SurfaceInterface.Header.StructSize = sizeof(mfxSurfaceD3D11Tex2D);
    // Always import m_transferTex: it is created with D3D11_RESOURCE_MISC_SHARED,
    // which MFX_SURFACE_FLAG_IMPORT_SHARED requires. pTexture comes from
    // FrameRender and is not shareable, so importing it directly fails with
    // MFX_ERR_NULL_PTR (-4).
    extSurfD3D11.texture2D = m_transferTex.Get();

    mfxFrameSurface1* encSurface = nullptr;
    VPL_VERIFY(m_vplMemoryInterface->ImportFrameSurface(
        m_vplMemoryInterface,
        MFX_SURFACE_COMPONENT_ENCODE,
        &extSurfD3D11.SurfaceInterface.Header,
        &encSurface
    ));

    return encSurface;
}

void VideoEncoderVPL::ChooseParams() {
    const Settings* s = Settings_Instance();

    m_refreshRate = s->m_refreshRate;
    m_codec = s->m_codec;
    m_vplVbvFrames = s->m_vplVbvFrames;
    m_vplGopFrames = s->m_vplGopFrames;

    // h264 encoding is currently broken due to an encoding
    // error when forcing the idr frame type:
    //
    // mfxEncodeCtrl encodeCtrl = {};
    // encodeCtrl.FrameType = MFX_FRAMETYPE_IDR;
    //
    // Results in error -15 (MFX_ERR_INVALID_VIDEO_PARAM)
    // when encoding a frame.
    if (m_codec == ALVR_CODEC_H264) {
        VPL_WARN("h264 codec currently unsupported, forcing HEVC");
        m_codec = ALVR_CODEC_HEVC;
    }

    // AV1 Main and HEVC Main are both 4:2:0-only profiles.
    //
    // Feeding oneVPL a BGR4/4:4:4 surface while declaring one of those profiles
    // is not corrected by the driver: the EFFECTIVE PARAMS dump confirms it
    // stays BGR4/4:4:4. For AV1 that produced a stream the headset could not
    // decode at all. For HEVC it "works" but is pure waste - the decoder only
    // implements 4:2:0, so the extra chroma is thrown away after being encoded,
    // transmitted and received. Measured on Quest 3 over a wired link: 4:4:4
    // pushed decode to 25.5 ms and transport to 23.9 ms at 295 Mbps, capping the
    // client at 44 fps with constant frame-queue overflow.
    //
    // So convert both codecs to NV12 unless HDR needs a 10-bit format.
    const bool needsNv12 = !s->m_enableHdr
        && (m_codec == ALVR_CODEC_AV1 || m_codec == ALVR_CODEC_HEVC);

    if (s->m_enableHdr) {
        if (s->m_use10bitEncoder) {
            m_dxColorFormat = DXGI_FORMAT_P010;
            m_vplColorFormat = MFX_FOURCC_P010;
            m_vplChromaFormat = MFX_CHROMAFORMAT_YUV420;
        } else {
            m_dxColorFormat = DXGI_FORMAT_NV12;
            m_vplColorFormat = MFX_FOURCC_NV12;
            m_vplChromaFormat = MFX_CHROMAFORMAT_YUV420;
        }
    } else if (needsNv12) {
        // FrameRender converts to NV12 on the GPU (see its RGB->YUV420 pipeline),
        // so the surface we receive is already 4:2:0. Declaring it truthfully is
        // what makes the Main-profile bitstream valid: previously the encoder ran
        // 4:4:4 while claiming a 4:2:0-only profile.
        m_dxColorFormat = DXGI_FORMAT_NV12;
        m_vplColorFormat = MFX_FOURCC_NV12;
        m_vplChromaFormat = MFX_CHROMAFORMAT_YUV420;
    } else {
        // NOTE: BGR4 input surface with YUV444 chroma is what the A770 driver
        // actually accepts. Attempting MFX_CHROMAFORMAT_YUV420 with a BGR4
        // surface makes MFXVideoENCODE_Query return MFX_ERR_UNSUPPORTED (-3),
        // which surfaces as "query unsupported" / "your GPU does not meet the
        // requirements" and aborts encoder init (SteamVR then crashes).
        //
        // oneVPL cannot convert BGR4 -> 4:2:0 here: FrameRender hands us an
        // R8G8B8A8 composition texture, so the import surface must stay 4-channel.
        // Converting to NV12/P010 requires a separate conversion pass, not just
        // a chroma flag. Do not "fix" this line by flipping the value.
        m_dxColorFormat = DXGI_FORMAT_R8G8B8A8_UNORM;
        m_vplColorFormat = MFX_FOURCC_BGR4;
        m_vplChromaFormat = MFX_CHROMAFORMAT_YUV444;
    }

    switch (m_codec) {
    case ALVR_CODEC_H264:
        m_vplCodec = MFX_CODEC_AVC;
        break;
    case ALVR_CODEC_HEVC:
        m_vplCodec = MFX_CODEC_HEVC;
        break;
    case ALVR_CODEC_AV1:
        m_vplCodec = MFX_CODEC_AV1;
        break;
    default:
        ERROR_THROW("unsupported video encoding %d", s->m_codec);
    }

    m_vplCodecProfile = MFX_PROFILE_UNKNOWN;
    if (m_codec == ALVR_CODEC_H264) {
        switch (s->m_h264Profile) {
        case ALVR_H264_PROFILE_BASELINE:
            m_vplCodecProfile = MFX_PROFILE_AVC_BASELINE;
            break;
        case ALVR_H264_PROFILE_MAIN:
            m_vplCodecProfile = MFX_PROFILE_AVC_MAIN;
            break;
        case ALVR_H264_PROFILE_HIGH:
            m_vplCodecProfile = MFX_PROFILE_AVC_HIGH;
            break;
        default:
            ERROR_THROW("unsupported h264 profile %d", s->m_h264Profile);
        }
    }

    // AV1 previously had no profile branch at all, leaving CodecProfile =
    // MFX_PROFILE_UNKNOWN (0 = "let the driver decide").
    //
    // Main is the only option here, and it must stay Main: Qualcomm Snapdragon
    // XR2 (Quest 2/3/Pro, Pico 4) decodes AV1 Main (4:2:0 8-bit) in hardware
    // only. AV1 High (4:4:4) and AV1 Pro would land in the software decoder or
    // fail to configure outright.
    //
    // This is the same arrangement HEVC already uses in this file and is known
    // to work: m_vplChromaFormat describes the IMPORT SURFACE (BGR4), while the
    // profile constrains what may appear in the CODED BITSTREAM. HEVC Main
    // forces 4:2:0 there, which is why the current HEVC stream decodes on XR2
    // despite reporting chroma=4:4:4 in the params dump.
    if (m_codec == ALVR_CODEC_AV1) {
        m_vplCodecProfile = MFX_PROFILE_AV1_MAIN;
    }

    // Choose an AV1 level that actually covers this resolution and frame rate.
    //
    // We never set CodecLevel before, so the driver picked its default (level
    // 5.1 = seq_level_idx 17). At 3072x1408@90 that declares 4.3 Mpx per frame
    // and 389 Mpx/s, while level 5.1 allows 2.2 Mpx and 83.6 Mpx/s. Software
    // decoders (libdav1d) ignore the mismatch, which is why the recording
    // decodes cleanly in ffmpeg, but Qualcomm's hardware AV1 decoder enforces
    // the level and refuses to produce frames - the headset stays black and
    // reports no error.
    //
    // Pick the smallest level whose MaxPicSize and MaxLumaSampleRate both cover
    // the stream. Values from the AV1 spec, Annex A.5.
    m_vplCodecLevel = MFX_LEVEL_UNKNOWN;
    if (m_codec == ALVR_CODEC_AV1) {
        const uint64_t picSize = uint64_t(m_renderWidth) * uint64_t(m_renderHeight);
        const uint64_t lumaRate = picSize * uint64_t(m_refreshRate ? m_refreshRate : 60);

        struct Av1LevelLimit {
            uint16_t level;
            uint64_t maxPicSize;
            uint64_t maxLumaRate;
        };
        static const Av1LevelLimit kLimits[] = {
            { MFX_LEVEL_AV1_4, 147456, 7372800 },
            { MFX_LEVEL_AV1_41, 147456, 7372800 },
            { MFX_LEVEL_AV1_5, 393216, 12288000 },
            { MFX_LEVEL_AV1_51, 2228224, 83558400 },
            { MFX_LEVEL_AV1_6, 8912896, 160432128 },
            { MFX_LEVEL_AV1_61, 8912896, 160432128 },
            { MFX_LEVEL_AV1_62, 8912896, 160432128 },
            { MFX_LEVEL_AV1_63, 8912896, 160432128 },
        };

        for (const auto& limit : kLimits) {
            if (picSize <= limit.maxPicSize && lumaRate <= limit.maxLumaRate) {
                m_vplCodecLevel = limit.level;
                break;
            }
        }

        if (m_vplCodecLevel == MFX_LEVEL_UNKNOWN) {
            // Nothing covers it; 6.3 is the highest level oneVPL exposes here.
            // Still better to declare a too-low level than the 5.1 default.
            m_vplCodecLevel = MFX_LEVEL_AV1_63;
            VPL_WARN(
                "AV1: no level covers %llux%llu@%u (picSize=%llu lumaRate=%llu); "
                "falling back to level 6.3",
                (unsigned long long)m_renderWidth,
                (unsigned long long)m_renderHeight,
                unsigned(m_refreshRate),
                (unsigned long long)picSize,
                (unsigned long long)lumaRate
            );
        }

        VPL_INFO(
            "AV1: level for %ux%u@%u -> %u (picSize=%llu lumaRate=%llu)",
            unsigned(m_renderWidth),
            unsigned(m_renderHeight),
            unsigned(m_refreshRate),
            unsigned(m_vplCodecLevel),
            (unsigned long long)picSize,
            (unsigned long long)lumaRate
        );
    }

    if (s->m_use10bitEncoder) {
        switch (m_codec) {
        case ALVR_CODEC_H264:
            m_vplCodecProfile = MFX_PROFILE_AVC_HIGH10;
            break;
        case ALVR_CODEC_HEVC:
            m_vplCodecProfile = MFX_PROFILE_HEVC_MAIN10;
            break;
        }
    }

    // Intel-specific target usage (settings: Video -> Encoder config -> Intel ARC (oneVPL)).
    // MFX_TARGETUSAGE_* values: 1 = best quality, 4 = balanced, 7 = best speed.
    switch (s->m_vplTargetUsage) {
    case 1:
        m_vplQualityPreset = MFX_TARGETUSAGE_BEST_QUALITY;
        break;
    case 4:
        m_vplQualityPreset = MFX_TARGETUSAGE_BALANCED;
        break;
    case 7:
        m_vplQualityPreset = MFX_TARGETUSAGE_BEST_SPEED;
        break;
    default:
        m_vplQualityPreset = MFX_TARGETUSAGE_BALANCED;
        break;
    }

    m_vplLowPower = s->m_vplLowPower ? MFX_CODINGOPTION_ON : MFX_CODINGOPTION_OFF;

    switch (s->m_rateControlMode) {
    case ALVR_CBR:
        m_vplRateControlMode = MFX_RATECONTROL_CBR;
        break;
    case ALVR_VBR:
        m_vplRateControlMode = MFX_RATECONTROL_VBR;
        break;
    default:
        ERROR_THROW("invalid rate control mode");
    }
}

// Diagnostic: report the parameters the driver ACTUALLY accepted, not the ones
// we requested. MFXVideoENCODE_Query auto-corrects incompatible fields and still
// returns a success status, so the requested FourCC/ChromaFormat can silently
// differ from what the encoder really runs. The client (Quest) hardware decoder
// only implements HEVC Main/Main10 at 4:2:0; if 4:4:4 or BGR4 slipped through,
// decode cost roughly doubles - which matches a decode latency that does not
// respond to bitrate.
void VideoEncoderVPL::LogEffectiveParams(mfxStatus query_status, const mfxVideoParam& actual) {
    auto four_cc_name = [](mfxU32 four_cc) -> const char* {
        switch (four_cc) {
        case MFX_FOURCC_NV12:
            return "NV12";
        case MFX_FOURCC_P010:
            return "P010";
        case MFX_FOURCC_BGR4:
            return "BGR4";
        case MFX_FOURCC_RGB4:
            return "RGB4";
        case MFX_FOURCC_YUY2:
            return "YUY2";
        case MFX_FOURCC_NV16:
            return "NV16";
        case MFX_FOURCC_Y210:
            return "Y210";
        case MFX_FOURCC_Y410:
            return "Y410";
        default:
            return "OTHER";
        }
    };

    auto chroma_name = [](mfxU16 chroma) -> const char* {
        switch (chroma) {
        case MFX_CHROMAFORMAT_MONOCHROME:
            return "4:0:0";
        case MFX_CHROMAFORMAT_YUV420:
            return "4:2:0";
        case MFX_CHROMAFORMAT_YUV422:
            return "4:2:2";
        case MFX_CHROMAFORMAT_YUV444:
            return "4:4:4";
        default:
            return "unknown";
        }
    };

    auto profile_name = [](mfxU16 profile) -> const char* {
        switch (profile) {
        case MFX_PROFILE_HEVC_MAIN:
            return "HEVC Main";
        case MFX_PROFILE_HEVC_MAIN10:
            return "HEVC Main10";
        case MFX_PROFILE_HEVC_MAINSP:
            return "HEVC MainSP";
        case MFX_PROFILE_AVC_BASELINE:
            return "AVC Baseline";
        case MFX_PROFILE_AVC_MAIN:
            return "AVC Main";
        case MFX_PROFILE_AVC_HIGH:
            return "AVC High";
        default:
            return "unknown/unspecified";
        }
    };

    // Warn (not Info) so the line is duplicated into the SteamVR driver log via
    // DriverLog(). Info only goes to the ALVR dashboard log, which is discarded
    // unless "Log to disk" is enabled.
    VPL_WARN(
        "EFFECTIVE PARAMS (query status %d): requested FourCC=%s chroma=%s | "
        "driver FourCC=%s chroma=%s profile=%s",
        query_status, four_cc_name(m_vplColorFormat), chroma_name(m_vplChromaFormat),
        four_cc_name(actual.mfx.FrameInfo.FourCC),
        chroma_name(actual.mfx.FrameInfo.ChromaFormat), profile_name(actual.mfx.CodecProfile)
    );

    VPL_WARN(
        "EFFECTIVE PARAMS: %ux%u (crop %ux%u) ratecontrol=%u targetUsage=%u lowPower=%u "
        "asyncDepth=%u gopPicSize=%u gopRefDist=%u numSlice=%u numRefFrame=%u",
        actual.mfx.FrameInfo.Width, actual.mfx.FrameInfo.Height, actual.mfx.FrameInfo.CropW,
        actual.mfx.FrameInfo.CropH, actual.mfx.RateControlMethod, actual.mfx.TargetUsage,
        actual.mfx.LowPower, actual.AsyncDepth, actual.mfx.GopPicSize, actual.mfx.GopRefDist,
        unsigned(actual.mfx.NumSlice), unsigned(actual.mfx.NumRefFrame)
    );

    // BRC fields: the driver is free to rewrite these in Query(), and if it
    // caps or rescales them the requested bitrate is silently not applied.
    // The buffer is reported both in MB and in frames, because "frames" is the
    // number that matters for frame-size regularity and it is the one the other
    // backends size directly (NVENC/AMF/FFmpeg all use ~1.1).
    const double eff_bitrate_bps
        = double(actual.mfx.TargetKbps) * double(actual.mfx.BRCParamMultiplier) * 1000.0;
    const double eff_buffer_bytes
        = double(actual.mfx.BufferSizeInKB) * double(actual.mfx.BRCParamMultiplier) * 1000.0;
    const double frame_bits
        = m_refreshRate > 0 ? eff_bitrate_bps / double(m_refreshRate) : eff_bitrate_bps / 90.0;
    VPL_WARN(
        "EFFECTIVE BRC: TargetKbps=%u MaxKbps=%u BufferSizeInKB=%u InitialDelayInKB=%u "
        "mult=%u => %.1f Mbps, buffer %.1f MB = %.1f frames",
        unsigned(actual.mfx.TargetKbps), unsigned(actual.mfx.MaxKbps),
        unsigned(actual.mfx.BufferSizeInKB), unsigned(actual.mfx.InitialDelayInKB),
        unsigned(actual.mfx.BRCParamMultiplier), eff_bitrate_bps / 1e6,
        eff_buffer_bytes / (1024.0 * 1024.0),
        frame_bits > 0.0 ? eff_buffer_bytes * 8.0 / frame_bits : 0.0
    );

    if (actual.mfx.FrameInfo.ChromaFormat == MFX_CHROMAFORMAT_YUV444
        || actual.mfx.FrameInfo.FourCC == MFX_FOURCC_BGR4) {
        VPL_WARN(
            "EFFECTIVE PARAMS: encoder is running 4:4:4 (%s/%s). Quest/Pico hardware "
            "decoders implement only 4:2:0; this is very likely the cause of the "
            "decode latency spike.",
            four_cc_name(actual.mfx.FrameInfo.FourCC),
            chroma_name(actual.mfx.FrameInfo.ChromaFormat)
        );
    }

}

void VideoEncoderVPL::CheckVPLConfig() {
    mfxConfig cfg[5];
    mfxVariant cfgVal[5];

    // Implementation used must be the hardware implementation
    cfg[0] = MFXCreateConfig(m_vplLoader);
    VERIFY(cfg[0] != NULL, "MFXCreateConfig failed");
    cfgVal[0].Type = MFX_VARIANT_TYPE_U32;
    cfgVal[0].Data.U32 = MFX_IMPL_TYPE_HARDWARE;
    VPL_VERIFY(MFXSetConfigFilterProperty(cfg[0], (mfxU8*)"mfxImplDescription.Impl", cfgVal[0]));

    // Implementation used must provide API version 2.9 or newer
    cfg[1] = MFXCreateConfig(m_vplLoader);
    VERIFY(NULL != cfg[1], "MFXCreateConfig failed")
    cfgVal[1].Type = MFX_VARIANT_TYPE_U32;
    cfgVal[1].Data.U32 = VPLVERSION(2, 9);
    VPL_VERIFY(MFXSetConfigFilterProperty(
        cfg[1], (mfxU8*)"mfxImplDescription.ApiVersion.Version", cfgVal[1]
    ));

    // Implementation used must be D3D11 acceleration mode
    cfg[2] = MFXCreateConfig(m_vplLoader);
    VERIFY(NULL != cfg[2], "MFXCreateConfig failed")
    cfgVal[2].Type = MFX_VARIANT_TYPE_U32;
    cfgVal[2].Data.U32 = MFX_ACCEL_MODE_VIA_D3D11;
    VPL_VERIFY(
        MFXSetConfigFilterProperty(cfg[2], (mfxU8*)"mfxImplDescription.AccelerationMode", cfgVal[2])
    );

    // Implementation used must be D3D11 surface sharing mode
    // Applying the 3 associated parameters (logical AND operation) using a single mfxConfig
    cfg[3] = MFXCreateConfig(m_vplLoader);
    VERIFY(NULL != cfg[3], "MFXCreateConfig failed")
    cfgVal[3].Type = MFX_VARIANT_TYPE_U32;
    cfgVal[3].Data.U32 = MFX_SURFACE_TYPE_D3D11_TEX2D;
    VPL_VERIFY(MFXSetConfigFilterProperty(
        cfg[3], (mfxU8*)"mfxSurfaceTypesSupported.surftype.SurfaceType", cfgVal[3]
    ));

    cfgVal[3].Data.U32 = MFX_SURFACE_COMPONENT_ENCODE;
    VPL_VERIFY(MFXSetConfigFilterProperty(
        cfg[3], (mfxU8*)"mfxSurfaceTypesSupported.surftype.surfcomp.SurfaceComponent", cfgVal[3]
    ));

    cfgVal[3].Data.U32 = MFX_SURFACE_FLAG_IMPORT_COPY;
    VPL_VERIFY(MFXSetConfigFilterProperty(
        cfg[3], (mfxU8*)"mfxSurfaceTypesSupported.surftype.surfcomp.SurfaceFlags", cfgVal[3]
    ));

    // Implementation must provide correct codec
    cfg[4] = MFXCreateConfig(m_vplLoader);
    VERIFY(NULL != cfg[4], "MFXCreateConfig failed")
    cfgVal[4].Type = MFX_VARIANT_TYPE_U32;
    cfgVal[4].Data.U32 = m_vplCodec;
    VPL_VERIFY(MFXSetConfigFilterProperty(
        cfg[4], (mfxU8*)"mfxImplDescription.mfxEncoderDescription.encoder.CodecID", cfgVal[4]
    ));
}

void VideoEncoderVPL::LogImplementationInfo() {
    mfxImplDescription* idesc = nullptr;
    mfxStatus sts;
    // Loads info about implementation at specified list location
    sts = MFXEnumImplementations(m_vplLoader, 0, MFX_IMPLCAPS_IMPLDESCSTRUCTURE, (mfxHDL*)&idesc);
    if (!idesc || (sts != MFX_ERR_NONE))
        return;

    const char* accel_mode;
    switch (idesc->AccelerationMode) {
    case MFX_ACCEL_MODE_NA:
        accel_mode = "na";
        break;
    case MFX_ACCEL_MODE_VIA_D3D9:
        accel_mode = "d3d9";
        break;
    case MFX_ACCEL_MODE_VIA_D3D11:
        accel_mode = "d3d11";
        break;
    case MFX_ACCEL_MODE_VIA_VAAPI:
        accel_mode = "vaapi";
        break;
    case MFX_ACCEL_MODE_VIA_VAAPI_DRM_MODESET:
        accel_mode = "vaapi_drm_modeset";
        break;
    case MFX_ACCEL_MODE_VIA_VAAPI_GLX:
        accel_mode = "vaapi_glx";
        break;
    case MFX_ACCEL_MODE_VIA_VAAPI_X11:
        accel_mode = "vaapi_x11";
        break;
    case MFX_ACCEL_MODE_VIA_VAAPI_WAYLAND:
        accel_mode = "vaapi_wayland";
        break;
    case MFX_ACCEL_MODE_VIA_HDDLUNITE:
        accel_mode = "hddlunite";
        break;
    default:
        accel_mode = "unknown";
        break;
    }

    VPL_INFO(
        "using api version %hu.%hu on device %s",
        idesc->ApiVersion.Major,
        idesc->ApiVersion.Minor,
        idesc->Dev.DeviceID
    );
    VPL_DEBUG("api version: %hu.%hu", idesc->ApiVersion.Major, idesc->ApiVersion.Minor);
    VPL_DEBUG("impl type: hw");
    VPL_DEBUG("accel mode: %s", accel_mode);
    VPL_DEBUG("device id: %s", idesc->Dev.DeviceID);
    MFXDispReleaseImplDescription(m_vplLoader, idesc);

#if (MFX_VERSION >= 2004)
    // Show implementation path, added in 2.4 API
    mfxHDL implPath = nullptr;
    sts = MFXEnumImplementations(m_vplLoader, 0, MFX_IMPLCAPS_IMPLPATH, &implPath);
    if (!implPath || (sts != MFX_ERR_NONE))
        return;

    VPL_DEBUG("path: %s", reinterpret_cast<mfxChar*>(implPath));
    MFXDispReleaseImplDescription(m_vplLoader, implPath);
#endif
}
