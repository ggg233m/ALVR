#include "IDRScheduler.h"

#include "Utils.h"
#include <algorithm>
#include <mutex>

IDRScheduler::IDRScheduler() { }

IDRScheduler::~IDRScheduler() { }

void IDRScheduler::OnStreamStart() {
    m_minIDRFrameInterval = Settings_Instance()->m_minimumIdrIntervalMs * 1000;
    m_scheduled = false;
    m_lastIDRTime = 0;
    InsertIDR();
}

void IDRScheduler::InsertIDR() {
    std::unique_lock lock(m_mutex);

    if (m_scheduled) {
        return;
    }

    const uint64_t now = GetTimestampUs();
    const uint64_t earliest = m_lastIDRTime + m_minIDRFrameInterval;
    m_insertIDRTime = (std::max)(now, earliest);
    m_scheduled = true;
}

bool IDRScheduler::CheckIDRInsertion() {
    std::unique_lock lock(m_mutex);

    if (m_scheduled && m_insertIDRTime <= GetTimestampUs()) {
        m_scheduled = false;
        m_lastIDRTime = GetTimestampUs();
        return true;
    }
    return false;
}
