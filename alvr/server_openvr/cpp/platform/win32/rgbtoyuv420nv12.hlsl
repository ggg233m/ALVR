// RGBA -> NV12 (4:2:0) conversion for the oneVPL encoder.
//
// Deliberately separate from rgbtoyuv420.hlsl, which belongs to the HDR
// pipeline and computes its chroma coordinate as
//
//     (uvTexels.x * 2) % renderWidth / renderWidth
//
// That wraps back to zero at x = width/2 and folds the chroma plane
// horizontally; reusing it for the SDR NV12 path produced exactly that defect.
//
// Two entry points, each writing a single plane. Writing Y and UV as two
// render targets in one draw only covers the top-left corner of Y on this
// driver (the same defect RenderPipelineYUV works around with an extra draw),
// and it makes the two planes share one viewport, which is wrong because the
// chroma plane is half height. One draw per plane keeps viewport and sampling
// coordinates trivially consistent:
//
//   mainY  -> Y  plane, R8_UNORM,    viewport (0, 0, W,   H)
//   mainUV -> UV plane, R8G8_UNORM,  viewport (0, 0, W/2, H/2)
//
// The chroma viewport is W/2 wide, not W: an R8G8_UNORM view of an NV12 texture
// is half width because each texel stores the UV pair of a 2x2 pixel block.
// Rendering it W wide only fills the left half of the plane and squeezes chroma
// 2x, which misaligns colour against luma and reads as a colour ghost over a
// sharp greyscale image.
//
// renderSize must be the *bound viewport* size, not the frame size: sampling is
// normalised against the plane being written.

cbuffer Nv12Params : register(b0)
{
    float4 offset;      // x: Y offset, y: U offset, z: V offset
    float4 yCoeff;
    float4 uCoeff;
    float4 vCoeff;
    float2 renderSize;  // size of the plane being written
    float2 padding;
};

Texture2D<float4> sourceTexture : register(t0);
SamplerState linearSampler : register(s0);

// Luma plane, full resolution.
float4 mainY(float4 position : SV_Position) : SV_Target
{
    float2 uv = position.xy / renderSize;
    float3 rgb = sourceTexture.Sample(linearSampler, uv).rgb;
    float y = dot(rgb, yCoeff.rgb) + offset.x;
    return float4(y, 0.0f, 0.0f, 1.0f);
}

// Chroma plane, half height. Each texel covers a 2x2 block of the source, so
// sample at the centre of that block instead of at the texel corner.
float4 mainUV(float4 position : SV_Position) : SV_Target
{
    float2 uv = (position.xy * 2.0f + 1.0f) / (renderSize * 2.0f);
    float3 rgb = sourceTexture.Sample(linearSampler, uv).rgb;
    float u = dot(rgb, uCoeff.rgb) + offset.y;
    float v = dot(rgb, vCoeff.rgb) + offset.z;
    return float4(u, v, 0.0f, 1.0f);
}
