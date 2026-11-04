# Clipboard dependency update: 5.4.1 to 5.4.3

The Harness lock previously selected `clipboard-win 5.4.1`. The official
[crates.io index](https://index.crates.io/cl/ip/clipboard-win) and
[package metadata](https://crates.io/api/v1/crates/clipboard-win), retrieved
2026-11-04, report 5.4.1 yanked and 5.4.3 not yanked. The new lock checksum,
`052af7878466feccda12ba7a6a67d019d712f296809559b47c12c8cc196d3a9a`, matches
the independently downloaded [release archive](https://static.crates.io/crates/clipboard-win/clipboard-win-5.4.3.crate)
and local Cargo cache. Its VCS metadata names upstream commit
[`ed27008ae5d1f10cfbb266de039f0fb35fd05a56`](https://github.com/DoumanAsh/clipboard-win/commit/ed27008ae5d1f10cfbb266de039f0fb35fd05a56).
The old archive checksum is
`bde03770d3df201d4fb868f2c9c59e66a3e4e2bd06692a0fe701e7103c7e84d4`.

The upstream [issue](https://github.com/DoumanAsh/clipboard-win/issues/42)
reports an out-of-bounds read when untrusted HTML fragment offsets point past
the clipboard data; the maintainer identifies 5.4.2 as containing the fix.
The [source comparison](https://github.com/DoumanAsh/clipboard-win/compare/3b27cf2bfd1adcfa6e0264eb51c1025ddaf0f342...ed27008ae5d1f10cfbb266de039f0fb35fd05a56)
shows no public API signature changes: 5.4.2 replaces the unchecked copy with
a bounded slice, and 5.4.3 changes the delay between failed clipboard-open
retries from `Sleep(0)` to `Sleep(5)`. The latter can increase waiting during
clipboard contention. Other changes are documentation, test and package
metadata. This is not a full security audit of the new package. No RustSec
exception, cargo-deny ignore, or cargo-vet audit is added.

All-features locked/frozen Cargo metadata for the Harness at base
`274e8f3ea6b955bccdf0534af63126eca69f5437` resolves 824 packages before and
after. The only package ID change is `clipboard-win 5.4.1` to `5.4.3`; after
normalizing that ID, every dependency edge is unchanged and the selected
feature remains `std`. The target-all reverse path is
`tool-computer-use --dev--> platform-macos-computer-control -> arboard 3.6.1 ->
clipboard-win`; the latter two links have opposing macOS and Windows target
conditions. Target-filtered trees on both `aarch64-apple-darwin` and
`x86_64-pc-windows-gnu` contain no clipboard-win node in this Harness graph.
Harness's macOS backend uses arboard's text methods; these observations do
not establish the reachability of other product graphs or the HTML issue.

A small independent fixture using the actual `arboard 3.6.1` with default
features disabled, `clipboard-win 5.4.3` with `std`, and `error-code 3.3.2`
passed `cargo +1.94.0 check --target x86_64-pc-windows-gnu --locked --frozen`.
This is a cross-target type check, not a Windows execution or clipboard test.
The workspace command selecting arboard directly for Windows instead hit a
Cargo resolver panic because that package is not active on that target; the
fixture supplies an actual Windows dependency edge and does not claim it is
active in Harness.

On local RustSec advisory database snapshot
`965bf9ad083b58e6340d65829d85217b18be9290`, offline locked cargo-deny before
and after each still failed on the three existing unmaintained `zbus_names`,
`zvariant`, and `zvariant_derive` advisories. The `clipboard-win 5.4.1`
yanked warning disappeared; the separate `async-executor 1.14.0` yanked
warning remained. This is an offline snapshot, not a complete current CI pass.
The parallel xcap graph reduction may remove the latter dependency and the
three advisories; it requires its own integrated graph and deny verification.
