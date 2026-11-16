#pragma once

#include "ALVR-common/packet_types.h"
#include "openvr_driver_wrap.h"

#include <list>
#include <mutex>
#include <optional>

class PoseHistory {
public:
    struct TrackingHistoryFrame {
        uint64_t targetTimestampNs;
        FfiDeviceMotion motion;
        vr::HmdMatrix34_t rotationMatrix;
    };

    void OnPoseUpdated(uint64_t targetTimestampNs, FfiDeviceMotion motion);

    std::optional<TrackingHistoryFrame> GetBestPoseMatch(const vr::HmdMatrix34_t& pose) const;
    // Return the most recent pose known at the given timestamp
    std::optional<TrackingHistoryFrame> GetPoseAt(uint64_t timestampNs) const;

    void SetTransform(const vr::HmdMatrix34_t& transform);

    // TEMPORARY: age of the entry returned by the last GetBestPoseMatch call,
    // in milliseconds, relative to the newest entry.
    float AgeOfMatchMs() const { return m_lastMatchAgeMs; }

    // TEMPORARY DIAGNOSTIC: distance (in mm) between the submitted pose's translation
    // and the matched entry's translation. This is the ground truth we were missing:
    // it says directly whether the image is being rendered with the head position the
    // runtime actually used, instead of inferring it from a timestamp age.
    float MatchPosDeltaMm() const { return m_lastMatchPosDeltaMm; }

    const float* LastSubmitPos() const { return m_lastSubmitPos; }
    const float* LastMatchedPos() const { return m_lastMatchedPos; }
    const float* LastMatchedVelocity() const { return m_lastMatchedVelocity; }
    int LastMatchIndexDelta() const { return m_lastMatchIndexDelta; }

private:
    mutable std::mutex m_mutex;
    std::list<TrackingHistoryFrame> m_poseBuffer;
    vr::HmdMatrix34_t m_transform
        = { { { 1.0, 0.0, 0.0, 0.0 }, { 0.0, 1.0, 0.0, 0.0 }, { 0.0, 0.0, 1.0, 0.0 } } };
    bool m_transformIdentity = true;
    mutable float m_lastMatchAgeMs = 0.0f;
    mutable float m_lastMatchPosDeltaMm = 0.0f;
    mutable float m_lastSubmitPos[3] = { 0.0f, 0.0f, 0.0f };
    mutable float m_lastMatchedPos[3] = { 0.0f, 0.0f, 0.0f };
    mutable float m_lastMatchedVelocity[3] = { 0.0f, 0.0f, 0.0f };
    mutable int m_lastMatchIndexDelta = 0;
};
