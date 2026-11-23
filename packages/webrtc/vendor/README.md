# nix iOS target fix

Source: crates.io nix 0.31.3 (MIT), copied from the Cargo registry. See nix/LICENSE.
The cfg guards change for: SockProtocol::KextControl and sockopt::UtunIfname
are macOS-only, matching libc availability; the private GetCString helper has the same guard. iOS libc has neither
SYSPROTO_CONTROL nor UTUN_OPT_IFNAME. The dependency arrives via rtc-shared.
No constants are fabricated and no replacement WebRTC stack is introduced.
