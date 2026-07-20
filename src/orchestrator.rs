use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Result of a fix loop run.
pub enum GateResult {
    Passed,
    Failed { reason: String },
    TimedOut { attempts: usize },
}

/// Maximum number of clippy auto-heal passes per attempt.
const MAX_HEAL_ITERATIONS: usize = 5;

/// Orchestrated fix loop: context → LLM → risk check → impact verify → drift check.
///
/// This implements the feedback loop described in the improvement plan:
/// 1. Build context pack
/// 2. Present context + prompt to LLM (user handles this part)
/// 3. Optionally run the Clippy Auto-Healer (opt-in via `--auto-heal`)
/// 4. Check risk of the diff
/// 5. Verify impact (run affected tests)
/// 6. Check for spec drift
/// 7. If anything fails, feed results back and retry
pub struct FixLoop {
    root: PathBuf,
    prompt: String,
    max_attempts: usize,
    risk_threshold: f32,
    since: String,
    auto_heal: bool,
}

impl FixLoop {
    pub fn new(
        root: &Path,
        prompt: &str,
        max_attempts: usize,
        risk_threshold: f32,
        since: &str,
        auto_heal: bool,
    ) -> Self {
        Self {
            root: root.to_path_buf(),
            prompt: prompt.to_string(),
            max_attempts,
            risk_threshold,
            since: since.to_string(),
            auto_heal,
        }
    }

    /// Run the fix loop. This is a guided process where the user is prompted
    /// at each step. The loop produces structured feedback for an LLM at
    /// each stage.
    pub fn run(&mut self) -> Result<GateResult> {
        for attempt in 1..=self.max_attempts {
            eprintln!("\n═══ Attempt {attempt}/{} ═══", self.max_attempts);

            // Step 1: Build context for the LLM
            eprintln!("[1/4] Building context...");
            let context = self.build_context()?;
            eprintln!("      Context: {} chars ready for LLM", context.len());

            // Step 2: User pastes context + prompt into LLM and applies changes
            eprintln!("[2/4] Context assembled. Paste it into your LLM with this prompt:");
            eprintln!("{}", "─".repeat(60));
            eprintln!("{}", self.prompt);
            eprintln!("{}", "─".repeat(60));
            eprintln!("      Press Enter after applying LLM changes (or 'q' to quit)...");

            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;
            if input.trim() == "q" {
                return Ok(GateResult::Failed {
                    reason: "user quit".to_string(),
                });
            }

            // Optional: Clippy Auto-Healer. Strictly opt-in via --auto-heal so
            // no files are ever modified without the user asking for it.
            let mut auto_heal_applied = 0;
            if self.auto_heal {
                match self.run_clippy_auto_healer() {
                    Ok(applied) => auto_heal_applied = applied,
                    Err(e) => eprintln!("      Clippy Auto-Healer encountered an error: {e}"),
                }
            }

            // Step 3: Risk check
            eprintln!("[3/4] Checking risk...");
            if auto_heal_applied > 0 {
                eprintln!(
                    "      Note: the scored diff includes {auto_heal_applied} auto-applied clippy fix(es) (see summary above)."
                );
            }
            let diff = ai_tools_core::git_utils::unified_diff(&self.root, &self.since);
            let risk_passed = match diff {
                Some(diff_text) => {
                    if diff_text.trim().is_empty() {
                        eprintln!("      No changes detected — nothing to check.");
                        true
                    } else {
                        self.check_risk(attempt)?
                    }
                }
                None => {
                    eprintln!("      No diff available, skipping risk check.");
                    true
                }
            };

            if !risk_passed {
                eprintln!(
                    "      Risk threshold exceeded. The LLM should generate a safer alternative."
                );
                if attempt < self.max_attempts {
                    eprintln!("      Feed this back to the LLM and try again.");
                    continue;
                } else {
                    return Ok(GateResult::Failed {
                        reason: "risk threshold exceeded after all attempts".to_string(),
                    });
                }
            }
            eprintln!("      Risk: passed");

            // Step 4: Impact + drift check
            eprintln!("[4/4] Verifying impact and drift...");
            let (impact_passed, drift_passed) = self.verify()?;

            if impact_passed && drift_passed {
                eprintln!("      All checks passed!");
                return Ok(GateResult::Passed);
            }

            if !impact_passed {
                eprintln!("      Impact check: some tests may be affected.");
            }
            if !drift_passed {
                eprintln!("      Drift check: docs/tests/CI may be stale.");
            }

            if attempt < self.max_attempts {
                eprintln!("      Feed the failures back to the LLM and try again.");
            }
        }

        Ok(GateResult::TimedOut {
            attempts: self.max_attempts,
        })
    }

    fn build_context(&self) -> Result<String> {
        // Try to use cargo-context if available
        let output = Command::new("cargo-context")
            .args(["--preset", "fix"])
            .current_dir(&self.root)
            .output();

        match output {
            Ok(out) if out.status.success() => Ok(String::from_utf8_lossy(&out.stdout).to_string()),
            _ => {
                // Fallback: build minimal context manually
                let mut ctx = String::new();
                ctx.push_str("# Project Context\n\n");

                if let Some(diff) = ai_tools_core::git_utils::unified_diff(&self.root, &self.since)
                    && !diff.trim().is_empty()
                {
                    ctx.push_str("## Recent Changes\n\n```diff\n");
                    // Truncate to reasonable size
                    let diff = if diff.len() > 4000 {
                        format!("{}...\n(truncated)", &diff[..4000])
                    } else {
                        diff
                    };
                    ctx.push_str(&diff);
                    ctx.push_str("\n```\n\n");
                }

                ctx.push_str("## Instructions\n\n");
                ctx.push_str(&self.prompt);
                Ok(ctx)
            }
        }
    }

    fn check_risk(&self, attempt: usize) -> Result<bool> {
        let diff = match ai_tools_core::git_utils::unified_diff(&self.root, &self.since) {
            Some(d) => d,
            None => return Ok(true),
        };

        let tmp = std::env::temp_dir().join(format!("cargo-vibe-fix-{attempt}.diff"));
        std::fs::write(&tmp, &diff)?;
        let diff_file = std::fs::File::open(&tmp)?;

        let out = Command::new("diff-risk")
            .args(["--threshold", &self.risk_threshold.to_string()])
            .stdin(std::process::Stdio::from(diff_file))
            .output()
            .map_err(|e| {
                anyhow::anyhow!("running diff-risk — required for `cargo vibe fix`: {e}")
            })?;

        let text = String::from_utf8_lossy(&out.stdout);
        let err = String::from_utf8_lossy(&out.stderr);
        if !text.trim().is_empty() {
            eprintln!("{text}");
        }
        if !err.trim().is_empty() {
            eprintln!("{err}");
        }

        Ok(out.status.success())
    }

    fn verify(&self) -> Result<(bool, bool)> {
        let impact_ok = match Command::new("cargo-impact")
            .args(["--since", &self.since, "--fail-on", "high"])
            .current_dir(&self.root)
            .output()
        {
            Ok(out) => {
                if !out.status.success() {
                    let text = String::from_utf8_lossy(&out.stdout);
                    if !text.trim().is_empty() {
                        eprintln!("{}", text);
                    }
                    false
                } else {
                    true
                }
            }
            Err(_) => {
                eprintln!("      cargo-impact not available — skipping.");
                true
            }
        };

        let drift_ok = match Command::new("spec-drift")
            .args(["--format", "json", "--deny", "warning"])
            .current_dir(&self.root)
            .output()
        {
            Ok(out) => {
                let text = String::from_utf8_lossy(&out.stdout);
                let count = text.matches("\"rule\"").count();
                if count > 0 {
                    eprintln!("      spec-drift: {count} divergence(s) found.");
                    if !text.trim().is_empty() && text.len() < 2000 {
                        eprintln!("{}", text);
                    }
                }
                out.status.success()
            }
            Err(_) => {
                eprintln!("      spec-drift not available — skipping.");
                true
            }
        };

        Ok((impact_ok, drift_ok))
    }

    /// Run up to `MAX_HEAL_ITERATIONS` self-healing iterations of cargo clippy
    /// and apply machine-applicable suggestions. Returns the total number of
    /// applied suggestions and prints a per-file summary so nothing changes
    /// files invisibly.
    fn run_clippy_auto_healer(&self) -> Result<usize> {
        let mut per_file: BTreeMap<PathBuf, (usize, BTreeSet<String>)> = BTreeMap::new();
        let mut total_applied = 0;

        for iteration in 1..=MAX_HEAL_ITERATIONS {
            eprintln!("      Clippy Auto-Healer: iteration {iteration}/{MAX_HEAL_ITERATIONS}...");
            let output = Command::new("cargo")
                .args(["clippy", "--all-targets", "--message-format=json"])
                .current_dir(&self.root)
                .output()?;

            let stdout_str = String::from_utf8_lossy(&output.stdout);
            let mut suggestions = Vec::new();
            for line in stdout_str.lines() {
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(line)
                    && val.get("reason").and_then(|r| r.as_str()) == Some("compiler-message")
                    && let Some(msg) = val.get("message")
                {
                    collect_suggestions_from_message(msg, &mut suggestions);
                }
            }

            if suggestions.is_empty() {
                eprintln!("      Clippy Auto-Healer: no machine-applicable suggestions found.");
                break;
            }

            let applied_files = self.apply_suggestions(&suggestions)?;
            let applied: usize = applied_files.iter().map(|f| f.applied).sum();
            for file_fix in applied_files {
                let entry = per_file.entry(file_fix.file).or_default();
                entry.0 += file_fix.applied;
                entry.1.extend(file_fix.lints);
            }
            total_applied += applied;
            eprintln!("      Clippy Auto-Healer: applied {applied} suggestion(s).");
            if applied == 0 {
                break;
            }
        }

        if total_applied > 0 {
            eprintln!(
                "      Clippy Auto-Healer: auto-applied {total_applied} fix(es) across {} file(s):",
                per_file.len()
            );
            for (file, (count, lints)) in &per_file {
                if lints.is_empty() {
                    eprintln!("        {}: {count} fix(es)", file.display());
                } else {
                    let lint_list = lints.iter().cloned().collect::<Vec<_>>().join(", ");
                    eprintln!("        {}: {count} fix(es) [{lint_list}]", file.display());
                }
            }
            eprintln!(
                "      These changes are part of your working tree and will be included in the risk-checked diff."
            );
        }

        Ok(total_applied)
    }

    /// Group, sort, and transactionally apply suggestions to workspace files with overlap guards.
    /// Returns one entry per modified file with the number of applied fixes and the lint names.
    pub fn apply_suggestions(
        &self,
        suggestions: &[ClippySuggestion],
    ) -> Result<Vec<AppliedFileFix>> {
        let mut grouped: HashMap<PathBuf, Vec<ClippySuggestion>> = HashMap::new();
        for sugg in suggestions {
            let abs_path = if sugg.file_path.is_absolute() {
                sugg.file_path.clone()
            } else {
                self.root.join(&sugg.file_path)
            };
            grouped.entry(abs_path).or_default().push(sugg.clone());
        }

        let canonical_root =
            std::fs::canonicalize(&self.root).unwrap_or_else(|_| self.root.clone());
        let mut applied_files = Vec::new();

        for (file_path, mut file_suggestions) in grouped {
            let canonical_file =
                std::fs::canonicalize(&file_path).unwrap_or_else(|_| file_path.clone());
            if !canonical_file.starts_with(&canonical_root) {
                continue;
            }

            let mut content = match std::fs::read(&file_path) {
                Ok(bytes) => bytes,
                Err(e) => {
                    eprintln!(
                        "      Clippy Auto-Healer: failed to read {}: {e}",
                        file_path.display()
                    );
                    continue;
                }
            };

            // Sort descending: by byte_start first, then byte_end.
            file_suggestions.sort_by(|a, b| {
                b.byte_start
                    .cmp(&a.byte_start)
                    .then_with(|| b.byte_end.cmp(&a.byte_end))
            });

            let mut last_applied_start = usize::MAX;
            let mut file_applied = 0;
            let mut applied_lints = BTreeSet::new();

            for sugg in file_suggestions {
                if sugg.byte_start > sugg.byte_end || sugg.byte_end > content.len() {
                    continue;
                }

                // Overlap safety guard
                if sugg.byte_end <= last_applied_start {
                    content.splice(sugg.byte_start..sugg.byte_end, sugg.replacement.bytes());
                    last_applied_start = sugg.byte_start;
                    file_applied += 1;
                    if let Some(lint) = sugg.lint {
                        applied_lints.insert(lint);
                    }
                }
            }

            if file_applied > 0 {
                if let Err(e) = std::fs::write(&file_path, &content) {
                    eprintln!(
                        "      Clippy Auto-Healer: failed to write {}: {e}",
                        file_path.display()
                    );
                } else {
                    applied_files.push(AppliedFileFix {
                        file: file_path,
                        applied: file_applied,
                        lints: applied_lints.into_iter().collect(),
                    });
                }
            }
        }

        Ok(applied_files)
    }
}

/// Per-file record of applied clippy fixes, used to report auto-heal changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedFileFix {
    pub file: PathBuf,
    pub applied: usize,
    pub lints: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClippySuggestion {
    pub file_path: PathBuf,
    pub byte_start: usize,
    pub byte_end: usize,
    pub replacement: String,
    /// Lint name (e.g. `clippy::needless_return`), inherited from the parent
    /// diagnostic when the suggestion span lives in a child message.
    pub lint: Option<String>,
}

pub fn collect_suggestions_from_message(
    msg: &serde_json::Value,
    suggestions: &mut Vec<ClippySuggestion>,
) {
    collect_suggestions_with_lint(msg, None, suggestions);
}

fn collect_suggestions_with_lint(
    msg: &serde_json::Value,
    parent_lint: Option<&str>,
    suggestions: &mut Vec<ClippySuggestion>,
) {
    let lint = msg
        .get("code")
        .and_then(|c| c.get("code"))
        .and_then(|c| c.as_str())
        .or(parent_lint);

    if let Some(spans) = msg.get("spans").and_then(|s| s.as_array()) {
        for span in spans {
            if let Some(sugg) = parse_suggestion_from_span(span, lint) {
                suggestions.push(sugg);
            }
        }
    }
    if let Some(children) = msg.get("children").and_then(|c| c.as_array()) {
        for child in children {
            collect_suggestions_with_lint(child, lint, suggestions);
        }
    }
}

fn parse_suggestion_from_span(
    span: &serde_json::Value,
    lint: Option<&str>,
) -> Option<ClippySuggestion> {
    let applicability = span
        .get("suggestion_applicability")
        .and_then(|v| v.as_str())?;
    if applicability != "MachineApplicable" {
        return None;
    }
    let replacement = span
        .get("suggested_replacement")
        .and_then(|v| v.as_str())?
        .to_string();
    let file_name = span.get("file_name").and_then(|v| v.as_str())?.to_string();
    let byte_start = span.get("byte_start").and_then(|v| v.as_u64())? as usize;
    let byte_end = span.get("byte_end").and_then(|v| v.as_u64())? as usize;

    Some(ClippySuggestion {
        file_path: PathBuf::from(file_name),
        byte_start,
        byte_end,
        replacement,
        lint: lint.map(str::to_string),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Unique, self-cleaning temp directory (same pattern as tests/cli_contracts.rs)
    /// so parallel test runs never collide on shared filenames.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock")
                .as_nanos();
            let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "cargo-vibe-{label}-{}-{nanos}-{counter}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("create temp dir");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn total_applied(fixes: &[AppliedFileFix]) -> usize {
        fixes.iter().map(|f| f.applied).sum()
    }

    #[test]
    fn test_parse_suggestions() {
        let msg = json!({
            "reason": "compiler-message",
            "message": {
                "message": "use of a blacklisted word",
                "code": { "code": "clippy::example_lint" },
                "spans": [
                    {
                        "file_name": "src/lib.rs",
                        "byte_start": 10,
                        "byte_end": 15,
                        "suggestion_applicability": "MachineApplicable",
                        "suggested_replacement": "hello"
                    }
                ],
                "children": [
                    {
                        "message": "try this instead",
                        "code": null,
                        "spans": [
                            {
                                "file_name": "src/lib.rs",
                                "byte_start": 20,
                                "byte_end": 25,
                                "suggestion_applicability": "MaybeIncorrect",
                                "suggested_replacement": "world"
                            },
                            {
                                "file_name": "src/lib.rs",
                                "byte_start": 30,
                                "byte_end": 35,
                                "suggestion_applicability": "MachineApplicable",
                                "suggested_replacement": "rust"
                            }
                        ]
                    }
                ]
            }
        });

        let mut suggestions = Vec::new();
        if let Some(m) = msg.get("message") {
            collect_suggestions_from_message(m, &mut suggestions);
        }

        assert_eq!(suggestions.len(), 2);
        assert_eq!(suggestions[0].replacement, "hello");
        assert_eq!(suggestions[0].byte_start, 10);
        assert_eq!(suggestions[0].byte_end, 15);
        assert_eq!(suggestions[0].lint.as_deref(), Some("clippy::example_lint"));
        assert_eq!(suggestions[1].replacement, "rust");
        assert_eq!(suggestions[1].byte_start, 30);
        assert_eq!(suggestions[1].byte_end, 35);
        // Child spans inherit the lint name from the parent diagnostic.
        assert_eq!(suggestions[1].lint.as_deref(), Some("clippy::example_lint"));
    }

    #[test]
    fn test_apply_suggestions_ordering_and_overlap() {
        let temp = TempDir::new("heal-test");
        let root = temp.path();
        let test_file = root.join("test_overlap.rs");
        std::fs::write(&test_file, b"abcdefghijklmnopqrstuvwxyz").unwrap();

        // 1. We have non-overlapping suggestions.
        // abcdefghijklmnopqrstuvwxyz
        // Sugg 1: replace "def" (3..6) with "123"
        // Sugg 2: replace "uvw" (20..23) with "89"
        // Sugg 3: replace "op" (14..16) with "XYZ"
        let suggestions = vec![
            ClippySuggestion {
                file_path: PathBuf::from("test_overlap.rs"),
                byte_start: 3,
                byte_end: 6,
                replacement: "123".to_string(),
                lint: Some("clippy::lint_a".to_string()),
            },
            ClippySuggestion {
                file_path: PathBuf::from("test_overlap.rs"),
                byte_start: 20,
                byte_end: 23,
                replacement: "89".to_string(),
                lint: Some("clippy::lint_b".to_string()),
            },
            ClippySuggestion {
                file_path: PathBuf::from("test_overlap.rs"),
                byte_start: 14,
                byte_end: 16,
                replacement: "XYZ".to_string(),
                lint: None,
            },
        ];

        let fixer = FixLoop::new(root, "test", 1, 7.0, "HEAD", false);
        let applied = fixer.apply_suggestions(&suggestions).unwrap();
        assert_eq!(total_applied(&applied), 3);
        assert_eq!(applied.len(), 1);
        assert_eq!(
            applied[0].lints,
            vec!["clippy::lint_a".to_string(), "clippy::lint_b".to_string()]
        );

        let content = std::fs::read_to_string(&test_file).unwrap();
        assert_eq!(content, "abc123ghijklmnXYZqrst89xyz");

        // 2. Now let's test overlapping suggestions.
        std::fs::write(&test_file, b"abcdefghijklmnopqrstuvwxyz").unwrap();

        // Sugg A: 3..6 "def" -> "123"
        // Sugg B: 4..5 "e" -> "4" (overlaps with A)
        // Sugg C: 20..23 "uvw" -> "89" (safe)
        let overlapping_suggestions = vec![
            ClippySuggestion {
                file_path: PathBuf::from("test_overlap.rs"),
                byte_start: 3,
                byte_end: 6,
                replacement: "123".to_string(),
                lint: None,
            },
            ClippySuggestion {
                file_path: PathBuf::from("test_overlap.rs"),
                byte_start: 4,
                byte_end: 5,
                replacement: "4".to_string(),
                lint: None,
            },
            ClippySuggestion {
                file_path: PathBuf::from("test_overlap.rs"),
                byte_start: 20,
                byte_end: 23,
                replacement: "89".to_string(),
                lint: None,
            },
        ];

        let applied = fixer.apply_suggestions(&overlapping_suggestions).unwrap();
        assert_eq!(total_applied(&applied), 2);

        let content = std::fs::read_to_string(&test_file).unwrap();
        assert_eq!(content, "abcd4fghijklmnopqrst89xyz");
    }
}
