use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Output, Stdio};

mod orchestrator;
mod report;

use orchestrator::{FixLoop, GateResult};

/// cargo-vibe — unified AI-native development lifecycle for Rust.
///
/// Orchestrates four tools:
///   cargo context  — assemble token-budgeted context for LLMs
///   diff-risk      — score semantic risk of code changes
///   cargo impact   — find the blast radius of changes
///   spec-drift     — detect documentation/test/CI drift
#[derive(Parser, Debug)]
#[command(name = "cargo-vibe", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Project root directory.
    #[arg(long, default_value = ".")]
    root: PathBuf,

    /// Path to .cargo-vibe.toml config.
    #[arg(long)]
    config: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Run all health checks (risk + impact + drift).
    Check {
        /// Output format: text, json, or sarif.
        #[arg(long, default_value = "text")]
        format: String,
        /// Fail if any tool's threshold is exceeded.
        #[arg(long)]
        strict: bool,
        /// Git ref to diff against.
        #[arg(long, default_value = "HEAD")]
        since: String,
    },

    /// Score semantic risk of code changes (wraps diff-risk).
    Risk {
        /// Git ref to diff against.
        #[arg(long, default_value = "HEAD")]
        since: String,
        /// Risk threshold (0.0-10.0). Exit code 1 if exceeded.
        #[arg(long)]
        threshold: Option<f32>,
        /// Output format.
        #[arg(long, default_value = "text")]
        format: String,
    },

    /// Find blast radius of changes (wraps cargo-impact).
    Impact {
        /// Git ref to diff against.
        #[arg(long, default_value = "HEAD")]
        since: String,
        /// Emit a cargo-nextest filter expression instead of report.
        #[arg(long)]
        test: bool,
        /// Output format.
        #[arg(long, default_value = "text")]
        format: String,
    },

    /// Detect documentation/test/CI drift (wraps spec-drift).
    Drift {
        /// Git ref to diff against.
        #[arg(long)]
        diff: Option<String>,
        /// Output format.
        #[arg(long, default_value = "text")]
        format: String,
        /// Exit non-zero only for drifts at this severity.
        #[arg(long, default_value = "notice")]
        deny: String,
    },

    /// Assemble token-budgeted context for LLMs (wraps cargo-context).
    Context {
        /// Preset: fix or feature.
        #[arg(long, default_value = "fix")]
        preset: String,
        /// Token budget.
        #[arg(long)]
        budget: Option<usize>,
        /// Read user prompt from stdin.
        #[arg(long)]
        stdin: bool,
    },

    /// Run the AI fix loop: context → LLM → risk check → impact verify → drift check.
    Fix {
        /// User prompt describing what to fix/implement.
        #[arg(long, short)]
        prompt: String,
        /// Maximum fix attempts.
        #[arg(long, default_value = "3")]
        max_attempts: usize,
        /// Risk threshold for auto-rejection (default: config or 7.0).
        #[arg(long)]
        risk_threshold: Option<f32>,
        /// Git ref to base the fix on.
        #[arg(long, default_value = "HEAD")]
        since: String,
        /// Opt in to the Clippy Auto-Healer: after LLM changes are applied,
        /// run `cargo clippy` and auto-apply machine-applicable suggestions
        /// (up to 5 passes). Off by default; applied fixes are summarized
        /// per file and included in the risk-checked diff.
        #[arg(long)]
        auto_heal: bool,
    },
}

fn main() -> ExitCode {
    let cli = parse_cli();

    let config = match load_config(&cli.root, cli.config.as_deref()) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("cargo-vibe: {e:#}");
            return ExitCode::from(2);
        }
    };

    let result = match cli.command {
        Commands::Check {
            format,
            strict,
            since,
        } => run_check(&cli.root, &format, strict, &since, config.as_ref()),
        Commands::Risk {
            since,
            threshold,
            format,
        } => run_risk(&cli.root, &since, threshold, &format, config.as_ref()),
        Commands::Impact {
            since,
            test,
            format,
        } => run_impact(&cli.root, &since, test, &format, config.as_ref()),
        Commands::Drift { diff, format, deny } => {
            run_drift(&cli.root, diff.as_deref(), &format, &deny, config.as_ref())
        }
        Commands::Context {
            preset,
            budget,
            stdin,
        } => run_context(&cli.root, &preset, budget, stdin, config.as_ref()),
        Commands::Fix {
            prompt,
            max_attempts,
            risk_threshold,
            since,
            auto_heal,
        } => run_fix(
            &cli.root,
            &prompt,
            max_attempts,
            risk_threshold,
            &since,
            auto_heal,
            config.as_ref(),
        ),
    };

    match result {
        Ok(exit_code) => exit_code,
        Err(e) => {
            eprintln!("cargo-vibe: {e:#}");
            ExitCode::from(2)
        }
    }
}

fn run_check(
    root: &Path,
    format: &str,
    strict: bool,
    since: &str,
    config: Option<&ai_tools_core::config::VibeConfig>,
) -> Result<ExitCode> {
    let risk_threshold = config
        .and_then(|c| c.diff_risk.threshold)
        .or(config.and_then(|c| c.vibe.threshold))
        .unwrap_or(7.0);

    eprintln!("cargo-vibe: running health check against {since}...");
    let mut all_findings: Vec<ai_tools_core::finding::Finding> = Vec::new();
    let mut risk_failed = false;

    // 1. Run diff-risk
    eprintln!("  [1/3] diff-risk...");
    let diff_risk_config = config.map(|c| &c.diff_risk);
    if !tool_enabled(diff_risk_config) {
        eprintln!("  diff-risk: disabled by config, skipped");
    } else if let Some(diff_output) = ai_tools_core::git_utils::unified_diff(root, since) {
        let tmp = std::env::temp_dir().join("cargo-vibe-diff.txt");
        std::fs::write(&tmp, &diff_output)?;

        let mut cmd = Command::new("diff-risk");
        cmd.args(["--threshold", &risk_threshold.to_string()])
            .args(tool_extra_args(diff_risk_config))
            .stdin(Stdio::from(std::fs::File::open(&tmp)?));

        if let Some(risk_output) = output_or_skip_missing(&mut cmd, "diff-risk")? {
            let risk_ok = risk_output.status.success();
            let risk_text = String::from_utf8_lossy(&risk_output.stdout);
            let risk_err = String::from_utf8_lossy(&risk_output.stderr);
            if !risk_err.trim().is_empty() {
                eprintln!("{risk_err}");
            }
            if !risk_ok {
                risk_failed = true;
                eprintln!("  diff-risk: FAILED (threshold {risk_threshold} exceeded)");
            } else {
                eprintln!("  diff-risk: passed");
            }
            if !risk_text.trim().is_empty() {
                all_findings.extend(parse_findings_from_text(&risk_text, "diff-risk"));
            }
        } else {
            eprintln!("  diff-risk: not installed, skipped");
        }
    } else {
        eprintln!("  diff-risk: no diff available, skipped");
    }

    // 2. Run cargo-impact
    eprintln!("  [2/3] cargo-impact...");
    let cargo_impact_config = config.map(|c| &c.cargo_impact);
    if !tool_enabled(cargo_impact_config) {
        eprintln!("  cargo-impact: disabled by config, skipped");
    } else {
        let cargo_impact_extra_args = tool_extra_args(cargo_impact_config);
        let mut cmd = Command::new("cargo-impact");
        cmd.args(["--since", since, "--format", "json"]);
        if !has_flag(cargo_impact_extra_args, "--confidence-min") {
            cmd.args(["--confidence-min", "0.5"]);
        }
        cmd.args(cargo_impact_extra_args).current_dir(root);

        if let Some(impact_output) = output_or_skip_missing(&mut cmd, "cargo-impact")? {
            let impact_text = String::from_utf8_lossy(&impact_output.stdout);
            if !impact_output.stderr.is_empty() {
                eprintln!("{}", String::from_utf8_lossy(&impact_output.stderr));
            }
            let findings = parse_impact_json(&impact_text);
            let count = findings.len();
            if impact_output.status.success() {
                eprintln!("  cargo-impact: {count} finding(s)");
            } else {
                eprintln!("  cargo-impact: FAILED ({count} parsed finding(s))");
            }
            all_findings.extend(findings);
        } else {
            eprintln!("  cargo-impact: not installed, skipped");
        }
    }

    // 3. Run spec-drift
    eprintln!("  [3/3] spec-drift...");
    let spec_drift_config = config.map(|c| &c.spec_drift);
    if !tool_enabled(spec_drift_config) {
        eprintln!("  spec-drift: disabled by config, skipped");
    } else {
        let mut drift_cmd = Command::new("spec-drift");
        drift_cmd
            .args(["--format", "json", "--diff", since])
            .args(tool_extra_args(spec_drift_config))
            .current_dir(root);

        if let Some(drift_output) = output_or_skip_missing(&mut drift_cmd, "spec-drift")? {
            let drift_text = String::from_utf8_lossy(&drift_output.stdout);
            if !drift_output.stderr.is_empty() {
                eprintln!("{}", String::from_utf8_lossy(&drift_output.stderr));
            }
            let drift_findings = parse_drift_json(&drift_text);
            let drift_count = drift_findings.len();
            all_findings.extend(drift_findings);
            eprintln!("  spec-drift: {drift_count} divergence(s)");
        } else {
            eprintln!("  spec-drift: not installed, skipped");
        }
    }

    // Render results
    let total = all_findings.len();
    let critical = all_findings
        .iter()
        .filter(|f| f.severity == ai_tools_core::finding::Severity::Critical)
        .count();
    let high = all_findings
        .iter()
        .filter(|f| f.severity == ai_tools_core::finding::Severity::High)
        .count();

    eprintln!("\n  Total: {total} finding(s) | Critical: {critical} | High: {high}");

    match format {
        "json" => {
            let report = report::AggregatedReport::new("cargo-vibe check", &all_findings);
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        "sarif" => {
            let renderer = ai_tools_core::sarif::SarifRenderer::new("cargo-vibe", "0.1.0");
            println!("{}", renderer.render(&all_findings));
        }
        _ => {
            for f in &all_findings {
                println!(
                    "{} [{}] {}:{}  {}",
                    f.severity.marker(),
                    f.rule.as_str(),
                    f.location.file.display(),
                    f.location.line,
                    f.message
                );
            }
        }
    }

    if strict && (risk_failed || critical > 0 || high > 0) {
        Ok(ExitCode::from(1))
    } else {
        Ok(ExitCode::SUCCESS)
    }
}

fn run_risk(
    root: &Path,
    since: &str,
    threshold: Option<f32>,
    _format: &str,
    config: Option<&ai_tools_core::config::VibeConfig>,
) -> Result<ExitCode> {
    let t = threshold.unwrap_or_else(|| {
        config
            .and_then(|c| c.diff_risk.threshold)
            .or(config.and_then(|c| c.vibe.threshold))
            .unwrap_or(7.0)
    });

    let diff_risk_config = config.map(|c| &c.diff_risk);
    if !tool_enabled(diff_risk_config) {
        eprintln!("diff-risk: disabled by config, skipped");
        return Ok(ExitCode::SUCCESS);
    }

    let diff = ai_tools_core::git_utils::unified_diff(root, since)
        .context("no git diff available — is this a git repository?")?;

    let tmp = std::env::temp_dir().join("cargo-vibe-diff.txt");
    std::fs::write(&tmp, &diff)?;

    let risk_output = Command::new("diff-risk")
        .args(["--threshold", &t.to_string()])
        .args(tool_extra_args(diff_risk_config))
        .stdin(Stdio::from(std::fs::File::open(&tmp)?))
        .output()
        .context("running diff-risk — is it installed? (cargo install diff-risk)")?;

    let risk_text = String::from_utf8_lossy(&risk_output.stdout);
    let risk_err = String::from_utf8_lossy(&risk_output.stderr);
    if !risk_err.trim().is_empty() {
        eprintln!("{risk_err}");
    }
    print!("{risk_text}");

    if risk_output.status.success() {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::from(1))
    }
}

fn run_impact(
    root: &Path,
    since: &str,
    test: bool,
    format: &str,
    config: Option<&ai_tools_core::config::VibeConfig>,
) -> Result<ExitCode> {
    let cargo_impact_config = config.map(|c| &c.cargo_impact);
    if !tool_enabled(cargo_impact_config) {
        eprintln!("cargo-impact: disabled by config, skipped");
        return Ok(ExitCode::SUCCESS);
    }

    let mut cmd = Command::new("cargo-impact");
    cmd.args(["--since", since]);
    if test {
        cmd.arg("--test");
    } else {
        cmd.args(["--format", format]);
    }
    cmd.args(tool_extra_args(cargo_impact_config));
    cmd.current_dir(root);

    let output = cmd
        .output()
        .context("running cargo-impact — is it installed?")?;
    print!("{}", String::from_utf8_lossy(&output.stdout));
    if !output.stderr.is_empty() {
        eprintln!("{}", String::from_utf8_lossy(&output.stderr));
    }

    if output.status.success() {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::from(1))
    }
}

fn run_drift(
    root: &Path,
    diff: Option<&str>,
    format: &str,
    deny: &str,
    config: Option<&ai_tools_core::config::VibeConfig>,
) -> Result<ExitCode> {
    let spec_drift_config = config.map(|c| &c.spec_drift);
    if !tool_enabled(spec_drift_config) {
        eprintln!("spec-drift: disabled by config, skipped");
        return Ok(ExitCode::SUCCESS);
    }

    let mut cmd = Command::new("spec-drift");
    cmd.args(["--format", format]).args(["--deny", deny]);
    if let Some(d) = diff {
        cmd.args(["--diff", d]);
    }
    cmd.args(tool_extra_args(spec_drift_config));
    cmd.current_dir(root);

    let output = cmd
        .output()
        .context("running spec-drift — is it installed?")?;
    print!("{}", String::from_utf8_lossy(&output.stdout));
    if !output.stderr.is_empty() {
        eprintln!("{}", String::from_utf8_lossy(&output.stderr));
    }

    if output.status.success() {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::from(1))
    }
}

fn run_context(
    root: &Path,
    preset: &str,
    budget: Option<usize>,
    stdin: bool,
    config: Option<&ai_tools_core::config::VibeConfig>,
) -> Result<ExitCode> {
    let cargo_context_config = config.map(|c| &c.cargo_context);
    if !tool_enabled(cargo_context_config) {
        eprintln!("cargo-context: disabled by config, skipped");
        return Ok(ExitCode::SUCCESS);
    }

    let mut cmd = Command::new("cargo-context");
    cmd.args(["--preset", preset]);
    if let Some(b) = budget.or_else(|| config.and_then(|c| c.vibe.token_budget)) {
        cmd.args(["--max-tokens", &b.to_string()]);
    }
    cmd.args(tool_extra_args(cargo_context_config));
    cmd.current_dir(root);

    let output = if stdin {
        let mut input = String::new();
        std::io::stdin().read_to_string(&mut input)?;
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("running cargo-context — is it installed?")?;
        if let Some(mut child_stdin) = child.stdin.take() {
            match child_stdin.write_all(input.as_bytes()) {
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::BrokenPipe => {}
                Err(e) => return Err(e.into()),
            }
        }
        child.wait_with_output()?
    } else {
        cmd.stdin(Stdio::null())
            .output()
            .context("running cargo-context — is it installed?")?
    };
    print!("{}", String::from_utf8_lossy(&output.stdout));
    if !output.stderr.is_empty() {
        eprintln!("{}", String::from_utf8_lossy(&output.stderr));
    }

    if output.status.success() {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::from(1))
    }
}

fn run_fix(
    root: &Path,
    prompt: &str,
    max_attempts: usize,
    risk_threshold: Option<f32>,
    since: &str,
    auto_heal: bool,
    config: Option<&ai_tools_core::config::VibeConfig>,
) -> Result<ExitCode> {
    let risk_threshold = risk_threshold.unwrap_or_else(|| {
        config
            .and_then(|c| c.diff_risk.threshold)
            .or(config.and_then(|c| c.vibe.threshold))
            .unwrap_or(7.0)
    });
    let mut fix_loop = FixLoop::new(root, prompt, max_attempts, risk_threshold, since, auto_heal);

    eprintln!("cargo-vibe: starting fix loop with {max_attempts} max attempts...");
    eprintln!("  Risk threshold: {risk_threshold}");
    eprintln!("  Base ref: {since}");
    eprintln!(
        "  Clippy Auto-Healer: {}",
        if auto_heal {
            "enabled (--auto-heal)"
        } else {
            "disabled (opt in with --auto-heal)"
        }
    );

    let result = fix_loop.run()?;

    match result {
        GateResult::Passed => {
            eprintln!("cargo-vibe: fix loop completed successfully.");
            Ok(ExitCode::SUCCESS)
        }
        GateResult::Failed { reason } => {
            eprintln!("cargo-vibe: fix loop failed: {reason}");
            Ok(ExitCode::from(1))
        }
        GateResult::TimedOut { attempts } => {
            eprintln!("cargo-vibe: fix loop exhausted {attempts} attempts without success.");
            Ok(ExitCode::from(1))
        }
    }
}

// ---- Helpers ----

fn parse_cli() -> Cli {
    let mut args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if args.get(1).and_then(|arg| arg.to_str()) == Some("vibe") {
        args.remove(1);
    }
    Cli::parse_from(args)
}

fn load_config(
    root: &Path,
    config_path: Option<&Path>,
) -> Result<Option<ai_tools_core::config::VibeConfig>> {
    match config_path {
        Some(path) => {
            let contents = std::fs::read_to_string(path)
                .with_context(|| format!("failed to read config {}", path.display()))?;
            let config = toml::from_str(&contents)
                .with_context(|| format!("failed to parse config {}", path.display()))?;
            Ok(Some(config))
        }
        None => Ok(ai_tools_core::config::load_project_config(root)),
    }
}

fn tool_enabled(config: Option<&ai_tools_core::config::VibeToolConfig>) -> bool {
    config.and_then(|c| c.enabled).unwrap_or(true)
}

fn tool_extra_args(config: Option<&ai_tools_core::config::VibeToolConfig>) -> &[String] {
    config.map(|c| c.extra_args.as_slice()).unwrap_or(&[])
}

fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|arg| {
        arg == flag
            || arg
                .strip_prefix(flag)
                .is_some_and(|rest| rest.starts_with('='))
    })
}

fn output_or_skip_missing(cmd: &mut Command, tool: &str) -> Result<Option<Output>> {
    match cmd.output() {
        Ok(output) => Ok(Some(output)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("running {tool}")),
    }
}

fn parse_findings_from_text(text: &str, tool: &str) -> Vec<ai_tools_core::finding::Finding> {
    let mut findings = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with("diff-risk") {
            continue;
        }
        let (severity, rest) = split_marker(line);
        if rest.starts_with("DIFF RISK ASSESSMENT") || rest == "No risky patterns detected." {
            continue;
        }
        if let Some(finding) = parse_bracketed_finding(rest, severity, tool)
            .or_else(|| parse_diff_risk_finding(rest, severity, tool))
        {
            findings.push(finding);
        }
    }
    findings
}

fn split_marker(line: &str) -> (ai_tools_core::finding::Severity, &str) {
    let trimmed = line.trim_start();
    for (marker, severity) in [
        ("🚨", ai_tools_core::finding::Severity::Critical),
        ("⚠️", ai_tools_core::finding::Severity::High),
        ("⚠", ai_tools_core::finding::Severity::High),
        ("🟡", ai_tools_core::finding::Severity::Medium),
        ("✅", ai_tools_core::finding::Severity::Low),
        ("🔵", ai_tools_core::finding::Severity::Low),
    ] {
        if let Some(rest) = trimmed.strip_prefix(marker) {
            return (severity, rest.trim());
        }
    }
    (ai_tools_core::finding::Severity::Medium, trimmed)
}

fn parse_bracketed_finding(
    text: &str,
    severity: ai_tools_core::finding::Severity,
    tool: &str,
) -> Option<ai_tools_core::finding::Finding> {
    let rest = text.strip_prefix('[')?;
    let rule_end = rest.find(']')?;
    let rule = rule_from_id(&rest[..rule_end]);
    let after = rest[rule_end + 1..].trim();
    let (file, line_num, msg) = parse_location_and_message(after);
    Some(
        ai_tools_core::finding::Finding::new(
            rule,
            severity,
            ai_tools_core::finding::Confidence::Heuristic,
            ai_tools_core::finding::Location::new(file, line_num),
            msg,
        )
        .with_tool(tool),
    )
}

fn parse_diff_risk_finding(
    text: &str,
    severity: ai_tools_core::finding::Severity,
    tool: &str,
) -> Option<ai_tools_core::finding::Finding> {
    let (header, msg) = text.split_once(" — ")?;
    let (category, location) = header.split_once(": ")?;
    let (file, line_num) = parse_location(location);
    Some(
        ai_tools_core::finding::Finding::new(
            rule_from_diff_risk_label(category),
            severity,
            ai_tools_core::finding::Confidence::Heuristic,
            ai_tools_core::finding::Location::new(file, line_num),
            msg.trim().to_string(),
        )
        .with_tool(tool),
    )
}

fn parse_location_and_message(text: &str) -> (&str, u32, String) {
    let (location, msg) = text.split_once("  ").unwrap_or((text, ""));
    let (file, line_num) = parse_location(location.trim());
    (file, line_num, msg.trim().to_string())
}

fn parse_location(location: &str) -> (&str, u32) {
    if let Some((file, line)) = location.rsplit_once(':')
        && let Ok(line_num) = line.parse::<u32>()
    {
        return (file, line_num);
    }
    (location, 1)
}

fn rule_from_diff_risk_label(label: &str) -> ai_tools_core::finding::RuleId {
    match label {
        "API Contract Change" => ai_tools_core::finding::RuleId::ApiContract,
        "Async Boundary Change" => ai_tools_core::finding::RuleId::AsyncBoundary,
        "Serde Schema Drift" => ai_tools_core::finding::RuleId::SerdeDrift,
        "Auth / Permission Gate" => ai_tools_core::finding::RuleId::AuthGate,
        "Concurrency / Memory Safety" => ai_tools_core::finding::RuleId::Concurrency,
        _ => ai_tools_core::finding::RuleId::Other,
    }
}

fn rule_from_id(id: &str) -> ai_tools_core::finding::RuleId {
    match id {
        "api_contract" => ai_tools_core::finding::RuleId::ApiContract,
        "async_boundary" => ai_tools_core::finding::RuleId::AsyncBoundary,
        "serde_drift" => ai_tools_core::finding::RuleId::SerdeDrift,
        "auth_gate" => ai_tools_core::finding::RuleId::AuthGate,
        "concurrency" => ai_tools_core::finding::RuleId::Concurrency,
        _ => ai_tools_core::finding::RuleId::Other,
    }
}

fn parse_impact_json(text: &str) -> Vec<ai_tools_core::finding::Finding> {
    #[derive(serde::Deserialize)]
    struct ImpactEnvelope {
        findings: Vec<ImpactFinding>,
    }
    #[derive(serde::Deserialize)]
    struct ImpactFinding {
        id: String,
        severity: String,
        #[allow(dead_code)]
        tier: String,
        evidence: String,
        #[serde(default)]
        suggested_action: Option<String>,
        #[serde(flatten)]
        kind: serde_json::Value,
    }

    let envelope: ImpactEnvelope = match serde_json::from_str(text) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    envelope
        .findings
        .into_iter()
        .map(|f| {
            let severity = match f.severity.as_str() {
                "high" => ai_tools_core::finding::Severity::High,
                "medium" => ai_tools_core::finding::Severity::Medium,
                "low" => ai_tools_core::finding::Severity::Low,
                _ => ai_tools_core::finding::Severity::Medium,
            };
            let rule = match f
                .kind
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("other")
            {
                "test_reference" => ai_tools_core::finding::RuleId::TestReference,
                "trait_impl" => ai_tools_core::finding::RuleId::TraitImpl,
                "ffi_signature_change" => ai_tools_core::finding::RuleId::FfiSignatureChange,
                "build_script_changed" => ai_tools_core::finding::RuleId::BuildScriptChanged,
                _ => ai_tools_core::finding::RuleId::Other,
            };
            let file = f
                .kind
                .get("test")
                .and_then(|v| v.get("file"))
                .or_else(|| f.kind.get("file"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");

            ai_tools_core::finding::Finding::new(
                rule,
                severity,
                ai_tools_core::finding::Confidence::Heuristic,
                ai_tools_core::finding::Location::new(file, 1),
                f.evidence,
            )
            .with_id(f.id)
            .with_tool("cargo-impact")
            .with_action(f.suggested_action.unwrap_or_default())
        })
        .collect()
}

fn parse_drift_json(text: &str) -> Vec<ai_tools_core::finding::Finding> {
    #[derive(serde::Deserialize)]
    struct DriftDivergence {
        rule: String,
        severity: String,
        location: DriftLocation,
        stated: String,
        #[allow(dead_code)]
        reality: String,
        risk: String,
    }
    #[derive(serde::Deserialize)]
    struct DriftLocation {
        file: String,
        line: u32,
    }

    let divergences: Vec<DriftDivergence> = match serde_json::from_str(text) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };

    divergences
        .into_iter()
        .map(|d| {
            let severity = match d.severity.as_str() {
                "critical" => ai_tools_core::finding::Severity::Critical,
                "warning" => ai_tools_core::finding::Severity::High,
                _ => ai_tools_core::finding::Severity::Medium,
            };
            let rule = match d.rule.as_str() {
                "symbol_absence" => ai_tools_core::finding::RuleId::SymbolAbsence,
                "compile_failure" => ai_tools_core::finding::RuleId::CompileFailure,
                "lying_test" => ai_tools_core::finding::RuleId::LyingTest,
                "ghost_command" => ai_tools_core::finding::RuleId::GhostCommand,
                _ => ai_tools_core::finding::RuleId::Other,
            };
            ai_tools_core::finding::Finding::new(
                rule,
                severity,
                ai_tools_core::finding::Confidence::Deterministic,
                ai_tools_core::finding::Location::new(d.location.file, d.location.line),
                format!("{} — expected: {}", d.risk, d.stated),
            )
            .with_tool("spec-drift")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai_tools_core::finding::{RuleId, Severity};

    #[test]
    fn parses_diff_risk_human_output_without_banner() {
        let text = "\
🚨 DIFF RISK ASSESSMENT: [SCORE: 9.0/10.0 — CRITICAL RISK]

  🚨 Auth / Permission Gate: src/auth.rs:42 — auth check removed
  ⚠️ API Contract Change: src/lib.rs:7 — public function signature changed
";

        let findings = parse_findings_from_text(text, "diff-risk");

        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].rule, RuleId::AuthGate);
        assert_eq!(findings[0].severity, Severity::Critical);
        assert_eq!(findings[0].location.file, PathBuf::from("src/auth.rs"));
        assert_eq!(findings[0].location.line, 42);
        assert_eq!(findings[1].rule, RuleId::ApiContract);
        assert_eq!(findings[1].severity, Severity::High);
    }

    #[test]
    fn parses_legacy_bracketed_diff_risk_output() {
        let text = "  🟡 [serde_drift] src/model.rs:12  serde field renamed";

        let findings = parse_findings_from_text(text, "diff-risk");

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].rule, RuleId::SerdeDrift);
        assert_eq!(findings[0].severity, Severity::Medium);
        assert_eq!(findings[0].message, "serde field renamed");
    }
}
