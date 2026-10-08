//! Engine → sandbox-runtime config conversion.
//!
//! The engine's [`sandbox::runtime_config::SandboxRuntimeConfig`] is the
//! wire-shape mirror of claude-code's `SandboxSettingsSchema`. The
//! `sandbox-runtime` crate has its OWN [`sandbox_runtime::SandboxRuntimeConfig`]
//! (a faithful port of the npm package's zod schemas). The two are
//! field-compatible but not identical types, so the live runner translates the
//! engine value into the runtime value before handing it to a
//! [`sandbox_runtime::SandboxManager`].
//!
//! ## Mapping notes (engine fields that don't line up 1:1)
//!
//! - **`network.denied_domains`** — engine `Vec<String>` → runtime
//!   `Vec<String>`, forwarded verbatim. The engine populates this computed
//!   denylist from `WebFetch(domain:...)` DENY rules; the runtime checks it
//!   before the allow-list.
//! - **`network.strict_allowlist`** — engine `bool` → runtime `Option<bool>`
//!   as `true.then_some(true)`: the key exists only when true, mirroring the
//!   sandbox-adapter's `strictAllowlist: …||void 0` (2.1.220 @230074348). The
//!   runtime matcher denies unmatched hosts deterministically (never asks)
//!   when set.
//! - **`network.allow_unix_sockets`** — engine `Vec<String>` → runtime
//!   `Option<Vec<String>>`: `Some` when non-empty, `None` when empty (the
//!   runtime treats `None` and `Some(vec![])` identically, but `None` keeps the
//!   serialized shape minimal and matches "no override supplied").
//! - **`network.{allow_all_unix_sockets, allow_local_binding}`** — engine
//!   `bool` → runtime `Option<bool>`: always `Some(bool)` (the engine carries a
//!   concrete value; we pass it through verbatim).
//! - **`filesystem.allow_read`** — engine `Vec<String>` → runtime
//!   `Option<Vec<String>>`: `Some` when non-empty, else `None` (same rationale
//!   as `allow_unix_sockets`).
//! - **`ripgrep`** — engine `RipgrepConfig{command, args, argv0}` → runtime
//!   `Some(RipgrepConfig{command, args: Some(..) when non-empty, argv0})`. The
//!   engine carries `argv0` (embedded `argv0='rg'` dispatch); it is forwarded
//!   verbatim.
//! - **`enable_weaker_nested_sandbox` / `enable_weaker_network_isolation`** —
//!   engine `bool` → runtime `Option<bool>` as `Some(bool)`.
//! - **`allow_apple_events`** — engine `bool` → runtime `Option<bool>` as
//!   `Some(bool)`. Forwarded so the manager emits the macOS Apple Events block
//!   (`appleevent-send` + the `appleeventsd` mach-lookup + `lsopen`). claude-code
//!   parity: the sandbox-adapter sets `allowAppleEvents` on the srt config it
//!   initializes (2.1.207 @223928133) and the manager reads it (@216407085
//!   `Bjh(){return Cl?.allowAppleEvents}`). Before this, the live runner dropped
//!   it (`None` ⇒ manager reads `false`), so sandboxed `open`/`osascript`/
//!   browser-auth could never be enabled — a real behavioral gap vs CC.
//! - **`allow_pty`** — INTENTIONALLY left `None`. claude-code has NO `allowPty`
//!   settings key (2.1.207 @212793011 zod schema / @212939769 settings-key
//!   allowlist both omit it), and its sandbox-adapter never forwards one into the
//!   srt config — all 7 binary `allowPty` occurrences are internal to the
//!   vendored sandbox-runtime package, so in real CC usage it is always undefined
//!   ⇒ false. Forwarding the engine field here would be ANTI-parity. Only the
//!   legacy SBPL wrap path (`sandbox::wrap`) honors the engine field, for callers
//!   that set it directly.
//! - **`bwrap_path` / `socat_path`** — the engine config has NO such fields, so
//!   the runtime values are left `None` (the manager falls back to `$PATH`).
//!
//! Engine fields with no runtime counterpart on the *manager* path (`enabled`,
//! `fail_if_unavailable`, `auto_allow_bash_if_sandboxed`, `excluded_commands`,
//! `allow_unsandboxed_commands`, `allow_managed_domains_only`, the
//! `ro_bind_in_place`/`scrub_paths` host artifacts, …) are sandbox-DECISION
//! inputs consumed before the runner is ever called; they are intentionally not
//! forwarded into the runtime wrap config.

use sandbox::runtime_config::SandboxRuntimeConfig as EngineConfig;
use sandbox_runtime::SandboxRuntimeConfig as RuntimeConfig;
use sandbox_runtime::config::{FilesystemConfig, NetworkConfig, RipgrepConfig};

/// `Some(v)` when the vector is non-empty, else `None`.
fn some_if_nonempty(v: Vec<String>) -> Option<Vec<String>> {
    if v.is_empty() { None } else { Some(v) }
}

/// Convert the engine's sandbox config into the `sandbox-runtime` config the
/// [`sandbox_runtime::SandboxManager`] consumes. See the module docs for the
/// per-field mapping rationale.
#[must_use]
pub fn to_runtime_config(engine: &EngineConfig) -> RuntimeConfig {
    let net = &engine.network;
    let fs = &engine.filesystem;

    let network = NetworkConfig {
        allowed_domains: net.allowed_domains.clone(),
        // Computed denylist (from WebFetch deny rules) — forwarded verbatim.
        denied_domains: net.denied_domains.clone(),
        // strictAllowlist (2.1.219): the adapter sets the key only when true
        // (`…some(===!0)||void 0`, 2.1.220 @230074348), so engine `false` maps
        // to `None`, not `Some(false)`. The runtime matcher then denies
        // unmatched hosts deterministically instead of consulting the ask
        // callback.
        //
        // It MUST cross this boundary: the engine is what resolves the flag
        // (source-restricted to user/managed/CLI) but only the runtime matcher
        // can act on it. Dropping it here is how the flag came to be honored in
        // settings and ignored in practice.
        strict_allowlist: net.strict_allowlist.then_some(true),
        allow_unix_sockets: some_if_nonempty(net.allow_unix_sockets.clone()),
        allow_all_unix_sockets: Some(net.allow_all_unix_sockets),
        allow_local_binding: Some(net.allow_local_binding),
        allow_mach_lookup: None,
        http_proxy_port: net.http_proxy_port,
        socks_proxy_port: net.socks_proxy_port,
        mitm_proxy: None,
        tls_terminate: None,
        parent_proxy: None,
    };

    let filesystem = FilesystemConfig {
        deny_read: fs.deny_read.clone(),
        allow_read: some_if_nonempty(fs.allow_read.clone()),
        allow_write: fs.allow_write.clone(),
        deny_write: fs.deny_write.clone(),
        allow_git_config: None,
    };

    let ripgrep = RipgrepConfig {
        command: engine.ripgrep.command.clone(),
        args: some_if_nonempty(engine.ripgrep.args.clone()),
        argv0: engine.ripgrep.argv0.clone(),
    };

    RuntimeConfig {
        network,
        filesystem,
        // Forward the engine's computed ignore-violations map
        // (sandbox-adapter.ts:375 — `ignoreViolations: settings.sandbox?.ignoreViolations`).
        // `Some` when non-empty, else `None` (the runtime treats `None` and
        // `Some(empty)` identically; `None` keeps the serialized shape minimal,
        // matching the allow_read / allow_unix_sockets convention above).
        ignore_violations: if engine.ignore_violations.is_empty() {
            None
        } else {
            Some(engine.ignore_violations.clone())
        },
        enable_weaker_nested_sandbox: Some(engine.enable_weaker_nested_sandbox),
        enable_weaker_network_isolation: Some(engine.enable_weaker_network_isolation),
        // Forward the engine's resolved allowAppleEvents so the manager emits the
        // macOS Apple Events block. claude-code parity: the sandbox-adapter sets
        // `allowAppleEvents` on the srt config (2.1.207 @223928133) and the
        // manager reads it (@216407085). See the module mapping notes.
        allow_apple_events: Some(engine.allow_apple_events),
        ripgrep: Some(ripgrep),
        mandatory_deny_search_depth: None,
        // KEEP `None`: claude-code's sandbox-adapter NEVER forwards `allowPty`
        // into the srt config (it has no `allowPty` settings key — 2.1.207
        // @212793011 / @212939769). Forwarding the engine field would be
        // anti-parity. See the module mapping notes.
        allow_pty: None,
        seccomp: None,
        // Engine config has no bwrap/socat path overrides — fall back to $PATH.
        bwrap_path: None,
        socat_path: None,
        windows: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sandbox::runtime_config::{
        FilesystemRestrictionConfig, NetworkRestrictionConfig, RipgrepConfig as EngineRipgrep,
        SandboxRuntimeConfig as EngineConfig,
    };

    fn engine_full() -> EngineConfig {
        EngineConfig {
            network: NetworkRestrictionConfig {
                allowed_domains: vec!["github.com".into(), "*.npmjs.org".into()],
                denied_domains: vec![],
                allow_managed_domains_only: true,
                strict_allowlist: false,
                allow_unix_sockets: vec!["/tmp/sock".into()],
                allow_all_unix_sockets: true,
                allow_local_binding: true,
                http_proxy_port: Some(8080),
                socks_proxy_port: Some(1080),
                allow_mach_lookup: vec![],
            },
            filesystem: FilesystemRestrictionConfig {
                disabled: false,
                allow_write: vec!["/work".into()],
                deny_write: vec!["/work/.git".into()],
                deny_read: vec!["/secret".into()],
                allow_read: vec!["/secret/ok".into()],
                allow_managed_read_paths_only: false,
                allow_git_config: false,
            },
            ripgrep: EngineRipgrep {
                command: "rg".into(),
                args: vec!["--hidden".into()],
                argv0: None,
            },
            enable_weaker_nested_sandbox: true,
            enable_weaker_network_isolation: true,
            ..Default::default()
        }
    }

    #[test]
    fn maps_domains_and_ports() {
        let rt = to_runtime_config(&engine_full());
        assert_eq!(
            rt.network.allowed_domains,
            vec!["github.com", "*.npmjs.org"]
        );
        // denied_domains forwarded verbatim (engine_full carries none).
        assert_eq!(
            rt.network.denied_domains,
            engine_full().network.denied_domains
        );
        assert_eq!(rt.network.http_proxy_port, Some(8080));
        assert_eq!(rt.network.socks_proxy_port, Some(1080));
    }

    #[test]
    fn denied_domains_and_argv0_carry_through() {
        let mut engine = engine_full();
        engine.network.denied_domains = vec!["evil.com".into()];
        engine.ripgrep.argv0 = Some("rg".into());
        let rt = to_runtime_config(&engine);
        assert_eq!(rt.network.denied_domains, vec!["evil.com".to_string()]);
        let rg = rt.ripgrep.expect("ripgrep is always Some");
        assert_eq!(rg.argv0, Some("rg".to_string()));
    }

    #[test]
    fn maps_bool_flags_to_some() {
        let rt = to_runtime_config(&engine_full());
        assert_eq!(rt.network.allow_all_unix_sockets, Some(true));
        assert_eq!(rt.network.allow_local_binding, Some(true));
        assert_eq!(rt.enable_weaker_nested_sandbox, Some(true));
        assert_eq!(rt.enable_weaker_network_isolation, Some(true));
    }

    #[test]
    fn forwards_strict_allowlist_true_as_some_true() {
        // Engine strictAllowlist=true reaches the srt config as Some(true) so
        // the runtime matcher denies unmatched hosts before the ask callback
        // (2.1.219 strictAllowlist; adapter shape @230074348).
        let mut engine = engine_full();
        engine.network.strict_allowlist = true;
        let rt = to_runtime_config(&engine);
        assert_eq!(rt.network.strict_allowlist, Some(true));
    }

    #[test]
    fn strict_allowlist_false_is_omitted_not_some_false() {
        // `…||void 0`: the key must be ABSENT when false — never Some(false) —
        // so the serialized runtime config only ever carries the key as `true`.
        let rt = to_runtime_config(&engine_full());
        assert!(rt.network.strict_allowlist.is_none());
    }

    #[test]
    fn maps_filesystem_paths() {
        let rt = to_runtime_config(&engine_full());
        assert_eq!(rt.filesystem.allow_write, vec!["/work"]);
        assert_eq!(rt.filesystem.deny_write, vec!["/work/.git"]);
        assert_eq!(rt.filesystem.deny_read, vec!["/secret"]);
        assert_eq!(
            rt.filesystem.allow_read,
            Some(vec!["/secret/ok".to_string()])
        );
    }

    #[test]
    fn maps_ripgrep() {
        let rt = to_runtime_config(&engine_full());
        let rg = rt.ripgrep.expect("ripgrep is always Some");
        assert_eq!(rg.command, "rg");
        assert_eq!(rg.args, Some(vec!["--hidden".to_string()]));
        assert!(rg.argv0.is_none());
    }

    #[test]
    fn maps_nonempty_unix_sockets_to_some() {
        let rt = to_runtime_config(&engine_full());
        assert_eq!(
            rt.network.allow_unix_sockets,
            Some(vec!["/tmp/sock".to_string()])
        );
    }

    #[test]
    fn empty_vectors_become_none() {
        // Default engine config: empty allow_read / allow_unix_sockets / args.
        let rt = to_runtime_config(&EngineConfig::default());
        assert!(rt.network.allow_unix_sockets.is_none());
        assert!(rt.filesystem.allow_read.is_none());
        let rg = rt.ripgrep.expect("ripgrep is always Some");
        assert!(rg.args.is_none());
    }

    #[test]
    fn defaults_map_bools_to_some_false() {
        let rt = to_runtime_config(&EngineConfig::default());
        assert_eq!(rt.network.allow_all_unix_sockets, Some(false));
        assert_eq!(rt.network.allow_local_binding, Some(false));
        assert_eq!(rt.enable_weaker_nested_sandbox, Some(false));
        assert_eq!(rt.enable_weaker_network_isolation, Some(false));
        // No path overrides ever come from the engine config.
        assert!(rt.bwrap_path.is_none());
        assert!(rt.socat_path.is_none());
    }

    #[test]
    fn ignore_violations_forwarded_when_nonempty() {
        let mut engine = EngineConfig::default();
        engine
            .ignore_violations
            .insert("fs.read".to_string(), vec!["~/.cache".to_string()]);
        let rt = to_runtime_config(&engine);
        let iv = rt.ignore_violations.expect("ignore_violations forwarded");
        assert_eq!(iv.get("fs.read"), Some(&vec!["~/.cache".to_string()]));
    }

    #[test]
    fn ignore_violations_none_when_empty() {
        let rt = to_runtime_config(&EngineConfig::default());
        assert!(rt.ignore_violations.is_none());
    }

    #[test]
    fn forwards_allow_apple_events_true() {
        // Engine allowAppleEvents=true must reach the srt config as Some(true) so
        // the manager emits the macOS Apple Events block (CC parity @223928133).
        let mut engine = engine_full();
        engine.allow_apple_events = true;
        let rt = to_runtime_config(&engine);
        assert_eq!(rt.allow_apple_events, Some(true));
    }

    #[test]
    fn forwards_allow_apple_events_default_false() {
        // Default engine ⇒ Some(false), matching the Some(bool) convention used
        // for enable_weaker_*.
        let rt = to_runtime_config(&EngineConfig::default());
        assert_eq!(rt.allow_apple_events, Some(false));
    }

    #[test]
    fn allow_pty_always_none_regardless_of_engine() {
        // CC-parity guard: claude-code's sandbox-adapter has NO `allowPty`
        // settings key and never forwards one into the srt config (all 7 binary
        // `allowPty` hits are internal to the vendored sandbox-runtime package —
        // 2.1.207 @212793011 / @212939769). The live runner must therefore leave
        // `allow_pty: None` even when the engine field is set. Forwarding it would
        // be anti-parity; do NOT "fix" this into `Some(engine.allow_pty)`.
        let mut engine = engine_full();
        engine.allow_pty = true;
        let rt = to_runtime_config(&engine);
        assert!(
            rt.allow_pty.is_none(),
            "allow_pty must stay None to match CC (adapter never forwards allowPty)"
        );
        assert!(
            to_runtime_config(&EngineConfig::default())
                .allow_pty
                .is_none()
        );
    }

    #[test]
    fn default_config_validates() {
        // The converted config must pass the runtime's own validation so
        // `SandboxManager::initialize` accepts it.
        let rt = to_runtime_config(&EngineConfig::default());
        rt.validate()
            .expect("converted default config must validate");
        let rt_full = to_runtime_config(&engine_full());
        rt_full
            .validate()
            .expect("converted full config must validate");
    }
}
