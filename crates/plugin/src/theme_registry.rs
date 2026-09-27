//! Live registry of plugin-declared custom themes (`plugin.json`'s `themes`
//! field, §14).
//!
//! `plugin::discovery` already parses `themes` (a `union([path, path[]])`
//! over directories/`.json` files, replacing — not merging with — the
//! `themes/` auto-scan) into `PluginComponents::themes`, but nothing
//! materializes it. Unlike workflows, this is not a wiring gap: **this port
//! has no theme subsystem at all** — no theme registry, no theme settings
//! key, no `custom:` slug handling anywhere in `tui`/`tui-core`/`traits`.
//! `tui_core::theme` only offers 6 fixed built-in [`BASE_THEME_NAMES`]. This
//! module ports the one piece that is coherent to build without a TUI
//! theme-*selection* surface to feed: the plugin side of the oracle's theme
//! host.
//!
//! Oracle (2.1.251, bundled `chunk-vp5jcrpf.js`). Class `x` (@163546801) is
//! the theme host: `pluginThemes` is a live store, distinct from
//! `userThemes`/`customThemeBases` (the user `themes/` directory + file
//! watcher side, which this port has no equivalent of and does not attempt —
//! see "Deliberately deferred" below). The loader that FILLS `pluginThemes`
//! (`w0e`/`Dre`, @178023368) runs once per enabled-plugin-list refresh:
//!
//! ```js
//! async function w0e(S) { // S = enabled plugins, each {name, themesPath?, themesPaths?}
//!   let x = [];
//!   for (let P of S) {
//!     let j = {plugin: P.name}, H = `${P.name}:`;
//!     if (P.themesPath) x.push(...await sMt(P.themesPath, j, H));
//!     for (let Z of P.themesPaths ?? []) x.push(...await sMt(Z, j, H));
//!   }
//!   return KR().addCustomThemeBases(x), y("plugin_load_themes"), x.sort(...);
//! }
//! async function Dre(S) { ...; let x = await w0e(S); KR().pluginThemes.setState(() => x); return x; }
//! ```
//!
//! and the per-file validator (`j(e,t,r)`, @163547684 — `e` is the
//! already-namespaced slug, `t` the raw file text, `r` a source tag this
//! port has no reader for):
//!
//! ```js
//! function j(e, t, r) {
//!   let o;
//!   try { o = V(t) } catch { warn(`[theme] ${e}.json: invalid JSON`); return }
//!   if (typeof o !== "object" || o === null || Array.isArray(o)) return; // silent skip
//!   let a = o, h = Pht(a.base) ? a.base : "dark", c = typeof a.name === "string" ? a.name : e, s = {};
//!   if (typeof a.overrides === "object" && a.overrides !== null) {
//!     let g = kk(h);
//!     for (let [p, y] of Object.entries(a.overrides))
//!       if (Object.hasOwn(g, p) && q2(y)) s[p] = y;
//!   }
//!   return {slug: e, name: c, base: h, overrides: s, source: r};
//! }
//! ```
//!
//! with a 256KB size cap on each file, checked BEFORE it is read
//! (`N = 262144`, @163546801: `"[theme] ${e} exceeds 256KB; skipping"`).
//!
//! Namespacing matches every other component slot: `{plugin}:{basename}`
//! (`H = ${P.name}:` joined with the file's basename minus `.json`) — this is
//! NOT the `custom:` prefix (oracle `k`/`IW`/`Lb`), which encodes a CHOSEN
//! theme *setting*'s wire value (`theme: "custom:{slug}"`); that encoding is
//! the deferred TUI theme-selection surface's concern, not the slug a theme
//! is stored under.
//!
//! **Deliberately thinner than the oracle** (`Object.hasOwn(kk(base), key)`
//! key-gating on `overrides`): the oracle checks each override key against
//! the resolved base theme's full palette object (~40 keys —
//! `accent`/`bashBorder`/`rainbow_*`/…, `kk()`/`v`/`Y`/`L`/`G`/`F`/`B`
//! @158497306). This port's `tui_core::theme::Theme` exposes only the 13
//! keys the TUI render path actually consumes, not that full palette, and
//! `plugin` does not (and should not) depend on `tui-core`. Only the
//! override VALUE's color-string FORMAT is validated here (oracle `q2`, see
//! [`is_valid_theme_color`]); key membership is left ungated until a
//! render-side palette large enough to check against exists.
//!
//! **Deliberately deferred** (per the batch brief): the TUI theme-*selection*
//! surface — a `/theme` picker, persisting a chosen theme, live re-render —
//! and therefore also `Aon`'s user-theme half (`cachedUserThemes()`) and the
//! `custom:` wire encoding (`IW`/`Lb`). [`resolve_theme`] ports `Aon`'s
//! PRECEDENCE rule (user themes checked before plugin themes) against an
//! INJECTED user-theme lookup, so a future user-theme store can be wired in
//! without revisiting this rule; passing `|_| None` (today's only caller)
//! reduces it to plugin-only resolution.

use std::collections::HashMap;
use std::sync::{LazyLock, RwLock};

/// The 6 base theme names a `themes/*.json` file's `base` key may reference
/// (oracle `lAn`, @154582240 — the same 6 names as `tui_core::theme::ThemeName`,
/// duplicated here rather than making `plugin` depend on `tui-core`).
pub const BASE_THEME_NAMES: [&str; 6] = [
    "dark",
    "light",
    "light-daltonized",
    "dark-daltonized",
    "light-ansi",
    "dark-ansi",
];

/// Oracle `N` (@163546801): a `themes/*.json` file larger than this is
/// skipped WITHOUT being read (`"[theme] {path} exceeds 256KB; skipping"`).
pub const MAX_THEME_FILE_BYTES: u64 = 262_144;

/// One plugin-declared custom theme (the oracle's theme record, minus the
/// `source` tag no caller here reads).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginThemeEntry {
    /// `{plugin}:{basename}` — see the module doc's namespacing rule.
    pub slug: String,
    /// The JSON body's own `name` string, or `slug` when absent/non-string.
    pub name: String,
    /// One of [`BASE_THEME_NAMES`]; falls back to `"dark"` when the body's
    /// `base` is missing or not a recognized name.
    pub base: String,
    /// Validated `{key: colorString}` overrides — VALUE format only; see the
    /// module doc's "Deliberately thinner than the oracle" note.
    pub overrides: HashMap<String, String>,
}

/// Outcome of validating one theme file's parsed body, mirroring the
/// oracle's three-way branch in `j(e,t,r)` (@163547684) so a caller can
/// reproduce its exact log behavior at each site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThemeParseOutcome {
    /// Valid JSON, right shape.
    Valid(PluginThemeEntry),
    /// Valid JSON, but not an object (`null`, an array, or a scalar) — the
    /// oracle returns `undefined` silently here, no warning.
    WrongShape,
    /// `JSON.parse` (`serde_json::from_str`) failed — the oracle warns
    /// `[theme] {slug}.json: invalid JSON`.
    InvalidJson,
}

/// Oracle `j(e, t, r)` minus the `source` tag (`slug` is already the
/// namespaced `e`; `raw` is the file's text).
#[must_use]
pub fn parse_theme_json(slug: &str, raw: &str) -> ThemeParseOutcome {
    let parsed: serde_json::Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => return ThemeParseOutcome::InvalidJson,
    };
    let Some(obj) = parsed.as_object() else {
        // `null`, an array, or a JSON scalar — oracle's
        // `typeof o!=="object"||o===null||Array.isArray(o)` guard.
        return ThemeParseOutcome::WrongShape;
    };
    let base = obj
        .get("base")
        .and_then(|v| v.as_str())
        .filter(|b| BASE_THEME_NAMES.contains(b))
        .unwrap_or("dark")
        .to_string();
    let name = obj
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| slug.to_string());
    let mut overrides = HashMap::new();
    if let Some(raw_overrides) = obj.get("overrides").and_then(|v| v.as_object()) {
        for (key, value) in raw_overrides {
            if let Some(color) = value.as_str() {
                if is_valid_theme_color(color) {
                    overrides.insert(key.clone(), color.to_string());
                }
            }
        }
    }
    ThemeParseOutcome::Valid(PluginThemeEntry {
        slug: slug.to_string(),
        name,
        base,
        overrides,
    })
}

static RGB_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^rgb\(\s?\d{1,3},\s?\d{1,3},\s?\d{1,3}\s?\)$").unwrap());
static HEX6_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^#[0-9a-fA-F]{6}$").unwrap());
static HEX3_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^#[0-9a-fA-F]{3}$").unwrap());
static ANSI256_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^ansi256\(\d{1,3}\)$").unwrap());

/// The 16 standard/bright ANSI color names an `ansi:<name>` override value
/// may reference (oracle `c`, @158479383 — the same 16-name set
/// `tui_core::theme::ansi` matches against).
const ANSI_NAMES: [&str; 16] = [
    "black",
    "red",
    "green",
    "yellow",
    "blue",
    "magenta",
    "cyan",
    "white",
    "blackBright",
    "redBright",
    "greenBright",
    "yellowBright",
    "blueBright",
    "magentaBright",
    "cyanBright",
    "whiteBright",
];

/// Oracle `q2(r)` (@158497484): `rgb(r,g,b)` (1-3 digit components, an
/// optional single space after `(`/each `,`/before `)`) / `#RGB` / `#RRGGBB`
/// / `ansi256(N)` (1-3 digits) / `ansi:<name>`.
#[must_use]
pub fn is_valid_theme_color(value: &str) -> bool {
    if RGB_RE.is_match(value)
        || HEX6_RE.is_match(value)
        || HEX3_RE.is_match(value)
        || ANSI256_RE.is_match(value)
    {
        return true;
    }
    value
        .strip_prefix("ansi:")
        .is_some_and(|name| ANSI_NAMES.contains(&name))
}

/// Live table of plugin-declared custom themes, keyed by namespaced slug
/// (oracle `pluginThemes` state).
///
/// **Loading is LAZY.** `load_plugin` records only `(slug, path)` pairs; the
/// stat + read + parse happens on the first `get` for that slug and the result
/// is cached. Nothing in the port reads this registry yet (see
/// `PluginManager::plugin_themes`), so eager loading would spend one
/// stat+read+parse per declared file per plugin enable — plus `[theme]` parse
/// warnings — for a feature no user can currently see. Deferring costs nothing
/// until a reader exists, and a reader gets identical results.
#[derive(Debug, Default)]
pub struct PluginThemeRegistry {
    by_slug: RwLock<HashMap<String, PluginThemeEntry>>,
    /// Declared-but-not-yet-read themes: slug -> absolute `.json` path.
    pending: RwLock<HashMap<String, std::path::PathBuf>>,
}

impl PluginThemeRegistry {
    /// A new, empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert (or overwrite) a batch of entries — one plugin's full
    /// contribution, called once per `load_plugin`. A slug collision across
    /// two plugins is last-registered-wins (the oracle rebuilds the WHOLE
    /// `pluginThemes` list from the full enabled-plugin set on each refresh,
    /// so its "last write" is list order, not registration order — a
    /// difference that only matters if two plugins declare the identical
    /// `{plugin}:{name}` slug, which requires two plugins sharing a name).
    pub fn register(&self, entries: Vec<PluginThemeEntry>) {
        if entries.is_empty() {
            return;
        }
        let mut guard = self.by_slug.write().unwrap_or_else(|e| e.into_inner());
        for entry in entries {
            guard.insert(entry.slug.clone(), entry);
        }
    }

    /// Remove exactly the named slugs (a single plugin's set, tracked by the
    /// caller) — the symmetric counterpart to `register`.
    pub fn unregister(&self, slugs: &[String]) {
        if slugs.is_empty() {
            return;
        }
        let mut guard = self.by_slug.write().unwrap_or_else(|e| e.into_inner());
        let mut pending = self.pending.write().unwrap_or_else(|e| e.into_inner());
        for slug in slugs {
            guard.remove(slug);
            pending.remove(slug);
        }
    }

    /// Record declared themes WITHOUT touching the filesystem. The stat, read
    /// and parse are deferred to the first [`Self::get`] for each slug.
    pub fn register_paths(&self, entries: Vec<(String, std::path::PathBuf)>) {
        if entries.is_empty() {
            return;
        }
        let mut guard = self.pending.write().unwrap_or_else(|e| e.into_inner());
        for (slug, path) in entries {
            guard.insert(slug, path);
        }
    }

    /// Oracle `Aon`'s plugin-side half: `pluginThemes.getState().find(slug)`.
    ///
    /// Materializes a pending entry on first call (size cap, then JSON parse —
    /// the same gates the eager path applied, in the same order), caching both
    /// success and failure so a bad file is not re-read on every lookup.
    #[must_use]
    pub fn get(&self, slug: &str) -> Option<PluginThemeEntry> {
        {
            let guard = self.by_slug.read().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = guard.get(slug) {
                return Some(entry.clone());
            }
        }
        let path = {
            let mut pending = self.pending.write().unwrap_or_else(|e| e.into_inner());
            pending.remove(slug)?
        };
        let entry = Self::materialize(slug, &path);
        if let Some(entry) = entry.clone() {
            let mut guard = self.by_slug.write().unwrap_or_else(|e| e.into_inner());
            guard.insert(slug.to_string(), entry);
        }
        entry
    }

    /// The deferred half of loading: the byte cap and parse the oracle's
    /// `j(e,t,r)` applies, warning on exactly the two cases it warns on.
    fn materialize(slug: &str, path: &std::path::Path) -> Option<PluginThemeEntry> {
        let metadata = std::fs::metadata(path).ok()?;
        if metadata.len() > MAX_THEME_FILE_BYTES {
            tracing::warn!(
                path = %path.display(),
                "[theme] {} exceeds 256KB; skipping",
                path.display()
            );
            return None;
        }
        let raw = std::fs::read_to_string(path).ok()?;
        match parse_theme_json(slug, &raw) {
            ThemeParseOutcome::Valid(entry) => Some(entry),
            ThemeParseOutcome::WrongShape => None,
            ThemeParseOutcome::InvalidJson => {
                tracing::warn!(slug = %slug, "[theme] {slug}.json: invalid JSON");
                None
            }
        }
    }

    /// All registered slugs — loaded and still-pending — sorted.
    #[must_use]
    pub fn slugs(&self) -> Vec<String> {
        let loaded = self.by_slug.read().unwrap_or_else(|e| e.into_inner());
        let pending = self.pending.read().unwrap_or_else(|e| e.into_inner());
        let mut slugs: Vec<String> = loaded.keys().chain(pending.keys()).cloned().collect();
        slugs.sort();
        slugs.dedup();
        slugs
    }
}

/// Oracle `Aon(slug)` (@163547194):
/// `cachedUserThemes().find(slug) ?? pluginThemes.getState().find(slug)` —
/// user themes win over plugin themes on a slug collision. This port has no
/// user-theme store, so the user-theme half is an INJECTED lookup rather
/// than a real store; `|_| None` (the only caller today) reduces this to
/// plugin-only resolution while keeping the PRECEDENCE rule itself ported
/// and tested.
#[must_use]
pub fn resolve_theme(
    slug: &str,
    user_lookup: impl FnOnce(&str) -> Option<PluginThemeEntry>,
    plugin_themes: &PluginThemeRegistry,
) -> Option<PluginThemeEntry> {
    user_lookup(slug).or_else(|| plugin_themes.get(slug))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(slug: &str) -> PluginThemeEntry {
        PluginThemeEntry {
            slug: slug.to_string(),
            name: slug.to_string(),
            base: "dark".to_string(),
            overrides: HashMap::new(),
        }
    }

    // ---------- parse_theme_json ----------

    #[test]
    fn invalid_json_is_reported_distinctly_from_wrong_shape() {
        assert_eq!(
            parse_theme_json("acme:dark-purple", "{not json"),
            ThemeParseOutcome::InvalidJson
        );
        assert_eq!(
            parse_theme_json("acme:dark-purple", "[1,2,3]"),
            ThemeParseOutcome::WrongShape
        );
        assert_eq!(
            parse_theme_json("acme:dark-purple", "null"),
            ThemeParseOutcome::WrongShape
        );
        assert_eq!(
            parse_theme_json("acme:dark-purple", "\"just a string\""),
            ThemeParseOutcome::WrongShape
        );
    }

    #[test]
    fn missing_base_and_name_fall_back() {
        let ThemeParseOutcome::Valid(theme) = parse_theme_json("acme:dark-purple", "{}") else {
            panic!("expected Valid");
        };
        assert_eq!(theme.slug, "acme:dark-purple");
        // Oracle: `c=typeof a.name==="string"?a.name:e` — falls back to the
        // NAMESPACED SLUG, not a bare basename.
        assert_eq!(theme.name, "acme:dark-purple");
        // Oracle: `h=Pht(a.base)?a.base:"dark"`.
        assert_eq!(theme.base, "dark");
        assert!(theme.overrides.is_empty());
    }

    #[test]
    fn unrecognized_base_falls_back_to_dark() {
        let ThemeParseOutcome::Valid(theme) = parse_theme_json("acme:x", r#"{"base":"solarized"}"#)
        else {
            panic!("expected Valid");
        };
        assert_eq!(theme.base, "dark");
    }

    #[test]
    fn recognized_base_and_name_are_kept() {
        let ThemeParseOutcome::Valid(theme) =
            parse_theme_json("acme:x", r#"{"base":"light-ansi","name":"Acme Light"}"#)
        else {
            panic!("expected Valid");
        };
        assert_eq!(theme.base, "light-ansi");
        assert_eq!(theme.name, "Acme Light");
    }

    #[test]
    fn overrides_keeps_only_valid_color_values() {
        let raw = r##"{
            "overrides": {
                "claude": "#ff6b80",
                "error": "rgb(255, 0, 0)",
                "warning": "ansi:yellowBright",
                "success": "ansi256(34)",
                "bogus_not_a_color": "not-a-color",
                "bogus_number": 5
            }
        }"##;
        let ThemeParseOutcome::Valid(theme) = parse_theme_json("acme:x", raw) else {
            panic!("expected Valid");
        };
        assert_eq!(theme.overrides.get("claude"), Some(&"#ff6b80".to_string()));
        assert_eq!(
            theme.overrides.get("error"),
            Some(&"rgb(255, 0, 0)".to_string())
        );
        assert_eq!(
            theme.overrides.get("warning"),
            Some(&"ansi:yellowBright".to_string())
        );
        assert_eq!(
            theme.overrides.get("success"),
            Some(&"ansi256(34)".to_string())
        );
        assert!(!theme.overrides.contains_key("bogus_not_a_color"));
        assert!(!theme.overrides.contains_key("bogus_number"));
        assert_eq!(theme.overrides.len(), 4);
    }

    #[test]
    fn overrides_array_yields_no_overrides() {
        // `typeof [] === "object"` in JS, so the oracle's shape guard admits
        // an array `overrides` — but every resulting `Object.entries` key is
        // a numeric-string index that fails the (unported here) `hasOwn`
        // base-key check, so the net effect is identical: zero overrides
        // survive either way.
        let ThemeParseOutcome::Valid(theme) =
            parse_theme_json("acme:x", r##"{"overrides":["#fff"]}"##)
        else {
            panic!("expected Valid");
        };
        assert!(theme.overrides.is_empty());
    }

    // ---------- is_valid_theme_color ----------

    #[test]
    fn color_format_accepts_every_oracle_form() {
        assert!(is_valid_theme_color("#abc"));
        assert!(is_valid_theme_color("#AABBCC"));
        assert!(is_valid_theme_color("rgb(1,2,3)"));
        assert!(is_valid_theme_color("rgb(255, 255, 255)"));
        assert!(is_valid_theme_color("ansi256(0)"));
        assert!(is_valid_theme_color("ansi256(255)"));
        assert!(is_valid_theme_color("ansi:red"));
        assert!(is_valid_theme_color("ansi:whiteBright"));
    }

    #[test]
    fn color_format_rejects_malformed_and_unknown_forms() {
        assert!(!is_valid_theme_color("not-a-color"));
        assert!(!is_valid_theme_color("#gggggg"));
        assert!(!is_valid_theme_color("#abcd")); // 4 hex digits: neither #3 nor #6
        assert!(!is_valid_theme_color("rgb(1,2)")); // missing a component
        assert!(!is_valid_theme_color("rgb(1,  2,3)")); // two spaces — oracle allows only one
        assert!(!is_valid_theme_color("ansi:orange")); // not one of the 16 names
        assert!(!is_valid_theme_color("ansi256()"));
    }

    // ---------- PluginThemeRegistry ----------

    #[test]
    fn register_then_get_round_trips() {
        let registry = PluginThemeRegistry::new();
        registry.register(vec![entry("acme:dark-purple")]);
        assert_eq!(
            registry.get("acme:dark-purple"),
            Some(entry("acme:dark-purple"))
        );
        assert_eq!(registry.get("acme:missing"), None);
    }

    #[test]
    fn unregister_removes_exactly_the_named_slugs() {
        let registry = PluginThemeRegistry::new();
        registry.register(vec![
            entry("acme:one"),
            entry("acme:two"),
            entry("other:keep"),
        ]);
        registry.unregister(&["acme:one".to_string(), "acme:two".to_string()]);
        assert_eq!(registry.get("acme:one"), None);
        assert_eq!(registry.get("acme:two"), None);
        assert_eq!(registry.get("other:keep"), Some(entry("other:keep")));
    }

    #[test]
    fn slugs_are_sorted() {
        let registry = PluginThemeRegistry::new();
        registry.register(vec![entry("zeta:z"), entry("alpha:a")]);
        assert_eq!(
            registry.slugs(),
            vec!["alpha:a".to_string(), "zeta:z".to_string()]
        );
    }

    // ---------- resolve_theme (Aon precedence) ----------

    /// Loading must be LAZY: `register_paths` records the path and performs no
    /// filesystem access, so a file that does not exist at registration time is
    /// still picked up by the first `get`.
    ///
    /// This is the decisive probe. Under the previous eager implementation the
    /// stat+read+parse happened at registration, so a theme written *after*
    /// registration was invisible forever. Writing the file only after
    /// `register_paths` returns therefore fails closed on any regression back
    /// to eager loading.
    #[test]
    fn register_paths_defers_all_io_until_the_first_get() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("midnight.json");

        let registry = PluginThemeRegistry::new();
        registry.register_paths(vec![("acme:midnight".to_string(), path.clone())]);

        assert!(
            !path.exists(),
            "precondition: the theme file must not exist yet"
        );
        assert_eq!(
            registry.slugs(),
            vec!["acme:midnight".to_string()],
            "a pending theme must still be listed before it is read"
        );

        // Only now does the file appear. An eager registry has already given up.
        std::fs::write(
            &path,
            r##"{"name":"Midnight","base":"dark","overrides":{"text":"#ffffff"}}"##,
        )
        .unwrap();

        let entry = registry.get("acme:midnight").expect(
            "register_paths must NOT read the file; the first get must, so a file written \
             after registration is still resolved",
        );
        assert_eq!(entry.slug, "acme:midnight");
    }

    #[test]
    fn user_theme_wins_over_a_same_slug_plugin_theme() {
        let plugin_themes = PluginThemeRegistry::new();
        let mut plugin_side = entry("acme:dark-purple");
        plugin_side.name = "Plugin's version".to_string();
        plugin_themes.register(vec![plugin_side]);

        let mut user_side = entry("acme:dark-purple");
        user_side.name = "User's version".to_string();
        let resolved = resolve_theme(
            "acme:dark-purple",
            |_| Some(user_side.clone()),
            &plugin_themes,
        );
        assert_eq!(resolved.map(|t| t.name), Some("User's version".to_string()));
    }

    #[test]
    fn falls_back_to_plugin_theme_when_no_user_theme_matches() {
        let plugin_themes = PluginThemeRegistry::new();
        plugin_themes.register(vec![entry("acme:dark-purple")]);
        let resolved = resolve_theme("acme:dark-purple", |_| None, &plugin_themes);
        assert_eq!(resolved, Some(entry("acme:dark-purple")));
    }

    #[test]
    fn resolves_to_none_when_neither_side_has_it() {
        let plugin_themes = PluginThemeRegistry::new();
        let resolved = resolve_theme("acme:missing", |_| None, &plugin_themes);
        assert_eq!(resolved, None);
    }
}
