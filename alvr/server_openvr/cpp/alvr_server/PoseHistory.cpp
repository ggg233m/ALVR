#include "PoseHistory.h"
#include "Logger.h"
#include "Settings.h"
#include "Utils.h"
#include "include/openvr_math.h"
#include <cmath>
#include <mutex>
#include <optional>

void PoseHistory::OnPoseUpdated(uint64_t targetTimestampNs, FfiDeviceMotion motion) {
    // Put pose history buffer
    TrackingHistoryFrame history;
    history.targetTimestampNs = targetTimestampNs;
    history.motion = motion;

    HmdMatrix_QuatToMat(
        motion.pose.orientation.w,
        motion.pose.orientation.x,
        motion.pose.orientation.y,
        motion.pose.orientation.z,
        &history.rotationMatrix
    );

    std::unique_lock<std::mutex> lock(m_mutex);
    if (!m_transformIdentity) {
        vr::HmdMatrix34_t rotation = vrmath::matMul33(m_transform, history.rotationMatrix);
        history.rotationMatrix = rotation;
    }

    // TEMPORARY DIAGNOSTIC (tracking arrival rate). MATCHIDX shows 84.5% of submissions
    // match the newest buffered entry, yet POSESEL still sees ~2.6% of selected
    // timestamps spaced more than 1.25 frame periods apart. That combination is only
    // possible if this buffer is updated LESS often than SubmitLayer is called: two
    // submissions then resolve to the same entry, and the next update skips ahead.
    // Measuring the spacing of pushed timestamps separates that from a bad matcher.
    {
        static std::atomic<uint64_t> n(0), dup(0), gap1(0), gap2(0), gap3(0);
        static std::atomic<uint64_t> prevNs(0), maxGapNs(0);
        const uint64_t periodNs = 11111111ull; // 90 Hz nominal
        const uint64_t cur = targetTimestampNs;
        const uint64_t prev = prevNs.exchange(cur);
        if (prev != 0) {
            if (cur == prev) {
                dup.fetch_add(1);
            } else if (cur > prev) {
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
            }
        }
        const uint64_t i = n.fetch_add(1) + 1;
        if (DiagEnabled() && i % 180 == 0) {
            Warn(
                "TRACKARRIVE n=%llu dup=%llu gap>1.25f=%llu gap>=2f=%llu gap>=3f=%llu gap_max=%.2fms (period=11.11ms)\n",
                (unsigned long long)i,
                (unsigned long long)dup.load(),
                (unsigned long long)gap1.load(),
                (unsigned long long)gap2.load(),
                (unsigned long long)gap3.load(),
                (double)maxGapNs.load() / 1e6
            );
        }
    }

    if (m_poseBuffer.size() == 0) {
        m_poseBuffer.push_back(history);
    } else {
        if (m_poseBuffer.back().targetTimestampNs != targetTimestampNs) {
            // New track info
            m_poseBuffer.push_back(history);
        }
    }
    // The value should match with the client's MAXIMUM_TRACKING_FRAMES in ovr_context.cpp
    if (m_poseBuffer.size() > 120 * 3) {
        m_poseBuffer.pop_front();
    }
}

std::optional<PoseHistory::TrackingHistoryFrame>
PoseHistory::GetBestPoseMatch(const vr::HmdMatrix34_t& pose) const {
    std::unique_lock<std::mutex> lock(m_mutex);
    // Walk newest -> oldest so that ties resolve to the MOST RECENT frame.
    //
    // This comparison only looks at the 3x3 rotation, and the buffer is
    // push_back/pop_front, so begin() is the oldest entry (~4 s of history).
    // With the old forward walk plus a strict >, any tie kept the first match,
    // i.e. the oldest frame. Pure translation is exactly that case: the head's
    // orientation does not change, so every candidate scores ~0 and the match
    // landed on a frame up to 4 seconds old.
    //
    // The result feeds m_targetTimestampNs (OvrDirectModeComponent.cpp:186),
    // which becomes the frame index used for SteamVR-side frame alignment and
    // for the reprojection reference rotation (m_framePoseRotation). A 4-second
    // stale reference is invisible while the head only rotates (rotation gives
    // the candidates distinct scores) but wrong while it translates - which is
    // the reported symptom.
    float minDiff = 100000;
    auto minIt = m_poseBuffer.end();
    for (auto it = m_poseBuffer.rbegin(); it != m_poseBuffer.rend(); ++it) {
        float distance = 0;
        // Rotation matrix composes a part of ViewMatrix of TrackingInfo.
        // Be carefull of transpose.
        // And bottom side and right side of matrix should not be compared, because pPose does not
        // contain that part of matrix.
        for (int i = 0; i < 3; i++) {
            for (int j = 0; j < 3; j++) {
                distance += pow(it->rotationMatrix.m[j][i] - pose.m[j][i], 2);
            }
        }
        if (minDiff > distance) {
            minDiff = distance;
            minIt = std::next(it).base();
        }
    }
    if (minIt != m_poseBuffer.end()) {
        if (!m_poseBuffer.empty()) {
            m_lastMatchAgeMs
                = float(double(m_poseBuffer.back().targetTimestampNs - minIt->targetTimestampNs)
                    / 1'000'000.0);
        }

        // TEMPORARY DIAGNOSTIC: dump the RAW translations from both sides instead of
        // their difference. A constant zero difference can mean "they agree", or "both
        // are zero", or "one field was never filled" - printing the raw values is the
        // only way to tell those apart.
        //   rawSubmit  = pose.m[i][3]           : translation SteamVR put in mHmdPose
        //   rawMatched = motion.pose.position[i]: translation we will bake into the image
        {
            const float dx = minIt->motion.pose.position[0] - pose.m[0][3];
            const float dy = minIt->motion.pose.position[1] - pose.m[1][3];
            const float dz = minIt->motion.pose.position[2] - pose.m[2][3];
            m_lastMatchPosDeltaMm = sqrtf(dx * dx + dy * dy + dz * dz) * 1000.0f;

            m_lastSubmitPos[0] = pose.m[0][3];
            m_lastSubmitPos[1] = pose.m[1][3];
            m_lastSubmitPos[2] = pose.m[2][3];

            m_lastMatchedPos[0] = minIt->motion.pose.position[0];
            m_lastMatchedPos[1] = minIt->motion.pose.position[1];
            m_lastMatchedPos[2] = minIt->motion.pose.position[2];

            // How many entries separate the matched one from the newest. Confirms the
            // matcher really is picking different entries when matched_age moves.
            m_lastMatchIndexDelta = 0;
            for (auto it = std::next(minIt); it != m_poseBuffer.end(); ++it) {
                m_lastMatchIndexDelta++;
            }

            // TEMPORARY DIAGNOSTIC (search window sizing). POSESEL shows the selected
            // timestamps skip frames 4.89% of the time, so the rotation-only scoring is
            // picking entries that are not time-adjacent. Before narrowing the search to
            // the newest few entries we need to know how far back the match normally sits:
            // if nearly all matches are within a couple of entries, a small window is safe;
            // if the distribution has a long tail, a narrow window would start missing.
            {
                static std::atomic<uint64_t> n(0), maxIdx(0), sumIdx(0);
                static std::atomic<uint64_t> b0(0), b1(0), b2(0), b3(0), b4(0);
                const uint64_t d = (uint64_t)m_lastMatchIndexDelta;
                n.fetch_add(1);
                sumIdx.fetch_add(d);
                uint64_t pm = maxIdx.load();
                while (d > pm && !maxIdx.compare_exchange_weak(pm, d)) { }
                if (d == 0) {
                    b0.fetch_add(1);
                } else if (d <= 2) {
                    b1.fetch_add(1);
                } else if (d <= 5) {
                    b2.fetch_add(1);
                } else if (d <= 10) {
                    b3.fetch_add(1);
                } else {
                    b4.fetch_add(1);
                }

                const uint64_t i = n.load();
                if (DiagEnabled() && (i % 180 == 0)) {
                    Warn(
                        "MATCHIDX n=%llu avg=%.2f max=%llu | hist(=0,1-2,3-5,6-10,>10)=%llu/%llu/%llu/%llu/%llu\n",
                        (unsigned long long)i,
                        (double)sumIdx.load() / (double)i,
                        (unsigned long long)maxIdx.load(),
                        (unsigned long long)b0.load(),
                        (unsigned long long)b1.load(),
                        (unsigned long long)b2.load(),
                        (unsigned long long)b3.load(),
                        (unsigned long long)b4.load()
                    );
                }
            }

            m_lastMatchedVelocity[0] = minIt->motion.linearVelocity[0];
            m_lastMatchedVelocity[1] = minIt->motion.linearVelocity[1];
            m_lastMatchedVelocity[2] = minIt->motion.linearVelocity[2];
        }

        return *minIt;
    }

    Debug("PoseHistory::GetBestPoseMatch: No pose matched.");
    return {};
}

std::optional<PoseHistory::TrackingHistoryFrame> PoseHistory::GetPoseAt(uint64_t timestampNs
) const {
    std::unique_lock<std::mutex> lock(m_mutex);
    for (auto it = m_poseBuffer.rbegin(), end = m_poseBuffer.rend(); it != end; ++it) {
        if (it->targetTimestampNs == timestampNs)
            return *it;
    }

    Debug("PoseHistory::GetPoseAt: No pose matched.");
    return {};
}

void PoseHistory::SetTransform(const vr::HmdMatrix34_t& transform) {
    std::unique_lock<std::mutex> lock(m_mutex);
    m_transform = transform;

    for (int i = 0; i < 3; ++i) {
        for (int j = 0; j < 3; ++j) {
            if (transform.m[i][j] != (i == j ? 1 : 0)) {
                m_transformIdentity = false;
                return;
            }
        }
    }
    m_transformIdentity = true;
}
