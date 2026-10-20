#include "../ios-native/LXWebRTCFrameGate.h"
#include <assert.h>
#include <stdio.h>

int main(void) {
    const uint64_t period = 33333333;
    uint64_t deadline = 0;
    assert(LXWebRTCAdmitBroadcastFrame(1000000000, &deadline, period));
    assert(deadline == 1000000000 + period);
    const uint64_t first = deadline;
    // A 60 Hz callback can land just before the 30 Hz deadline. Admission must
    // retain the 30 Hz phase, rather than underflowing unsigned subtraction.
    assert(!LXWebRTCAdmitBroadcastFrame(first - period / 2 - 1, &deadline, period));
    assert(deadline == first);
    assert(LXWebRTCAdmitBroadcastFrame(first - 1000000, &deadline, period));
    assert(deadline == first + period);
    assert(!LXWebRTCAdmitBroadcastFrame(first, &deadline, period));
    assert(LXWebRTCAdmitBroadcastFrame(first + period + 1000000, &deadline, period));
    assert(deadline == first + 2 * period);
    // A stalled producer rebases once; it must not burst to catch up.
    const uint64_t resumed = first + 10 * period;
    assert(LXWebRTCAdmitBroadcastFrame(resumed, &deadline, period));
    assert(deadline == resumed + period);
    assert(!LXWebRTCAdmitBroadcastFrame(resumed, &deadline, period));
    deadline = 0; // ReplayKit resume starts a fresh phase.
    assert(LXWebRTCAdmitBroadcastFrame(resumed, &deadline, period));
    assert(deadline == resumed + period);
    puts("Broadcast frame admission: all checks passed");
    return 0;
}
