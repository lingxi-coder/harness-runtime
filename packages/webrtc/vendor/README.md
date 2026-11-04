# nix iOS target fix

Source: crates.io nix 0.31.3 (MIT), copied from the Cargo registry. See nix/LICENSE.
The cfg guards change for: SockProtocol::KextControl and sockopt::UtunIfname
are macOS-only, matching libc availability; the private GetCString helper has the same guard. iOS libc has neither
SYSPROTO_CONTROL nor UTUN_OPT_IFNAME. The dependency arrives via rtc-shared.
No constants are fabricated and no replacement WebRTC stack is introduced.


# webrtc TURN close fix

Source: crates.io webrtc 0.21.1 (MIT OR Apache-2.0), archive SHA-256
`b621e736302bef784948cc8d8099fd0a8fdb5757693c80a344ffbe38c49387d2`.
Preserve the upstream license files.

The previous 0.20.3 fork queued authenticated zero-lifetime TURN Refreshes,
flushed them on both driver shutdown paths before dropping sockets, and gave
both driver types up to 250 ms to finish. Upstream 0.21.1 contains those
release and flush behaviors, including both runtime modes, via
https://github.com/webrtc-rs/webrtc/pull/912. The only local source change
retains the 250 ms shutdown bound instead of the upstream 100 ms bound, keeping
this below the embedding core's two-second close deadline.

The ignored forced-relay and same-port Coturn regressions exercise relay-only
negotiation, RTP, DataChannel, authentication failure and allocation cleanup.
