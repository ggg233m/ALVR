
#include "Logger.h"
#include "Utils.h"
#include "bindings.h"
#include <cstdint>
#include <mutex>
#include <string.h>

static const char NAL_PREFIX_3B[] = { 0x00, 0x00, 0x01 };
static const char NAL_PREFIX_4B[] = { 0x00, 0x00, 0x00, 0x01 };

static const unsigned char H264_NAL_TYPE_SPS = 7;
static const unsigned char HEVC_NAL_TYPE_VPS = 32;

static const unsigned char H264_NAL_TYPE_AUD = 9;
static const unsigned char HEVC_NAL_TYPE_AUD = 35;

// AV1 OBU types (AV1 spec, section 6.2.2 "OBU header semantics").
static const unsigned char OBU_SEQUENCE_HEADER = 1;
static const unsigned char OBU_TEMPORAL_DELIMITER = 2;
static const unsigned char OBU_FRAME_HEADER = 3;
static const unsigned char OBU_TILE_GROUP = 4;
static const unsigned char OBU_METADATA = 5;
static const unsigned char OBU_FRAME = 6;

// Strip an IVF container header if present. NVENC wraps AV1 output in IVF; the
// same stripping already happens in VideoEncoderNVENC::Transmit, but that is
// per-backend and this parser is shared, so be tolerant of both layouts.
// Returns the number of bytes to skip.
static int skipIvfHeader(const unsigned char* buf, int len) {
    static const unsigned char IVF_MAGIC[4] = { 0x44, 0x4B, 0x49, 0x46 };
    if (len >= 4 && memcmp(buf, IVF_MAGIC, 4) == 0) {
        return 32;
    }
    return 0;
}

// Read an AV1 leb128 value (used for obu_size). Returns bytes consumed, or 0 on
// overflow/malformed input.
static int readLeb128(const unsigned char* buf, int len, uint64_t* out) {
    uint64_t value = 0;
    int i = 0;
    for (; i < len && i < 8; i++) {
        value |= (uint64_t)(buf[i] & 0x7F) << (i * 7);
        if ((buf[i] & 0x80) == 0) {
            *out = value;
            return i + 1;
        }
    }
    return 0;
}

// Minimal bit reader over an AV1 sequence header payload, used to fill in the
// AV1CodecConfigurationRecord fields below.
struct BitReader {
    const unsigned char* data;
    int size;
    int bitPos = 0;

    bool ok() const { return bitPos <= size * 8; }

    int read(int n) {
        int value = 0;
        for (int i = 0; i < n; i++) {
            const int byteIdx = bitPos >> 3;
            if (byteIdx >= size) {
                bitPos += (n - i);
                return value;
            }
            const int bit = (data[byteIdx] >> (7 - (bitPos & 7))) & 1;
            value = (value << 1) | bit;
            bitPos++;
        }
        return value;
    }
};

// Build an AV1CodecConfigurationRecord (the "av1C" box payload) from a raw
// Sequence Header OBU.
//
// Android's MediaCodec takes csd-0 for video/av01 as this record, not as a bare
// OBU: MediaExtractor in MP4/WebM pipelines produces exactly this layout and the
// platform decoders are written against it. Passing raw OBU bytes instead is
// silently accepted at configure() time and then never produces a frame, which
// matches the observed symptom (no decode error, no output, black screen).
//
// Layout (AV1 ISOBMFF binding, section 2.3):
//   marker(1)=1 | version(7)=1 | seq_profile(3) | seq_level_idx_0(5) |
//   seq_tier_0(1) | high_bitdepth(1) | twelve_bit(1) | monochrome(1) |
//   chroma_subsampling_x(1) | chroma_subsampling_y(1) |
//   chroma_sample_position(2) | reserved(3)=0 | initial_presentation_delay_present(1)
//   [initial_presentation_delay_minus_one(4)] | configOBUs[]
static int buildAv1CodecConfigRecord(
    const unsigned char* seqHeaderObu,
    int seqHeaderLen,
    unsigned char* out,
    int outCapacity
) {
    if (seqHeaderLen < 2) {
        return 0;
    }

    const unsigned char header = seqHeaderObu[0];
    if (((header >> 3) & 0x0F) != OBU_SEQUENCE_HEADER || (header & 0x04) != 0) {
        return 0;
    }

    uint64_t payloadLen = 0;
    const int sizeFieldLen = readLeb128(seqHeaderObu + 1, seqHeaderLen - 1, &payloadLen);
    if (sizeFieldLen == 0) {
        return 0;
    }

    const int payloadOffset = 1 + sizeFieldLen;
    if (payloadLen == 0 || payloadOffset + (int)payloadLen > seqHeaderLen) {
        return 0;
    }

    BitReader br { seqHeaderObu + payloadOffset, (int)payloadLen };

    const int seqProfile = br.read(3);
    const int stillPicture = br.read(1);
    const int reducedStillPicture = br.read(1);

    int seqLevelIdx0 = 0;
    int seqTier0 = 0;
    int initialPresentationDelayPresent = 0;

    if (!reducedStillPicture) {
        const int timingInfoPresent = br.read(1);
        if (timingInfoPresent) {
            // We never ask oneVPL for timing info in the sequence header, and
            // parsing it would need the full timing_info() syntax. Bail out
            // rather than emit a bogus record.
            return 0;
        }
        initialPresentationDelayPresent = br.read(1);
        const int operatingPointsCnt = br.read(5) + 1;
        for (int i = 0; i < operatingPointsCnt; i++) {
            br.read(12); // operating_point_idc
            const int level = br.read(5);
            int tier = 0;
            if (level > 7) {
                tier = br.read(1);
            }
            if (i == 0) {
                seqLevelIdx0 = level;
                seqTier0 = tier;
            }
            if (initialPresentationDelayPresent) {
                if (br.read(1)) {
                    br.read(4);
                }
            }
        }
    }

    const int frameWidthBits = br.read(4) + 1;
    const int frameHeightBits = br.read(4) + 1;
    br.read(frameWidthBits);
    br.read(frameHeightBits);

    if (!reducedStillPicture) {
        if (br.read(1)) { // frame_id_numbers_present_flag
            br.read(4);
            br.read(3);
        }
    }

    br.read(1); // use_128x128_superblock
    br.read(1); // enable_filter_intra
    br.read(1); // enable_intra_edge_filter

    int enableOrderHint = 0;
    if (!reducedStillPicture) {
        br.read(1); // enable_interintra_compound
        br.read(1); // enable_masked_compound
        br.read(1); // enable_warped_motion
        br.read(1); // enable_dual_filter
        enableOrderHint = br.read(1);
        if (enableOrderHint) {
            br.read(1); // enable_jnt_comp
            br.read(1); // enable_ref_frame_mvs
        }
        const int seqChooseScreenContentTools = br.read(1);
        const int seqForceScreenContentTools
            = seqChooseScreenContentTools ? 2 : br.read(1);
        if (seqForceScreenContentTools > 0) {
            const int seqChooseIntegerMv = br.read(1);
            if (!seqChooseIntegerMv) {
                br.read(1);
            }
        }
        if (enableOrderHint) {
            br.read(3); // order_hint_bits_minus_1
        }
    }

    br.read(1); // enable_superres
    br.read(1); // enable_cdef
    br.read(1); // enable_restoration

    // color_config()
    const int highBitdepth = br.read(1);
    int twelveBit = 0;
    if (seqProfile == 2 && highBitdepth) {
        twelveBit = br.read(1);
    }
    const int bitDepth = seqProfile == 2 ? (twelveBit ? 12 : (highBitdepth ? 10 : 8))
                                         : (highBitdepth ? 10 : 8);
    const int monochrome = seqProfile == 1 ? 0 : br.read(1);

    if (br.read(1)) { // color_description_present_flag
        br.read(8); // color_primaries
        br.read(8); // transfer_characteristics
        br.read(8); // matrix_coefficients
    }

    int subX = 1;
    int subY = 1;
    int chromaSamplePosition = 0;

    if (monochrome) {
        br.read(1); // color_range
    } else {
        br.read(1); // color_range
        if (seqProfile == 0) {
            subX = 1;
            subY = 1;
        } else if (seqProfile == 1) {
            subX = 0;
            subY = 0;
        } else {
            if (bitDepth == 12) {
                subX = br.read(1);
                subY = subX ? br.read(1) : 0;
            } else {
                subX = 1;
                subY = 0;
            }
        }
        if (subX && subY) {
            chromaSamplePosition = br.read(2);
        }
    }
    br.read(1); // separate_uv_delta_q

    if (!br.ok()) {
        return 0;
    }

    // 4 byte fixed header + configOBUs
    if (outCapacity < 4 + seqHeaderLen) {
        return 0;
    }

    out[0] = (unsigned char)(0x80 | 0x01); // marker = 1, version = 1
    out[1] = (unsigned char)((seqProfile << 5) | (seqLevelIdx0 & 0x1F));
    out[2] = (unsigned char)(
        ((seqTier0 & 0x01) << 7) | ((highBitdepth & 0x01) << 6)
        | ((twelveBit & 0x01) << 5) | ((monochrome & 0x01) << 4)
        | ((subX & 0x01) << 3) | ((subY & 0x01) << 2)
        | (chromaSamplePosition & 0x03)
    );
    out[3] = (unsigned char)(
        (initialPresentationDelayPresent ? 0x10 : 0x00)
        // initial_presentation_delay_minus_one is left as 0 when present
    );

    memcpy(out + 4, seqHeaderObu, seqHeaderLen);
    return 4 + seqHeaderLen;
}

// Collect all OBUs of the given type into outOfs/outLen. Returns how many were
// found. outOfs/outLen describe the COMPLETE OBU (header + size field + payload).
static int findObus(
    const unsigned char* buf,
    int len,
    unsigned char wantedType,
    int maxResults,
    int* outOfs,
    int* outLen
) {
    int found = 0;
    int i = skipIvfHeader(buf, len);

    while (i < len && found < maxResults) {
        const unsigned char header = buf[i];
        // forbidden bit must be zero for a valid OBU header.
        if ((header & 0x80) != 0) {
            // Not an OBU boundary we understand; stop rather than misparse.
            break;
        }

        const unsigned char obuType = (header >> 3) & 0x0F;
        const bool hasExtension = (header & 0x04) != 0;
        const bool hasSizeField = (header & 0x02) != 0;

        int pos = i + 1;
        if (hasExtension) {
            pos += 1;
        }
        if (pos > len) {
            break;
        }

        int payloadLen = 0;
        if (hasSizeField) {
            uint64_t obuSize = 0;
            const int consumed = readLeb128(buf + pos, len - pos, &obuSize);
            if (consumed == 0 || obuSize > (uint64_t)(len - pos - consumed)) {
                break;
            }
            pos += consumed;
            payloadLen = (int)obuSize;
        } else {
            // Without a size field the OBU extends to the end of the buffer.
            payloadLen = len - pos;
        }

        if (obuType == wantedType) {
            // Start at the OBU header, not at the payload.
            outOfs[found] = i;
            outLen[found] = (pos + payloadLen) - i;
            found++;
        }

        i = pos + payloadLen;
    }

    return found;
}

int8_t getNalPrefixSize(unsigned char* buf) {
    if (memcmp(buf, NAL_PREFIX_3B, sizeof(NAL_PREFIX_3B)) == 0) {
        return sizeof(NAL_PREFIX_3B);
    } else if (memcmp(buf, NAL_PREFIX_4B, sizeof(NAL_PREFIX_4B)) == 0) {
        return sizeof(NAL_PREFIX_4B);
    } else {
        return -1;
    }
}

/*
Sends the (VPS + )SPS + PPS video configuration headers from H.264 or H.265 stream as a sequence of
NALs. (VPS + )SPS + PPS have short size (8bytes + 28bytes in some environment), so we can assume
SPS + PPS is contained in first fragment.
*/
void sendHeaders(int codec, unsigned char*& buf, int& len, int nalNum) {
    unsigned char* cursor = buf;
    int headersLen = 0;
    int foundHeaders = -1; // Offset by 1 header to find the length until the next header

    while (headersLen <= len) {
        if (headersLen + sizeof(NAL_PREFIX_4B) > (unsigned)len) {
            cursor++;
            headersLen++;
            continue;
        }
        int8_t prefixSize = getNalPrefixSize(cursor);
        if (prefixSize == -1) {
            cursor++;
            headersLen++;
            continue;
        }
        foundHeaders++;
        if (foundHeaders == nalNum) {
            break;
        }
        headersLen += prefixSize;
        cursor += prefixSize;
    }

    if (foundHeaders != nalNum) {
        return;
    }

    SetVideoConfigNals((const unsigned char*)buf, headersLen, codec);

    // move the cursor forward excluding config NALs
    buf = cursor;
    len -= headersLen;
}

void processH264Nals(unsigned char*& buf, int& len) {
    unsigned char prefixSize = getNalPrefixSize(buf);
    unsigned char nalType = buf[prefixSize] & 0x1F;

    if (nalType == H264_NAL_TYPE_AUD && len > prefixSize * 2 + 2) {
        buf += prefixSize + 2;
        len -= prefixSize + 2;
        prefixSize = getNalPrefixSize(buf);
        nalType = buf[prefixSize] & 0x1F;
    }
    if (nalType == H264_NAL_TYPE_SPS) {
        sendHeaders(ALVR_CODEC_H264, buf, len, 2); // 2 headers SPS and PPS
    }
}

void processHevcNals(unsigned char*& buf, int& len) {
    unsigned char prefixSize = getNalPrefixSize(buf);
    unsigned char nalType = (buf[prefixSize] >> 1) & 0x3F;

    if (nalType == HEVC_NAL_TYPE_AUD && len > prefixSize * 2 + 3) {
        buf += prefixSize + 3;
        len -= prefixSize + 3;
        prefixSize = getNalPrefixSize(buf);
        nalType = (buf[prefixSize] >> 1) & 0x3F;
    }
    if (nalType == HEVC_NAL_TYPE_VPS) {
        sendHeaders(ALVR_CODEC_HEVC, buf, len, 3); // 3 headers VPS, SPS and PPS
    }
}

// AV1 cannot use the NAL-prefix scanning above: AV1 uses OBUs, not Annex-B, so
// there is no 00 00 01 start code to look for. H264/HEVC hand the client their
// SPS/PPS as csd-0; for AV1 the equivalent is the Sequence Header OBU. Without
// it MediaCodec cannot configure its decoder and streaming never starts, which
// is what the previous "SetVideoConfigNals(0, 0, codec)" stub caused - it sent a
// NULL/zero-length config, so csd-0 arrived empty on the headset.
//
// The Sequence Header is emitted periodically (at least on every keyframe) but
// not necessarily in the very first packet we see, so scan every frame and stop
// once we have captured it.
static void processAv1Obus(unsigned char*& buf, int& len) {
    static bool av1GotConfig = false;
    static int av1FramesScanned = 0;

    if (av1GotConfig) {
        return;
    }

    av1FramesScanned++;

    constexpr int MAX_OBUS = 8;
    int ofs[MAX_OBUS];
    int obuLen[MAX_OBUS];

    const int seqCount
        = findObus(buf, len, OBU_SEQUENCE_HEADER, MAX_OBUS, ofs, obuLen);
    if (seqCount <= 0) {
        // Report on the first few frames so a failure is visible immediately
        // rather than after two seconds of silence.
        if (av1FramesScanned <= 3 || av1FramesScanned % 120 == 0) {
            Warn(
                "AV1: no Sequence Header OBU (frame %d, %d bytes, first=%02X %02X %02X %02X)\n",
                av1FramesScanned,
                len,
                len > 0 ? buf[0] : 0,
                len > 1 ? buf[1] : 0,
                len > 2 ? buf[2] : 0,
                len > 3 ? buf[3] : 0
            );
        }
        return;
    }

    // csd-0 for AV1 must be an AV1CodecConfigurationRecord (av1C), not a bare
    // Sequence Header OBU. See buildAv1CodecConfigRecord for why.
    static unsigned char configBuffer[4096];
    const int total = buildAv1CodecConfigRecord(
        buf + ofs[0], obuLen[0], configBuffer, (int)sizeof(configBuffer)
    );

    if (total > 0) {
        av1GotConfig = true;
        // Warn (not Info) purely so this is visible in the SteamVR driver log;
        // Info() deliberately skips DriverLog(). Downgrade once AV1 is verified.
        Warn(
            "AV1: csd-0 av1C built, %d bytes (frame %d), first bytes:"
            " %02X %02X %02X %02X %02X %02X %02X %02X (seqProfile=%d level=%d)\n",
            total,
            av1FramesScanned,
            configBuffer[0],
            configBuffer[1],
            configBuffer[2],
            configBuffer[3],
            configBuffer[4],
            configBuffer[5],
            configBuffer[6],
            configBuffer[7],
            (configBuffer[1] >> 5) & 0x07,
            configBuffer[1] & 0x1F
        );
        SetVideoConfigNals(configBuffer, total, ALVR_CODEC_AV1);
    } else {
        if (av1FramesScanned <= 3 || av1FramesScanned % 120 == 0) {
            Warn(
                "AV1: failed to build av1C from Sequence Header OBU (frame %d, %d bytes)\n",
                av1FramesScanned,
                obuLen[0]
            );
        }
    }
}

// Detect a true AV1 KEY_FRAME in a temporal unit.
//
// The encoder is given MFX_FRAMETYPE_IDR on a frame we would like to be a
// keyframe, but a keyframe request is only a hint: on the Arc A770 the request
// is frequently ignored, and GopPicSize is not honoured either. Reporting
// "this is an IDR" for a frame that is not one is actively harmful, because the
// client clears its corrupted-stream flag on it and then feeds the following
// inter frames to the decoder without a reference - which is exactly the
// periodic corruption this path is trying to remove.
//
// frame_type is the first field of the uncompressed header, immediately after
// the OBU_FRAME header and its size field, so only a few bits need decoding.
// Uncompressed header layout (AV1 spec 5.9.2):
//   show_existing_frame(1)
//   if (show_existing_frame) frame_to_show_map_idx(3)
//   else frame_type(2)  <- 0 == KEY_FRAME
//
// Any parsing doubt is resolved as "not a keyframe": claiming a keyframe that
// does not exist breaks recovery, while missing one only delays it until the
// next real keyframe.
bool IsAv1Keyframe(const unsigned char* buf, int len) {
    if (!buf || len <= 0) {
        return false;
    }

    int i = skipIvfHeader(buf, len);
    while (i < len) {
        const unsigned char header = buf[i];
        if ((header & 0x80) != 0) {
            return false;
        }

        const unsigned char obuType = (header >> 3) & 0x0F;
        const bool hasExtension = (header & 0x04) != 0;
        const bool hasSizeField = (header & 0x02) != 0;

        int pos = i + 1;
        if (hasExtension) {
            pos += 1;
        }
        if (pos > len) {
            return false;
        }

        int payloadLen = 0;
        if (hasSizeField) {
            uint64_t obuSize = 0;
            const int consumed = readLeb128(buf + pos, len - pos, &obuSize);
            if (consumed == 0 || obuSize > (uint64_t)(len - pos - consumed)) {
                return false;
            }
            pos += consumed;
            payloadLen = (int)obuSize;
        } else {
            payloadLen = len - pos;
        }

        // A coded frame may be carried either as OBU_FRAME_HEADER + OBU_TILE_GROUP
        // or as a single combined OBU_FRAME. Both start their payload with the
        // same uncompressed frame header, so the same decoding applies.
        if (obuType == OBU_FRAME_HEADER || obuType == OBU_FRAME) {
            BitReader br { buf + pos, payloadLen };
            const int showExistingFrame = br.read(1);
            if (showExistingFrame) {
                // A frame that only re-displays a stored reference; it is not a
                // recovery point.
                return false;
            }
            const int frameType = br.read(2);
            return br.ok() && frameType == 0;
        }

        i = pos + payloadLen;
    }

    return false;
}

void ParseFrameNals(
    int codec, unsigned char* buf, int len, unsigned long long targetTimestampNs, bool isIdr
) {
    if (codec == ALVR_CODEC_AV1) {
        processAv1Obus(buf, len);
        // Only report an IDR when the bitstream really starts with a KEY_FRAME.
        // The encoder treats a keyframe request as a hint and drops it often
        // enough that the flag must be verified here; see IsAv1Keyframe.
        const bool actualKeyframe = IsAv1Keyframe(buf, len);

        // Log unconditionally on keyframes and on ignored requests: the ratio of
        // the two is the whole story for corruption recovery, and a size-based
        // check cannot tell them apart (an ignored request still returns a
        // perfectly valid inter frame).
        {
            static int dropped = 0;
            static int honored = 0;
            if (isIdr && !actualKeyframe) {
                dropped++;
                Warn(
                    "AV1: keyframe request IGNORED by encoder (%d dropped / %d honoured)\n",
                    dropped,
                    honored
                );
            } else if (actualKeyframe) {
                honored++;
                Warn(
                    "AV1: keyframe emitted, %d bytes (%d dropped / %d honoured)\n",
                    len,
                    dropped,
                    honored
                );
            }
        }

        VideoSend(targetTimestampNs, buf, len, isIdr && actualKeyframe);
        return;
    }

    if ((unsigned)len < sizeof(NAL_PREFIX_4B)) {
        return;
    }

    if (codec == ALVR_CODEC_H264) {
        processH264Nals(buf, len);
    } else if (codec == ALVR_CODEC_HEVC) {
        processHevcNals(buf, len);
    }

    VideoSend(targetTimestampNs, buf, len, isIdr);
}
