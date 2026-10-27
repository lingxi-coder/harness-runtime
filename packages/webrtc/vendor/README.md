# nix iOS target fix

Source: crates.io nix 0.31.3 (MIT), copied from the Cargo registry. See nix/LICENSE.
The cfg guards change for: SockProtocol::KextControl and sockopt::UtunIfname
are macOS-only, matching libc availability; the private GetCString helper has the same guard. iOS libc has neither
SYSPROTO_CONTROL nor UTUN_OPT_IFNAME. The dependency arrives via rtc-shared.
No constants are fabricated and no replacement WebRTC stack is introduced.


# webrtc TURN close

The workspace uses crates.io webrtc 0.21.1 (MIT OR Apache-2.0). Its upstream
driver queues and flushes authenticated Refresh(LIFETIME=0) requests on close
with a bounded wait for driver shutdown, replacing the former local 0.20.3 fix.
Against a local Coturn 4.18 server, the ignored transport tests verified 20
repeated allocations on the same source port and relay-only binary, audio and
camera/screen video exchange. This is a loopback result, not device or NAT acceptance.
