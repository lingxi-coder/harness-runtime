//! `sandbox-runtime` is a complete behavioral 1:1 Rust rewrite of the
//! `@anthropic-ai/sandbox-runtime@0.0.54` npm package (vendored reference at
//! `docs/superpowers/references/sandbox-runtime-0.0.54/`). It provides OS-level
//! filesystem and network sandboxing for arbitrary processes via `bubblewrap`
//! (Linux), `sandbox-exec` (macOS), and `AppContainer` (Windows), with
//! proxy-based network domain filtering: a host HTTP/`SOCKS` forward proxy plus
//! an optional TLS-MITM layer, bridged into the bwrap `--unshare-net` network
//! namespace via `socat`.
//!
//! Built in phases (see the umbrella design spec under
//! `docs/superpowers/specs/`). Landed so far: the pure core (domain pattern
//! grammar plus host matcher in [`config`], [`host`], [`matcher`]), parent-proxy
//! plus `NO_PROXY` resolution and the CONNECT tunnel dialers in [`parent_proxy`],
//! CONNECT-target parsing plus the bounded direct dial in [`dial`], the
//! per-request filter body hook in [`request_filter`], and the unified hyper
//! forward proxy (plain-HTTP forwarding + `CONNECT` tunnelling + parent-proxy
//! chaining) in [`http_proxy`]. Remaining: the `socat`/bwrap bridge, `SOCKS5`,
//! TLS-MITM (P6), seccomp, the `sandbox-manager` orchestration, the
//! macOS/Windows backends, and the CLI.

//! # Public API (the `index.js` surface)
//!
//! The crate re-exports a flat public API mirroring the npm package's
//! `index.js`, so consumers can `use sandbox_runtime::{SandboxManager, ...}`
//! without reaching into the module tree:
//!
//! - [`SandboxManager`] — the orchestrator ([`manager`]).
//! - [`SandboxViolationStore`] + [`Violation`] — the violation store
//!   ([`violation_store`]).
//! - The config types [`SandboxRuntimeConfig`], [`NetworkConfig`],
//!   [`FilesystemConfig`], [`IgnoreViolationsConfig`], [`RipgrepConfig`],
//!   [`WindowsConfig`] (the Rust structs replace the TS zod `*Schema` exports)
//!   ([`config`]).
//! - The Windows status/path API [`get_srt_win_path`] plus the ported status
//!   command-arg/parse helpers ([`group_status_args`] / [`parse_group_status`]
//!   and [`wfp_status_args`] / [`parse_wfp_status`] — the Rust split of the TS
//!   `getWindowsGroupStatus`/`getWindowsWfpStatus`, which build an argv and
//!   parse its stdout), and the consts [`DEFAULT_WINDOWS_GROUP_NAME`] /
//!   [`DEFAULT_WINDOWS_PROXY_PORT_RANGE`] ([`windows`]). The admin
//!   install/uninstall flow ([`install_windows_sandbox`],
//!   [`uninstall_windows_sandbox`], [`delete_windows_group`],
//!   [`create_windows_group`], [`create_windows_wfp`],
//!   [`windows_install_instructions`]) is also re-exported (matching
//!   `index.js`).
//! - [`get_default_write_paths`] — the default-write-path utility
//!   ([`path_utils`]).

#![forbid(unsafe_code)]

pub use crate::config::{
    FilesystemConfig, IgnoreViolationsConfig, NetworkConfig, RipgrepConfig, SandboxRuntimeConfig,
    WindowsConfig,
};
pub use crate::manager::SandboxManager;
pub use crate::path_utils::get_default_write_paths;
pub use crate::violation_store::{SandboxViolationStore, Violation};
pub use crate::windows::{
    create_windows_group, create_windows_wfp, delete_windows_group, get_srt_win_path,
    group_status_args, install_windows_sandbox, parse_group_status, parse_wfp_status,
    uninstall_windows_sandbox, wfp_status_args, windows_install_instructions,
    DEFAULT_WINDOWS_GROUP_NAME, DEFAULT_WINDOWS_PROXY_PORT_RANGE,
};

pub mod config;
pub mod dial;
pub mod env;
pub mod fs_args;
pub mod host;
pub mod http_proxy;
pub mod linux;
pub mod macos;
pub mod manager;
pub mod matcher;
pub mod mitm_ca;
pub mod mitm_leaf;
pub mod parent_proxy;
pub mod path_utils;
pub mod request_filter;
pub mod socks_proxy;
pub mod tls_terminate;
pub mod violation_store;
pub mod windows;
