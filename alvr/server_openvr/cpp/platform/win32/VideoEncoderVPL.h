#pragma once

#include "VideoEncoder.h"
#include "shared/d3drender.h"
#include <cstdint>
#include <d3d11.h>
#include <dxgi.h>
#include <vector>
#include <wrl/client.h>

#include "vpl/mfx.h"
#include "vpl/mfxjpeg.h"
#include "vpl/mfxmemory.h"
#include "vpl/mfxstructures.h"
#include "vpl/mfxvideo.h"
#if (MFX_VERSION >= 2000)
#include "vpl/mfxdispatcher.h"
#endif

class VideoEncoderVPL : public VideoEncoder {
public:
    VideoEncoderVPL(std::shared_ptr<CD3DRender> pD3DRender, int width, int height);
    ~VideoEncoderVPL();

    void Initialize();
    void Shutdown();

    void Transmit(
        ID3D11Texture2D* pTexture,
        uint64_t presentationTime,
        uint64_t targetTimestampNs,
        bool insertIDR
    );

private:
    void CheckVPLConfig();
    void ChooseParams();
    void InitTransferTex();
    void InitVpl();
    void InitVplEncode();
    mfxFrameSurface1* VplImportTexture(ID3D11Texture2D* texture);
    // Set up the GPU-side RGBA -> NV12 (4:2:0) conversion used when the encoder
    // must run 4:2:0 (AV1 Main). Returns false if unavailable.
    bool InitNv12Converter();
    mfxFrameSurface1* ConvertToNv12(ID3D11Texture2D* rgbaTexture);

    // Shader-based RGBA -> NV12 conversion. This replaces the oneVPL VPP stage
    // above, which was measured on an Arc A770 to cost 2.8 ms/frame at
    // 3648x1824, while the equivalent full-screen shader pass costs 0.016 ms
    // (a 175x difference). The VPP is a sync call on the encode thread, so its
    // cost lands directly in the per-frame budget and was the single biggest
    // cause of the encoder topping out at ~81 fps instead of 90.
    //
    // Implemented with the rgba -> yuv420 shader already compiled into this DLL
    // (RGBTOYUV420_CSO_PTR, see FrameRender.cpp): it writes the Y and UV planes
    // as two render targets of one draw. Colour coefficients are supplied
    // through its constant buffer, so no shader changes are needed.
    bool InitNv12ShaderConverter();
    mfxFrameSurface1* ConvertToNv12Shader(ID3D11Texture2D* rgbaTexture);
    void ReleaseNv12ShaderResources();
    void LogImplementationInfo();
    void LogEffectiveParams(mfxStatus query_status, const mfxVideoParam& actual);

    std::shared_ptr<CD3DRender> m_pD3DRender;
    int m_codec;
    int m_renderWidth;
    int m_renderHeight;
    int m_refreshRate;

    mfxU32 m_vplCodec;
    mfxU32 m_vplCodecProfile;
    mfxU16 m_vplCodecLevel = MFX_LEVEL_UNKNOWN;
    mfxU32 m_vplColorFormat;
    mfxU32 m_vplChromaFormat;
    mfxU32 m_vplQualityPreset;
    mfxU32 m_vplRateControlMode;
    mfxU16 m_vplLowPower;
    DXGI_FORMAT m_dxColorFormat;
    mfxVideoParam m_vplEncodeParams = {};


    mfxLoader m_vplLoader = nullptr;
    mfxSession m_vplSession = nullptr;
    mfxBitstream m_vplBitstream = {};
    mfxMemoryInterface* m_vplMemoryInterface = nullptr;
    Microsoft::WRL::ComPtr<ID3D11Texture2D> m_transferTex;

    // Shader-based NV12 conversion state (see InitNv12ShaderConverter).
    // m_nv12Tex is the shareable NV12 texture we import into oneVPL; it is the
    // *output* of the shader pass and the *input* of the encoder, so it must
    // carry D3D11_RESOURCE_MISC_SHARED.
    bool m_useNv12Shader = false;
    Microsoft::WRL::ComPtr<ID3D11Texture2D> m_nv12Tex;
    Microsoft::WRL::ComPtr<ID3D11RenderTargetView> m_nv12RtvY;
    Microsoft::WRL::ComPtr<ID3D11RenderTargetView> m_nv12RtvUV;
    Microsoft::WRL::ComPtr<ID3D11ShaderResourceView> m_nv12SrcSrv;
    Microsoft::WRL::ComPtr<ID3D11VertexShader> m_nv12Vs;
    // One pixel shader per plane. A single shader writing both planes through
    // two render targets only covers the top-left corner of Y on this driver,
    // and the two planes cannot share a viewport because chroma is half height.
    Microsoft::WRL::ComPtr<ID3D11PixelShader> m_nv12PsY;
    Microsoft::WRL::ComPtr<ID3D11PixelShader> m_nv12PsUV;
    // Coefficients are identical for both planes; only renderSize differs,
    // because sampling is normalised against the plane being written.
    Microsoft::WRL::ComPtr<ID3D11Buffer> m_nv12CbufY;
    Microsoft::WRL::ComPtr<ID3D11Buffer> m_nv12CbufUV;
    Microsoft::WRL::ComPtr<ID3D11SamplerState> m_nv12Sampler;
    Microsoft::WRL::ComPtr<ID3D11RasterizerState> m_nv12Raster;

    // True when the encoder needs a 4:2:0 surface, which is the case for every
    // Main-profile HEVC/AV1 session. The conversion itself is a shader pass
    // (InitNv12ShaderConverter); it used to be a oneVPL VPP stage, which cost
    // 2.8 ms/frame versus 0.016 ms for the shader equivalent.
    bool m_useNv12 = false;
};
