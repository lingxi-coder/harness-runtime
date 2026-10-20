# nix iOS target fix

Source: crates.io nix 0.31.3 (MIT), copied from the Cargo registry. See nix/LICENSE.
The cfg guards change for: SockProtocol::KextControl and sockopt::UtunIfname
are macOS-only, matching libc availability; the private GetCString helper has the same guard. iOS libc has neither
SYSPROTO_CONTROL nor UTUN_OPT_IFNAME. The dependency arrives via rtc-shared.
No constants are fabricated and no replacement WebRTC stack is introduced.


# webrtc TURN close fix

Source: crates.io webrtc 0.20.3 (MIT OR Apache-2.0), copied unchanged except
three runtime files. The registry archive and original manifests identify the
pinned version. Preserve the upstream license files.

- turn_relayer.rs: queue authenticated zero-lifetime Refresh before dropping an
  allocation, as required by RFC 8656 section 8.
- driver.rs: flush queued release packets before ending either shutdown path.
- mod.rs: allow both shared and dedicated drivers up to 250 ms to finish before
  aborting; this stays inside the embedding core's two-second close deadline.

This fixes fixed-source-port reconnect collisions and retained relay allocations;
it does not add another transport or change ordinary SDP/DataChannel ownership.
The ignored same-port Coturn regression exercises repeated real allocation/close.
