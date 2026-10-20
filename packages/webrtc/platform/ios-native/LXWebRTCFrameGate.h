#pragma once
#include <stdbool.h>
#include <stdint.h>

// Select the sample nearest the next deadline without moving the capture phase
// to an early callback. Callers use a nonzero fixed period and a monotonic clock.
static inline bool LXWebRTCAdmitBroadcastFrame(uint64_t now, uint64_t *deadline, uint64_t period) {
    if (*deadline && now < *deadline && *deadline - now > period / 2) return false;
    *deadline = *deadline && (now < *deadline || now - *deadline < period)
        ? *deadline + period : now + period;
    return true;
}
