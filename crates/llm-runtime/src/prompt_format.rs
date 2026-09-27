//! System-prompt cache-block formatter — pure Anthropic protocol.
//!
//! Owns the split of an assembled system-prompt string into
//! [`SystemBlock`] slices with appropriate [`CacheControl`] placement,
//! mirroring claude-code `splitSysPromptPrefix`
//! (`utils/api.ts:362-434`) + `buildSystemPromptBlocks`
//! (`services/api/claude.ts:3213-3237`).
//!
//! This is provider-protocol logic (Anthropic cache-breakpoint
//! placement), not prompt *content*. It references only
//! `crate::{SystemBlock, CacheControl, CacheScope}` and its own consts.
#![forbid(unsafe_code)]

use crate::{CacheControl, CacheScope, SystemBlock};

/// Opening literal of every assembled system prompt — LingXi's branded
/// equivalent of Claude Code's `DEFAULT_PREFIX`.
///
/// Private to this module: it is the formatting boundary the splitter
/// uses to detect the prefix bucket, not a content constant exposed to
/// callers.
const HEADER: &str = "You are LingXi, an agentic command-line coding assistant.";

/// Section separator — two LFs (one blank line). Value copied from
/// `locked_templates::SECTION_SEP` in the orchestrator; it is pure
/// formatting and carries no prompt content.
const SECTION_SEP: &str = "\n\n";

/// `SYSTEM_PROMPT_DYNAMIC_BOUNDARY` marker — byte-identical to
/// claude-code's constant (`constants/prompts.ts:114-115`). Separates
/// the static prefix of the system prompt (cacheable at `global` scope
/// under the 1P feature) from the dynamic suffix. LingXi does **not**
/// currently assemble this marker into the prompt, so the 1P global
/// path always falls through to the org default — see
/// [`split_system_blocks_with`] and the residual note in `build_request`.
pub const SYSTEM_PROMPT_DYNAMIC_BOUNDARY: &str = "__SYSTEM_PROMPT_DYNAMIC_BOUNDARY__";

/// Options controlling [`split_system_blocks_with`].
#[derive(Debug, Clone, Copy, Default)]
pub struct SplitOptions {
    /// When `true`, take the 1P global-cache path (`splitSysPromptPrefix` global
    /// mode, `utils/api.ts:362-410`): the prefix block is **un**cached
    /// (`cacheScope=null`), the static content before
    /// [`SYSTEM_PROMPT_DYNAMIC_BOUNDARY`] is `global`-scoped, and the dynamic
    /// content after it is `org`-scoped (NOT uncached — see `y8s`). When the
    /// boundary marker is absent
    /// (`boundaryIndex===-1`) this falls through to the org default — faithful to
    /// the TS `else` branch (`utils/api.ts:405-409`).
    pub global_scope: bool,
    /// When `true`, org/global breakpoints carry `ttl:'1h'`
    /// (claude-code `should1hCacheTTL`). Folded into the emitted
    /// [`CacheControl`].
    pub ttl_1h: bool,
}

/// Split an assembled system-prompt string into prompt-cache blocks,
/// mirroring claude-code `splitSysPromptPrefix` default mode
/// (`utils/api.ts:411-434`) + `buildSystemPromptBlocks`
/// (`services/api/claude.ts:3213-3237`).
///
/// claude-code's `SystemPrompt` is a `string[]` bucketed by content
/// match into three buckets — attribution header (`cacheScope=null`),
/// the CLI prefix (`cacheScope='org'`), and everything else joined with
/// `\n\n` (`cacheScope='org'`) — then `buildSystemPromptBlocks` maps
/// each block to a text block that carries `cache_control` **only when
/// the block's cacheScope is not null** and caching is enabled. The
/// attribution block (scope `null`) therefore never gets a breakpoint.
///
/// LingXi assembles ONE concatenated string (see
/// `assemble_system_prompt_with_style` in the orchestrator) whose
/// leading section is exactly the [`HEADER`] literal. It occupies the same
/// cache bucket as Claude Code's `DEFAULT_PREFIX`, but intentionally uses
/// LingXi's product identity. LingXi has no
/// `x-anthropic-billing-header` attribution machinery (it is GrowthBook
/// / Bun-attestation gated even in TS and is never emitted here), so
/// the attribution bucket is permanently empty. The faithful split for
/// LingXi's input domain is therefore the prefix bucket (`HEADER`) plus
/// the rest bucket (everything after the first section separator) — i.e.
/// the default 3-way split **minus the always-absent attribution block**:
///
/// * `s == HEADER`, or `s` does not start with `HEADER + SECTION_SEP`
///   (a `--system-prompt` override / preview bypass): a single block —
///   matches the TS splitter emitting only the non-empty buckets
///   (prefix-only or rest-only).
/// * `s` starts with `HEADER + SECTION_SEP`: two blocks — `HEADER`
///   (prefix, org-scoped) and the remainder (rest, org-scoped).
///
/// Both produced buckets are org-scoped, so each carries
/// [`CacheControl::Ephemeral`] when `enable_caching` is `true`, and
/// none when it is `false`. The order of the blocks matches
/// `req.system`.
#[must_use]
pub fn split_system_blocks(s: &str, enable_caching: bool) -> Vec<SystemBlock> {
    split_system_blocks_with(s, enable_caching, SplitOptions::default())
}

/// Core splitter — see [`split_system_blocks`] for the default-mode
/// contract and [`SplitOptions`] for the 1P global variant.
#[must_use]
pub fn split_system_blocks_with(
    s: &str,
    enable_caching: bool,
    opts: SplitOptions,
) -> Vec<SystemBlock> {
    // org breakpoint, ttl-aware: plain Ephemeral unless 1h is requested.
    let org_cc = || -> Option<CacheControl> {
        if !enable_caching {
            return None;
        }
        if opts.ttl_1h {
            Some(CacheControl::EphemeralScoped {
                scope: None,
                ttl_1h: true,
            })
        } else {
            Some(CacheControl::Ephemeral)
        }
    };
    // global breakpoint, ttl-aware.
    let global_cc = || -> Option<CacheControl> {
        enable_caching.then_some(CacheControl::EphemeralScoped {
            scope: Some(CacheScope::Global),
            ttl_1h: opts.ttl_1h,
        })
    };

    // The prefix bucket is exactly HEADER, present only when `s` begins with
    // `HEADER + SECTION_SEP` (the assembled-prompt shape). `SECTION_SEP` is the
    // boundary the assembler always emits between HEADER and the first body
    // section, so the rest bucket starts immediately after it.
    let prefix_boundary = {
        let mut b = String::with_capacity(HEADER.len() + SECTION_SEP.len());
        b.push_str(HEADER);
        b.push_str(SECTION_SEP);
        b
    };

    let Some(rest) = s.strip_prefix(&prefix_boundary) else {
        // No HEADER prefix (override / custom prompt) — single rest-only block,
        // org-scoped (the global path also treats a no-prefix body as the
        // rest/dynamic bucket; with no boundary it is org by the fallthrough).
        return vec![SystemBlock {
            text: s.to_string(),
            cache_control: org_cc(),
        }];
    };

    if rest.is_empty() {
        // HEADER followed by an empty body — prefix-only (degenerate).
        // (Prefix is org-scoped in default mode; in the global no-boundary
        // fallthrough it is likewise org.)
        return vec![SystemBlock {
            text: HEADER.to_string(),
            cache_control: org_cc(),
        }];
    }

    // 1P global path: only taken when the gate is on AND a boundary marker is
    // present in the body. Mirrors splitSysPromptPrefix global mode — prefix
    // uncached, static-before-boundary global-scoped, dynamic-after-boundary
    // uncached.
    if opts.global_scope {
        let marker = format!("{SECTION_SEP}{SYSTEM_PROMPT_DYNAMIC_BOUNDARY}{SECTION_SEP}");
        if let Some(idx) = rest.find(&marker) {
            let static_part = &rest[..idx];
            let dynamic_part = &rest[idx + marker.len()..];
            let mut out = Vec::with_capacity(3);
            // prefix: cacheScope=null → never cached.
            out.push(SystemBlock {
                text: HEADER.to_string(),
                cache_control: None,
            });
            if !static_part.is_empty() {
                out.push(SystemBlock {
                    text: static_part.to_string(),
                    cache_control: global_cc(),
                });
            }
            if !dynamic_part.is_empty() {
                // dynamic: cacheScope="org".
                //
                // This was `None` with a comment claiming "never cached". The
                // binary says otherwise — `y8s` (2.1.220 @237509234), the
                // boundary split, ends:
                //   let m = d.join("\n\n"); if (m) f.push({text:m, cacheScope:"global"});
                //   let g = p.join("\n\n"); if (g) f.push({text:g, cacheScope:"org"});
                // i.e. static-before-boundary is `global`, dynamic-after is
                // `org`. Only the billing header and the `Pdo` block are null.
                //
                // The old comment cited `utils/api.ts:405-409`, which no longer
                // describes this build — the citation outlived the behaviour.
                // Leaving the dynamic half uncached means paying full input
                // tokens for memory/env/output-style on EVERY request instead
                // of a cache read.
                out.push(SystemBlock {
                    text: dynamic_part.to_string(),
                    cache_control: org_cc(),
                });
            }
            return out;
        }
        // boundaryIndex === -1 → fall through to the org default below.
    }

    // Default / org mode (and global-no-boundary fallthrough): prefix + rest,
    // both org-scoped.
    vec![
        SystemBlock {
            text: HEADER.to_string(),
            cache_control: org_cc(),
        },
        SystemBlock {
            text: rest.to_string(),
            cache_control: org_cc(),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CacheControl;

    #[test]
    fn split_header_plus_rest_yields_two_org_blocks_with_cache() {
        let s = format!("{HEADER}{SECTION_SEP}rest section A\n\nrest section B");
        let blocks = split_system_blocks(&s, true);
        assert_eq!(blocks.len(), 2);
        // Prefix block is exactly HEADER, org-scoped → carries the breakpoint.
        assert_eq!(blocks[0].text, HEADER);
        assert_eq!(blocks[0].cache_control, Some(CacheControl::Ephemeral));
        // Rest block is everything after the first separator, org-scoped → breakpoint.
        assert_eq!(blocks[1].text, "rest section A\n\nrest section B");
        assert_eq!(blocks[1].cache_control, Some(CacheControl::Ephemeral));
    }

    #[test]
    fn split_omits_cache_control_when_caching_disabled() {
        let s = format!("{HEADER}{SECTION_SEP}rest");
        let blocks = split_system_blocks(&s, false);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].cache_control, None);
        assert_eq!(blocks[1].cache_control, None);
    }

    #[test]
    fn split_prefix_only_yields_single_block() {
        // HEADER with no body after the separator (degenerate) collapses to one
        // block, matching the TS splitter emitting only the non-empty prefix.
        let s = format!("{HEADER}{SECTION_SEP}");
        let blocks = split_system_blocks(&s, true);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].text, HEADER);
    }

    #[test]
    fn split_override_without_header_yields_single_rest_block() {
        // A custom / --system-prompt override that does not start with HEADER is
        // the rest-only bucket — one block.
        let s = "You are a custom assistant.\n\nDo X.";
        let blocks = split_system_blocks(s, true);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].text, s);
    }

    #[test]
    fn split_global_with_boundary_marks_only_static_global() {
        use crate::CacheScope;
        // Global mode + a boundary present: prefix uncached, static-before
        // global-scoped, dynamic-after uncached (splitSysPromptPrefix global).
        let s = format!(
            "{HEADER}{SECTION_SEP}static body{SECTION_SEP}{SYSTEM_PROMPT_DYNAMIC_BOUNDARY}{SECTION_SEP}dynamic body"
        );
        let opts = SplitOptions {
            global_scope: true,
            ttl_1h: false,
        };
        let blocks = split_system_blocks_with(&s, true, opts);
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0].text, HEADER);
        assert_eq!(blocks[0].cache_control, None); // prefix cacheScope=null
        assert_eq!(blocks[1].text, "static body");
        assert_eq!(
            blocks[1].cache_control,
            Some(CacheControl::EphemeralScoped {
                scope: Some(CacheScope::Global),
                ttl_1h: false
            })
        );
        assert_eq!(blocks[2].text, "dynamic body");
        // dynamic cacheScope="org" — `y8s` @237509234 ends
        //   if (m) f.push({text:m, cacheScope:"global"});   // static
        //   if (g) f.push({text:g, cacheScope:"org"});      // dynamic
        // This previously asserted `None`, matching a stale comment that cited
        // a TS line no longer describing this build. Uncached would mean paying
        // full input tokens for the dynamic half on every request.
        assert_eq!(blocks[2].cache_control, Some(CacheControl::Ephemeral));
    }

    #[test]
    fn split_global_without_boundary_falls_through_to_org() {
        // boundaryIndex === -1 → org default (prefix + rest, both org-scoped).
        let s = format!("{HEADER}{SECTION_SEP}no boundary here");
        let opts = SplitOptions {
            global_scope: true,
            ttl_1h: false,
        };
        let blocks = split_system_blocks_with(&s, true, opts);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].cache_control, Some(CacheControl::Ephemeral));
        assert_eq!(blocks[1].cache_control, Some(CacheControl::Ephemeral));
    }

    #[test]
    fn split_1h_ttl_folds_into_org_breakpoints() {
        let s = format!("{HEADER}{SECTION_SEP}rest");
        let opts = SplitOptions {
            global_scope: false,
            ttl_1h: true,
        };
        let blocks = split_system_blocks_with(&s, true, opts);
        assert_eq!(blocks.len(), 2);
        assert_eq!(
            blocks[0].cache_control,
            Some(CacheControl::EphemeralScoped {
                scope: None,
                ttl_1h: true
            })
        );
    }
}
