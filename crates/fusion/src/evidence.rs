//! Host-side evidence check.
//!
//! After the panel bar passes and before the analyst runs, the host checks
//! whether the code each panel cites exists in the workspace. The results are
//! facts handed to the analyst and the parent model; they never down-weight or
//! drop a panel on their own, and they say nothing about whether a panel's
//! reasoning holds.
//!
//! Every check is a `Grep` call through the parent's [`ToolInvoker`], so the
//! parent's permission rules (deny rules, workspace scope) apply exactly as
//! they would to the model. The caller is marked non-interactive and unable
//! to show prompts: anything that would need a prompt is refused, never asked.
//! `Read` is deliberately not used — it records the file as read for the
//! session, which would let the parent `Edit` a file it never read itself.
//!
//! No provider is contacted and nothing leaves the machine.

use crate::panel::PanelInternal;
use platform_api::tool_invoker::{
    SubagentInvocationContext, ToolExecutionPolicy, ToolInvoker, ToolInvokerError,
};
use platform_api::{EvidenceCheckStatus, EvidenceKind, PanelReport};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::{Duration, Instant};

/// Most evidence items checked per run; the rest stay `unverifiable`.
pub(crate) const MAX_CHECKED_EVIDENCE: usize = 32;
/// Evidence items checked at once.
pub(crate) const CHECK_CONCURRENCY: usize = 8;
/// Longest the whole stage may take.
pub(crate) const CHECK_TIME_CAP: Duration = Duration::from_secs(10);
/// Most excerpt lines searched per evidence item.
const MAX_EXCERPT_LINES: usize = 6;
/// Shorter lines (`}`, `} else {`) match almost anywhere and prove nothing.
const MIN_LINE_BYTES: usize = 8;
/// A long line is identified by its prefix; searching all of it adds nothing.
const MAX_LINE_BYTES: usize = 200;
const LOCAL_SEARCH_PREFIX: &str = "lingxi-search:";

/// The workspace path a file evidence locator cites, or `None` when the host
/// cannot check it: not file evidence, a URL, a host search locator, or a
/// locator that is not a plain path. Line suffixes (`:12`, `:12-20`,
/// `:12:5`, `#L12`, `#L12-L20`) are dropped.
pub(crate) fn locator_path(kind: EvidenceKind, locator: &str) -> Option<String> {
    if kind != EvidenceKind::File {
        return None;
    }
    let locator = locator.trim();
    if locator.is_empty() || locator.starts_with(LOCAL_SEARCH_PREFIX) || locator.contains("://") {
        return None;
    }
    let mut path = match locator.find("#L") {
        Some(at) => &locator[..at],
        None => locator,
    };
    for _ in 0..3 {
        let Some((head, tail)) = path.rsplit_once(':') else {
            break;
        };
        let is_line_range = tail.chars().any(|c| c.is_ascii_digit())
            && tail.chars().all(|c| c.is_ascii_digit() || c == '-');
        if !is_line_range {
            break;
        }
        if head.is_empty() {
            return None;
        }
        path = head;
    }
    // A path with whitespace is far more often a description ("src/a.rs
    // lines 3-9") than a real file name; checking it would report a missing
    // file the panel never meant.
    if path.is_empty() || path.chars().any(char::is_whitespace) {
        return None;
    }
    Some(path.to_string())
}

/// Regex patterns for the distinctive lines of an excerpt: each line is
/// trimmed, stripped of a Read-style line-number prefix (`12\t`, `12→`),
/// matched with any run of spaces or tabs between its tokens, and literal
/// otherwise. Short, punctuation-only, and elided (`…`) lines are skipped.
pub(crate) fn excerpt_patterns(excerpt: &str) -> Vec<String> {
    let mut patterns: Vec<String> = Vec::new();
    for raw in excerpt.lines() {
        let line = strip_line_number(raw.trim());
        if line.len() < MIN_LINE_BYTES
            || line.contains('…')
            || !line.chars().any(char::is_alphanumeric)
        {
            continue;
        }
        let line = prefix_at_char_boundary(line, MAX_LINE_BYTES);
        let pattern = line
            .split_whitespace()
            .map(escape_regex)
            .collect::<Vec<_>>()
            .join("[ \\t]+");
        if !patterns.contains(&pattern) {
            patterns.push(pattern);
        }
        if patterns.len() == MAX_EXCERPT_LINES {
            break;
        }
    }
    patterns
}

fn strip_line_number(line: &str) -> &str {
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 {
        return line;
    }
    let rest = &line[digits..];
    rest.strip_prefix('\t')
        .or_else(|| rest.strip_prefix('→'))
        .map_or(line, str::trim_start)
}

fn prefix_at_char_boundary(text: &str, cap: usize) -> &str {
    if text.len() <= cap {
        return text;
    }
    let mut end = cap;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Escape every character the Grep tool's regex engine treats as syntax.
fn escape_regex(token: &str) -> String {
    let mut out = String::with_capacity(token.len());
    for ch in token.chars() {
        if "\\.+*?()|[]{}^$#&-~".contains(ch) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

struct Job {
    panel: usize,
    evidence: usize,
    path: String,
    patterns: Vec<String>,
}

/// Choose what to check: evidence that a claim cites first, then the rest,
/// taken round-robin across panels (in anonymous-id order) so one verbose
/// panel cannot use up the whole budget.
fn plan_jobs(panels: &[PanelInternal]) -> Vec<Job> {
    let mut order: Vec<usize> = (0..panels.len()).collect();
    order.sort_by(|a, b| panels[*a].anonymous_id.cmp(&panels[*b].anonymous_id));
    let mut queues: Vec<std::collections::VecDeque<Job>> = order
        .iter()
        .map(|&panel| {
            panels[panel]
                .report
                .as_ref()
                .map(|report| checkable_in_priority_order(panel, report))
                .unwrap_or_default()
        })
        .collect();
    let mut jobs = Vec::new();
    while jobs.len() < MAX_CHECKED_EVIDENCE && queues.iter().any(|q| !q.is_empty()) {
        for queue in &mut queues {
            if jobs.len() == MAX_CHECKED_EVIDENCE {
                break;
            }
            if let Some(job) = queue.pop_front() {
                jobs.push(job);
            }
        }
    }
    jobs
}

fn checkable_in_priority_order(
    panel: usize,
    report: &PanelReport,
) -> std::collections::VecDeque<Job> {
    let mut indices: Vec<usize> = Vec::new();
    for claim in &report.claims {
        for id in &claim.evidence_refs {
            if let Some(index) = report.evidence.iter().position(|ev| &ev.id == id) {
                if !indices.contains(&index) {
                    indices.push(index);
                }
            }
        }
    }
    for index in 0..report.evidence.len() {
        if !indices.contains(&index) {
            indices.push(index);
        }
    }
    indices
        .into_iter()
        .filter_map(|index| {
            let ev = &report.evidence[index];
            let path = locator_path(ev.kind, &ev.locator)?;
            Some(Job {
                panel,
                evidence: index,
                path,
                patterns: ev
                    .excerpt
                    .as_deref()
                    .map(excerpt_patterns)
                    .unwrap_or_default(),
            })
        })
        .collect()
}

/// Fill every reported panel's `evidence_checks` (index-aligned with its
/// `report.evidence`). Items outside the limits, or not reached by
/// `deadline`, stay [`EvidenceCheckStatus::Unverifiable`].
pub(crate) async fn check_panels(
    panels: &mut [PanelInternal],
    invoker: Arc<dyn ToolInvoker>,
    deadline: Instant,
) {
    for panel in panels.iter_mut() {
        let len = panel
            .report
            .as_ref()
            .map_or(0, |report| report.evidence.len());
        panel.evidence_checks = vec![EvidenceCheckStatus::Unverifiable; len];
    }
    let jobs = plan_jobs(panels);
    if jobs.is_empty() || deadline <= Instant::now() {
        return;
    }
    let permits = Arc::new(Semaphore::new(CHECK_CONCURRENCY));
    let mut tasks = JoinSet::new();
    for job in jobs {
        let invoker = Arc::clone(&invoker);
        let permits = Arc::clone(&permits);
        tasks.spawn(async move {
            let _permit = permits.acquire_owned().await;
            let status = check_one(invoker.as_ref(), &job.path, &job.patterns).await;
            (job.panel, job.evidence, status)
        });
    }
    // Keep whatever finished by the deadline; dropping the set aborts the rest.
    while let Ok(Some(joined)) = tokio::time::timeout_at(deadline, tasks.join_next()).await {
        if let Ok((panel, evidence, status)) = joined {
            if let Some(slot) = panels[panel].evidence_checks.get_mut(evidence) {
                *slot = status;
            }
        }
    }
}

async fn check_one(
    invoker: &dyn ToolInvoker,
    path: &str,
    patterns: &[String],
) -> EvidenceCheckStatus {
    if patterns.is_empty() {
        return match grep_count(invoker, path, "^").await {
            Ok(_) => EvidenceCheckStatus::FileExists,
            Err(status) => status,
        };
    }
    let mut found = 0;
    for pattern in patterns {
        match grep_count(invoker, path, pattern).await {
            Ok(0) => {}
            Ok(_) => found += 1,
            Err(status) => return status,
        }
    }
    if found == patterns.len() {
        EvidenceCheckStatus::Verified
    } else if found == 0 {
        EvidenceCheckStatus::NotFound
    } else {
        EvidenceCheckStatus::Partial
    }
}

async fn grep_count(
    invoker: &dyn ToolInvoker,
    path: &str,
    pattern: &str,
) -> Result<u64, EvidenceCheckStatus> {
    let input = json!({ "pattern": pattern, "path": path, "output_mode": "count" });
    match invoker.invoke("Grep", input, host_context()).await {
        Ok(data) => data
            .get("numMatches")
            .and_then(Value::as_u64)
            .ok_or(EvidenceCheckStatus::Unverifiable),
        // Grep reports a missing search path as invalid input; it probes the
        // path only after the permission check, so this never leaks the
        // existence of a path the session may not look at.
        Err(ToolInvokerError::InvalidInput(message)) if message.contains("does not exist") => {
            Err(EvidenceCheckStatus::MissingFile)
        }
        // How the invoker surfaces a permission denial or a path outside the
        // trusted workspace.
        Err(ToolInvokerError::Internal(_) | ToolInvokerError::Abort(_)) => {
            Err(EvidenceCheckStatus::Denied)
        }
        Err(_) => Err(EvidenceCheckStatus::Unverifiable),
    }
}

/// The host is the caller: no agent identity, no prompts, and a
/// non-interactive session so an ask resolves to a denial.
fn host_context() -> SubagentInvocationContext {
    SubagentInvocationContext {
        permission_pause_observer: None,
        parent_agent_id: None,
        origin_session_id: None,
        tool_execution_policy: ToolExecutionPolicy::Ordinary,
        agent_name: None,
        team_name: None,
        is_async: false,
        is_non_interactive_session: true,
        can_show_permission_prompts: false,
        cwd: None,
        tool_use_id: None,
        assistant_message_id: None,
        depth: 0,
        observer: None,
        parent_model: None,
        parent_model_profile: None,
        mode_override: None,
        request_source: None,
        frozen_command_denies: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use platform_api::{PanelClaim, PanelEvidence, PanelRunStatus};
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Grep stand-in over an in-memory workspace. It understands exactly the
    /// patterns this module builds: escaped tokens joined by `[ \t]+`.
    #[derive(Default)]
    struct FakeGrep {
        files: HashMap<String, String>,
        denied: Vec<String>,
        calls: Mutex<Vec<(String, String)>>,
        delay: Option<Duration>,
    }

    impl FakeGrep {
        fn with_file(mut self, path: &str, body: &str) -> Self {
            self.files.insert(path.into(), body.into());
            self
        }

        fn matches(line: &str, pattern: &str) -> bool {
            if pattern == "^" {
                return true;
            }
            let tokens: Vec<String> = pattern
                .split("[ \\t]+")
                .map(|token| {
                    let mut out = String::new();
                    let mut escaped = false;
                    for ch in token.chars() {
                        if ch == '\\' && !escaped {
                            escaped = true;
                            continue;
                        }
                        escaped = false;
                        out.push(ch);
                    }
                    out
                })
                .collect();
            let normalized = line.split_whitespace().collect::<Vec<_>>().join(" ");
            normalized.contains(&tokens.join(" "))
        }
    }

    #[async_trait]
    impl ToolInvoker for FakeGrep {
        async fn invoke(
            &self,
            name: &str,
            input: Value,
            ctx: SubagentInvocationContext,
        ) -> Result<Value, ToolInvokerError> {
            assert_eq!(name, "Grep");
            assert!(ctx.is_non_interactive_session);
            assert!(!ctx.can_show_permission_prompts);
            let path = input["path"].as_str().unwrap().to_string();
            let pattern = input["pattern"].as_str().unwrap().to_string();
            self.calls
                .lock()
                .unwrap()
                .push((path.clone(), pattern.clone()));
            if let Some(delay) = self.delay {
                tokio::time::sleep(delay).await;
            }
            if self.denied.contains(&path) {
                return Err(ToolInvokerError::Internal(
                    "Permission to use Grep has been denied.".into(),
                ));
            }
            let Some(body) = self.files.get(&path) else {
                return Err(ToolInvokerError::InvalidInput(format!(
                    "Path does not exist: {path}."
                )));
            };
            let count = body
                .lines()
                .filter(|line| Self::matches(line, &pattern))
                .count();
            Ok(json!({ "mode": "count", "numMatches": count }))
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    fn evidence(
        id: &str,
        kind: EvidenceKind,
        locator: &str,
        excerpt: Option<&str>,
    ) -> PanelEvidence {
        PanelEvidence {
            id: id.into(),
            kind,
            locator: locator.into(),
            excerpt: excerpt.map(str::to_string),
        }
    }

    fn panel(id: &str, claims: Vec<PanelClaim>, evidence: Vec<PanelEvidence>) -> PanelInternal {
        PanelInternal {
            index: 0,
            profile: String::new(),
            model: String::new(),
            anonymous_id: id.into(),
            status: PanelRunStatus::Completed,
            report: Some(PanelReport {
                schema_version: platform_api::FUSION_SCHEMA_VERSION,
                summary: "s".into(),
                candidate_answer: "a".into(),
                claims,
                evidence,
                assumptions: vec![],
                risks: vec![],
                unresolved_questions: vec![],
            }),
            duration_ms: 0,
            error_category: None,
            error_detail: None,
            usage: None,
            spawn_prompt: String::new(),
            evidence_checks: Vec::new(),
        }
    }

    fn claim(refs: &[&str]) -> PanelClaim {
        PanelClaim {
            statement: "c".into(),
            evidence_refs: refs.iter().map(|r| (*r).to_string()).collect(),
            confidence: 50,
        }
    }

    const SOURCE: &str = "pub fn run_inner(\n    &self,\n) -> Result<FusionResult, FusionError> {\n    let request = validate_request(request)?;\n}\n";

    #[test]
    fn locator_path_drops_line_suffixes_and_rejects_non_paths() {
        let file = |l: &str| locator_path(EvidenceKind::File, l);
        assert_eq!(file("src/a.rs").as_deref(), Some("src/a.rs"));
        assert_eq!(file(" src/a.rs:12 ").as_deref(), Some("src/a.rs"));
        assert_eq!(file("src/a.rs:12-20").as_deref(), Some("src/a.rs"));
        assert_eq!(file("src/a.rs:12:5").as_deref(), Some("src/a.rs"));
        assert_eq!(file("src/a.rs#L12").as_deref(), Some("src/a.rs"));
        assert_eq!(file("src/a.rs#L12-L20").as_deref(), Some("src/a.rs"));
        assert_eq!(file("/abs/c.rs:3").as_deref(), Some("/abs/c.rs"));
        assert_eq!(file("C:/repo/a.rs:3").as_deref(), Some("C:/repo/a.rs"));
        assert_eq!(file("https://example.com/a.rs"), None);
        assert_eq!(file("lingxi-search:abc123"), None);
        assert_eq!(file("src/a.rs lines 3-9"), None);
        assert_eq!(file(":12"), None);
        assert_eq!(file(""), None);
        assert_eq!(locator_path(EvidenceKind::Url, "src/a.rs"), None);
        assert_eq!(locator_path(EvidenceKind::Command, "cargo test"), None);
    }

    #[test]
    fn excerpt_patterns_keep_distinctive_lines_only() {
        let excerpt = "12\tpub fn run_inner(\n}\n    // ...\n13→    let x = foo(a, b)?;\nfn elided(…) {\nlet  x = foo(a,   b)?;";
        assert_eq!(
            excerpt_patterns(excerpt),
            vec![
                "pub[ \\t]+fn[ \\t]+run_inner\\(".to_string(),
                "let[ \\t]+x[ \\t]+=[ \\t]+foo\\(a,[ \\t]+b\\)\\?;".to_string(),
            ]
        );
    }

    #[test]
    fn excerpt_patterns_cap_line_count_and_length() {
        let many = (0..20)
            .map(|i| format!("let value_{i} = compute();"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(excerpt_patterns(&many).len(), MAX_EXCERPT_LINES);
        let long = format!("let s = \"{}\";", "é".repeat(300));
        let pattern = &excerpt_patterns(&long)[0];
        assert!(pattern.len() < MAX_LINE_BYTES + 32);
    }

    #[tokio::test]
    async fn every_status_maps_from_the_grep_outcome() {
        let mut fake = FakeGrep::default()
            .with_file("src/a.rs", SOURCE)
            .with_file("src/secret.rs", SOURCE);
        fake.denied.push("src/secret.rs".into());
        let invoker: Arc<dyn ToolInvoker> = Arc::new(fake);
        let mut panels = vec![panel(
            "P1",
            vec![],
            vec![
                evidence(
                    "v",
                    EvidenceKind::File,
                    "src/a.rs:1-4",
                    Some("pub fn run_inner(\n    let request = validate_request(request)?;"),
                ),
                evidence(
                    "p",
                    EvidenceKind::File,
                    "src/a.rs",
                    Some("pub fn run_inner(\nlet invented = nothing_here();"),
                ),
                evidence(
                    "n",
                    EvidenceKind::File,
                    "src/a.rs",
                    Some("let invented = nothing_here();"),
                ),
                evidence("e", EvidenceKind::File, "src/a.rs#L1", None),
                evidence(
                    "m",
                    EvidenceKind::File,
                    "src/gone.rs:4",
                    Some("pub fn run_inner("),
                ),
                evidence(
                    "d",
                    EvidenceKind::File,
                    "src/secret.rs",
                    Some("pub fn run_inner("),
                ),
                evidence("u", EvidenceKind::Url, "https://example.com", None),
            ],
        )];
        check_panels(
            &mut panels,
            invoker,
            Instant::now() + Duration::from_secs(5),
        )
        .await;
        use EvidenceCheckStatus as S;
        assert_eq!(
            panels[0].evidence_checks,
            vec![
                S::Verified,
                S::Partial,
                S::NotFound,
                S::FileExists,
                S::MissingFile,
                S::Denied,
                S::Unverifiable
            ]
        );
    }

    #[tokio::test]
    async fn stops_at_the_first_refusal_for_an_item() {
        let fake = Arc::new(FakeGrep::default());
        let invoker: Arc<dyn ToolInvoker> = fake.clone();
        let mut panels = vec![panel(
            "P1",
            vec![],
            vec![evidence(
                "m",
                EvidenceKind::File,
                "src/gone.rs",
                Some("first distinctive line\nsecond distinctive line"),
            )],
        )];
        check_panels(
            &mut panels,
            invoker,
            Instant::now() + Duration::from_secs(5),
        )
        .await;
        assert_eq!(
            panels[0].evidence_checks,
            vec![EvidenceCheckStatus::MissingFile]
        );
        assert_eq!(fake.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn plan_prefers_cited_evidence_and_round_robins_panels() {
        let many = |prefix: &str| {
            (0..40)
                .map(|i| {
                    evidence(
                        &format!("{prefix}{i}"),
                        EvidenceKind::File,
                        &format!("src/{prefix}{i}.rs"),
                        None,
                    )
                })
                .collect::<Vec<_>>()
        };
        // P2 is listed first but P1 sorts first; P1 cites its last item.
        let panels = vec![
            panel("P2", vec![], many("b")),
            panel("P1", vec![claim(&["a39"])], many("a")),
        ];
        let jobs = plan_jobs(&panels);
        assert_eq!(jobs.len(), MAX_CHECKED_EVIDENCE);
        assert_eq!(
            (jobs[0].panel, jobs[0].evidence),
            (1, 39),
            "cited evidence first"
        );
        assert_eq!(jobs[1].panel, 0, "then the next panel");
        let per_panel = |p: usize| jobs.iter().filter(|job| job.panel == p).count();
        assert_eq!(per_panel(0), MAX_CHECKED_EVIDENCE / 2);
        assert_eq!(per_panel(1), MAX_CHECKED_EVIDENCE / 2);
    }

    #[tokio::test]
    async fn items_beyond_the_budget_or_the_deadline_stay_unverifiable() {
        let invoker: Arc<dyn ToolInvoker> = Arc::new(FakeGrep {
            delay: Some(Duration::from_secs(60)),
            ..FakeGrep::default().with_file("src/a.rs", SOURCE)
        });
        let mut panels = vec![panel(
            "P1",
            vec![],
            vec![evidence("e", EvidenceKind::File, "src/a.rs", None)],
        )];
        check_panels(
            &mut panels,
            invoker,
            Instant::now() + Duration::from_millis(20),
        )
        .await;
        assert_eq!(
            panels[0].evidence_checks,
            vec![EvidenceCheckStatus::Unverifiable]
        );
    }

    #[tokio::test]
    async fn a_passed_deadline_makes_no_calls() {
        let fake = Arc::new(FakeGrep::default().with_file("src/a.rs", SOURCE));
        let invoker: Arc<dyn ToolInvoker> = fake.clone();
        let mut panels = vec![panel(
            "P1",
            vec![],
            vec![evidence("e", EvidenceKind::File, "src/a.rs", None)],
        )];
        check_panels(&mut panels, invoker, Instant::now()).await;
        assert_eq!(
            panels[0].evidence_checks,
            vec![EvidenceCheckStatus::Unverifiable]
        );
        assert!(fake.calls.lock().unwrap().is_empty());
    }
}
