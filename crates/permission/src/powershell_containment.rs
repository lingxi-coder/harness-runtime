//! PowerShell command **path-containment** — byte-faithful port of claude-code's
//! `powershellPermissions.ts` `validatePowerShellCommandPaths` (`xgg`/`Z_u`,
//! binary 2.1.206 @218426477+). Sibling of the bash [`crate::command_path_containment`]
//! guard: it covers the path arguments of PowerShell cmdlets so a
//! `Get-Content`/`Set-Content`/`Out-File`/`Remove-Item`/… targeting a path
//! OUTSIDE the allowed working directories ASKS (or denies) instead of running.
//!
//! # Architecture (matches claude-code exactly)
//!
//! claude-code does NOT hand-parse PowerShell. It base64-encodes the command and
//! shells out to `pwsh` (`[System.Management.Automation.Language.Parser]::ParseInput`
//! via an embedded script), receives the AST as JSON, transforms it into a small
//! node model, then runs the pure validation [`validate_statements`] (`Z_u`) over
//! it. When `pwsh` is absent or the command does not parse, claude-code returns
//! `passthrough` — NO containment — so the guard is inert on hosts without
//! PowerShell (the port's macOS/Linux/mobile targets).
//!
//! This module is built bottom-up. THIS file is the pure, subprocess-free
//! foundation: the cmdlet maps + normalizers ([`normalize_cmdlet`] `y_`, the
//! alias table `hhe`, the read-only set `Rgg`, and the per-cmdlet path-param
//! config `FKn`). The `pwsh` subprocess + JSON-AST transform, the `X_u` path
//! extractor, and the `xgg` validation land in follow-up units against this base.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

/// Whether a cmdlet reads, writes, or creates — the `operationType` field of a
/// claude-code `FKn` entry (only `read`/`write` are used by the table; `Create`
/// is reserved for the `NKn`/`xgg` write-vs-create path branches).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PsOperation {
    /// A read/inspect cmdlet (e.g. `Get-Content`, `Test-Path`).
    Read,
    /// A write/modify/delete cmdlet (e.g. `Set-Content`, `Remove-Item`).
    Write,
    /// A create cmdlet — reserved for the `NKn`/`xgg` write-vs-create branches.
    Create,
}

/// Per-cmdlet path-parameter configuration — a claude-code `FKn[cmdlet]` entry.
///
/// `X_u` uses this to decide which of a command's arguments are FILE PATHS:
/// `path_params` (and `leaf_only_path_params`) name the parameters whose value
/// is a path; `known_switches` are boolean flags (no value); `known_value_params`
/// take a non-path value that must be skipped; `positional_skip` positional args
/// are ignored before positional paths begin (e.g. `Invoke-WebRequest`'s URI);
/// `optional_write` marks a write whose target path may legitimately be absent.
#[derive(Debug, Clone)]
pub struct FkEntry {
    /// Whether this cmdlet reads, writes, or creates.
    pub operation_type: PsOperation,
    /// Parameters whose value is a file path (e.g. `-Path`, `-LiteralPath`).
    pub path_params: &'static [&'static str],
    /// Boolean flags that take no value (e.g. `-Recurse`, `-Force`).
    pub known_switches: &'static [&'static str],
    /// Parameters that take a non-path value which must be skipped (e.g. `-Value`).
    pub known_value_params: &'static [&'static str],
    /// `leafOnlyPathParams` — parameters (e.g. `New-Item -Name`) whose value is a
    /// bare leaf name; a value containing a separator / `.` / `..` is treated as
    /// un-validatable (forces `hasUnvalidatablePathArg`) rather than a path.
    pub leaf_only_path_params: &'static [&'static str],
    /// `positionalSkip` — count of leading POSITIONAL args that are not paths.
    pub positional_skip: usize,
    /// `optionalWrite` — a write op whose path may be absent without asking.
    pub optional_write: bool,
}

impl FkEntry {
    const fn new(
        operation_type: PsOperation,
        path_params: &'static [&'static str],
        known_switches: &'static [&'static str],
        known_value_params: &'static [&'static str],
    ) -> Self {
        Self {
            operation_type,
            path_params,
            known_switches,
            known_value_params,
            leaf_only_path_params: &[],
            positional_skip: 0,
            optional_write: false,
        }
    }
    const fn leaf(mut self, leaf: &'static [&'static str]) -> Self {
        self.leaf_only_path_params = leaf;
        self
    }
    const fn skip(mut self, n: usize) -> Self {
        self.positional_skip = n;
        self
    }
    const fn opt_write(mut self) -> Self {
        self.optional_write = true;
        self
    }
}

use PsOperation::{Read, Write};

/// Parameters appended to EVERY cmdlet's `known_switches` (claude-code `WWi` —
/// the common `-Verbose`/`-Debug` switches).
pub const WWI: &[&str] = &["-verbose", "-debug"];

/// Parameters appended to EVERY cmdlet's `known_value_params` (claude-code `GWi`
/// — the common parameters, e.g. `-ErrorAction`, that take a non-path value).
pub const GWI: &[&str] = &[
    "-erroraction",
    "-warningaction",
    "-informationaction",
    "-progressaction",
    "-errorvariable",
    "-warningvariable",
    "-informationvariable",
    "-outvariable",
    "-outbuffer",
    "-pipelinevariable",
    "-ea",
    "-wa",
    "-infa",
    "-proga",
];

/// AST element types considered statically VALIDATABLE (claude-code `Agg`). A
/// path argument whose following element type is not in this set forces
/// `hasUnvalidatablePathArg` (→ ask).
pub static AGG: LazyLock<HashSet<&'static str>> =
    LazyLock::new(|| ["StringConstant", "Parameter"].into_iter().collect());

/// Characters that begin a PowerShell PARAMETER token (claude-code `EY`): ASCII
/// hyphen plus the en/em/horizontal-bar dashes PowerShell also accepts.
pub const EY: [char; 4] = ['-', '\u{2013}', '\u{2014}', '\u{2015}'];

/// Read-only cmdlets (claude-code `Rgg`) — used by `xgg` to track whether any
/// non-read-only command appeared upstream in a pipeline (path-source safety).
pub static RGG: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "get-childitem",
        "get-item",
        "get-itemproperty",
        "resolve-path",
        "convert-path",
        "get-filehash",
        "get-acl",
        "test-path",
    ]
    .into_iter()
    .collect()
});

/// PowerShell alias → canonical cmdlet name (claude-code `hhe`). Keys are the
/// lowercase alias; values are the canonical cmdlet in the binary's original
/// casing (lowercased at lookup time by [`normalize_cmdlet`]).
static HHE: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
    [
        ("ls", "Get-ChildItem"),
        ("dir", "Get-ChildItem"),
        ("gci", "Get-ChildItem"),
        ("cat", "Get-Content"),
        ("type", "Get-Content"),
        ("gc", "Get-Content"),
        ("cd", "Set-Location"),
        ("sl", "Set-Location"),
        ("chdir", "Set-Location"),
        ("pushd", "Push-Location"),
        ("popd", "Pop-Location"),
        ("pwd", "Get-Location"),
        ("gl", "Get-Location"),
        ("gi", "Get-Item"),
        ("gp", "Get-ItemProperty"),
        ("ni", "New-Item"),
        ("mkdir", "New-Item"),
        ("md", "New-Item"),
        ("ri", "Remove-Item"),
        ("del", "Remove-Item"),
        ("rd", "Remove-Item"),
        ("rmdir", "Remove-Item"),
        ("rm", "Remove-Item"),
        ("erase", "Remove-Item"),
        ("mi", "Move-Item"),
        ("mv", "Move-Item"),
        ("move", "Move-Item"),
        ("ci", "Copy-Item"),
        ("cp", "Copy-Item"),
        ("copy", "Copy-Item"),
        ("cpi", "Copy-Item"),
        ("si", "Set-Item"),
        ("rni", "Rename-Item"),
        ("ren", "Rename-Item"),
        ("ps", "Get-Process"),
        ("gps", "Get-Process"),
        ("kill", "Stop-Process"),
        ("spps", "Stop-Process"),
        ("start", "Start-Process"),
        ("saps", "Start-Process"),
        ("sajb", "Start-Job"),
        ("ipmo", "Import-Module"),
        ("echo", "Write-Output"),
        ("write", "Write-Output"),
        ("sleep", "Start-Sleep"),
        ("help", "Get-Help"),
        ("man", "Get-Help"),
        ("gcm", "Get-Command"),
        ("gsv", "Get-Service"),
        ("gv", "Get-Variable"),
        ("sv", "Set-Variable"),
        ("h", "Get-History"),
        ("history", "Get-History"),
        ("iex", "Invoke-Expression"),
        ("iwr", "Invoke-WebRequest"),
        ("irm", "Invoke-RestMethod"),
        ("icm", "Invoke-Command"),
        ("ii", "Invoke-Item"),
        ("iwmi", "Invoke-WmiMethod"),
        ("icim", "Invoke-CimMethod"),
        ("nsn", "New-PSSession"),
        ("etsn", "Enter-PSSession"),
        ("exsn", "Exit-PSSession"),
        ("gsn", "Get-PSSession"),
        ("rsn", "Remove-PSSession"),
        ("cls", "Clear-Host"),
        ("clear", "Clear-Host"),
        ("select", "Select-Object"),
        ("where", "Where-Object"),
        ("foreach", "ForEach-Object"),
        ("%", "ForEach-Object"),
        ("?", "Where-Object"),
        ("measure", "Measure-Object"),
        ("ft", "Format-Table"),
        ("fl", "Format-List"),
        ("fw", "Format-Wide"),
        ("oh", "Out-Host"),
        ("ogv", "Out-GridView"),
        ("ac", "Add-Content"),
        ("clc", "Clear-Content"),
        ("tee", "Tee-Object"),
        ("epcsv", "Export-Csv"),
        ("sp", "Set-ItemProperty"),
        ("rp", "Remove-ItemProperty"),
        ("cli", "Clear-Item"),
        ("epal", "Export-Alias"),
        ("sls", "Select-String"),
    ]
    .into_iter()
    .collect()
});

/// Per-cmdlet path-param config (claude-code `FKn`) — 40 path-taking cmdlets.
/// Keyed by the lowercase canonical cmdlet name (post-[`normalize_cmdlet`]).
pub static FKN: LazyLock<HashMap<&'static str, FkEntry>> = LazyLock::new(|| {
    let pp: &[&str] = &["-path", "-literalpath", "-pspath", "-lp"];
    let mut m: HashMap<&'static str, FkEntry> = HashMap::new();
    m.insert(
        "set-content",
        FkEntry::new(
            Write,
            pp,
            &[
                "-passthru",
                "-force",
                "-whatif",
                "-confirm",
                "-usetransaction",
                "-nonewline",
                "-asbytestream",
            ],
            &[
                "-value",
                "-filter",
                "-include",
                "-exclude",
                "-credential",
                "-encoding",
                "-stream",
            ],
        ),
    );
    m.insert(
        "add-content",
        FkEntry::new(
            Write,
            pp,
            &[
                "-passthru",
                "-force",
                "-whatif",
                "-confirm",
                "-usetransaction",
                "-nonewline",
                "-asbytestream",
            ],
            &[
                "-value",
                "-filter",
                "-include",
                "-exclude",
                "-credential",
                "-encoding",
                "-stream",
            ],
        ),
    );
    m.insert(
        "remove-item",
        FkEntry::new(
            Write,
            pp,
            &[
                "-recurse",
                "-force",
                "-whatif",
                "-confirm",
                "-usetransaction",
            ],
            &["-filter", "-include", "-exclude", "-credential", "-stream"],
        ),
    );
    m.insert(
        "clear-content",
        FkEntry::new(
            Write,
            pp,
            &["-force", "-whatif", "-confirm", "-usetransaction"],
            &["-filter", "-include", "-exclude", "-credential", "-stream"],
        ),
    );
    m.insert(
        "out-file",
        FkEntry::new(
            Write,
            &["-filepath", "-path", "-literalpath", "-pspath", "-lp"],
            &[
                "-append",
                "-force",
                "-noclobber",
                "-nonewline",
                "-whatif",
                "-confirm",
            ],
            &["-inputobject", "-encoding", "-width"],
        ),
    );
    m.insert(
        "tee-object",
        FkEntry::new(
            Write,
            &["-filepath", "-path", "-literalpath", "-pspath", "-lp"],
            &["-append"],
            &["-inputobject", "-variable", "-encoding"],
        ),
    );
    m.insert(
        "export-csv",
        FkEntry::new(
            Write,
            pp,
            &[
                "-append",
                "-force",
                "-noclobber",
                "-notypeinformation",
                "-includetypeinformation",
                "-useculture",
                "-noheader",
                "-whatif",
                "-confirm",
            ],
            &[
                "-inputobject",
                "-delimiter",
                "-encoding",
                "-quotefields",
                "-usequotes",
            ],
        ),
    );
    m.insert(
        "export-clixml",
        FkEntry::new(
            Write,
            pp,
            &["-force", "-noclobber", "-whatif", "-confirm"],
            &["-inputobject", "-depth", "-encoding"],
        ),
    );
    m.insert(
        "new-item",
        FkEntry::new(
            Write,
            pp,
            &["-force", "-whatif", "-confirm", "-usetransaction"],
            &["-itemtype", "-value", "-credential", "-type"],
        )
        .leaf(&["-name"]),
    );
    m.insert(
        "copy-item",
        FkEntry::new(
            Write,
            &["-path", "-literalpath", "-pspath", "-lp", "-destination"],
            &[
                "-container",
                "-force",
                "-passthru",
                "-recurse",
                "-whatif",
                "-confirm",
                "-usetransaction",
            ],
            &[
                "-filter",
                "-include",
                "-exclude",
                "-credential",
                "-fromsession",
                "-tosession",
            ],
        ),
    );
    m.insert(
        "move-item",
        FkEntry::new(
            Write,
            &["-path", "-literalpath", "-pspath", "-lp", "-destination"],
            &[
                "-force",
                "-passthru",
                "-whatif",
                "-confirm",
                "-usetransaction",
            ],
            &["-filter", "-include", "-exclude", "-credential"],
        ),
    );
    m.insert(
        "rename-item",
        FkEntry::new(
            Write,
            pp,
            &[
                "-force",
                "-passthru",
                "-whatif",
                "-confirm",
                "-usetransaction",
            ],
            &["-newname", "-credential", "-filter", "-include", "-exclude"],
        ),
    );
    m.insert(
        "set-item",
        FkEntry::new(
            Write,
            pp,
            &[
                "-force",
                "-passthru",
                "-whatif",
                "-confirm",
                "-usetransaction",
            ],
            &["-value", "-credential", "-filter", "-include", "-exclude"],
        ),
    );
    m.insert(
        "get-content",
        FkEntry::new(
            Read,
            pp,
            &[
                "-force",
                "-usetransaction",
                "-wait",
                "-raw",
                "-asbytestream",
            ],
            &[
                "-readcount",
                "-totalcount",
                "-tail",
                "-first",
                "-head",
                "-last",
                "-filter",
                "-include",
                "-exclude",
                "-credential",
                "-delimiter",
                "-encoding",
                "-stream",
            ],
        ),
    );
    m.insert(
        "get-childitem",
        FkEntry::new(
            Read,
            pp,
            &[
                "-recurse",
                "-force",
                "-name",
                "-usetransaction",
                "-followsymlink",
                "-directory",
                "-file",
                "-hidden",
                "-readonly",
                "-system",
            ],
            &[
                "-filter",
                "-include",
                "-exclude",
                "-depth",
                "-attributes",
                "-credential",
            ],
        ),
    );
    m.insert(
        "get-item",
        FkEntry::new(
            Read,
            pp,
            &["-force", "-usetransaction"],
            &["-filter", "-include", "-exclude", "-credential", "-stream"],
        ),
    );
    m.insert(
        "get-itemproperty",
        FkEntry::new(
            Read,
            pp,
            &["-usetransaction"],
            &["-name", "-filter", "-include", "-exclude", "-credential"],
        ),
    );
    m.insert(
        "get-itempropertyvalue",
        FkEntry::new(
            Read,
            pp,
            &["-usetransaction"],
            &["-name", "-filter", "-include", "-exclude", "-credential"],
        ),
    );
    m.insert(
        "get-filehash",
        FkEntry::new(Read, pp, &[], &["-algorithm", "-inputstream"]),
    );
    m.insert(
        "get-acl",
        FkEntry::new(
            Read,
            pp,
            &["-audit", "-allcentralaccesspolicies", "-usetransaction"],
            &["-inputobject", "-filter", "-include", "-exclude"],
        ),
    );
    m.insert(
        "get-module",
        FkEntry::new(
            Read,
            &["-name", "-fullyqualifiedname"],
            &["-listavailable", "-all", "-refresh", "-skipeditioncheck"],
            &["-psedition", "-pssession", "-cimsession"],
        ),
    );
    m.insert(
        "format-hex",
        FkEntry::new(
            Read,
            pp,
            &["-raw"],
            &["-inputobject", "-encoding", "-count", "-offset"],
        ),
    );
    m.insert(
        "test-path",
        FkEntry::new(
            Read,
            pp,
            &["-isvalid", "-usetransaction"],
            &[
                "-filter",
                "-include",
                "-exclude",
                "-pathtype",
                "-credential",
                "-olderthan",
                "-newerthan",
            ],
        ),
    );
    m.insert(
        "resolve-path",
        FkEntry::new(
            Read,
            pp,
            &["-relative", "-usetransaction", "-force"],
            &["-credential", "-relativebasepath"],
        ),
    );
    m.insert(
        "convert-path",
        FkEntry::new(Read, pp, &["-usetransaction"], &[]),
    );
    m.insert(
        "select-string",
        FkEntry::new(
            Read,
            pp,
            &[
                "-simplematch",
                "-casesensitive",
                "-quiet",
                "-list",
                "-notmatch",
                "-allmatches",
                "-noemphasis",
                "-raw",
            ],
            &[
                "-inputobject",
                "-pattern",
                "-include",
                "-exclude",
                "-encoding",
                "-context",
                "-culture",
            ],
        ),
    );
    m.insert(
        "set-location",
        FkEntry::new(Read, pp, &["-passthru", "-usetransaction"], &["-stackname"]),
    );
    m.insert(
        "push-location",
        FkEntry::new(Read, pp, &["-passthru", "-usetransaction"], &["-stackname"]),
    );
    m.insert(
        "pop-location",
        FkEntry::new(
            Read,
            &[],
            &["-passthru", "-usetransaction"],
            &["-stackname"],
        ),
    );
    m.insert(
        "select-xml",
        FkEntry::new(Read, pp, &[], &["-xml", "-content", "-xpath", "-namespace"]),
    );
    m.insert(
        "get-winevent",
        FkEntry::new(
            Read,
            &["-path"],
            &["-force", "-oldest"],
            &[
                "-listlog",
                "-logname",
                "-listprovider",
                "-providername",
                "-maxevents",
                "-computername",
                "-credential",
                "-filterxpath",
                "-filterxml",
                "-filterhashtable",
            ],
        ),
    );
    m.insert(
        "invoke-webrequest",
        FkEntry::new(
            Write,
            &["-outfile", "-infile"],
            &[
                "-allowinsecureredirect",
                "-allowunencryptedauthentication",
                "-disablekeepalive",
                "-nobodyprogress",
                "-passthru",
                "-preservefileauthorizationmetadata",
                "-resume",
                "-skipcertificatecheck",
                "-skipheadervalidation",
                "-skiphttperrorcheck",
                "-usebasicparsing",
                "-usedefaultcredentials",
            ],
            &[
                "-uri",
                "-method",
                "-body",
                "-contenttype",
                "-headers",
                "-maximumredirection",
                "-maximumretrycount",
                "-proxy",
                "-proxycredential",
                "-retryintervalsec",
                "-sessionvariable",
                "-timeoutsec",
                "-token",
                "-transferencoding",
                "-useragent",
                "-websession",
                "-credential",
                "-authentication",
                "-certificate",
                "-certificatethumbprint",
                "-form",
                "-httpversion",
            ],
        )
        .skip(1)
        .opt_write(),
    );
    m.insert(
        "invoke-restmethod",
        FkEntry::new(
            Write,
            &["-outfile", "-infile"],
            &[
                "-allowinsecureredirect",
                "-allowunencryptedauthentication",
                "-disablekeepalive",
                "-followrellink",
                "-nobodyprogress",
                "-passthru",
                "-preservefileauthorizationmetadata",
                "-resume",
                "-skipcertificatecheck",
                "-skipheadervalidation",
                "-skiphttperrorcheck",
                "-usebasicparsing",
                "-usedefaultcredentials",
            ],
            &[
                "-uri",
                "-method",
                "-body",
                "-contenttype",
                "-headers",
                "-maximumfollowrellink",
                "-maximumredirection",
                "-maximumretrycount",
                "-proxy",
                "-proxycredential",
                "-responseheaderstvariable",
                "-retryintervalsec",
                "-sessionvariable",
                "-statuscodevariable",
                "-timeoutsec",
                "-token",
                "-transferencoding",
                "-useragent",
                "-websession",
                "-credential",
                "-authentication",
                "-certificate",
                "-certificatethumbprint",
                "-form",
                "-httpversion",
            ],
        )
        .skip(1)
        .opt_write(),
    );
    m.insert(
        "expand-archive",
        FkEntry::new(
            Write,
            &[
                "-path",
                "-literalpath",
                "-pspath",
                "-lp",
                "-destinationpath",
            ],
            &["-force", "-passthru", "-whatif", "-confirm"],
            &[],
        ),
    );
    m.insert(
        "compress-archive",
        FkEntry::new(
            Write,
            &[
                "-path",
                "-literalpath",
                "-pspath",
                "-lp",
                "-destinationpath",
            ],
            &["-force", "-update", "-passthru", "-whatif", "-confirm"],
            &["-compressionlevel"],
        ),
    );
    m.insert(
        "set-itemproperty",
        FkEntry::new(
            Write,
            pp,
            &[
                "-passthru",
                "-force",
                "-whatif",
                "-confirm",
                "-usetransaction",
            ],
            &[
                "-name",
                "-value",
                "-type",
                "-filter",
                "-include",
                "-exclude",
                "-credential",
                "-inputobject",
            ],
        ),
    );
    m.insert(
        "new-itemproperty",
        FkEntry::new(
            Write,
            pp,
            &["-force", "-whatif", "-confirm", "-usetransaction"],
            &[
                "-name",
                "-value",
                "-propertytype",
                "-type",
                "-filter",
                "-include",
                "-exclude",
                "-credential",
            ],
        ),
    );
    m.insert(
        "remove-itemproperty",
        FkEntry::new(
            Write,
            pp,
            &["-force", "-whatif", "-confirm", "-usetransaction"],
            &["-name", "-filter", "-include", "-exclude", "-credential"],
        ),
    );
    m.insert(
        "clear-item",
        FkEntry::new(
            Write,
            pp,
            &["-force", "-whatif", "-confirm", "-usetransaction"],
            &["-filter", "-include", "-exclude", "-credential"],
        ),
    );
    m.insert(
        "export-alias",
        FkEntry::new(
            Write,
            pp,
            &[
                "-append",
                "-force",
                "-noclobber",
                "-passthru",
                "-whatif",
                "-confirm",
            ],
            &["-name", "-description", "-scope", "-as"],
        ),
    );
    m
});

/// Trailing-executable-extension regex characters (claude-code `dgg =
/// /\.(exe|cmd|bat|com)$/`) — stripped by [`normalize_cmdlet`] when the name has
/// no path separator.
fn strip_exe_ext(name: &str) -> &str {
    for ext in [".exe", ".cmd", ".bat", ".com"] {
        if name.len() > ext.len() && name[name.len() - ext.len()..].eq_ignore_ascii_case(ext) {
            return &name[..name.len() - ext.len()];
        }
    }
    name
}

/// Normalize a cmdlet/alias to its canonical lowercase cmdlet name (claude-code
/// `y_`): lowercase; if the name has no `/` or `\`, strip a trailing
/// `.exe/.cmd/.bat/.com`; resolve through the alias table `hhe`; return the
/// canonical name lowercased, else the (extension-stripped) name itself.
#[must_use]
pub fn normalize_cmdlet(name: &str) -> String {
    let lower = name.to_lowercase();
    let base = if !lower.contains('\\') && !lower.contains('/') {
        strip_exe_ext(&lower)
    } else {
        lower.as_str()
    };
    if let Some(canon) = HHE.get(base) {
        canon.to_lowercase()
    } else {
        base.to_string()
    }
}

/// A cmdlet argument is a PARAMETER token (claude-code `Rtt`): if the parsed AST
/// element type is known, it is `"Parameter"`; otherwise the raw token begins
/// with a parameter-prefix char ([`EY`]).
#[must_use]
pub fn is_parameter(arg: &str, element_type: Option<&str>) -> bool {
    if let Some(t) = element_type {
        return t == "Parameter";
    }
    arg.chars().next().is_some_and(|c| EY.contains(&c))
}

/// Prefix-match a normalized parameter `short` against a param list (claude-code
/// `LKn`): matches on exact equality, or (for a ≥2-char abbreviation) when a
/// list entry starts with `short` — PowerShell's unambiguous-prefix binding.
#[must_use]
pub fn param_in_list(short: &str, list: &[&str]) -> bool {
    list.iter()
        .any(|&r| r == short || (short.len() > 1 && r.starts_with(short)))
}

/// A single parsed PowerShell command (claude-code's transformed `CommandAst`
/// node). `element_types[0]` is the command NAME's simplified AST type;
/// `element_types[i + 1]` is the type of `args[i]` (`_Br`-mapped, e.g.
/// `"StringConstant"`, `"Parameter"`, `"SubExpression"`, `"Variable"`). A missing
/// entry (short vector) reads as "unknown", exactly like JS `a[p+1] === undefined`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PsCommand {
    /// The command name as written (alias or canonical cmdlet, any casing).
    pub name: String,
    /// The command NAME's resolved kind (claude-code `nameType`): `"cmdlet"`
    /// (Verb-Noun), `"application"` (path-like / non-ASCII → resolved from a
    /// path), or `"unknown"`. Used by the `acceptEdits` whole-pipeline validator
    /// (`zLs`) to refuse a path-resolved application name. Default `""` reads as
    /// "not classified" (never `"application"`), so a hand-built command is never
    /// wrongly refused by the application check.
    pub name_type: String,
    /// The command's arguments, in order, as reconstructed token text.
    pub args: Vec<String>,
    /// Simplified AST element types: `[0]` is the name's type; `[i + 1]` is the
    /// type of `args[i]`. A short vector reads as "unknown" for missing entries.
    pub element_types: Vec<String>,
    /// Per-argument inline-value children (claude-code `children`): for a
    /// `-Param:value` argument, `children[i]` holds the simplified element type(s)
    /// of the bound value (e.g. an array literal → `["Other"]`). Aligned with
    /// [`Self::args`]; `None` when the argument has no inline value. Empty vector
    /// = "no children array" (claude-code `l.children === undefined`). Consulted
    /// by the `acceptEdits` validator's `h3` unvalidatable-argument check.
    pub children: Vec<Option<Vec<String>>>,
    /// This command's output redirections (claude-code `l.redirections`).
    pub redirections: Vec<PsRedirection>,
}

/// A PowerShell output redirection (claude-code transformed `RedirectionAst`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PsRedirection {
    /// The redirection target path (empty / merging redirections are skipped).
    pub target: String,
    /// A `2>&1`-style stream merge (no file target → skipped by `xgg`).
    pub is_merging: bool,
}

/// One element of a pipeline statement (claude-code `e.commands` entry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PsElement {
    /// A `CommandAst` — a cmdlet/command invocation to validate.
    Command(PsCommand),
    /// A non-`CommandAst` pipeline element (subexpression, script block, …) whose
    /// output could feed a downstream command's path — a pipeline source.
    Expression {
        /// The element's source text (used for the pipeline deny-rule pre-check).
        text: String,
    },
}

/// A parsed PowerShell statement / pipeline (claude-code `xgg`'s input `e`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PsStatement {
    /// The pipeline's elements, in order.
    pub commands: Vec<PsElement>,
    /// Flattened nested commands (inside script blocks / control flow).
    pub nested_commands: Vec<PsCommand>,
    /// Statement-level output redirections.
    pub redirections: Vec<PsRedirection>,
    /// The raw AST statement type (claude-code `statementType`, e.g.
    /// `"PipelineAst"` / `"AssignmentStatementAst"`). The `acceptEdits` validator
    /// treats `"AssignmentStatementAst"` as an assignment feature (→ passthrough).
    /// Default `""` (no assignment).
    pub statement_type: String,
    /// Dangerous-construct flags discovered anywhere in this statement's AST
    /// (claude-code `securityPatterns`). The `acceptEdits` validator's `Voe`
    /// feature aggregate ORs these into its member-invocation / sub-expression /
    /// expandable-string / script-block features. Default all-false.
    pub security_patterns: PsSecurityPatterns,
}

/// Dangerous-construct flags for a statement (claude-code `securityPatterns`).
/// Populated by the `pwsh` parse (`Get-SecurityPatterns`), which finds member /
/// sub / array / paren / expandable-string / script-block expressions anywhere in
/// the statement AST. Consumed by the `acceptEdits` validator's `Voe` aggregate.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PsSecurityPatterns {
    /// A member access / method invocation (`$x.Prop`, `$x.Method()`).
    pub has_member_invocations: bool,
    /// A sub-expression / array-expression / paren-expression (`$( )`, `@( )`, `( )`).
    pub has_sub_expressions: bool,
    /// An expandable (double-quoted / interpolating) string.
    pub has_expandable_strings: bool,
    /// A script block (`{ … }`).
    pub has_script_blocks: bool,
}

/// A variable reference discovered by the `pwsh` parse (claude-code
/// `parseResult.variables[]`). The `acceptEdits` validator's `Voe` aggregate uses
/// [`Self::is_splatted`] to detect splatting (`@args`), which is unvalidatable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PsVariable {
    /// The variable path text (e.g. `"args"` for `$args` / `@args`).
    pub path: String,
    /// Whether the variable was splatted (`@name`) — feeds `hasSplatting`.
    pub is_splatted: bool,
}

/// Result of [`extract_paths`] (claude-code `X_u`): the file-path arguments, the
/// cmdlet's operation, whether any path argument could not be statically
/// validated (→ the caller asks), and whether the write target is optional.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathExtraction {
    /// The file-path arguments discovered for the command.
    pub paths: Vec<String>,
    /// The cmdlet's read/write/create classification.
    pub operation_type: PsOperation,
    /// True when some path argument could not be statically validated (→ ask).
    pub has_unvalidatable_path_arg: bool,
    /// True when the write target may legitimately be absent (e.g. `Invoke-WebRequest`).
    pub optional_write: bool,
}

/// A quote character stripped by `L1`/tested by `eGi` (claude-code
/// `/['"‘-‟]/`): ASCII `'`/`"` plus the Unicode quote block
/// U+2018..U+201F (curly single/double quotes).
fn is_quote_char(c: char) -> bool {
    c == '\'' || c == '"' || ('\u{2018}'..='\u{201F}').contains(&c)
}

/// Leading whitespace stripped by `qWi` (`/^[\s᠎]+/`): JS `\s`
/// verbatim (incl. `﻿`, excl. Rust's ``) plus the explicit U+0085 and
/// U+180E.
fn is_ps_leading_ws(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'
            | '\u{000A}'
            | '\u{000B}'
            | '\u{000C}'
            | '\u{000D}'
            | '\u{0020}'
            | '\u{0085}'
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{180E}'
            | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

/// Strip surrounding quote characters (claude-code `L1`: remove leading and
/// trailing runs of [`is_quote_char`]).
fn strip_surrounding_quotes(s: &str) -> &str {
    let start = s.find(|c| !is_quote_char(c)).unwrap_or(s.len());
    let t = &s[start..];
    let end = t
        .rfind(|c| !is_quote_char(c))
        .map_or(0, |i| i + t[i..].chars().next().unwrap().len_utf8());
    &t[..end]
}

/// Strip EVERY quote character (claude-code 2.1.238 `wW`: the GLOBAL
/// `/['"\u{2018}-\u{201F}]+/g` replace). 2.1.220's path guard used the ANCHORED
/// strip (`z2`, now spelled `Hce` and kept here as
/// [`strip_surrounding_quotes`]); 2.1.238's `w$i` switched its base
/// normalization to this global form.
fn strip_quote_chars(s: &str) -> String {
    s.chars().filter(|c| !is_quote_char(*c)).collect()
}

/// True when the path carries a quote character anywhere — claude-code `w$i`'s
/// `i = wW(e) !== e`.
fn has_quote_chars(s: &str) -> bool {
    s.chars().any(is_quote_char)
}

/// Strip a leading whitespace run + any PowerShell comments (claude-code `Gee`):
/// leading `\s᠎`, then repeatedly a `<# … #>` block comment or a
/// `# … <newline>` line comment (each followed by another leading-ws strip).
fn strip_comments_and_leading_ws(s: &str) -> &str {
    let mut t = s.trim_start_matches(is_ps_leading_ws);
    loop {
        if let Some(rest) = t.strip_prefix("<#") {
            match rest.find("#>") {
                Some(i) => t = rest[i + 2..].trim_start_matches(is_ps_leading_ws),
                None => break,
            }
        } else if t.starts_with('#') {
            match t.find(['\r', '\n']) {
                // slice FROM the newline (claude-code `t.slice(r)`), then strip ws.
                Some(i) => t = t[i..].trim_start_matches(is_ps_leading_ws),
                None => break,
            }
        } else {
            break;
        }
    }
    t
}

/// `$Kn` — strip comments/leading-ws then surrounding quotes.
fn clean_value(s: &str) -> &str {
    strip_surrounding_quotes(strip_comments_and_leading_ws(s))
}

/// `eGi` — a parameter/positional VALUE that cannot be statically validated as a
/// single literal path: it contains a quote char, or (after [`clean_value`])
/// looks like an array / subexpression / variable / backtick-escape.
fn is_unvalidatable_value(s: &str) -> bool {
    if s.chars().any(is_quote_char) {
        return true;
    }
    let t = clean_value(s);
    t.contains(',')
        || t.starts_with('(')
        || t.starts_with('[')
        || t.contains('`')
        || t.contains("@(")
        || t.starts_with('@')
        || t.contains('$')
}

/// Extract the file-path arguments of a PowerShell command (claude-code `X_u`).
///
/// Cmdlets not in [`FKN`] take no path args (`{paths: [], read}`). For a
/// path-taking cmdlet, walk the args: parameters are matched against the
/// cmdlet's path / leaf-only / switch / value-param lists (with `WWi`/`GWi`
/// appended); a `-Param:value` value is quote-stripped, a `-Param value` value
/// consumes the next arg raw; positional args past `positional_skip` are paths.
/// Any arg whose following AST element type is not in [`AGG`], any array /
/// subexpression value, or any unknown parameter marks `has_unvalidatable_path_arg`.
#[must_use]
pub fn extract_paths(cmd: &PsCommand) -> PathExtraction {
    let canon = normalize_cmdlet(&cmd.name);
    let Some(entry) = FKN.get(canon.as_str()) else {
        return PathExtraction {
            paths: Vec::new(),
            operation_type: PsOperation::Read,
            has_unvalidatable_path_arg: false,
            optional_write: false,
        };
    };
    let switches: Vec<&str> = entry.known_switches.iter().chain(WWI).copied().collect();
    let value_params: Vec<&str> = entry
        .known_value_params
        .iter()
        .chain(GWI)
        .copied()
        .collect();

    let s = &cmd.args;
    // element type of args[p] lives at element_types[p + 1] (index 0 = cmd name).
    let elem_type = |p: usize| -> Option<&str> { cmd.element_types.get(p + 1).map(String::as_str) };

    let mut paths: Vec<String> = Vec::new();
    let mut unvalidatable = false;
    let mut positional = 0usize;

    let mut p = 0usize;
    while p < s.len() {
        let f = &s[p];
        if f.is_empty() {
            p += 1;
            continue;
        }
        // `d(p)`: mark unvalidatable when the arg-at-p's element type is known and
        // not one of the statically-validatable types (StringConstant/Parameter).
        let peek_unvalidatable =
            |idx: usize| -> bool { matches!(elem_type(idx), Some(t) if !AGG.contains(t)) };

        if is_parameter(f, elem_type(p)) {
            // Normalize the leading (possibly Unicode) dash to a single "-" and
            // split off an inline `-Param:value` value. Work on the tail after the
            // dash so a multi-byte dash never desyncs the colon offset.
            let first_len = f.chars().next().map_or(0, char::len_utf8);
            let rest = &f[first_len..];
            let (sname, colon_value): (String, Option<&str>) = match rest.find(':') {
                Some(ci) => (
                    format!("-{}", &rest[..ci]).to_lowercase(),
                    Some(&rest[ci + 1..]),
                ),
                None => (format!("-{rest}").to_lowercase(), None),
            };

            if param_in_list(&sname, entry.path_params) {
                let mut b: Option<String> = None;
                if let Some(v) = colon_value {
                    if is_unvalidatable_value(v) {
                        unvalidatable = true;
                    }
                    b = Some(clean_value(v).to_string());
                } else if let Some(v) = s.get(p + 1) {
                    // JS `if(v && !Rtt(v,w))` — an empty next-arg is falsy, not consumed.
                    if !v.is_empty() && !is_parameter(v, elem_type(p + 1)) {
                        b = Some(v.clone());
                        if peek_unvalidatable(p + 1) {
                            unvalidatable = true;
                        }
                        p += 1;
                    }
                }
                if let Some(b) = b {
                    if !b.is_empty() {
                        paths.push(b);
                    }
                }
            } else if !entry.leaf_only_path_params.is_empty()
                && param_in_list(&sname, entry.leaf_only_path_params)
            {
                let mut b: Option<String> = None;
                if let Some(v) = colon_value {
                    if is_unvalidatable_value(v) {
                        unvalidatable = true;
                    }
                    b = Some(clean_value(v).to_string());
                } else if let Some(v) = s.get(p + 1) {
                    // JS `if(v && !Rtt(v,w))` — an empty next-arg is falsy, not consumed.
                    if !v.is_empty() && !is_parameter(v, elem_type(p + 1)) {
                        b = Some(v.clone());
                        if peek_unvalidatable(p + 1) {
                            unvalidatable = true;
                        }
                        p += 1;
                    }
                }
                if let Some(b) = b {
                    if b.contains('/') || b.contains('\\') || b == "." || b == ".." {
                        unvalidatable = true;
                    } else {
                        paths.push(b);
                    }
                }
            } else if param_in_list(&sname, &switches) {
                // known switch → no value, ignore
            } else if param_in_list(&sname, &value_params) {
                // known value param → skip its (non-path) value
                if let Some(v) = colon_value {
                    if is_unvalidatable_value(v) {
                        unvalidatable = true;
                    }
                } else if let Some(v) = s.get(p + 1) {
                    // JS `if(v && !Rtt(v,w))` — an empty next-arg is falsy, not consumed.
                    if !v.is_empty() && !is_parameter(v, elem_type(p + 1)) {
                        if peek_unvalidatable(p + 1) {
                            unvalidatable = true;
                        }
                        p += 1;
                    }
                }
            } else {
                // unknown parameter → cannot validate; a `-X:value` form still
                // contributes its value as a path candidate.
                unvalidatable = true;
                if let Some(v) = colon_value {
                    paths.push(clean_value(v).to_string());
                }
            }
            p += 1;
            continue;
        }

        // positional argument
        if positional < entry.positional_skip {
            positional += 1;
            p += 1;
            continue;
        }
        positional += 1;
        if peek_unvalidatable(p) {
            unvalidatable = true;
        }
        paths.push(f.clone());
        p += 1;
    }

    PathExtraction {
        paths,
        operation_type: entry.operation_type,
        has_unvalidatable_path_arg: unvalidatable,
        optional_write: entry.optional_write,
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Message builders + `NKn` string-guard reasons + leaf path helpers.
//
// The model-facing brand is "LingXi" (the product rebrand — claude-code says
// "Claude Code"), matching the port's existing containment messages in
// `command_path_containment` / `path_constraints`.
// ───────────────────────────────────────────────────────────────────────────

/// `MKn` dir-list truncation threshold (claude-code `ZWi = 5`).
const DIR_LIST_MAX: usize = 5;

/// Format the allowed-working-directory list (claude-code `MKn`): up to
/// [`DIR_LIST_MAX`] quoted dirs joined by `", "`, else the first five plus
/// `", and N more"`.
#[must_use]
pub fn format_dir_list(dirs: &[String]) -> String {
    let quoted = |d: &str| format!("'{d}'");
    if dirs.len() <= DIR_LIST_MAX {
        dirs.iter()
            .map(|d| quoted(d))
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        let head = dirs[..DIR_LIST_MAX]
            .iter()
            .map(|d| quoted(d))
            .collect::<Vec<_>>()
            .join(", ");
        format!("{head}, and {} more", dirs.len() - DIR_LIST_MAX)
    }
}

/// The cmdlet path-containment deny/ask message (claude-code template B). Note
/// the verb is the fixed `"access files in"` for ALL cmdlets (read AND write) —
/// only output-redirection targets use `"write to files in"`.
#[must_use]
pub fn cmdlet_containment_message(cmdlet: &str, path: &str, dirs: &[String]) -> String {
    format!(
        "{cmdlet} targeting '{path}' was blocked. For security, LingXi may only access files in the allowed working directories for this session: {}.",
        format_dir_list(dirs)
    )
}

/// The output-redirection containment message (claude-code, `"write to files in"`
/// variant). Identical wording to [`crate::path_constraints`]'s redirection block.
#[must_use]
pub fn redirection_containment_message(path: &str, dirs: &[String]) -> String {
    format!(
        "Output redirection to '{path}' was blocked. For security, LingXi may only write to files in the allowed working directories for this session: {}.",
        format_dir_list(dirs)
    )
}

/// `Remove-Item` protected-system-path deny message (claude-code `Hwt`).
#[must_use]
pub fn remove_item_protected_message(path: &str) -> String {
    format!("Remove-Item on system path '{path}' is blocked. This path is protected from removal.")
}

/// The `NKn` string-guard "other"-type reasons — the manual-approval messages a
/// PowerShell path triggers when it cannot be statically validated. Ported
/// verbatim from claude-code `NKn` (binary 2.1.206).
pub mod ps_path_reasons {
    /// A `~user` (tilde not followed by `/`) home reference.
    pub const TILDE_USER: &str =
        "Paths beginning with ~user cannot be statically validated and require manual approval";
    /// A backtick escape in the path.
    pub const BACKTICK: &str =
        "Backtick escape characters in paths cannot be statically validated and require manual approval";
    /// A `::` module-qualified provider path.
    pub const PROVIDER_QUALIFIED: &str =
        "Module-qualified provider paths (::) cannot be statically validated and require manual approval";
    /// A UNC / WebDAV / SSL path.
    pub const UNC: &str =
        "UNC paths are blocked because they can trigger network requests and credential leakage";
    /// A `$`/`%` variable-expansion path.
    pub const VARIABLE_EXPANSION: &str =
        "Variable expansion syntax in paths requires manual approval";
    /// A `..` traversal after a real directory segment.
    pub const TRAVERSAL: &str =
        "Path contains '..' traversal after a directory segment, which may follow a symlink outside the working directory";
    /// A glob in a write/create operation.
    pub const GLOB_WRITE: &str =
        "Glob patterns are not allowed in write operations. Please specify an exact file path.";
    /// A glob in a read operation.
    pub const GLOB_READ: &str =
        "Glob patterns in paths cannot be statically validated \u{2014} symlinks inside the glob expansion are not examined. Requires manual approval.";
    /// A path that carried quote characters anywhere — claude-code 2.1.238
    /// `w$i`'s `s(m)` verdict (new since 2.1.220). Emitted when quote-stripping
    /// changed the path AND the stripped path would otherwise have been allowed
    /// (or hits the `..`-bearing glob-read arm).
    pub const QUOTE_CHARS: &str =
        "Paths containing quote characters cannot be statically validated and require manual approval";
}

/// Reason for a Windows drive-relative path (claude-code interpolates the path).
#[must_use]
pub fn drive_relative_reason(path: &str) -> String {
    format!("Path '{path}' is drive-relative (resolves against the per-drive current directory, which cannot be statically validated) and requires manual approval")
}

/// Reason for a non-filesystem provider path (claude-code interpolates the path).
#[must_use]
pub fn non_fs_provider_reason(path: &str) -> String {
    format!("Path '{path}' uses a non-filesystem provider and requires manual approval")
}

/// Expand a leading `~`/`~/`/`~\` to the home directory (claude-code `UKn`).
/// With no home available the path is returned unchanged.
#[must_use]
pub fn expand_tilde(path: &str, home: Option<&str>) -> String {
    let is_tilde = path == "~" || path.starts_with("~/") || path.starts_with("~\\");
    match (is_tilde, home) {
        (true, Some(h)) => format!("{h}{}", &path[1..]),
        _ => path.to_string(),
    }
}

/// True when a `..` segment appears AFTER a real directory segment (claude-code
/// `eUr`) — i.e. a traversal that could escape via a symlinked component. Empty
/// and `.` segments are ignored; a leading run of `..` (before any real segment)
/// does not count.
#[must_use]
pub fn has_traversal_after_segment(path: &str, is_windows: bool) -> bool {
    let segs: Vec<&str> = if is_windows {
        path.split(['\\', '/']).collect()
    } else {
        path.split('/').collect()
    };
    let mut real_seen = false;
    for seg in segs {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            if real_seen {
                return true;
            }
        } else {
            real_seen = true;
        }
    }
    false
}

/// First glob-metacharacter index (claude-code `Uxe`): the position of `*`, `?`,
/// or a `[` that has a later matching `]`; `None` when the path has no glob.
#[must_use]
pub fn glob_index(path: &str) -> Option<usize> {
    let bytes: Vec<char> = path.chars().collect();
    for (t, &r) in bytes.iter().enumerate() {
        if r == '*' || r == '?' {
            return Some(t);
        }
        if r == '[' && bytes[t + 1..].contains(&']') {
            return Some(t);
        }
    }
    None
}

/// True when `..` appears as a full path segment (claude-code `OTe`:
/// `/(?:^|[\\/])\.\.(?:[\\/]|$)/`).
#[must_use]
pub fn has_dotdot_segment(path: &str) -> bool {
    let is_sep = |c: char| c == '/' || c == '\\';
    let chars: Vec<char> = path.chars().collect();
    let n = chars.len();
    let mut i = 0;
    while i + 1 < n {
        if chars[i] == '.' && chars[i + 1] == '.' {
            let before_ok = i == 0 || is_sep(chars[i - 1]);
            let after_ok = i + 2 >= n || is_sep(chars[i + 2]);
            if before_ok && after_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// The base directory of a glob path (claude-code `wgg`): the portion up to the
/// last separator before the first glob char; `"."` when the glob has no
/// leading directory; the glob path unchanged when it has no glob.
#[must_use]
pub fn glob_base_dir(path: &str) -> String {
    let Some(t) = glob_index(path) else {
        return path.to_string();
    };
    let prefix = &path[..path.char_indices().nth(t).map_or(path.len(), |(b, _)| b)];
    let last_sep = prefix.rfind(['/', '\\']);
    match last_sep {
        None => ".".to_string(),
        Some(n) => {
            let dir = &prefix[..n + 1];
            if dir.is_empty() {
                "/".to_string()
            } else {
                dir.to_string()
            }
        }
    }
}

/// Case-fold a path for comparison (claude-code `Hg`): lowercase, then map the
/// dotless-i (U+0131) → `i` and long-s (U+017F) → `s`.
#[must_use]
pub fn casefold_path(path: &str) -> String {
    path.to_lowercase()
        .replace('\u{0131}', "i")
        .replace('\u{017F}', "s")
}

/// Classification of a single PowerShell path argument by the `NKn` string-guard
/// sequence — the pure, roots-independent half of containment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PsPathClass {
    /// A guard (or glob) fired: block with this fixed `"other"`-type reason,
    /// which the caller surfaces as an ASK. `resolved` is the reported blocked
    /// path (metadata; the ask message is `reason`, not a template).
    Blocked {
        /// The reported blocked path.
        resolved: String,
        /// The manual-approval reason (used verbatim as the ask message).
        reason: String,
    },
    /// No guard fired; `normalized` (quotes stripped, tilde-expanded, backslashes
    /// normalized) proceeds to working-directory containment.
    Proceed {
        /// The normalized path to check against the allowed working directories.
        normalized: String,
    },
}

/// A Windows drive-relative path (claude-code `/^[a-z]:(?![/\\])/i`): a single
/// letter, a colon, and NOT immediately a separator.
fn is_drive_relative(i: &str) -> bool {
    let b = i.as_bytes();
    b.len() >= 2
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && !matches!(b.get(2), Some(b'/' | b'\\'))
}

/// A non-filesystem provider prefix (claude-code `/^[a-z0-9]{2,}:/i` on Windows,
/// `/^[a-z0-9]+:/i` elsewhere): an alphanumeric run then a colon.
fn is_provider_prefix(i: &str, is_windows: bool) -> bool {
    let run: usize = i.bytes().take_while(u8::is_ascii_alphanumeric).count();
    let min = if is_windows { 2 } else { 1 };
    run >= min && i.as_bytes().get(run) == Some(&b':')
}

/// Windows 8.3 short-name expansion (claude-code `tGi`) — a no-op off Windows,
/// which is the only platform the port validates PowerShell on.
fn short_name_expand(i: &str, _is_windows: bool) -> String {
    // `tGi` returns the input unchanged on every non-Windows platform; the
    // per-segment `PKn` 8.3 expansion is Windows-registry-backed and not modeled.
    i.to_string()
}

/// Classify a PowerShell path argument through the `NKn` string-guard sequence
/// (claude-code `NKn`, binary 2.1.206). Returns an early [`PsPathClass::Blocked`]
/// for a path that cannot be statically validated (`~user`, backtick, `::`,
/// drive-relative, UNC, `$`/`%`, non-fs provider, `..`-traversal, glob), else
/// [`PsPathClass::Proceed`] with the normalized path for working-dir containment.
///
/// NOTE: claude-code additionally pre-checks a deny RULE inside the backtick /
/// `::` / traversal / glob branches (returning a `deny` instead of an ask when a
/// rule matches). Those rule pre-checks are intentionally omitted here: the port
/// runs its deny-rule walk UPSTREAM of containment (in `PermissionPolicy`), so a
/// rule-matched command is already denied before this runs; the residual case
/// (a rule targeting a transformed sub-path) degrades to ASK, which still blocks
/// auto-execution.
#[must_use]
pub fn classify_ps_path(
    raw: &str,
    op: PsOperation,
    is_windows: bool,
    home: Option<&str>,
) -> PsPathClass {
    use ps_path_reasons as R;
    // 2.1.238 `w$i`: `let o=wW(e), i=o!==e;` … `l=Veo(o).replaceAll("\\","/")`.
    // The base normalization moved from the ANCHORED quote strip (2.1.220 `z2`)
    // to the GLOBAL one (`wW`), and `quoted` records that the raw path carried
    // quote characters at all.
    let unquoted = strip_quote_chars(raw);
    let quoted = unquoted != raw;
    let mut i = expand_tilde(&unquoted, home).replace('\\', "/");

    let blocked = |resolved: &str, reason: String| PsPathClass::Blocked {
        resolved: resolved.to_string(),
        reason,
    };

    // ~user (tilde NOT followed by '/'): `/^~[^/]/`
    if i.starts_with('~') && i.as_bytes().get(1).is_some_and(|&c| c != b'/') {
        return blocked(&i, R::TILDE_USER.to_string());
    }
    // backtick escape
    if i.contains('`') {
        return blocked(&i, R::BACKTICK.to_string());
    }
    // `::` module-qualified provider
    if i.contains("::") {
        return blocked(&i, R::PROVIDER_QUALIFIED.to_string());
    }
    // Windows drive-relative. 2.1.238 interpolates `${i?e:l}` — the RAW path when
    // quote-stripping changed it, else the normalized one (2.1.220 always used
    // the normalized path).
    if is_windows && is_drive_relative(&i) {
        return blocked(
            &i,
            drive_relative_reason(if quoted { raw } else { i.as_str() }),
        );
    }
    // tGi short-name expansion, then UNC / WebDAV / SSL
    i = short_name_expand(&i, is_windows);
    if i.starts_with("//")
        || i.to_ascii_lowercase().contains("davwwwroot")
        || i.to_ascii_uppercase().contains("@SSL@")
    {
        return blocked(&i, R::UNC.to_string());
    }
    // `$`/`%` variable expansion
    if i.contains('$') || i.contains('%') {
        return blocked(&i, R::VARIABLE_EXPANSION.to_string());
    }
    // non-filesystem provider prefix
    if is_provider_prefix(&i, is_windows) {
        return blocked(&i, non_fs_provider_reason(&i));
    }
    // `..` traversal after a real segment
    if has_traversal_after_segment(&i, is_windows) {
        return blocked(&i, R::TRAVERSAL.to_string());
    }
    // glob. 2.1.238 inserts `if(i)return s(v);` ahead of the read-glob reason in
    // the `p4e(l)` (`..`-segment-bearing) arm only — the dir-prefix arm keeps
    // GLOB_READ even for a quoted path.
    if glob_index(&i).is_some() {
        let reason = if matches!(op, PsOperation::Write | PsOperation::Create) {
            R::GLOB_WRITE
        } else if quoted && has_dotdot_segment(&i) {
            // `p4e(l)` — the same `/(?:^|[\\/])\.\.(?:[\\/]|$)/` predicate.
            R::QUOTE_CHARS
        } else {
            R::GLOB_READ
        };
        return blocked(&i, reason.to_string());
    }
    PsPathClass::Proceed { normalized: i }
}

/// The outcome of the complete per-path check (claude-code `NKn`): the string
/// guards ([`classify_ps_path`]) followed by working-directory containment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PsPathOutcome {
    /// The path resolves inside an allowed working directory — no constraint.
    /// Carries the resolved path so `xgg`'s `Remove-Item` `TKt` guard (which
    /// fires even on an allowed path) can inspect it.
    Allowed {
        /// The resolved path (inside the allowed working dirs).
        resolved: String,
    },
    /// Blocked by a string guard — ASK with `reason` used verbatim.
    AskReason {
        /// The reported blocked path.
        resolved: String,
        /// The manual-approval reason (the ask message).
        reason: String,
    },
    /// Blocked by working-directory containment — the caller builds the template-B
    /// `"<cmdlet> targeting '<resolved>' … access files in …"` message (or the
    /// redirection variant) from `resolved`.
    AskContainment {
        /// The resolved path outside the allowed working directories.
        resolved: String,
    },
}

/// The complete `NKn` per-path check: run the [`classify_ps_path`] string guards,
/// then (for a clean path) resolve it and test membership in the allowed working
/// directories, reusing the port's [`crate::filesystem::path_in_allowed_working_path`]
/// (the same containment `check_command_path_containment` uses for bash).
///
/// `additional` are the extra allowed working dirs (TS `additionalWorkingDirectories`);
/// `is_windows` selects the Windows-specific guard variants (drive-relative,
/// provider min length). Deny-RULE resolution is handled upstream in the policy
/// gate (see [`classify_ps_path`]).
///
/// PERM-03 (2.1.238): `w$i`'s tail `if(i&&f.allowed)return s(d)` is ported — a
/// path carrying quote characters can never auto-allow. Its sibling arm
/// (`i && !f.allowed && decisionReason.type==="safetyCheck"` → the
/// `"…resolves near a sensitive file under quote-stripping…"` reason) has no
/// counterpart here because this port of `vRg`/`eme` performs only working-dir
/// containment — the `_lt` sensitive-file safety walk that mints the
/// `safetyCheck` reason is a pre-existing, separately deferred scope omission,
/// so the arm could only ever be dead code.
///
/// PERM-PS-VRG-01: `mode` carries claude-code `vRg`'s in-working-dir auto-allow
/// gate (`if(s){{if(r==="read"||t.mode==="acceptEdits")return{{allowed:!0}}}}`): an
/// in-cwd path is auto-allowed ONLY for a `read` op OR `acceptEdits` mode. The gate
/// is universal (main-pipeline, nested, and redirection path checks alike).
#[must_use]
pub fn check_ps_path(
    raw: &str,
    op: PsOperation,
    roots: &crate::filesystem::FsRoots,
    additional: &[std::path::PathBuf],
    is_windows: bool,
    mode: crate::mode::PermissionMode,
) -> PsPathOutcome {
    let home = roots
        .home
        .as_deref()
        .map(|p| p.to_string_lossy().into_owned());
    match classify_ps_path(raw, op, is_windows, home.as_deref()) {
        PsPathClass::Blocked { resolved, reason } => {
            // PERM-PS-RM-05: claude-code `yeo`'s traversal branch reports
            // `resolvedPath: c8.resolve(cwd, i)` (all other guards report the raw
            // normalized path). The resolved path feeds the `Remove-Item` d7t
            // protected check, so a relative `../..` that resolves into a protected
            // root hard-denies rather than degrading to the traversal ask.
            let resolved = if reason == ps_path_reasons::TRAVERSAL {
                crate::filesystem::expand_path(&resolved, roots)
                    .to_string_lossy()
                    .into_owned()
            } else {
                resolved
            };
            PsPathOutcome::AskReason { resolved, reason }
        }
        PsPathClass::Proceed { normalized } => {
            let resolved = crate::filesystem::expand_path(&normalized, roots);
            let mut work_dirs = Vec::with_capacity(1 + additional.len());
            work_dirs.push(roots.cwd.clone());
            work_dirs.extend(additional.iter().cloned());
            if crate::filesystem::path_in_allowed_working_path(&resolved, &work_dirs, roots) {
                // claude-code `vRg` in-working-dir auto-allow gate, verified against
                // the 2.1.211 binary: `if(s){if(r==="read"||t.mode==="acceptEdits")
                // return{allowed:!0}}`. An in-cwd path is auto-allowed ONLY for a
                // `read` op OR `acceptEdits` mode; a write/create in `default`/`plan`
                // mode falls through to the containment ASK (`vRg`'s
                // `{allowed:false, isInWorkingDir:true}` tail). This gate is
                // UNIVERSAL — CC applies it to main-pipeline, NESTED-command, and
                // redirection path checks alike (all route through `yeo`→`vRg` with
                // the same mode/op). (The genuinely main-pipeline-only construct is
                // the SEPARATE `Remove-Item -Recurse` "would delete the working
                // directory" ask, handled elsewhere via the `!nested` gate.) The
                // deferred `$wt`/`Ott`/`Ltt` allow-rule + safety walks CC evaluates
                // before its final `allowed:false` are out of scope — the port goes
                // straight to the ask.
                if matches!(op, PsOperation::Read)
                    || matches!(mode, crate::mode::PermissionMode::AcceptEdits)
                {
                    // 2.1.238 `w$i` tail: `if(i&&f.allowed)return s(d)` — a path
                    // that carried quote characters never auto-allows, it
                    // degrades to the QUOTE_CHARS manual approval.
                    if has_quote_chars(raw) {
                        PsPathOutcome::AskReason {
                            resolved: resolved.to_string_lossy().into_owned(),
                            reason: ps_path_reasons::QUOTE_CHARS.to_string(),
                        }
                    } else {
                        PsPathOutcome::Allowed {
                            resolved: resolved.to_string_lossy().into_owned(),
                        }
                    }
                } else {
                    PsPathOutcome::AskContainment {
                        resolved: resolved.to_string_lossy().into_owned(),
                    }
                }
            } else {
                PsPathOutcome::AskContainment {
                    resolved: resolved.to_string_lossy().into_owned(),
                }
            }
        }
    }
}

/// A protected-system-path predicate for `Remove-Item` targets (claude-code
/// `TKt`): the `*`/`/*` globs, filesystem root, the home dir, ANY top-level
/// `/X` directory, and (on Windows) a drive root/child. `home` is the home dir
/// string; `is_macos` enables the `/private/(etc|var|tmp|home)` normalization.
fn is_protected_removal_resolved(path: &str, home: Option<&str>, is_macos: bool) -> bool {
    // collapse runs of separators to a single '/'
    let mut t = String::with_capacity(path.len());
    let mut prev_sep = false;
    for c in path.chars() {
        let sep = c == '/' || c == '\\';
        if sep {
            if !prev_sep {
                t.push('/');
            }
        } else {
            t.push(c);
        }
        prev_sep = sep;
    }
    if t == "*" || t.ends_with("/*") {
        return true;
    }
    let normalize_private = |c: &str| -> String {
        if !is_macos {
            return c.to_string();
        }
        // /private/(etc|var|tmp|home)(/|$) → /$1$2
        for base in ["etc", "var", "tmp", "home"] {
            let pfx = format!("/private/{base}");
            if c == pfx {
                return format!("/{base}");
            }
            if let Some(rest) = c.strip_prefix(&format!("{pfx}/")) {
                return format!("/{base}/{rest}");
            }
        }
        c.to_string()
    };
    let o = normalize_private(&t);
    let i = if o == "/" {
        o.clone()
    } else {
        o.trim_end_matches('/').to_string()
    };
    if i == "/" {
        return true;
    }
    // XOy: ^[A-Za-z]:/?$  (Windows drive root)
    if is_drive_root(&i) {
        return true;
    }
    if let Some(h) = home {
        let mut hs = String::with_capacity(h.len());
        let mut ps = false;
        for c in h.chars() {
            let sep = c == '/' || c == '\\';
            if sep {
                if !ps {
                    hs.push('/');
                }
            } else {
                hs.push(c);
            }
            ps = sep;
        }
        let hs = normalize_private(&hs);
        let hs = hs.trim_end_matches('/');
        if casefold_path(&i) == casefold_path(hs) {
            return true;
        }
    }
    // dirname(i) === "/"  →  any single top-level dir (/etc, /foo, …)
    if i.starts_with('/') && i.len() > 1 && !i[1..].contains('/') {
        return true;
    }
    // QOy: ^[A-Za-z]:/[^/]+$  (Windows drive child)
    is_drive_child(&i)
}

/// `XOy = /^[A-Za-z]:\/?$/` — a Windows drive root (`C:` / `C:/`).
fn is_drive_root(i: &str) -> bool {
    let b = i.as_bytes();
    (b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':')
        || (b.len() == 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'/')
}

/// `QOy = /^[A-Za-z]:\/[^/]+$/` — a Windows drive child (`C:/Users`).
fn is_drive_child(i: &str) -> bool {
    let b = i.as_bytes();
    b.len() > 3
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && b[2] == b'/'
        && !i[3..].contains('/')
}

/// The raw-path protected check (claude-code `O5r`): strip quotes, drop a `::`
/// prefix, expand `~`, normalize backslashes, lexically normalize an ABSOLUTE
/// path (collapsing `.`/`..`, matching Node `path.normalize`), then
/// [`is_protected_removal_resolved`].
fn is_protected_removal_raw(path: &str, home: Option<&str>, is_macos: bool) -> bool {
    let mut t = strip_surrounding_quotes(path).to_string();
    if let Some(idx) = t.find("::") {
        t = t[idx + 2..].to_string();
    }
    let mut t = expand_tilde(&t, home).replace('\\', "/");
    // PERM-PS-RM-05: `if(c8.isAbsolute(t))t=c8.normalize(t)` — collapse `..`/`.`
    // for an absolute path so `Remove-Item /a/b/../..` (→ `/a`) is seen as its
    // protected root. Relative paths are left untouched (Node `normalize` keeps a
    // leading `..`), matching the oracle.
    if std::path::Path::new(&t).is_absolute() {
        t = node_normalize_absolute(&t);
    }
    is_protected_removal_resolved(&t, home, is_macos)
}

/// Node `path.normalize` for an absolute POSIX path (claude-code `c8.normalize`):
/// collapse `.` and `..` segments (a `..` above the root is dropped) and duplicate
/// separators, keeping the leading `/`. Only called on an absolute path.
fn node_normalize_absolute(t: &str) -> String {
    use std::path::{Component, Path, PathBuf};
    let mut out = PathBuf::new();
    for comp in Path::new(t).components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                }
                // At (or above) root Node drops the `..` entirely.
            }
            other => out.push(other.as_os_str()),
        }
    }
    let s = out.to_string_lossy().into_owned();
    if s.is_empty() {
        "/".to_string()
    } else {
        s
    }
}

/// Whether an argument is a `-Recurse` parameter (claude-code's inline test in
/// `xgg`): the dash-normalized, lowercased, colon-stripped token is a ≥2-char
/// prefix of `-recurse`.
fn is_recurse_flag(arg: &str) -> bool {
    if arg.is_empty() {
        return false;
    }
    let first_len = arg.chars().next().map_or(0, char::len_utf8);
    let normalized = format!("-{}", &arg[first_len..]).to_lowercase();
    let w = match normalized.find(':') {
        Some(i) => &normalized[..i],
        None => &normalized[..],
    };
    w.len() >= 2 && "-recurse".starts_with(w)
}

/// The result of PowerShell path containment (claude-code `Z_u`/`xgg` return).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PsContainmentResult {
    /// No constraint fired — proceed to the normal permission flow.
    Passthrough,
    /// A path could not be validated — prompt the user (the first ask wins).
    Ask {
        /// The model-/user-facing ask message.
        message: String,
        /// The decision reason (equals `message` for the branches with no distinct reason).
        reason: String,
    },
    /// A `Remove-Item` targets a protected system path — hard deny (`Hwt`).
    Deny {
        /// The deny message.
        message: String,
        /// The decision reason.
        reason: String,
    },
}

/// Allowed-working-directory string list for containment messages (cwd + the
/// additional dirs, deduped) — the port analog of `allWorkingDirectories`/`vY`.
fn ps_working_dir_list(
    roots: &crate::filesystem::FsRoots,
    additional: &[std::path::PathBuf],
) -> Vec<String> {
    let mut out = vec![roots.cwd.to_string_lossy().into_owned()];
    for d in additional {
        let s = d.to_string_lossy().into_owned();
        if !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

/// Context for [`validate_ps_statements`] — the roots + extra dirs + platform.
pub struct PsCtx<'a> {
    /// Filesystem roots (cwd/home) for path resolution + containment.
    pub roots: &'a crate::filesystem::FsRoots,
    /// The additional allowed working directories.
    pub additional: &'a [std::path::PathBuf],
    /// Windows guard variants (drive-relative, provider min length).
    pub is_windows: bool,
    /// macOS `/private` normalization for the removal-protected check.
    pub is_macos: bool,
    /// Active permission mode — feeds the `vRg` in-working-dir auto-allow gate
    /// (PERM-PS-VRG-01): a main-pipeline in-cwd write auto-allows only under
    /// `AcceptEdits`.
    pub mode: crate::mode::PermissionMode,
}

/// Validate a parsed PowerShell command (claude-code `Z_u`): run [`xgg`-style]
/// [`validate_ps_statement`] over every statement, returning the first `deny`
/// immediately and otherwise the first `ask`, else `passthrough`.
///
/// `compound_cd` mirrors claude-code's compound-`cd` flag: a compound command
/// that changes the working directory makes relative paths unvalidatable.
#[must_use]
pub fn validate_ps_statements(
    statements: &[PsStatement],
    ctx: &PsCtx,
    compound_cd: bool,
) -> PsContainmentResult {
    let mut first_ask: Option<PsContainmentResult> = None;
    for stmt in statements {
        match validate_ps_statement(stmt, ctx, compound_cd) {
            deny @ PsContainmentResult::Deny { .. } => return deny,
            ask @ PsContainmentResult::Ask { .. } => {
                if first_ask.is_none() {
                    first_ask = Some(ask);
                }
            }
            PsContainmentResult::Passthrough => {}
        }
    }
    first_ask.unwrap_or(PsContainmentResult::Passthrough)
}

/// Set the first-ask accumulator if empty (claude-code `o ??= …`).
fn set_first_ask(ask: &mut Option<PsContainmentResult>, message: String) {
    if ask.is_none() {
        *ask = Some(PsContainmentResult::Ask {
            reason: message.clone(),
            message,
        });
    }
}

/// Like [`set_first_ask`] but with a `decisionReason` distinct from the display
/// `message` (claude-code attaches a separate `decisionReason:{type:'other',
/// reason}` on some asks — e.g. the compound-`cd` ask).
fn set_first_ask_with_reason(
    ask: &mut Option<PsContainmentResult>,
    message: String,
    reason: String,
) {
    if ask.is_none() {
        *ask = Some(PsContainmentResult::Ask { reason, message });
    }
}

/// Validate a single PowerShell statement/pipeline (claude-code `xgg`).
#[must_use]
pub fn validate_ps_statement(
    stmt: &PsStatement,
    ctx: &PsCtx,
    compound_cd: bool,
) -> PsContainmentResult {
    let dirs = ps_working_dir_list(ctx.roots, ctx.additional);
    let mut ask: Option<PsContainmentResult> = None;

    if compound_cd {
        // The compound-cd ask carries a `decisionReason` distinct from its display
        // message (claude-code `decisionReason:{type:"other",reason:"Compound
        // command contains cd with path operation — manual approval required to
        // prevent path resolution bypass"}`, @225367333).
        set_first_ask_with_reason(
            &mut ask,
            "Compound command changes working directory (Set-Location/Push-Location/Pop-Location/New-PSDrive) \u{2014} relative paths cannot be validated against the original cwd and require manual approval".to_string(),
            "Compound command contains cd with path operation \u{2014} manual approval required to prevent path resolution bypass".to_string(),
        );
    }

    // Main pipeline elements.
    let mut pipeline_source = false;
    let mut non_readonly_seen = false;
    for element in &stmt.commands {
        let l = match element {
            PsElement::Expression { .. } => {
                pipeline_source = true;
                continue;
            }
            PsElement::Command(c) => c,
        };
        let prev = non_readonly_seen;
        if !RGG.contains(normalize_cmdlet(&l.name).as_str()) {
            non_readonly_seen = true;
        }
        if let Some(deny) =
            run_ps_command(l, ctx, &dirs, pipeline_source, prev, false, false, &mut ask)
        {
            return deny;
        }
    }
    // Whether the main pipeline contained a non-CommandAst expression element
    // (claude-code `i`) — used only by the nested-command loop below.
    let stmt_has_expression = pipeline_source;

    // Nested commands (script blocks / control flow). claude-code's nested-command
    // loop (`RRg`) differs from the main loop: it does NOT run the pipeline-source
    // ask, the upstream-pipeline ask, or the `Remove-Item -Recurse` cwd check, and
    // it ends each command with the control-flow ask when the main pipeline held an
    // expression source (`stmt_has_expression`).
    for l in &stmt.nested_commands {
        if let Some(deny) = run_ps_command(
            l,
            ctx,
            &dirs,
            false,
            false,
            true,
            stmt_has_expression,
            &mut ask,
        ) {
            return deny;
        }
    }

    // Redirections (nested + statement-level) → "create" containment.
    for l in &stmt.nested_commands {
        if let Some(deny) = check_redirections(&l.redirections, ctx, &dirs, &mut ask) {
            return deny;
        }
    }
    if let Some(deny) = check_redirections(&stmt.redirections, ctx, &dirs, &mut ask) {
        return deny;
    }

    ask.unwrap_or(PsContainmentResult::Passthrough)
}

/// Run one command's path checks (claude-code `xgg`'s per-command body). Returns
/// `Some(deny)` on a `Remove-Item` protected-path hit; otherwise updates the
/// first-ask accumulator.
///
/// `nested` selects claude-code's nested-command loop shape: it suppresses the
/// `Remove-Item -Recurse` working-directory check (which claude-code runs only in
/// the main pipeline loop) and, when `stmt_has_expression` is set, appends the
/// control-flow/chain ask after the path loop.
#[allow(clippy::too_many_arguments)]
fn run_ps_command(
    l: &PsCommand,
    ctx: &PsCtx,
    dirs: &[String],
    pipeline_source: bool,
    prev_non_readonly: bool,
    nested: bool,
    stmt_has_expression: bool,
    ask: &mut Option<PsContainmentResult>,
) -> Option<PsContainmentResult> {
    let roots = ctx.roots;
    let home = roots
        .home
        .as_deref()
        .map(|p| p.to_string_lossy().into_owned());
    let extraction = extract_paths(l);
    let f = normalize_cmdlet(&l.name);
    let is_path_cmdlet = FKN.contains_key(f.as_str());

    if pipeline_source {
        set_first_ask(ask, format!(
            "{f} receives its path from a pipeline expression source that cannot be statically validated and requires manual approval"
        ));
    }
    if extraction.has_unvalidatable_path_arg {
        set_first_ask(ask, format!(
            "{f} uses a parameter or complex path expression (array literal, subexpression, unknown parameter, etc.) that cannot be statically validated and requires manual approval"
        ));
    }
    if extraction.operation_type != PsOperation::Read
        && !extraction.optional_write
        && extraction.paths.is_empty()
        && is_path_cmdlet
    {
        set_first_ask(ask, format!(
            "{f} is a write operation but no target path could be determined; requires manual approval"
        ));
        return None;
    }
    if prev_non_readonly && is_path_cmdlet {
        set_first_ask(ask, format!(
            "{f} may receive a path from an upstream pipeline command whose output cannot be statically validated and requires manual approval"
        ));
    }

    let is_remove = f == "remove-item";
    // claude-code runs the `-Recurse` cwd guard ONLY in the main pipeline loop —
    // the nested-command loop omits it. Gating on `!nested` removes the port's
    // anti-parity extra ask for nested `Remove-Item -Recurse`.
    if !nested && is_remove && l.args.iter().any(|a| is_recurse_flag(a)) {
        let cwd_fold = casefold_path(&roots.cwd.to_string_lossy());
        for b in &extraction.paths {
            let v = expand_tilde(&short_name_expand(b, ctx.is_windows), home.as_deref())
                .replace('\\', "/");
            let resolved = crate::filesystem::expand_path(&v, roots);
            let x = casefold_path(&resolved.to_string_lossy());
            if x == cwd_fold
                || cwd_fold.starts_with(&format!("{x}/"))
                || cwd_fold.starts_with(&format!("{x}\\"))
            {
                set_first_ask(ask, format!(
                    "Remove-Item -Recurse targeting '{b}' would delete the working directory including .git and .claude \u{2014} requires manual approval"
                ));
                break;
            }
        }
    }

    for path in &extraction.paths {
        if is_remove && is_protected_removal_raw(path, home.as_deref(), ctx.is_macos) {
            return Some(deny_removal(path));
        }
        // PERM-PS-VRG-01: the in-cwd write/create auto-allow gate (inside
        // check_ps_path) runs on this per-path check regardless of main-vs-nested
        // — CC's `vRg` is universal (main + nested both route through `yeo`→`vRg`).
        let outcome = check_ps_path(
            path,
            extraction.operation_type,
            roots,
            ctx.additional,
            ctx.is_windows,
            ctx.mode,
        );
        let resolved = match &outcome {
            PsPathOutcome::Allowed { resolved }
            | PsPathOutcome::AskReason { resolved, .. }
            | PsPathOutcome::AskContainment { resolved } => resolved.clone(),
        };
        if is_remove && is_protected_removal_resolved(&resolved, home.as_deref(), ctx.is_macos) {
            return Some(deny_removal(&resolved));
        }
        match outcome {
            PsPathOutcome::Allowed { .. } => {}
            PsPathOutcome::AskReason { reason, .. } => set_first_ask(ask, reason),
            PsPathOutcome::AskContainment { resolved } => {
                set_first_ask(ask, cmdlet_containment_message(&f, &resolved, dirs));
            }
        }
    }

    // claude-code nested-command loop tail (`if(i)o??=…`): when the statement's
    // main pipeline contained a non-CommandAst expression source, each nested
    // command ends with the control-flow/chain ask.
    if nested && stmt_has_expression {
        set_first_ask(ask, format!(
            "{f} appears inside a control-flow or chain statement where piped expression sources cannot be statically validated and requires manual approval"
        ));
    }
    None
}

/// `Hwt` — a `Remove-Item` protected-path hard deny.
fn deny_removal(path: &str) -> PsContainmentResult {
    PsContainmentResult::Deny {
        message: remove_item_protected_message(path),
        reason: "Removal targets a protected system path".to_string(),
    }
}

/// `$null` / `${null}` redirection-target test — 2.1.211 `xXt`. Case-insensitive
/// after trimming; these targets discard output and never touch the filesystem.
fn is_null_redirect(target: &str) -> bool {
    let t = target.trim().to_lowercase();
    t == "$null" || t == "${null}"
}

/// Validate a command's/statement's output redirections against the working dirs
/// (claude-code: each `create`-op redirection target → the "Output redirection
/// to '…' … write to files in …" message). Returns `Some(deny)` on a rule/deny,
/// else updates the first-ask accumulator.
fn check_redirections(
    redirs: &[PsRedirection],
    ctx: &PsCtx,
    dirs: &[String],
    ask: &mut Option<PsContainmentResult>,
) -> Option<PsContainmentResult> {
    for r in redirs {
        if r.is_merging || r.target.is_empty() {
            continue;
        }
        // `> $null` / `> ${null}` is the standard PowerShell discard idiom —
        // 2.1.211 `xXt` skips any redirection whose target trims/lowercases to
        // `$null`/`${null}` before path validation (else the `$` guard in
        // classify_ps_path would falsely ask on it).
        if is_null_redirect(&r.target) {
            continue;
        }
        // PERM-PS-VRG-01: redirection targets (op = Create) also route through
        // CC's `yeo`→`vRg` gate, so an in-cwd redirect-create in default/plan mode
        // asks (only `read`/`acceptEdits` auto-allow).
        match check_ps_path(
            &r.target,
            PsOperation::Create,
            ctx.roots,
            ctx.additional,
            ctx.is_windows,
            ctx.mode,
        ) {
            PsPathOutcome::Allowed { .. } => {}
            PsPathOutcome::AskReason { reason, .. } => {
                if ask.is_none() {
                    *ask = Some(PsContainmentResult::Ask {
                        message: reason.clone(),
                        reason,
                    });
                }
            }
            PsPathOutcome::AskContainment { resolved } => {
                if ask.is_none() {
                    let message = redirection_containment_message(&resolved, dirs);
                    *ask = Some(PsContainmentResult::Ask {
                        reason: message.clone(),
                        message,
                    });
                }
            }
        }
    }
    None
}

// ===========================================================================
// PERM-PS-CALLER-06 — git-security caller battery
//
// Byte-faithful port of the ASK battery emitted by claude-code's OUTER
// PowerShell permission wrapper `NTU` (the fn that WRAPS the
// `gTu`/[`validate_ps_statements`] call), 2.1.211. `NTU` accumulates every ask
// into an array `d`, then returns the first `deny` else the first `ask`. The
// `gTu` result is pushed AFTER these battery asks, so a battery ask outranks the
// generic containment ask on a tie (a `gTu` DENY still wins over everything).
//
// This module ports the 5 in-scope asks (detectable from command names +
// redirection/write targets). The 2 that need extra infra are DEFERRED:
//   * bare-repo-indicators (`E = S && nHr()`): an FS probe of cwd for
//     HEAD/objects/refs outside a `.git/` dir.
//   * PS5.1 cwd-first shadowing (`qt()==="windows" && …`): Windows-only.
// ===========================================================================

// ───────────────────────────────────────────────────────────────────────────
// PS-CALLER-06-5 — PowerShell 5.1 cwd-first command resolution (2.1.220
// `Lt()==="windows" && u.length>1` inside `NTU`).
//
// Windows PowerShell 5.1 resolves a bare command name against the CURRENT
// DIRECTORY before PATH. So an earlier sub-command that writes `./git.bat` makes
// a later bare `git` run that file instead of the real git — arbitrary code
// execution from what reads like an ordinary compound command.
//
// The check is genuinely OS-gated in the oracle, and stays gated here: on
// macOS/Linux PowerShell Core resolves PATH-first, so the shadowing does not
// exist and asking would be a false positive. The PREDICATE is pure and tested
// on every platform; only the gate is conditional.

/// The PATHEXT extensions, lowercased and without the leading dot (2.1.220
/// `kOd`: `process.env.PATHEXT`, keep entries starting with `.` and no longer
/// than 16 chars).
///
/// Falls back to the Windows default set when the variable is unset or yields
/// nothing — an absent PATHEXT must not silently disable the guard.
fn pathext_stems() -> Vec<String> {
    const DEFAULT: &str = ".COM;.EXE;.BAT;.CMD;.VBS;.VBE;.JS;.JSE;.WSF;.WSH;.MSC";
    let raw = std::env::var("PATHEXT").unwrap_or_default();
    let parse = |v: &str| -> Vec<String> {
        v.split(';')
            .map(str::trim)
            .filter(|e| e.starts_with('.') && e.chars().count() <= 16)
            .map(|e| e[1..].to_lowercase())
            .filter(|e| !e.is_empty())
            .collect()
    };
    let from_env = parse(&raw);
    if from_env.is_empty() {
        parse(DEFAULT)
    } else {
        from_env
    }
}

/// 2.1.220 `BQ_` — reduce a path to its final segment, lowercased.
///
/// Unquotes, drops a leading drive letter (`C:` NOT followed by a separator),
/// splits on either separator, strips each segment's alternate-data-stream
/// suffix (`name:stream`), resolves `.` / `..`, and lowercases the last
/// segment.
fn battery_bq_basename(e: &str) -> String {
    let unquoted = strip_surrounding_quotes(e.trim());
    // `C:foo` is drive-relative; `C:\foo` is absolute. Only the former's
    // prefix is dropped here — the latter's separator survives the split.
    let mut rest = unquoted;
    let bytes = rest.as_bytes();
    if bytes.len() >= 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && !matches!(bytes.get(2), Some(b'\\' | b'/'))
    {
        rest = &rest[2..];
    }
    let mut stack: Vec<String> = Vec::new();
    for seg in rest.split(['\\', '/']).filter(|s| !s.is_empty()) {
        // `name:stream` — the ADS suffix names a stream, not the file.
        let seg = seg.split(':').next().unwrap_or(seg);
        let seg = battery_backtick_decode(seg);
        if seg == "." || seg.is_empty() {
            continue;
        }
        if seg == ".." {
            match stack.last() {
                Some(top) if top != ".." => {
                    stack.pop();
                }
                _ => stack.push("..".to_string()),
            }
            continue;
        }
        stack.push(seg);
    }
    stack.last().map(|s| s.to_lowercase()).unwrap_or_default()
}

/// 2.1.220 `LDo` — `(base, stem)`: the lowercased basename, and the same with a
/// trailing PATHEXT extension removed.
///
/// Only a PATHEXT extension is stripped, not any dot-suffix: `git.bat` has stem
/// `git` (and would shadow `git`), while `my.notes` keeps its stem `my.notes`
/// because `.notes` is not executable.
fn battery_ldo(e: &str) -> (String, String) {
    let base = battery_bq_basename(e);
    let stem = pathext_stems()
        .iter()
        .find_map(|ext| {
            let suffix = format!(".{ext}");
            base.strip_suffix(&suffix).map(str::to_string)
        })
        .unwrap_or_else(|| base.clone());
    (base, stem)
}

/// The cwd-first shadowing ask message (2.1.220, verbatim).
fn battery_shadow_message(name: &str) -> String {
    format!(
        "An earlier sub-command writes a file (./{name}.*) that would shadow the later \
`{name}` command under Windows PowerShell 5.1 cwd-first resolution."
    )
}

/// cd-git ask message (2.1.211 `NTU`, `if(y&&S)`).
const BATTERY_CD_GIT: &str =
    "Compound commands with cd/Set-Location and git require approval to prevent bare repository attacks";
/// git-internal-write ask message (2.1.211 `NTU`, inside `if(S)`, `if(V||U)`).
const BATTERY_GIT_INTERNAL_WRITE: &str = "Command writes to a git-internal path (HEAD, objects/, refs/, hooks/, .git/) and runs git. This could plant a malicious hook that git then executes.";
/// xcopy/robocopy + git ask message (2.1.211 `NTU`, inside `if(S)`, `mxg`).
const BATTERY_XCOPY_ROBOCOPY: &str = "Compound command runs a native file copier (xcopy/robocopy) and git. The copier can place files at git-internal paths (HEAD, objects/, refs/) that git then treats as repository state.";
/// archive-extract + git ask message (2.1.211 `NTU`, `pxg`, git-present branch).
const BATTERY_ARCHIVE_GIT: &str = "Compound command extracts an archive and runs git. Archive contents may plant bare-repository indicators (HEAD, hooks/, refs/) that git then treats as the repository root.";
/// archive-extract (no git) ask message (2.1.211 `NTU`, `pxg`, no-git branch).
const BATTERY_ARCHIVE_NO_GIT: &str = "Compound command extracts an archive followed by other commands. Archive contents (symlinks, config files) cannot be validated and may redirect subsequent path operations.";
/// dotgit-write ask message (2.1.211 `NTU`, `deo`). Em-dash is U+2014.
const BATTERY_DOTGIT_WRITE: &str =
    "Command writes to .git/ \u{2014} hooks or config planted there execute on the next git operation.";

/// Native file copiers (2.1.211 `fxg`) — matched by PLAIN basename-lowercase, NOT
/// [`normalize_cmdlet`] (the set lists the `.exe` variants explicitly).
const COPIER_SET: [&str; 4] = ["xcopy", "xcopy.exe", "robocopy", "robocopy.exe"];
/// Archive extractors (2.1.211 `pxg`) — matched by PLAIN basename-lowercase.
const ARCHIVE_SET: [&str; 15] = [
    "tar",
    "tar.exe",
    "bsdtar",
    "bsdtar.exe",
    "unzip",
    "unzip.exe",
    "7z",
    "7z.exe",
    "7za",
    "7za.exe",
    "gzip",
    "gzip.exe",
    "gunzip",
    "gunzip.exe",
    "expand-archive",
];
/// Write-cmdlet set (2.1.211 `LYi`) — keyed by [`normalize_cmdlet`]. DISTINCT
/// from the path-taking `FKN` map.
const WRITE_CMDLETS: [&str; 13] = [
    "new-item",
    "set-content",
    "add-content",
    "out-file",
    "copy-item",
    "move-item",
    "rename-item",
    "expand-archive",
    "invoke-webrequest",
    "invoke-restmethod",
    "tee-object",
    "export-csv",
    "export-clixml",
];

// --- `Zbu` copy/move destination-analyzer parameter categories -------------
/// 2.1.211 `Y0g` — path parameters (prefix-matched).
const ZBU_PATH: [&str; 2] = ["path", "literalpath"];
/// 2.1.211 `J0g` — literal-path parameters (exact).
const ZBU_LITERALPATH: [&str; 2] = ["pspath", "lp"];
/// 2.1.211 `X0g` — common switch parameters (exact).
const ZBU_SWITCH_EXACT: [&str; 5] = ["cf", "wi", "vb", "db", "usetx"];
/// 2.1.211 `Q0g` — common value parameters (exact).
const ZBU_VALUE_EXACT: [&str; 10] = [
    "ea", "ev", "wa", "wv", "infa", "iv", "proga", "ov", "ob", "pv",
];
/// 2.1.211 `Z0g` — switch parameters (prefix-matched).
const ZBU_SWITCH_PREFIX: [&str; 9] = [
    "container",
    "force",
    "passthru",
    "recurse",
    "whatif",
    "confirm",
    "usetransaction",
    "verbose",
    "debug",
];
/// 2.1.211 `eRg` — value parameters (prefix-matched).
const ZBU_VALUE_PREFIX: [&str; 16] = [
    "filter",
    "include",
    "exclude",
    "credential",
    "fromsession",
    "tosession",
    "erroraction",
    "errorvariable",
    "warningaction",
    "warningvariable",
    "informationaction",
    "informationvariable",
    "progressaction",
    "outvariable",
    "outbuffer",
    "pipelinevariable",
];

/// PLAIN basename-lowercase of a command name (2.1.211 `mxg`/`pxg` normalization):
/// lowercase, then the substring after the last `\` or `/`. NO extension strip, NO
/// alias resolution — deliberately different from [`normalize_cmdlet`].
fn battery_basename_lower(name: &str) -> String {
    let lower = name.to_lowercase();
    let cut = lower.rfind(['\\', '/']).map_or(0, |i| i + 1);
    lower[cut..].to_string()
}

/// True when any command name resolves to `git` via [`normalize_cmdlet`]
/// (2.1.211 `S = u.some(({element:V})=>D_(V.name)==="git")`).
fn battery_has_git(names: &[&str]) -> bool {
    names.iter().any(|n| normalize_cmdlet(n) == "git")
}

/// 2.1.211 `Vwe` casefold: lowercase, `ı`→`i`, `ſ`→`s`, then final-sigma `ς`→`σ`.
/// NOTE: the binary also applies Unicode NFC before the final-sigma fold; that
/// step is omitted here because every target segment (`.git`, `head`, `objects`,
/// `refs`, `hooks`, `git~N`) is pure ASCII (NFC-stable), so its omission cannot
/// cause an UNDER-ask on the git-internal/dotgit sets.
fn battery_casefold(s: &str) -> String {
    casefold_path(s).replace('\u{03C2}', "\u{03C3}")
}

/// 2.1.211 `l8` = `Vbu(e, undefined)`: PowerShell backtick decoding with NO escape
/// table (so `` `n ``→`n`, not newline). Removes `` `<newline><ws> `` line
/// continuations, decodes `` `u{hex} ``, and turns any other `` `X `` into `X`.
fn battery_backtick_decode(s: &str) -> String {
    // Pass 1: /`[\r\n]+\s*/g → "" (line continuation).
    let chars: Vec<char> = s.chars().collect();
    let mut p1: Vec<char> = Vec::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '`' && i + 1 < chars.len() && matches!(chars[i + 1], '\r' | '\n') {
            let mut j = i + 1;
            while j < chars.len() && matches!(chars[j], '\r' | '\n') {
                j += 1;
            }
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            i = j;
            continue;
        }
        p1.push(chars[i]);
        i += 1;
    }
    // Pass 2: /`(?:u\{([0-9a-fA-F]{1,6})\}|([\s\S]?))/g.
    let mut out = String::with_capacity(p1.len());
    let mut i = 0;
    while i < p1.len() {
        if p1[i] != '`' {
            out.push(p1[i]);
            i += 1;
            continue;
        }
        // Try `u{hex}`.
        if i + 1 < p1.len() && (p1[i + 1] == 'u') && i + 2 < p1.len() && p1[i + 2] == '{' {
            let mut j = i + 3;
            let mut hex = String::new();
            while j < p1.len() && p1[j].is_ascii_hexdigit() && hex.len() < 6 {
                hex.push(p1[j]);
                j += 1;
            }
            if !hex.is_empty() && j < p1.len() && p1[j] == '}' {
                if let Ok(cp) = u32::from_str_radix(&hex, 16) {
                    match char::from_u32(cp) {
                        Some(c) if cp <= 0x10_FFFF => out.push(c),
                        _ => out.push('\u{FFFD}'),
                    }
                } else {
                    out.push('\u{FFFD}');
                }
                i = j + 1;
                continue;
            }
        }
        // Backtick + single char (or backtick at end → nothing).
        if i + 1 < p1.len() {
            out.push(p1[i + 1]);
            i += 2;
        } else {
            i += 1;
        }
    }
    out
}

/// Node `path.posix.normalize` for a relative/absolute posix string (used by
/// [`battery_ueo`]). Collapses `.`/`..`, preserves a leading `/` and a trailing
/// `/`, and returns `.` for an empty result.
fn battery_posix_normalize(s: &str) -> String {
    let is_abs = s.starts_with('/');
    let has_trailing = s.len() > 1 && s.ends_with('/');
    let mut out: Vec<&str> = Vec::new();
    for seg in s.split('/') {
        match seg {
            "" | "." => {}
            ".." => match out.last() {
                Some(&"..") => {
                    if !is_abs {
                        out.push("..");
                    }
                }
                Some(_) => {
                    out.pop();
                }
                None => {
                    if !is_abs {
                        out.push("..");
                    }
                }
            },
            other => out.push(other),
        }
    }
    let mut res = out.join("/");
    if is_abs {
        res = format!("/{res}");
    }
    if res.is_empty() {
        return if is_abs { "/".into() } else { ".".into() };
    }
    if has_trailing && !res.ends_with('/') {
        res.push('/');
    }
    res
}

/// Posix basename (2.1.211 `M3.posix.basename`): the last `/`-segment; `.`/`..`
/// are returned verbatim.
fn battery_posix_basename(s: &str) -> String {
    let trimmed = s.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(i) => trimmed[i + 1..].to_string(),
        None => trimmed.to_string(),
    }
}

/// Strip a leading `Provider\…\FileSystem::` prefix
/// (2.1.211 `/^(?:[A-Za-z0-9_.]+\\){0,3}FileSystem::/i`). Runs BEFORE the
/// backslash→slash conversion, so path separators are still `\`.
fn battery_strip_fs_provider(t: &str) -> String {
    let chars: Vec<char> = t.chars().collect();
    let target: Vec<char> = "filesystem::".chars().collect();
    let matches_at = |p: usize| -> bool {
        if p + target.len() > chars.len() {
            return false;
        }
        (0..target.len()).all(|k| chars[p + k].to_ascii_lowercase() == target[k])
    };
    let mut ends = vec![0usize];
    let mut pos = 0usize;
    for _ in 0..3 {
        let start = pos;
        let mut j = start;
        while j < chars.len()
            && (chars[j].is_ascii_alphanumeric() || chars[j] == '_' || chars[j] == '.')
        {
            j += 1;
        }
        if j > start && j < chars.len() && chars[j] == '\\' {
            pos = j + 1;
            ends.push(pos);
        } else {
            break;
        }
    }
    for &ge in ends.iter().rev() {
        if matches_at(ge) {
            return chars[ge + target.len()..].iter().collect();
        }
    }
    t.to_string()
}

/// Replace a leading `X:` NOT followed by a separator with `./`
/// (2.1.211 `/^[A-Za-z]:(?![/\\])/ → "./"`).
fn battery_strip_drive_relative(t: &str) -> String {
    let chars: Vec<char> = t.chars().collect();
    if chars.len() >= 2 && chars[0].is_ascii_alphabetic() && chars[1] == ':' {
        let after = chars.get(2);
        if after.is_none_or(|&c| c != '/' && c != '\\') {
            let rest: String = chars[2..].iter().collect();
            return format!("./{rest}");
        }
    }
    t.to_string()
}

/// Per-segment trailing strip inside [`battery_ueo`] (2.1.211 inner `.map`):
/// repeatedly drop trailing spaces then trailing dots; `.`/`..` are preserved; an
/// emptied segment becomes `.`.
fn battery_strip_segment(seg: &str) -> String {
    if seg.is_empty() {
        return String::new();
    }
    let mut n = seg.to_string();
    loop {
        let o = n.clone();
        while n.ends_with(' ') {
            n.pop();
        }
        if n == "." || n == ".." {
            return n;
        }
        while n.ends_with('.') {
            n.pop();
        }
        if n == o {
            break;
        }
    }
    if n.is_empty() {
        ".".to_string()
    } else {
        n
    }
}

/// 2.1.211 `ueo` — the path normalizer feeding `$Xt`/`deo`. Strips comments/quotes,
/// decodes backticks, drops a provider prefix / drive-relative marker, converts
/// separators, expands `~`, splits off a drive, strips trailing spaces/dots per
/// segment, posix-normalizes, and drops a leading `./`.
fn battery_ueo(e: &str, ctx: &PsCtx) -> String {
    // yre
    let mut t = strip_comments_and_leading_ws(e).to_string();
    // Drop a leading `-X:`/`/X:` parameter prefix, then re-yre.
    if let Some(fc) = t.chars().next() {
        if EY.contains(&fc) || fc == '/' {
            if let Some(rel) = t[fc.len_utf8()..].find(':') {
                let colon = fc.len_utf8() + rel;
                t = strip_comments_and_leading_ws(&t[colon + 1..]).to_string();
            }
        }
    }
    // FM (strip surrounding quotes) → l8 (backtick decode).
    t = strip_surrounding_quotes(&t).to_string();
    t = battery_backtick_decode(&t);
    // Provider prefix, drive-relative marker (both before separator conversion).
    t = battery_strip_fs_provider(&t);
    t = battery_strip_drive_relative(&t);
    t = t.replace('\\', "/");
    // Tilde expansion.
    if t == "~" || t.starts_with("~/") {
        if let Some(home) = ctx.roots.home.as_ref() {
            let h = home.to_string_lossy();
            t = format!("{h}{}", &t[1..]).replace('\\', "/");
        }
    }
    // Split off a leading `X:/` drive.
    let mut drive = String::new();
    {
        let c: Vec<char> = t.chars().collect();
        if c.len() >= 3 && c[0].is_ascii_alphabetic() && c[1] == ':' && c[2] == '/' {
            drive = t[..2].to_string();
            t = t[2..].to_string();
        }
    }
    // Per-segment trailing strip.
    t = t
        .split('/')
        .map(battery_strip_segment)
        .collect::<Vec<_>>()
        .join("/");
    t = battery_posix_normalize(&t);
    if !drive.is_empty() {
        t = format!("{drive}{t}");
    }
    if let Some(rest) = t.strip_prefix("./") {
        t = rest.to_string();
    }
    t
}

/// 2.1.211 `Xbu` — collapse a leading run of `../<casefold(basename(cwd))>/` and a
/// bare trailing `../<base>` → `.`. Input is already casefolded.
fn battery_xbu(e: &str, ctx: &PsCtx) -> String {
    if !e.starts_with("../") {
        return e.to_string();
    }
    let base = battery_casefold(&battery_posix_basename(&ctx.roots.cwd.to_string_lossy()));
    if base.is_empty() {
        return e.to_string();
    }
    let prefix = format!("../{base}/");
    let mut n = e.to_string();
    while n.starts_with(&prefix) {
        n = n[prefix.len()..].to_string();
    }
    if n == format!("../{base}") {
        return ".".to_string();
    }
    n
}

/// 2.1.211 `Qbu` — resolve `t` against cwd (lexically, per the module's
/// realpath-free convention) and return the casefolded cwd-relative path, `.` when
/// equal, or `None` when it escapes cwd.
fn battery_qbu(t: &str, ctx: &PsCtx) -> Option<String> {
    let resolved = crate::filesystem::expand_path(t, ctx.roots);
    let a = battery_casefold(&resolved.to_string_lossy());
    let cwd = ctx.roots.cwd.to_string_lossy().into_owned();
    let l = battery_casefold(&cwd);
    if a == l {
        return Some(".".to_string());
    }
    let sep = std::path::MAIN_SEPARATOR;
    let s = if cwd.ends_with(sep) {
        cwd
    } else {
        format!("{cwd}{sep}")
    };
    let c = battery_casefold(&s);
    if !a.starts_with(&c) {
        return None;
    }
    Some(a[c.len()..].replace('\\', "/"))
}

/// 2.1.211 `tRg` — true iff `e` resolves to exactly cwd (the realpath-free
/// reduction of the containment walk when `originalCwd == cwd`).
fn battery_trg(e: &str, ctx: &PsCtx) -> bool {
    let resolved = crate::filesystem::expand_path(e, ctx.roots);
    battery_casefold(&resolved.to_string_lossy())
        == battery_casefold(&ctx.roots.cwd.to_string_lossy())
}

/// `/^git~\d+($|\/)/` — a Windows 8.3 short name for a `.git` dir.
fn battery_git_shortname(e: &str) -> bool {
    let Some(rest) = e.strip_prefix("git~") else {
        return false;
    };
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 {
        return false;
    }
    let after = &rest[digits..];
    after.is_empty() || after.starts_with('/')
}

/// 2.1.211 `zbu` — git-internal segment matcher (HEAD/objects/refs/hooks/.git +
/// `git~N`).
fn battery_zbu(e: &str) -> bool {
    if e == "head" || e == ".git" {
        return true;
    }
    if e.starts_with(".git/") || battery_git_shortname(e) {
        return true;
    }
    // K0g minus "head" (the loop `continue`s on "head").
    for t in ["objects", "refs", "hooks"] {
        if e == t || e.starts_with(&format!("{t}/")) {
            return true;
        }
    }
    false
}

/// 2.1.211 `Kbu` — `.git` subtree matcher (`.git`, `.git/…`, `git~N`).
fn battery_kbu(e: &str) -> bool {
    if e == ".git" || e.starts_with(".git/") {
        return true;
    }
    battery_git_shortname(e)
}

/// 2.1.211 `$Xt` — a write target is a git-internal path (uses [`battery_zbu`]).
fn battery_xt_git_internal(e: &str, ctx: &PsCtx) -> bool {
    let t = battery_ueo(e, ctx);
    if battery_zbu(&battery_xbu(&battery_casefold(&t), ctx)) {
        return true;
    }
    matches!(battery_qbu(&t, ctx), Some(n) if battery_zbu(&n))
}

/// 2.1.211 `deo` — a write target is inside `.git/` (uses [`battery_kbu`]).
fn battery_deo_dotgit(e: &str, ctx: &PsCtx) -> bool {
    let t = battery_ueo(e, ctx);
    if battery_kbu(&battery_xbu(&battery_casefold(&t), ctx)) {
        return true;
    }
    matches!(battery_qbu(&t, ctx), Some(n) if battery_kbu(&n))
}

/// Whether the host resolves commands cwd-first (2.1.220 `Lt()==="windows"`).
///
/// `cfg!(windows)` rather than `#[cfg(windows)]` so the shadowing check is
/// COMPILED and type-checked on every platform — a `#[cfg]`-gated block would
/// only ever be built on the one OS nobody develops on here, which is how such
/// code rots.
///
/// The value is overridable in tests. Without that the WIRING (as opposed to
/// the predicate) could only be exercised on Windows, so the guard would ship
/// with its entry point untested on the machines it is written on.
#[cfg(not(test))]
fn battery_host_is_windows() -> bool {
    cfg!(windows)
}

#[cfg(test)]
thread_local! {
    /// Test override for [`battery_host_is_windows`]; `None` ⇒ the real host.
    static FORCE_WINDOWS_HOST: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn battery_host_is_windows() -> bool {
    FORCE_WINDOWS_HOST
        .with(|c| c.get())
        .unwrap_or(cfg!(windows))
}

/// Run `f` as though the host were (or were not) Windows. Thread-local, so
/// parallel tests cannot see each other's override.
#[cfg(test)]
pub(crate) fn with_windows_host<T>(is_windows: bool, f: impl FnOnce() -> T) -> T {
    FORCE_WINDOWS_HOST.with(|c| c.set(Some(is_windows)));
    let out = f();
    FORCE_WINDOWS_HOST.with(|c| c.set(None));
    out
}

/// The first sub-command whose name is shadowed by a file an EARLIER
/// sub-command writes (2.1.220 `NTU`'s `Lt()==="windows" && u.length>1` block).
///
/// Walks the sub-commands in order, carrying the set of write-target stems seen
/// so far. Order is the whole point: a file written AFTER a command runs cannot
/// shadow it, so each command is tested against the set BEFORE its own writes
/// are added.
///
/// The set is pre-seeded with every line-level redirection target, because a
/// redirection at the end of the line (`… > git.bat`) is not attached to any one
/// sub-command yet still lands before the next line runs.
///
/// Returns the ORIGINAL command name (not the normalized one) so the message
/// echoes what the user wrote. `None` when nothing is shadowed, or when there is
/// only one sub-command — a single command cannot be preceded by a write.
fn battery_shadowed_command(statements: &[PsStatement], u: &[BatteryUEntry<'_>]) -> Option<String> {
    if u.len() <= 1 {
        return None;
    }
    let mut written: std::collections::HashSet<String> = battery_r5r_targets(statements)
        .iter()
        .map(|t| battery_ldo(t).1)
        .filter(|s| !s.is_empty())
        .collect();

    for entry in u {
        let cmd = entry.cmd;
        let (base, stem) = battery_ldo(&cmd.name);
        // `git` shadowed by a written `git.bat` (stem match), or `git.exe`
        // shadowed by a written `git.exe.bat` (base match — only meaningful
        // when the invocation carried its own extension).
        if (!stem.is_empty() && written.contains(&stem))
            || (base != stem && written.contains(&base))
        {
            return Some(cmd.name.clone());
        }
        for r in &cmd.redirections {
            let s = battery_ldo(&r.target).1;
            if !s.is_empty() {
                written.insert(s);
            }
        }
        // A write CMDLET's arguments are write targets too (`Copy-Item x
        // ./git.bat`). Strip a leading `-Flag:` before reading the path, the
        // way the oracle does.
        let canon = normalize_cmdlet(&cmd.name);
        if WRITE_CMDLETS.contains(&canon.as_str()) {
            for a in cmd.args.iter().flat_map(|a| battery_peo(a.as_ref())) {
                let stripped = battery_strip_leading_flag(&a);
                let s = battery_ldo(&stripped).1;
                if !s.is_empty() {
                    written.insert(s);
                }
            }
        }
    }
    None
}

/// Drop a leading `-Flag` / `--Flag` / en/em-dash variant, with an optional
/// trailing colon (2.1.220 `re.replace(/^[-\u2013\u2014\u2015]+[A-Za-z]+:?/, "")`).
fn battery_strip_leading_flag(a: &str) -> String {
    let mut chars = a.char_indices();
    let mut idx = 0usize;
    let mut saw_dash = false;
    for (i, c) in chars.by_ref() {
        if matches!(c, '-' | '\u{2013}' | '\u{2014}' | '\u{2015}') {
            saw_dash = true;
            idx = i + c.len_utf8();
        } else {
            break;
        }
    }
    if !saw_dash {
        return a.to_string();
    }
    let rest = &a[idx..];
    let letters: usize = rest
        .chars()
        .take_while(char::is_ascii_alphabetic)
        .map(char::len_utf8)
        .sum();
    if letters == 0 {
        return a.to_string();
    }
    let mut end = idx + letters;
    if a[end..].starts_with(':') {
        end += 1;
    }
    a[end..].to_string()
}

/// 2.1.211 `peo` — comma-split flattener: `[e, ...e.split(",")]` when `e` has a
/// comma, else `[e]`.
fn battery_peo(e: &str) -> Vec<String> {
    if !e.contains(',') {
        return vec![e.to_string()];
    }
    let mut v = vec![e.to_string()];
    v.extend(e.split(',').map(str::to_string));
    v
}

/// 2.1.211 `Ybu` — a glob/`$` metacharacter is present (`/[*?[\]$]/`).
fn battery_ybu(e: &str) -> bool {
    e.chars().any(|c| matches!(c, '*' | '?' | '[' | ']' | '$'))
}

fn any_prefix_of(list: &[&str], g: &str) -> bool {
    list.iter().any(|h| h.starts_with(g))
}

/// 2.1.211 `Zbu` — copy/move DESTINATION git-internal analyzer. Walks the args of a
/// `Copy-Item`/`Move-Item` splitting them into parameters (by the `Zbu_*`
/// category sets) and positionals; returns `true` on ANY ambiguity or when a
/// destination/source basename resolves to a git-internal path. `has_siblings`
/// mirrors the `t` flag ("this command has sibling pipeline commands").
fn battery_zbu_analyze(args: &[String], has_siblings: bool, ctx: &PsCtx) -> bool {
    let mut path_vals: Vec<String> = Vec::new(); // r
    let mut positionals: Vec<String> = Vec::new(); // n
    let mut dest: Option<String> = None; // o
    let mut has_path_param = false; // i
    let mut container_non_true = false; // s
    let mut literal_vals: Vec<String> = Vec::new(); // a

    let mut p = 0usize;
    while p < args.len() {
        let f = strip_comments_and_leading_ws(&args[p]).to_string();
        let first = f.chars().next();
        if f.is_empty() || first.is_none_or(|c| !EY.contains(&c)) {
            positionals.push(args[p].clone());
            p += 1;
            continue;
        }
        // Parameter: -name[:value]. `indexOf(":",1)`.
        let fc_len = first.map_or(0, char::len_utf8);
        let colon = f[fc_len..].find(':').map(|r| fc_len + r);
        let (name, inline_val) = match colon {
            Some(ci) => (f[fc_len..ci].to_lowercase(), Some(f[ci + 1..].to_string())),
            None => (f[fc_len..].to_lowercase(), None),
        };
        let g = name;
        if g.is_empty() {
            return true;
        }
        let is_dest = "destination".starts_with(&g);
        let is_literal = ZBU_LITERALPATH.contains(&g.as_str()) || "literalpath".starts_with(&g);
        let is_path = is_literal || any_prefix_of(&ZBU_PATH, &g);
        let is_switch =
            ZBU_SWITCH_EXACT.contains(&g.as_str()) || any_prefix_of(&ZBU_SWITCH_PREFIX, &g);
        let is_value =
            ZBU_VALUE_EXACT.contains(&g.as_str()) || any_prefix_of(&ZBU_VALUE_PREFIX, &g);
        let categories =
            i32::from(is_dest) + i32::from(is_path) + i32::from(is_switch) + i32::from(is_value);
        if categories != 1 {
            return true;
        }
        if is_switch {
            if "container".starts_with(&g) {
                if let Some(v) = inline_val.as_ref() {
                    let h = strip_surrounding_quotes(strip_comments_and_leading_ws(v));
                    if !h.trim().eq_ignore_ascii_case("$true") {
                        container_non_true = true;
                    }
                }
            }
            p += 1;
            continue;
        }
        // Value = inline `:val` or the next arg (raw).
        let value = match inline_val {
            Some(v) => Some(v),
            None => {
                p += 1;
                args.get(p).cloned()
            }
        };
        let Some(value) = value else {
            p += 1;
            continue;
        };
        if is_dest {
            dest = Some(value);
        } else if is_path {
            has_path_param = true;
            if is_literal {
                literal_vals.extend(value.split(',').map(str::to_string));
            } else {
                path_vals.extend(value.split(',').map(str::to_string));
            }
        }
        p += 1;
    }

    let expected_positionals = i32::from(!has_path_param) + i32::from(dest.is_none());
    if positionals.len() as i32 > expected_positionals {
        return true;
    }
    let mut second_pos: Option<String> = None; // c
    let mut u = 0usize;
    if !has_path_param && u < positionals.len() {
        path_vals.extend(positionals[u].split(',').map(str::to_string));
        u += 1;
    }
    if dest.is_none() && u < positionals.len() {
        second_pos = Some(positionals[u].clone());
    }
    if path_vals.is_empty() && literal_vals.is_empty() && !has_siblings {
        return false;
    }
    let d = dest.clone().or(second_pos);
    if let Some(d) = d {
        let mut pp = battery_ueo(&d, ctx);
        if pp.is_empty() {
            pp = ".".to_string();
        }
        if battery_ybu(&pp) {
            return true;
        }
        if !battery_trg(&pp, ctx) {
            return false;
        }
    }
    if container_non_true || has_siblings {
        return true;
    }
    for (is_lit, list) in [(false, &path_vals), (true, &literal_vals)] {
        for m in list {
            let mut g = battery_ueo(m, ctx);
            if g.is_empty() {
                g = ".".to_string();
            }
            let bad = if is_lit {
                g.contains('$')
            } else {
                battery_ybu(&g)
            };
            if bad {
                return true;
            }
            let y = battery_posix_basename(&g);
            if y == "." || y == ".." {
                return true;
            }
            if battery_xt_git_internal(&y, ctx) {
                return true;
            }
        }
    }
    false
}

/// A flattened `u`-list entry (2.1.211 `LTU`): a command element plus its
/// containing statement and, for a main-pipeline element, its index within
/// `statement.commands`.
struct BatteryUEntry<'a> {
    cmd: &'a PsCommand,
    stmt: &'a PsStatement,
    /// `Some(idx)` for a main-pipeline `CommandAst`; `None` for a nested command.
    main_index: Option<usize>,
}

/// Build the `u`-list: every main-pipeline `CommandAst` (skipping `Expression`
/// elements) plus every `nested_commands` entry, in order.
fn battery_u_list(statements: &[PsStatement]) -> Vec<BatteryUEntry<'_>> {
    let mut u = Vec::new();
    for stmt in statements {
        for (idx, el) in stmt.commands.iter().enumerate() {
            if let PsElement::Command(c) = el {
                u.push(BatteryUEntry {
                    cmd: c,
                    stmt,
                    main_index: Some(idx),
                });
            }
        }
        for c in &stmt.nested_commands {
            u.push(BatteryUEntry {
                cmd: c,
                stmt,
                main_index: None,
            });
        }
    }
    u
}

/// Statement-level + nested-command redirection targets, filtered like 2.1.211
/// `R5r` (`!isMerging && !xXt`).
fn battery_r5r_targets(statements: &[PsStatement]) -> Vec<&str> {
    let mut out = Vec::new();
    for stmt in statements {
        for r in &stmt.redirections {
            if !r.is_merging && !is_null_redirect(&r.target) {
                out.push(r.target.as_str());
            }
        }
        for c in &stmt.nested_commands {
            for r in &c.redirections {
                if !r.is_merging && !is_null_redirect(&r.target) {
                    out.push(r.target.as_str());
                }
            }
        }
    }
    out
}

/// PERM-PS-CALLER-06 git-security caller battery (2.1.211 `NTU` in-scope asks).
///
/// Evaluated by [`crate::policy`]'s `check_powershell_containment` BEFORE the
/// [`validate_ps_statements`] (`gTu`) result is consumed, so a battery ask
/// outranks the generic containment ask (a `gTu` DENY still wins). Returns the
/// FIRST matching battery ask in `NTU` push order, or `None` (no battery ask →
/// fall through to `gTu`).
///
/// `compound_cd` is the already-computed `y` flag (`u.length>1 && any cd-like`).
#[must_use]
pub fn powershell_git_battery(
    statements: &[PsStatement],
    ctx: &PsCtx,
    compound_cd: bool,
) -> Option<PsContainmentResult> {
    let u = battery_u_list(statements);
    let names: Vec<&str> = u.iter().map(|e| e.cmd.name.as_str()).collect();
    let has_git = battery_has_git(&names);

    let ask = |msg: &str| {
        Some(PsContainmentResult::Ask {
            message: msg.to_string(),
            reason: msg.to_string(),
        })
    };

    // 1. cd-git — `if(y&&S)`.
    if compound_cd && has_git {
        return ask(BATTERY_CD_GIT);
    }

    // 2. bare-repo indicators — `if(E && vLr())`. Git reads config and runs
    //    hooks from a directory carrying planted HEAD/objects/refs, or from a
    //    `.git` file/symlink redirecting somewhere unverifiable, so a git
    //    command there needs approval. Shared with the shell battery via
    //    `crate::git_bare_repo` so the two cannot drift.
    if has_git {
        if let Some(gate) = crate::git_bare_repo::bare_repo_gate(&ctx.roots.cwd) {
            return ask(gate.powershell_message());
        }
    }

    if has_git {
        // 2. git-internal-write — inside `if(S)`, `if(V||U)`.
        let v = u.iter().any(|entry| {
            let j = entry.cmd;
            // command-level redirections: RAW (no merge/null filter).
            for r in &j.redirections {
                if battery_xt_git_internal(&r.target, ctx) {
                    return true;
                }
            }
            let canon = normalize_cmdlet(&j.name);
            if !WRITE_CMDLETS.contains(&canon.as_str()) {
                return false;
            }
            if j.args
                .iter()
                .flat_map(|a| battery_peo(a))
                .any(|a| battery_xt_git_internal(&a, ctx))
            {
                return true;
            }
            if canon == "copy-item" || canon == "move-item" {
                let te: isize = entry.main_index.map_or(-1, |i| i as isize);
                let ae = te > 0 || (te == -1 && entry.stmt.commands.len() > 1);
                if battery_zbu_analyze(&j.args, ae, ctx) {
                    return true;
                }
            }
            // Non-CommandAst expression elements in the statement.
            for el in &entry.stmt.commands {
                if let PsElement::Expression { text } = el {
                    if battery_xt_git_internal(text, ctx) {
                        return true;
                    }
                }
            }
            false
        });
        let u_redir = battery_r5r_targets(statements)
            .iter()
            .any(|t| battery_xt_git_internal(t, ctx));
        if v || u_redir {
            return ask(BATTERY_GIT_INTERNAL_WRITE);
        }

        // 3. xcopy/robocopy + git — inside `if(S)`, `mxg`.
        if names
            .iter()
            .any(|n| COPIER_SET.contains(&battery_basename_lower(n).as_str()))
        {
            return ask(BATTERY_XCOPY_ROBOCOPY);
        }
    }

    // PS-CALLER-06-5 — PowerShell 5.1 cwd-first command resolution.
    //
    // Windows PowerShell 5.1 looks in the CURRENT DIRECTORY before PATH, so an
    // earlier sub-command that writes `./git.bat` makes a later bare `git` run
    // that file. The oracle gates this on `Lt()==="windows"` and so does this
    // port: PowerShell Core on macOS/Linux resolves PATH-first, where the
    // shadowing cannot happen and the ask would be a false positive.
    if battery_host_is_windows() {
        if let Some(shadowed) = battery_shadowed_command(statements, &u) {
            return ask(&battery_shadow_message(&shadowed));
        }
    }

    // 4. archive-extract — `if(pxg && u.length>1)`.
    let archive_present = names
        .iter()
        .any(|n| ARCHIVE_SET.contains(&battery_basename_lower(n).as_str()));
    if archive_present && u.len() > 1 {
        return ask(if has_git {
            BATTERY_ARCHIVE_GIT
        } else {
            BATTERY_ARCHIVE_NO_GIT
        });
    }

    // 5. dotgit-write — `deo` (NOT gated on git presence).
    let dot = u.iter().any(|entry| {
        let j = entry.cmd;
        for r in &j.redirections {
            if battery_deo_dotgit(&r.target, ctx) {
                return true;
            }
        }
        let canon = normalize_cmdlet(&j.name);
        if !WRITE_CMDLETS.contains(&canon.as_str()) {
            return false;
        }
        j.args
            .iter()
            .flat_map(|a| battery_peo(a))
            .any(|a| battery_deo_dotgit(&a, ctx))
    });
    let dot_redir = battery_r5r_targets(statements)
        .iter()
        .any(|t| battery_deo_dotgit(t, ctx));
    if dot || dot_redir {
        return ask(BATTERY_DOTGIT_WRITE);
    }

    None
}

// ───────────────────────────────────────────────────────────────────────────
// acceptEdits whole-pipeline auto-allow — claude-code `zLs` (2.1.218).
//
// In `AcceptEdits` mode, CC auto-allows a STRUCTURALLY-SAFE PowerShell write
// pipeline via a whole-pipeline validator. This port covers the `acceptEdits`
// arm of `zLs`: the `Voe` feature aggregate, the New-Item link check (`VLs`), the
// compound cd+write guard (`gsn`/`GLs`), and the per-command / nested-command
// structural loops (`ANt`/`ULs`/`GLs`/`h3`). It ONLY ever ALLOWS or passes
// through — it never denies or asks. PATH containment stays the job of
// [`validate_ps_statements`]; the policy orchestrator composes them so an
// out-of-cwd containment ASK still overrides this structural ALLOW (see
// `PermissionPolicy::check_powershell_containment`). Consequently, being
// conservative here (passing through where the oracle would allow) is always the
// SAFE direction — it can only ask MORE, never auto-allow something dangerous.
// ───────────────────────────────────────────────────────────────────────────

/// Outcome of the `acceptEdits` whole-pipeline validator (claude-code `zLs`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PsAcceptEditsResult {
    /// The whole pipeline is structurally safe → auto-allow under `AcceptEdits`.
    Allow,
    /// Not auto-allowable — fall through to the normal permission flow (which may
    /// ask). Carries the claude-code passthrough reason (for diagnostics/tests).
    Passthrough(String),
}

/// `fG_` — cmdlets that WRITE a file (claude-code `GLs = fG_.has(wb(name))`).
const FG_WRITE: [&str; 4] = ["set-content", "add-content", "remove-item", "clear-content"];
/// `Y8_` — the out-null sink (claude-code `ANt = Y8_.has(wb(name))`): safe, no file.
const Y8_OUT_NULL: [&str; 1] = ["out-null"];
/// `J8_` — read-only formatting/display cmdlets (claude-code `ULs`'s set half).
const J8_FORMATTING: [&str; 11] = [
    "format-table",
    "format-list",
    "format-wide",
    "format-custom",
    "measure-object",
    "select-object",
    "sort-object",
    "group-object",
    "where-object",
    "out-string",
    "out-host",
];
/// `mG_` — New-Item link item-types (claude-code `VLs`).
const MG_SYMLINK_TYPES: [&str; 3] = ["symboliclink", "junction", "hardlink"];
const PLS_ACTION_PARAMS: [&str; 4] = [
    "-erroraction",
    "-warningaction",
    "-informationaction",
    "-progressaction",
];
const PLS_ACTION_SHORT_PARAMS: [&str; 4] = ["-ea", "-wa", "-infa", "-proga"];
const PLS_VARIABLE_PARAMS: [&str; 5] = [
    "-errorvariable",
    "-warningvariable",
    "-informationvariable",
    "-outvariable",
    "-pipelinevariable",
];
const PLS_VARIABLE_SHORT_PARAMS: [&str; 5] = ["-ev", "-wv", "-iv", "-ov", "-pv"];
const PLS_VARIABLE_EXTRA_PARAMS: [&str; 4] = [
    "-variable",
    "-sessionvariable",
    "-responseheadersvariable",
    "-statuscodevariable",
];
const PLS_EXACT_COMMON_PARAMS: [&str; 12] = [
    "-erroraction",
    "-warningaction",
    "-informationaction",
    "-progressaction",
    "-errorvariable",
    "-warningvariable",
    "-informationvariable",
    "-outvariable",
    "-pipelinevariable",
    "-outbuffer",
    "-verbose",
    "-debug",
];
const PLS_EXACT_COMMON_PARAMS_NO_PROGRESS: [&str; 11] = [
    "-erroraction",
    "-warningaction",
    "-informationaction",
    "-errorvariable",
    "-warningvariable",
    "-informationvariable",
    "-outvariable",
    "-pipelinevariable",
    "-outbuffer",
    "-verbose",
    "-debug",
];
const PLS_EXACT_BLOCKED_SHORT_PARAMS: [&str; 8] =
    ["-ev", "-wv", "-iv", "-ov", "-pv", "-ea", "-wa", "-p"];

static PLS_ALLOWED_ACTION_VALUES: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "silentlycontinue",
        "0",
        "stop",
        "1",
        "continue",
        "2",
        "ignore",
        "4",
    ]
    .into_iter()
    .collect()
});

static PLS_ALLOWED_VARIABLE_SCOPES: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    ["global", "script", "local", "private", "variable"]
        .into_iter()
        .collect()
});

static PLS_PROTECTED_VARIABLES: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "psdefaultparametervalues",
        "confirmpreference",
        "debugpreference",
        "erroractionpreference",
        "errorview",
        "formatenumerationlimit",
        "informationpreference",
        "maximumhistorycount",
        "ofs",
        "outputencoding",
        "progresspreference",
        "psemailserver",
        "psmoduleautoloadingpreference",
        "psnativecommandargumentpassing",
        "psnativecommanduseerroractionpreference",
        "pssessionapplicationname",
        "pssessionconfigurationname",
        "pssessionoption",
        "psstyle",
        "transcript",
        "verbosepreference",
        "warningpreference",
        "whatifpreference",
        "logcommandhealthevent",
        "logcommandlifecycleevent",
        "logenginehealthevent",
        "logenginelifecycleevent",
        "logproviderhealthevent",
        "logproviderlifecycleevent",
        "maximumaliascount",
        "maximumdrivecount",
        "maximumerrorcount",
        "maximumfunctioncount",
        "maximumvariablecount",
    ]
    .into_iter()
    .collect()
});

/// `GLs(name)` — is `name` a write cmdlet (post-`wb` normalization)?
fn gls_is_write(name: &str) -> bool {
    FG_WRITE.contains(&normalize_cmdlet(name).as_str())
}

/// `ANt(name)` — is `name` the out-null sink (post-`wb` normalization)?
fn ant_is_out_null(name: &str) -> bool {
    Y8_OUT_NULL.contains(&normalize_cmdlet(name).as_str())
}

#[derive(Clone)]
struct PsInlineBinding {
    colon_idx: usize,
    post: String,
    post_resolved: String,
    is_here_string: bool,
}

fn pls_matches_param_prefix(name: &str, candidates: &[&str]) -> bool {
    name.len() >= 2
        && candidates
            .iter()
            .any(|candidate| candidate.starts_with(name))
}

fn pls_unique_prefix_match<'a>(name: &str, candidates: &'a [&str]) -> Option<&'a str> {
    if name.len() < 2 {
        return None;
    }
    let mut found = None;
    for &candidate in candidates {
        if candidate.starts_with(name) {
            if found.is_some() {
                return None;
            }
            found = Some(candidate);
        }
    }
    found
}

fn pls_matches_exact_param(name: &str, shorts: &[&str], longs: &[&str]) -> bool {
    if PLS_EXACT_BLOCKED_SHORT_PARAMS.contains(&name) {
        return false;
    }
    shorts.contains(&name)
        || [
            PLS_EXACT_COMMON_PARAMS.as_slice(),
            PLS_EXACT_COMMON_PARAMS_NO_PROGRESS.as_slice(),
        ]
        .into_iter()
        .filter_map(|universe| pls_unique_prefix_match(name, universe))
        .any(|candidate| longs.contains(&candidate))
}

fn pls_matches_action_param(name: &str, exact: bool) -> bool {
    if exact {
        pls_matches_exact_param(name, &PLS_ACTION_SHORT_PARAMS, &PLS_ACTION_PARAMS)
    } else {
        PLS_ACTION_SHORT_PARAMS.contains(&name)
            || pls_matches_param_prefix(name, &PLS_ACTION_PARAMS)
    }
}

fn pls_matches_variable_param(name: &str, exact: bool) -> bool {
    if exact {
        pls_matches_exact_param(name, &PLS_VARIABLE_SHORT_PARAMS, &PLS_VARIABLE_PARAMS)
    } else {
        PLS_VARIABLE_SHORT_PARAMS.contains(&name)
            || pls_matches_param_prefix(name, &PLS_VARIABLE_PARAMS)
    }
}

fn pls_matches_extra_variable_param(name: &str) -> bool {
    name.len() >= 3
        && PLS_VARIABLE_EXTRA_PARAMS
            .iter()
            .any(|candidate| candidate.starts_with(name))
}

fn pls_normalize_param_variant(name: &str, exact: bool) -> String {
    if exact && name.as_bytes().get(1) == Some(&b'-') {
        return name.to_string();
    }
    name.char_indices()
        .filter_map(|(idx, ch)| {
            if idx > 0 && matches!(ch, '-' | '\'') {
                None
            } else {
                Some(ch)
            }
        })
        .collect()
}

fn pls_normalize_flag_prefix(arg: &str) -> String {
    let mut chars = arg.chars();
    match chars.next() {
        Some('-') => arg.to_string(),
        Some(_) => format!("-{}", chars.as_str()),
        None => String::new(),
    }
}

fn pls_parse_inline_binding(arg: &str) -> Option<PsInlineBinding> {
    let mut chars = arg.chars();
    let first = chars.next()?;
    if !EY.contains(&first) && first != '/' {
        return None;
    }
    let colon_idx = arg
        .char_indices()
        .skip(1)
        .find_map(|(idx, ch)| (ch == ':').then_some(idx))?;
    let post = strip_comments_and_leading_ws(&arg[colon_idx + 1..]).to_string();
    let post_resolved = strip_comments_and_leading_ws(&battery_backtick_decode(&post)).to_string();
    let is_here_string = matches!(post.chars().next(), Some('@'))
        && post.chars().nth(1).is_some_and(is_quote_char)
        || matches!(post_resolved.chars().next(), Some('@'))
            && post_resolved.chars().nth(1).is_some_and(is_quote_char);
    Some(PsInlineBinding {
        colon_idx,
        post,
        post_resolved,
        is_here_string,
    })
}

fn pls_has_action_preference_arg(args: &[String], element_types: &[String], exact: bool) -> bool {
    for (arg_idx, arg) in args.iter().enumerate() {
        if !arg.chars().next().is_some_and(|c| EY.contains(&c)) {
            continue;
        }
        if let Some(kind) = element_types.get(arg_idx + 1) {
            if kind != "Parameter" {
                continue;
            }
        }
        let normalized = pls_normalize_flag_prefix(arg);
        let colon_idx = normalized.find(':').filter(|idx| *idx > 0);
        let flag = match colon_idx {
            Some(idx) => normalized[..idx].to_lowercase(),
            None => normalized.to_lowercase(),
        };
        let mut decoded_value = None;
        if !pls_matches_action_param(&flag, exact)
            && !pls_matches_action_param(&pls_normalize_param_variant(&flag, exact), exact)
        {
            let decoded_flag = battery_backtick_decode(&flag).to_lowercase();
            let decoded_colon = decoded_flag.find(':').filter(|idx| *idx > 0);
            let decoded_head =
                decoded_colon.map_or(decoded_flag.as_str(), |idx| &decoded_flag[..idx]);
            if !pls_matches_action_param(decoded_head, exact)
                && !pls_matches_action_param(
                    &pls_normalize_param_variant(decoded_head, exact),
                    exact,
                )
                && decoded_flag.is_ascii()
            {
                continue;
            }
            if let Some(colon_idx) = decoded_colon {
                let inline = decoded_flag[colon_idx + 1..].trim();
                decoded_value = Some(if inline.is_empty() {
                    args.get(arg_idx + 1).cloned().unwrap_or_default()
                } else {
                    inline.to_string()
                });
            }
        }
        let fallback = colon_idx
            .map(|idx| normalized[idx + 1..].to_string())
            .filter(|value| !value.trim().is_empty())
            .or_else(|| args.get(arg_idx + 1).cloned())
            .unwrap_or_default();
        let value = decoded_value.unwrap_or(fallback);
        let normalized_value = strip_surrounding_quotes(&value).trim().to_lowercase();
        if !normalized_value.is_empty()
            && !PLS_ALLOWED_ACTION_VALUES.contains(normalized_value.as_str())
        {
            return true;
        }
    }
    false
}

fn pls_has_variable_write_arg(args: &[String], element_types: &[String], exact: bool) -> bool {
    let matches_param = |name: &str| {
        pls_matches_variable_param(name, exact)
            || (!exact && (name.ends_with("variable") || pls_matches_extra_variable_param(name)))
    };
    for (idx, arg) in args.iter().enumerate() {
        if !arg.chars().next().is_some_and(|c| EY.contains(&c)) {
            continue;
        }
        if let Some(kind) = element_types.get(idx + 1) {
            if kind != "Parameter" {
                continue;
            }
        }
        let normalized = pls_normalize_flag_prefix(arg);
        let binding = pls_parse_inline_binding(&normalized);
        let flag = binding
            .as_ref()
            .map_or_else(|| normalized.as_str(), |info| &normalized[..info.colon_idx])
            .to_lowercase();
        if !matches_param(&flag) && !matches_param(&pls_normalize_param_variant(&flag, exact)) {
            let decoded_flag = battery_backtick_decode(&flag).to_lowercase();
            if decoded_flag != flag
                && (matches_param(&decoded_flag)
                    || matches_param(&pls_normalize_param_variant(&decoded_flag, exact)))
            {
                return true;
            }
            if let Some(idx) = decoded_flag.find(':').filter(|idx| *idx > 0) {
                if matches_param(&pls_normalize_param_variant(&decoded_flag[..idx], exact)) {
                    return true;
                }
            }
            if !decoded_flag.is_ascii() {
                return true;
            }
            continue;
        }
        let value = if let Some(info) = binding {
            if info.is_here_string {
                return true;
            }
            if info.post.contains('$') || info.post.contains('`') {
                return true;
            }
            if !info.post_resolved.is_empty() {
                info.post_resolved
            } else {
                args.get(idx + 1).cloned().unwrap_or_default()
            }
        } else {
            args.get(idx + 1).cloned().unwrap_or_default()
        };
        if value.contains('$') || value.contains('`') {
            return true;
        }
        let normalized_value = clean_value(&value).trim().to_lowercase();
        if normalized_value.is_empty() {
            continue;
        }
        let trimmed = normalized_value
            .strip_prefix('+')
            .unwrap_or(&normalized_value);
        let mut variable_name = trimmed;
        if let Some(scope_idx) = trimmed.rfind(':') {
            let scope = &trimmed[..scope_idx];
            if !PLS_ALLOWED_VARIABLE_SCOPES.contains(scope)
                && !scope.chars().all(|ch| ch.is_ascii_digit())
            {
                return true;
            }
            variable_name = &trimmed[scope_idx + 1..];
        }
        if !variable_name
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
        {
            return true;
        }
        if PLS_PROTECTED_VARIABLES.contains(variable_name) {
            return true;
        }
    }
    false
}

fn pls_guard_message(cmd: &PsCommand, nested: bool) -> String {
    if nested {
        format!(
            "Variable-writing or ActionPreference argument in nested '{}' requires approval",
            cmd.name
        )
    } else {
        format!(
            "Variable-writing or ActionPreference argument in '{}' requires approval",
            cmd.name
        )
    }
}

/// `ULs(cmd, command)` — is `cmd` a read-only formatting/display cmdlet whose
/// arguments the read-only-command validator (`wNt`) accepts?
///
/// The `J8_` formatting SET is ported faithfully. The `wNt` half — CC's large
/// read-only-command validator (its own `jRd` regex/callback table + `X8_`
/// application whitelist) — is NOT ported here and is treated as UNSATISFIED
/// (returns `false`). Because `zLs` only ever ALLOWS or passes through, refusing
/// to treat a formatting cmdlet as auto-safe merely makes the pipeline fall to
/// `!GLs` → passthrough (an ASK). That is strictly the SAFE (over-ask) direction:
/// it can NEVER auto-allow a formatting-cmdlet pipeline the oracle would reject.
/// The residual is a bounded OVER-ASK (a pipeline that pipes through
/// `Select-Object`/`Sort-Object`/… into a write asks instead of auto-allowing);
/// porting `wNt` in a follow-up would close it without weakening safety.
fn uls_is_safe_formatting(cmd: &PsCommand) -> bool {
    if !J8_FORMATTING.contains(&normalize_cmdlet(&cmd.name).as_str()) {
        return false;
    }
    // wNt(cmd, command) conservatively unsatisfied — see the doc comment above.
    false
}

/// claude-code `/[$(@{[]/` — does `s` contain a metacharacter that begins an
/// expression (variable / subexpression / splat / hashtable / array)?
fn contains_expr_meta(s: &str) -> bool {
    s.chars().any(|c| matches!(c, '$' | '(' | '@' | '{' | '['))
}

/// `hG_(l)` — is `l` an `-ItemType` / `-Type` parameter name (≥3-char unambiguous
/// prefix)?
fn hg_is_itemtype(l: &str) -> bool {
    (l.len() >= 3 && "-itemtype".starts_with(l)) || (l.len() >= 3 && "-type".starts_with(l))
}

/// `gsn(name)` — a directory-CHANGING command (claude-code): the literal
/// `cd..`/`cd\`/`cd/`/`cd~` forms, a bare drive letter (`^[a-z]:$`), or a name
/// normalizing to `set-location`/`push-location`/`pop-location`/`new-psdrive`
/// (plus the Windows `ndr`/`mount` aliases). Sibling of `ps_element_is_cd_like`
/// in the policy layer; kept here because `zLs`'s compound-cd guard needs it.
fn gsn_is_dir_changing(name: &str) -> bool {
    let t = name.to_lowercase();
    if matches!(t.as_str(), "cd.." | "cd\\" | "cd/" | "cd~") {
        return true;
    }
    let b = t.as_bytes();
    if b.len() == 2 && b[0].is_ascii_lowercase() && b[1] == b':' {
        return true;
    }
    let r = normalize_cmdlet(name);
    matches!(
        r.as_str(),
        "set-location" | "push-location" | "pop-location" | "new-psdrive"
    ) || (cfg!(target_os = "windows") && matches!(r.as_str(), "ndr" | "mount"))
}

/// `VLs(cmd)` — a `New-Item` that creates a filesystem LINK (a `-ItemType` /
/// `-Type` value that is (a prefix of) SymbolicLink/Junction/HardLink, or a
/// glob/expression that cannot be statically validated). Such a command cannot
/// auto-allow because later path validation cannot follow a just-created link.
fn vls_is_symlink_new_item(cmd: &PsCommand) -> bool {
    if normalize_cmdlet(&cmd.name) != "new-item" {
        return false;
    }
    for r in 0..cmd.args.len() {
        let n = &cmd.args[r];
        if n.is_empty() {
            continue;
        }
        let first = n.chars().next().unwrap();
        // i = (fZ.has(n[0]) || n[0]==='/' ? "-"+n.slice(1) : n).toLowerCase().
        let i: String = if EY.contains(&first) || first == '/' {
            format!("-{}", &n[first.len_utf8()..])
        } else {
            n.clone()
        }
        .to_lowercase();
        // s = i.indexOf(":",1) with s>0 semantics — first ':' byte at position ≥1
        // (`:` is ASCII, never inside a multi-byte sequence).
        let s = i
            .bytes()
            .enumerate()
            .skip(1)
            .find(|&(_, byte)| byte == b':')
            .map(|(idx, _)| idx);
        // a = s>0 ? i.slice(0,s) : i ; l = TV(a…).toLowerCase().
        let a: &str = match s {
            Some(si) => &i[..si],
            None => &i,
        };
        let l = battery_backtick_decode(a).to_lowercase();
        if !hg_is_itemtype(&l) {
            continue;
        }
        // c = s>0 ? i.slice(s+1) : args[r+1]?.toLowerCase() ?? "".
        let c: String = match s {
            Some(si) => i[si + 1..].to_string(),
            None => cmd
                .args
                .get(r + 1)
                .map(|x| x.to_lowercase())
                .unwrap_or_default(),
        };
        // u = OF(Ose(TV(c…))).toLowerCase() — backtick-decode, strip comments/ws,
        // strip surrounding quotes (`OF(Ose(…))` == `clean_value`).
        let decoded = battery_backtick_decode(&c);
        let u = strip_surrounding_quotes(strip_comments_and_leading_ws(&decoded)).to_lowercase();
        // A glob / expression value cannot be validated → treat as a link.
        if u.chars()
            .any(|ch| matches!(ch, '?' | '*' | '[' | ']' | '(' | '$'))
        {
            return true;
        }
        // A (prefix of a) link item-type → a link.
        for d in MG_SYMLINK_TYPES {
            if !u.is_empty() && d.starts_with(u.as_str()) {
                return true;
            }
        }
    }
    false
}

/// The `Voe` feature aggregate (claude-code): dangerous constructs anywhere in the
/// parse that make the whole command unvalidatable for auto-allow. The seven
/// boolean features mirror the oracle's `Voe` object one-for-one.
#[allow(clippy::struct_excessive_bools)]
struct VoeFeatures {
    has_sub_expressions: bool,
    has_script_blocks: bool,
    has_splatting: bool,
    has_expandable_strings: bool,
    has_member_invocations: bool,
    has_assignments: bool,
    has_stop_parsing: bool,
}

impl VoeFeatures {
    /// Any dangerous feature present → the command passes through (never allows).
    fn any(&self) -> bool {
        self.has_sub_expressions
            || self.has_script_blocks
            || self.has_member_invocations
            || self.has_splatting
            || self.has_assignments
            || self.has_stop_parsing
            || self.has_expandable_strings
    }
}

/// `r(command)` in `Voe` — OR the command's simplified element types into the
/// feature aggregate.
fn voe_scan(cmd: &PsCommand, t: &mut VoeFeatures) {
    for o in &cmd.element_types {
        match o.as_str() {
            "ScriptBlock" => t.has_script_blocks = true,
            "SubExpression" => t.has_sub_expressions = true,
            "ExpandableString" => t.has_expandable_strings = true,
            "MemberInvocation" => t.has_member_invocations = true,
            _ => {}
        }
    }
}

/// `Voe(t)` — aggregate the dangerous features across all statements + variables.
fn voe(
    statements: &[PsStatement],
    variables: &[PsVariable],
    has_stop_parsing: bool,
) -> VoeFeatures {
    let mut t = VoeFeatures {
        has_sub_expressions: false,
        has_script_blocks: false,
        has_splatting: false,
        has_expandable_strings: false,
        has_member_invocations: false,
        has_assignments: false,
        has_stop_parsing,
    };
    for n in statements {
        if n.statement_type == "AssignmentStatementAst" {
            t.has_assignments = true;
        }
        // CC scans every `n.commands` element via `r(o)`. An Expression pipeline
        // element carries only a single element type in CC; every feature-setting
        // type (Sub/Script/Expandable/Member) is ALSO reported by the statement's
        // `securityPatterns` (folded in below), so scanning only Command elements
        // here loses no feature — verified against `Get-SecurityPatterns`.
        for el in &n.commands {
            if let PsElement::Command(c) = el {
                voe_scan(c, &mut t);
            }
        }
        for c in &n.nested_commands {
            voe_scan(c, &mut t);
        }
        let sp = &n.security_patterns;
        if sp.has_member_invocations {
            t.has_member_invocations = true;
        }
        if sp.has_sub_expressions {
            t.has_sub_expressions = true;
        }
        if sp.has_expandable_strings {
            t.has_expandable_strings = true;
        }
        if sp.has_script_blocks {
            t.has_script_blocks = true;
        }
    }
    for v in variables {
        if v.is_splatted {
            t.has_splatting = true;
            break;
        }
    }
    t
}

/// `h3(cmd)` — do the command's arguments contain something that cannot be
/// statically validated (a non-literal element type carrying an expression, or a
/// Parameter with an expression-typed inline value / colon-bound expression)?
fn h3_has_unvalidatable_args(cmd: &PsCommand) -> bool {
    // r = elementTypes.slice(1) ; n = args ; o = children (aligned with args).
    let types = cmd.element_types.get(1..).unwrap_or(&[]);
    for (i, ty) in types.iter().enumerate() {
        let ty = ty.as_str();
        if ty != "StringConstant" && ty != "Parameter" {
            let arg = cmd.args.get(i).map_or("", String::as_str);
            if !contains_expr_meta(arg) {
                continue;
            }
            return true;
        }
        if ty == "Parameter" {
            match cmd.children.get(i).and_then(Option::as_ref) {
                // A parsed inline value (`-Param:value`): a non-StringConstant
                // child (array literal, etc.) is unvalidatable.
                Some(child_types) => {
                    if child_types.iter().any(|c| c != "StringConstant") {
                        return true;
                    }
                }
                // No parsed child → check the raw colon-bound value for an
                // expression metacharacter.
                None => {
                    let arg = cmd.args.get(i).map_or("", String::as_str);
                    if let Some(ci) = arg.find(':') {
                        if ci > 0 && contains_expr_meta(&arg[ci + 1..]) {
                            return true;
                        }
                    }
                }
            }
        }
    }
    false
}

/// The shared per-command tail of `zLs` (`ANt`/`ULs` safe-continue → `!GLs`
/// passthrough → `h3` passthrough). Returns `Some(reason)` to pass through, or
/// `None` when the command is safe (a recognized read-only/out-null cmdlet, or a
/// write cmdlet with statically-validatable arguments). `nested` selects the
/// nested-command variant of the `h3` message.
fn zls_command_tail(cmd: &PsCommand, nested: bool) -> Option<String> {
    if ant_is_out_null(&cmd.name) || uls_is_safe_formatting(cmd) {
        return None;
    }
    if !gls_is_write(&cmd.name) {
        return Some(format!(
            "No mode-specific handling for '{}' in acceptEdits mode",
            cmd.name
        ));
    }
    if h3_has_unvalidatable_args(cmd) {
        return Some(if nested {
            format!(
                "Arguments in nested '{}' cannot be statically validated in acceptEdits mode",
                cmd.name
            )
        } else {
            format!(
                "Arguments in '{}' cannot be statically validated in acceptEdits mode",
                cmd.name
            )
        });
    }
    None
}

/// The `acceptEdits` whole-pipeline validator (claude-code `zLs`, `acceptEdits`
/// arm). Returns [`PsAcceptEditsResult::Allow`] only for a fully structurally-safe
/// write pipeline; otherwise [`PsAcceptEditsResult::Passthrough`] with the reason.
///
/// The caller MUST gate this on `mode == AcceptEdits` and a valid parse, and MUST
/// compose it so a path-containment ASK ([`validate_ps_statements`]) overrides
/// this ALLOW (an out-of-cwd write still asks). This function performs NO path
/// containment — it is purely structural.
#[must_use]
pub fn ps_accept_edits_validate(
    statements: &[PsStatement],
    variables: &[PsVariable],
    has_stop_parsing: bool,
) -> PsAcceptEditsResult {
    use PsAcceptEditsResult::{Allow, Passthrough};

    // Voe feature aggregate — any dangerous feature → passthrough.
    if voe(statements, variables, has_stop_parsing).any() {
        return Passthrough(
            "Command contains subexpressions, script blocks, or member invocations that require approval"
                .to_string(),
        );
    }
    if statements.is_empty() {
        return Passthrough("No commands found to validate for acceptEdits mode".to_string());
    }
    // i = total pipeline element count across all statements.
    let total: usize = statements.iter().map(|s| s.commands.len()).sum();

    // New-Item symlink/junction/hardlink → passthrough.
    let creates_link = statements.iter().any(|s| {
        s.commands.iter().any(|el| match el {
            PsElement::Command(c) => vls_is_symlink_new_item(c),
            PsElement::Expression { .. } => false,
        })
    });
    if creates_link {
        return Passthrough(
            "Command creates a filesystem link (New-Item -ItemType SymbolicLink/Junction/HardLink) \u{2014} cannot auto-allow because later path validation cannot follow just-created links"
                .to_string(),
        );
    }

    // Compound cd + write → passthrough (path validation would use a stale cwd).
    if total > 1 {
        let mut has_cd = false;
        let mut has_write = false;
        for s in statements {
            for el in &s.commands {
                if let PsElement::Command(c) = el {
                    if gsn_is_dir_changing(&c.name) {
                        has_cd = true;
                    }
                    if gls_is_write(&c.name) {
                        has_write = true;
                    }
                }
            }
        }
        if has_cd && has_write {
            return Passthrough(
                "Compound command contains a directory-changing command (Set-Location/Push-Location/Pop-Location) with a write operation \u{2014} cannot auto-allow because path validation uses stale cwd"
                    .to_string(),
            );
        }
    }

    // Per-statement command + nested-command structural checks.
    for s in statements {
        for el in &s.commands {
            let c = match el {
                PsElement::Expression { text } => {
                    return Passthrough(format!(
                        "Pipeline contains expression source ({text}) that cannot be statically validated"
                    ));
                }
                PsElement::Command(c) => c,
            };
            if c.name_type == "application" {
                return Passthrough(format!(
                    "Command '{}' resolved from a path-like name and requires approval",
                    c.name
                ));
            }
            // elementTypes[1..]: every arg type must be StringConstant|Parameter;
            // a Parameter with a colon-bound expression is unvalidatable.
            for idx in 1..c.element_types.len() {
                let u = c.element_types[idx].as_str();
                if u != "StringConstant" && u != "Parameter" {
                    return Passthrough(format!(
                        "Command argument has unvalidatable type ({u}) \u{2014} variable paths cannot be statically resolved"
                    ));
                }
                if u == "Parameter" {
                    let d = c.args.get(idx - 1).map_or("", String::as_str);
                    if let Some(p) = d.find(':') {
                        if p > 0 && contains_expr_meta(&d[p + 1..]) {
                            return Passthrough(
                                "Colon-bound parameter contains an expression that cannot be statically validated"
                                    .to_string(),
                            );
                        }
                    }
                }
            }
            if pls_has_variable_write_arg(&c.args, &c.element_types, false)
                || pls_has_action_preference_arg(&c.args, &c.element_types, false)
            {
                return Passthrough(pls_guard_message(c, false));
            }
            if let Some(reason) = zls_command_tail(c, false) {
                return Passthrough(reason);
            }
        }
        // Nested commands (script blocks / control flow). CC's nested loop omits
        // the per-arg type / colon loop (h3 re-checks types internally) but keeps
        // the application check + the ANt/ULs/GLs/h3 tail.
        for c in &s.nested_commands {
            if c.name_type == "application" {
                return Passthrough(format!(
                    "Nested command '{}' resolved from a path-like name and requires approval",
                    c.name
                ));
            }
            if pls_has_variable_write_arg(&c.args, &c.element_types, false)
                || pls_has_action_preference_arg(&c.args, &c.element_types, false)
            {
                return Passthrough(pls_guard_message(c, true));
            }
            if let Some(reason) = zls_command_tail(c, true) {
                return Passthrough(reason);
            }
        }
    }

    Allow
}

#[cfg(test)]
#[path = "powershell_containment_test.rs"]
mod powershell_containment_test;
