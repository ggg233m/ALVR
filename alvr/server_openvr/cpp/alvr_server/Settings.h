#pragma once

#include "bindings.h"

const Settings* Settings_Instance();

// Single predicate for the high-frequency timing instrumentation.
//
// Every diagnostic probe added during the stutter investigation is guarded by this.
// The probes are not free: they walk the pose buffer, accumulate atomics, and format a
// log line every 90-180 frames. Running them unconditionally (which is what the code did
// while the investigation was in progress) costs measurable CPU and perturbs the timing
// being measured, so they are off unless ExtraConfig::enable_diagnostics is set.
//
// Usage:
//     if (DiagEnabled()) {
//         ... probe ...
//     }
//
// The check is a single bool load on a pointer that is already hot in cache, so the
// guard itself is effectively free.
inline bool DiagEnabled() {
    const Settings* s = Settings_Instance();
    return s != nullptr && s->m_enableDiagnostics;
}
