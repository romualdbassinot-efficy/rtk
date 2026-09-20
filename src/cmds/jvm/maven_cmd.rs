//! Filters Maven build and test output with Surefire XML parser (70-90% token reduction).
//!
//! Strips Maven boilerplate, download progress, plugin headers, and
//! shows only failures and summary for test runs.

use crate::core::tracking;
use crate::core::utils::{exit_code_from_output, resolved_command, strip_ansi, truncate};
use anyhow::{Context, Result};
use lazy_static::lazy_static;
use regex::Regex;
use std::ffi::OsString;

lazy_static! {
    // All noise patterns collapsed into one alternation (O(1) per line).
    static ref NOISE_RE: Regex = Regex::new(
        r"^(?:\[INFO\]\s*$|\[INFO\] Scanning for projects|\[INFO\] -+(?:<|$)|\[INFO\] ={5,}|\[INFO\] Downloading\s|\[INFO\] Downloaded\s|Downloading:|Downloaded:|Progress|\[INFO\] --- maven-|\[INFO\] Using encoding|\[INFO\] skip non existing|\[INFO\] Nothing to compile|\[INFO\] Copying \d+ resources?|\[INFO\] Changes detected|\[INFO\] Finished at:|\[INFO\]\s+from\s+\S+/pom\.xml|\[INFO\] Using auto detected provider|\s*$)"
    ).unwrap();

    // Surefire test result line: "Tests run: N, Failures: M, Errors: E, Skipped: S"
    static ref TEST_RESULT_RE: Regex =
        Regex::new(r"Tests run:\s*(\d+),\s*Failures:\s*(\d+),\s*Errors:\s*(\d+),\s*Skipped:\s*(\d+)").unwrap();

    // Surefire failure line: "ClassName.methodName:line message"
    static ref FAILURE_SUMMARY_RE: Regex =
        Regex::new(r"^\[ERROR\]\s+(\S+\.\S+):(\d+)\s+(.+)").unwrap();

    // Surefire failure detail: "com.example.Test.method -- Time elapsed..."
    static ref FAILURE_DETAIL_RE: Regex =
        Regex::new(r"^\[ERROR\]\s+(\S+)\s+--\s+Time elapsed").unwrap();

    // Reactor summary line: "module ... SUCCESS/FAILURE [time]"
    static ref REACTOR_LINE_RE: Regex =
        Regex::new(r"^\[INFO\]\s+\S.*\.\.\s+(SUCCESS|FAILURE)\s+\[").unwrap();

    // Stack trace line
    static ref STACK_TRACE_RE: Regex =
        Regex::new(r"^\s+at\s+").unwrap();

    // Exception/assertion line
    static ref EXCEPTION_RE: Regex =
        Regex::new(r"^(java\.\S+Exception|java\.\S+Error|org\.junit\.\S+Error)").unwrap();

    // Maven BUILD SUCCESS/FAILURE
    static ref BUILD_STATUS_RE: Regex =
        Regex::new(r"^\[INFO\] BUILD (SUCCESS|FAILURE)").unwrap();

    // Total time line
    static ref TOTAL_TIME_RE: Regex =
        Regex::new(r"^\[INFO\] Total time:\s+(.+)").unwrap();

    // Compilation error: "[ERROR] <path>:[line,col] message". Accepts a Unix
    // absolute path or a Windows drive-letter path, since the same filter runs
    // on both.
    static ref COMPILE_ERROR_RE: Regex =
        Regex::new(r"^\[ERROR\]\s+(?:/|[A-Za-z]:[\\/])").unwrap();

    // Reactor Summary header
    static ref REACTOR_SUMMARY_RE: Regex =
        Regex::new(r"^\[INFO\] Reactor Summary").unwrap();

    // Compiling N source files
    static ref COMPILING_RE: Regex =
        Regex::new(r"^\[INFO\] Compiling \d+ source files").unwrap();
}

/// Maximum body lines kept by the build filter before truncation, matching the
/// `max_lines = 50` of the TOML filter this module replaced.
const MAX_BODY_LINES: usize = 50;

pub fn run_test(args: &[String], verbose: u8) -> Result<i32> {
    let timer = tracking::TimedExecution::start();

    let mut cmd = resolved_command("mvn");
    cmd.arg("test");

    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: mvn test {}", args.join(" "));
    }

    let output = cmd
        .output()
        .context("Failed to run mvn test. Is Maven installed?")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let raw = format!("{}\n{}", stdout, stderr);

    let exit_code = exit_code_from_output(&output, "mvn test");
    let filtered = filter_mvn_test(&raw);

    if let Some(hint) = crate::core::tee::tee_and_hint(&raw, "mvn_test", exit_code) {
        println!("{}\n{}", filtered, hint);
    } else {
        println!("{}", filtered);
    }

    timer.track(
        &format!("mvn test {}", args.join(" ")),
        &format!("rtk mvn test {}", args.join(" ")),
        &raw,
        &filtered,
    );

    Ok(exit_code)
}

pub fn run_compile(args: &[String], verbose: u8) -> Result<i32> {
    run_build_phase("compile", args, verbose)
}

pub fn run_package(args: &[String], verbose: u8) -> Result<i32> {
    run_build_phase("package", args, verbose)
}

/// Generic build phase runner for compile/package/install/clean
fn run_build_phase(phase: &str, args: &[String], verbose: u8) -> Result<i32> {
    let timer = tracking::TimedExecution::start();

    let mut cmd = resolved_command("mvn");
    cmd.arg(phase);

    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: mvn {} {}", phase, args.join(" "));
    }

    let output = cmd
        .output()
        .with_context(|| format!("Failed to run mvn {}. Is Maven installed?", phase))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let raw = format!("{}\n{}", stdout, stderr);

    let exit_code = exit_code_from_output(&output, &format!("mvn {}", phase));
    let filtered = super::reword_if_failed(filter_mvn_build(&raw), "mvn", exit_code);

    if let Some(hint) = crate::core::tee::tee_and_hint(&raw, &format!("mvn_{}", phase), exit_code) {
        println!("{}\n{}", filtered, hint);
    } else {
        println!("{}", filtered);
    }

    timer.track(
        &format!("mvn {} {}", phase, args.join(" ")),
        &format!("rtk mvn {} {}", phase, args.join(" ")),
        &raw,
        &filtered,
    );

    Ok(exit_code)
}

pub fn run_other(args: &[OsString], verbose: u8) -> Result<i32> {
    if args.is_empty() {
        anyhow::bail!("mvn: no subcommand specified");
    }

    let timer = tracking::TimedExecution::start();

    let subcommand = args[0].to_string_lossy();
    let mut cmd = resolved_command("mvn");
    cmd.arg(&*subcommand);

    for arg in &args[1..] {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: mvn {} ...", subcommand);
    }

    let output = cmd
        .output()
        .with_context(|| format!("Failed to run mvn {}", subcommand))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let raw = format!("{}\n{}", stdout, stderr);

    let exit_code = exit_code_from_output(&output, "mvn");

    // Passthrough, ANSI-stripped only — deliberately NOT filter_mvn_build.
    //
    // filter_mvn_build is a whitelist: it keeps reactor lines, [ERROR],
    // [WARNING], "Compiling N source files" and their continuation lines, and
    // drops everything else. Any goal whose payload is none of those had its
    // entire output erased — `mvn dependency:tree` returned just
    // "BUILD SUCCESS (1.2 s)", and the same went for help:effective-pom,
    // versions:*, and program output from exec:java. Deleting
    // src/filters/mvn-build.toml removed the net that used to cover them.
    //
    // develop's mvn_cmd.rs routes the same set (clean, site, dependency:*,
    // --version, --help, any unrecognised goal) to passthrough for this reason.
    let filtered = strip_ansi(&raw);

    if let Some(hint) =
        crate::core::tee::tee_and_hint(&raw, &format!("mvn_{}", subcommand), exit_code)
    {
        println!("{}\n{}", filtered, hint);
    } else {
        println!("{}", filtered);
    }

    timer.track(
        &format!("mvn {}", subcommand),
        &format!("rtk mvn {}", subcommand),
        &raw,
        &filtered,
    );

    Ok(exit_code)
}

/// Filter Maven build output (compile/package/install): strip noise, keep errors and summary
fn filter_mvn_build(output: &str) -> String {
    // Strip ANSI first: every pattern below is ^-anchored, so a single
    // colour escape would defeat the whole filter. Piped output is plain
    // by default, but --console=rich, -Dstyle.color=always and many CI
    // images force colour on.
    let output = &strip_ansi(output);
    let mut result_lines: Vec<String> = Vec::new();
    let mut in_reactor_summary = false;
    let mut build_status = String::new();
    let mut total_time = String::new();

    for line in output.lines() {
        let trimmed = line.trim();

        // Track reactor summary section
        if REACTOR_SUMMARY_RE.is_match(trimmed) {
            in_reactor_summary = true;
            continue;
        }

        // Capture build status
        if let Some(caps) = BUILD_STATUS_RE.captures(trimmed) {
            build_status = caps[1].to_string();
            continue;
        }

        // Capture total time
        if let Some(caps) = TOTAL_TIME_RE.captures(trimmed) {
            total_time = caps[1].to_string();
            continue;
        }

        // In reactor summary: keep module status lines
        if in_reactor_summary {
            if REACTOR_LINE_RE.is_match(trimmed) {
                result_lines.push(trimmed.to_string());
                continue;
            }
            // Empty [INFO] line ends reactor summary
            if trimmed == "[INFO]" || trimmed.starts_with("[INFO] ---") {
                in_reactor_summary = false;
                continue;
            }
        }

        // Skip noise
        if is_noise_line(trimmed) {
            continue;
        }

        // Keep ERROR lines
        if trimmed.starts_with("[ERROR]") {
            result_lines.push(truncate(trimmed, 150).to_string());
            continue;
        }

        // Keep WARNING lines
        if trimmed.starts_with("[WARNING]") {
            result_lines.push(truncate(trimmed, 150).to_string());
            continue;
        }

        // Keep compilation info
        if COMPILING_RE.is_match(trimmed) {
            result_lines.push(trimmed.to_string());
            continue;
        }

        // Keep javac's continuation lines, but only directly under an error.
        // `is_some_and` covers the empty case, so no length check is needed.
        if result_lines
            .last()
            .is_some_and(|l| l.starts_with("[ERROR]"))
            && (trimmed.starts_with("symbol:")
                || trimmed.starts_with("location:")
                || trimmed.starts_with("required:")
                || trimmed.starts_with("found:")
                || trimmed.starts_with("reason:"))
        {
            result_lines.push(format!("  {}", trimmed));
            continue;
        }
    }

    // Cap the body before the footer is appended, so the BUILD verdict always
    // survives truncation. Real reactors emit warnings in bulk - platform
    // encoding, deprecation and unchecked notes per file, plugin-version
    // warnings per module - and every one of them was kept unconditionally.
    // The deleted src/filters/mvn-build.toml had max_lines = 50.
    if result_lines.len() > MAX_BODY_LINES {
        let dropped = result_lines.len() - MAX_BODY_LINES;
        result_lines.truncate(MAX_BODY_LINES);
        result_lines.push(format!("... +{} more lines", dropped));
    }

    // Build summary footer
    if !build_status.is_empty() {
        let time_info = if total_time.is_empty() {
            String::new()
        } else {
            format!(" ({})", total_time)
        };
        result_lines.push(format!("BUILD {}{}", build_status, time_info));
    }

    if result_lines.is_empty() {
        return "mvn: ok".to_string();
    }

    result_lines.join("\n")
}

/// Filter Maven test output (Surefire): show failures and summary
fn filter_mvn_test(output: &str) -> String {
    // Strip ANSI first: every pattern below is ^-anchored, so a single
    // colour escape would defeat the whole filter. Piped output is plain
    // by default, but --console=rich, -Dstyle.color=always and many CI
    // images force colour on.
    let output = &strip_ansi(output);
    let mut total_run: usize = 0;
    let mut total_failures: usize = 0;
    let mut total_errors: usize = 0;
    let mut total_skipped: usize = 0;
    // Per-class totals, used only as a fallback when a run prints no module
    // summary at all (see the TEST_RESULT_RE branch below).
    let mut class_run: usize = 0;
    let mut class_failures: usize = 0;
    let mut class_errors: usize = 0;
    let mut class_skipped: usize = 0;
    let mut failures: Vec<MavenTestFailure> = Vec::new();
    let mut current_failure: Option<MavenTestFailure> = None;
    let mut in_failure_output = false;
    let mut stack_lines_collected: usize = 0;
    let mut build_status = String::new();
    let mut total_time = String::new();
    let mut in_failures_section = false;
    let mut build_errors: Vec<String> = Vec::new();

    for line in output.lines() {
        let trimmed = line.trim();

        // Capture build status
        if let Some(caps) = BUILD_STATUS_RE.captures(trimmed) {
            build_status = caps[1].to_string();
            continue;
        }

        // Capture total time
        if let Some(caps) = TOTAL_TIME_RE.captures(trimmed) {
            total_time = caps[1].to_string();
            continue;
        }

        // Compiler diagnostics. When compilation fails no test ever runs, so
        // without these the whole output collapses to "BUILD FAILURE" and the
        // caller has to re-run the raw command to learn anything. Maven prints
        // each diagnostic twice - once under "COMPILATION ERROR" and again
        // under "Failed to execute goal" - so dedupe while keeping order.
        // Deliberately no `continue`: this leaves every existing state
        // transition below untouched.
        if COMPILE_ERROR_RE.is_match(trimmed) {
            let entry = truncate(trimmed, 150).to_string();
            if !build_errors.contains(&entry) {
                build_errors.push(entry);
            }
        }

        // Detect [ERROR] Failures: section (Surefire summary)
        if trimmed == "[ERROR] Failures:" {
            in_failures_section = true;
            continue;
        }

        // In failures section: capture compact failure summaries
        if in_failures_section {
            if let Some(caps) = FAILURE_SUMMARY_RE.captures(trimmed) {
                let method_path = caps[1].to_string();
                let line_num = caps[2].to_string();
                let message = caps[3].to_string();

                upsert_failure(
                    &mut failures,
                    MavenTestFailure {
                        test_name: compact_test_name(&method_path),
                        fqn: String::new(),
                        message: truncate(&message, 120).to_string(),
                        location: format!("{}:{}", method_path, line_num),
                        stack_lines: Vec::new(),
                    },
                );
                continue;
            }
            // End of failures section
            if trimmed.is_empty()
                || trimmed.starts_with("[INFO]")
                || trimmed.starts_with("[ERROR] Tests run:")
            {
                in_failures_section = false;
                // Fall through to process the line normally
            }
        }

        // Capture test result counts.
        //
        // Surefire emits two shapes of this line and they must not be mixed:
        //   per-class  "[INFO] Tests run: 3, ... Time elapsed: 0.2 s -- in com.x.FooTest"
        //   per-module "[INFO] Tests run: 6, Failures: 0, Errors: 0, Skipped: 0"
        // The per-class line carries a " -- in <Class>" suffix; the module
        // summary inside each "Results:" block does not. A reactor prints one
        // module summary per module, so the totals are the sum of those — never
        // a single line, whichever arrives first.
        if let Some(caps) = TEST_RESULT_RE.captures(trimmed) {
            let run: usize = caps[1].parse().unwrap_or(0);
            let fail: usize = caps[2].parse().unwrap_or(0);
            let err: usize = caps[3].parse().unwrap_or(0);
            let skip: usize = caps[4].parse().unwrap_or(0);

            if trimmed.contains(" -- in ") {
                class_run += run;
                class_failures += fail;
                class_errors += err;
                class_skipped += skip;
            } else {
                total_run += run;
                total_failures += fail;
                total_errors += err;
                total_skipped += skip;
            }
            continue;
        }

        // Capture failure detail line
        if FAILURE_DETAIL_RE.is_match(trimmed) {
            // Save previous failure
            if let Some(f) = current_failure.take() {
                upsert_failure(&mut failures, f);
            }

            let test_name = trimmed
                .trim_start_matches("[ERROR] ")
                .split(" -- ")
                .next()
                .unwrap_or("")
                .to_string();
            current_failure = Some(MavenTestFailure {
                test_name: compact_test_name(&test_name),
                fqn: test_name.clone(),
                message: String::new(),
                location: String::new(),
                stack_lines: Vec::new(),
            });
            in_failure_output = true;
            stack_lines_collected = 0;
            continue;
        }

        // Inside failure output: capture exception and stack
        if in_failure_output {
            if let Some(ref mut f) = current_failure {
                if EXCEPTION_RE.is_match(trimmed) || trimmed.starts_with("java.lang.Assertion") {
                    // Extract message from "ExceptionType: message"
                    if let Some(pos) = trimmed.find(": ") {
                        f.message = truncate(&trimmed[pos + 2..], 120).to_string();
                    } else {
                        f.message = truncate(trimmed, 120).to_string();
                    }
                } else if STACK_TRACE_RE.is_match(trimmed) && stack_lines_collected < 3 {
                    // Keep first few relevant stack lines (skip framework)
                    if !trimmed.contains("org.junit.")
                        && !trimmed.contains("java.base/")
                        && !trimmed.contains("jdk.internal")
                        && !trimmed.contains("sun.reflect")
                    {
                        f.stack_lines
                            .push(truncate(trimmed.trim(), 120).to_string());
                        stack_lines_collected += 1;
                    }
                } else if trimmed.is_empty()
                    || trimmed.starts_with("[INFO]")
                    || trimmed.starts_with("[ERROR]")
                {
                    in_failure_output = false;
                }
            }
        }
    }

    // Save last failure
    if let Some(f) = current_failure.take() {
        upsert_failure(&mut failures, f);
    }

    // No module summary was seen — a log level or Surefire version that omits
    // the "Results:" block. Fall back to the per-class lines rather than
    // reporting zero tests for a run that clearly had some.
    if total_run == 0 && class_run > 0 {
        total_run = class_run;
        total_failures = class_failures;
        total_errors = class_errors;
        total_skipped = class_skipped;
    }

    let total_failed = total_failures + total_errors;
    let total_passed = total_run.saturating_sub(total_failed + total_skipped);

    // No tests ran
    if total_run == 0 {
        let time_info = if total_time.is_empty() {
            String::new()
        } else {
            format!(" ({})", total_time)
        };
        // Compilation failed before any test could run — report the
        // diagnostics, which are the only actionable thing in the output.
        if !build_errors.is_empty() {
            let mut result = format!(
                "mvn test: {} build errors{}\n",
                build_errors.len(),
                time_info
            );
            result.push_str("=======================================\n");
            for error in build_errors.iter().take(10) {
                result.push_str(&format!("  {}\n", error));
            }
            if build_errors.len() > 10 {
                result.push_str(&format!("  ... +{} more errors\n", build_errors.len() - 10));
            }
            return result.trim().to_string();
        }
        if build_status == "FAILURE" {
            return format!("mvn test: BUILD FAILURE{}", time_info);
        }
        return format!("mvn test: no tests found{}", time_info);
    }

    // All passed
    if total_failed == 0 {
        let time_info = if total_time.is_empty() {
            String::new()
        } else {
            format!(" ({})", total_time)
        };
        let skip_info = if total_skipped > 0 {
            format!(", {} skipped", total_skipped)
        } else {
            String::new()
        };
        return format!(
            "mvn test: {} passed{}{}",
            total_passed, skip_info, time_info
        );
    }

    // Has failures
    let time_info = if total_time.is_empty() {
        String::new()
    } else {
        format!(" ({})", total_time)
    };

    let mut result = format!(
        "FAILED: {}/{} tests{}\n",
        total_failed, total_run, time_info
    );
    result.push_str("=======================================\n");

    for f in &failures {
        result.push_str(&format!("  {} FAILED\n", f.test_name));
        if !f.message.is_empty() {
            result.push_str(&format!("    {}\n", f.message));
        }
        if !f.location.is_empty() {
            result.push_str(&format!("    at {}\n", truncate(&f.location, 100)));
        }
        for stack_line in &f.stack_lines {
            result.push_str(&format!("    {}\n", stack_line));
        }
    }

    if build_status == "FAILURE" {
        result.push_str("\nBUILD FAILURE");
    }

    result.trim().to_string()
}

struct MavenTestFailure {
    test_name: String,
    /// Fully-qualified name when the source line carried one. The per-test
    /// `<<< FAILURE!` block does; the `Failures:` summary section does not, so
    /// this is the only field that can tell two same-named tests in different
    /// packages apart — and only when both sides supply it.
    fqn: String,
    message: String,
    location: String,
    stack_lines: Vec<String>,
}

/// Merge a failure into the list, or add it if new.
///
/// Surefire reports the same failure through two channels - the per-test
/// `<<< FAILURE!` block (message + stack) and the `Failures:` summary section
/// (message + file:line) - and they appear in that order, so neither channel
/// alone is complete and a plain push emits the test twice.
fn upsert_failure(failures: &mut Vec<MavenTestFailure>, incoming: MavenTestFailure) {
    let existing = failures.iter_mut().find(|f| {
        f.test_name == incoming.test_name
            && (f.fqn.is_empty() || incoming.fqn.is_empty() || f.fqn == incoming.fqn)
    });

    match existing {
        Some(f) => {
            if f.fqn.is_empty() {
                f.fqn = incoming.fqn;
            }
            if f.message.is_empty() {
                f.message = incoming.message;
            }
            if f.location.is_empty() {
                f.location = incoming.location;
            }
            if f.stack_lines.is_empty() {
                f.stack_lines = incoming.stack_lines;
            }
        }
        None => failures.push(incoming),
    }
}

/// Check if a line matches any noise pattern
fn is_noise_line(line: &str) -> bool {
    NOISE_RE.is_match(line)
}

/// Compact test name: "com.edeal.frontline.UserServiceTest.testFoo" -> "UserServiceTest.testFoo"
fn compact_test_name(name: &str) -> String {
    let parts: Vec<&str> = name.rsplitn(3, '.').collect();
    if parts.len() >= 2 {
        format!("{}.{}", parts[1], parts[0])
    } else {
        name.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count_tokens(text: &str) -> usize {
        text.split_whitespace().count()
    }

    // ============================================================
    // Build filter tests
    // ============================================================

    #[test]
    fn test_filter_mvn_build_success() {
        let input = include_str!("../../../tests/fixtures/mvn_compile_success_raw.txt");
        let output = filter_mvn_build(input);

        // Should strip noise
        assert!(!output.contains("Scanning for projects"));
        assert!(!output.contains("Downloading from central"));
        assert!(!output.contains("Downloaded from central"));
        assert!(!output.contains("maven-resources-plugin"));
        assert!(!output.contains("maven-compiler-plugin"));
        assert!(!output.contains("Nothing to compile"));

        // Should keep build result
        assert!(output.contains("BUILD SUCCESS"));
        assert!(output.contains("18.234"));
    }

    #[test]
    fn test_filter_mvn_build_failure() {
        let input = include_str!("../../../tests/fixtures/mvn_compile_fail_raw.txt");
        let output = filter_mvn_build(input);

        // Should keep errors
        assert!(output.contains("[ERROR]"));
        assert!(output.contains("cannot find symbol"));
        assert!(output.contains("BUILD FAILURE"));

        // Should strip noise
        assert!(!output.contains("Scanning for projects"));
        assert!(!output.contains("maven-resources-plugin"));
    }

    #[test]
    fn test_build_filter_erases_dependency_tree_payload() {
        // Documents WHY run_other passes through instead of calling this
        // filter: filter_mvn_build is a whitelist, so a dependency tree - which
        // is neither [ERROR], [WARNING], a reactor line nor "Compiling N" -
        // survives none of it. If this ever stops holding, run_other can be
        // reconsidered; until then passthrough is the only non-destructive
        // option for unrecognised goals.
        let tree = "\
[INFO] --- maven-dependency-plugin:3.6.0:tree (default-cli) @ app ---
[INFO] com.example:app:jar:1.0.0
[INFO] +- org.springframework:spring-core:jar:5.3.0:compile
[INFO] |  \\- org.springframework:spring-jcl:jar:5.3.0:compile
[INFO] \\- junit:junit:jar:4.13.2:test
[INFO] BUILD SUCCESS
[INFO] Total time:  1.234 s
";
        assert_eq!(filter_mvn_build(tree), "BUILD SUCCESS (1.234 s)");
        assert!(!filter_mvn_build(tree).contains("spring-core"));

        // Passthrough keeps it, which is what run_other now does.
        assert!(strip_ansi(tree).contains("spring-core"));
    }

    #[test]
    fn test_build_body_is_capped_and_verdict_survives() {
        // Every [WARNING] was kept unconditionally with no bound. Cap the body,
        // and keep the BUILD footer outside the cap.
        let mut input = String::new();
        for i in 0..200 {
            input.push_str(&format!(
                "[WARNING] /src/main/java/A{}.java: uses unchecked operations\n",
                i
            ));
        }
        input.push_str("[INFO] BUILD SUCCESS\n");
        input.push_str("[INFO] Total time:  4.123 s\n");

        let output = filter_mvn_build(&input);
        let lines: Vec<&str> = output.lines().collect();

        assert_eq!(lines.len(), MAX_BODY_LINES + 2, "body + marker + footer");
        assert_eq!(lines[MAX_BODY_LINES], "... +150 more lines");
        assert_eq!(lines[MAX_BODY_LINES + 1], "BUILD SUCCESS (4.123 s)");
    }

    #[test]
    fn test_ansi_escapes_do_not_defeat_the_filters() {
        // Maven colourises with -Dstyle.color=always; NOISE_RE is ^-anchored on
        // "[INFO]", so an escape before it would keep every boilerplate line.
        let coloured = "\x1b[1m[INFO] Scanning for projects...\x1b[0m\n\
                        \x1b[34m[INFO] --- maven-compiler-plugin:3.11.0:compile ---\x1b[0m\n\
                        \x1b[32m[INFO] BUILD SUCCESS\x1b[0m\n\
                        \x1b[1m[INFO] Total time:  4.123 s\x1b[0m\n";
        let output = filter_mvn_build(coloured);

        assert!(!output.contains('\x1b'), "ANSI survived: {:?}", output);
        assert!(!output.contains("Scanning for projects"));
        assert!(!output.contains("maven-compiler-plugin"));
        assert_eq!(output, "BUILD SUCCESS (4.123 s)");
    }

    #[test]
    fn test_filter_mvn_build_empty() {
        let output = filter_mvn_build("");
        assert_eq!(output, "mvn: ok");
    }

    #[test]
    fn test_filter_mvn_build_savings() {
        let input = include_str!("../../../tests/fixtures/mvn_compile_success_raw.txt");
        let output = filter_mvn_build(input);

        let input_tokens = count_tokens(input);
        let output_tokens = count_tokens(&output);
        let savings = 100.0 - (output_tokens as f64 / input_tokens as f64 * 100.0);

        assert!(
            savings >= 60.0,
            "Maven build filter: expected >=60% savings, got {:.1}% ({} -> {} tokens)",
            savings,
            input_tokens,
            output_tokens
        );
    }

    // ============================================================
    // Test filter tests
    // ============================================================

    #[test]
    fn test_filter_mvn_test_all_pass() {
        let input = include_str!("../../../tests/fixtures/mvn_test_pass_raw.txt");
        let output = filter_mvn_test(input);

        // The fixture is a 2-module reactor: 6 tests in edeal-common, 14 in
        // edeal-webapp. Assert the exact total — a `contains("passed")` here
        // held just as well when the filter reported 3 of the 20.
        assert_eq!(output, "mvn test: 20 passed (22.345 s)");
    }

    #[test]
    fn test_filter_mvn_test_with_failures() {
        let input = include_str!("../../../tests/fixtures/mvn_test_fail_raw.txt");
        let output = filter_mvn_test(input);

        // 6 + 14 tests across the two modules, 2 failures in the second.
        assert!(
            output.starts_with("FAILED: 2/20 tests"),
            "expected the reactor total, got: {}",
            output
        );
        assert!(output.contains("UserServiceTest.testUpdateUserProfile"));
        assert!(output.contains("RestControllerTest.testAuthRequired"));
    }

    #[test]
    fn test_per_class_lines_do_not_become_the_total() {
        // Regression guard for the first-match-wins bug: the per-class line
        // ("-- in <Class>") must never be taken as the module total.
        let input = "\
[INFO] Tests run: 3, Failures: 0, Errors: 0, Skipped: 0, Time elapsed: 0.2 s -- in com.x.ATest
[INFO] Tests run: 4, Failures: 0, Errors: 0, Skipped: 0, Time elapsed: 0.3 s -- in com.x.BTest
[INFO] Results:
[INFO] Tests run: 7, Failures: 0, Errors: 0, Skipped: 0
[INFO] BUILD SUCCESS
";
        assert_eq!(filter_mvn_test(input), "mvn test: 7 passed");
    }

    #[test]
    fn test_per_class_fallback_when_no_module_summary() {
        // No "Results:" block at all — the per-class lines are all there is,
        // so they must be summed rather than reported as "no tests found".
        let input = "\
[INFO] Tests run: 3, Failures: 0, Errors: 0, Skipped: 0, Time elapsed: 0.2 s -- in com.x.ATest
[INFO] Tests run: 4, Failures: 1, Errors: 0, Skipped: 0, Time elapsed: 0.3 s -- in com.x.BTest
[INFO] BUILD FAILURE
";
        let output = filter_mvn_test(input);
        assert!(
            output.starts_with("FAILED: 1/7 tests"),
            "expected per-class fallback totals, got: {}",
            output
        );
    }

    #[test]
    fn test_filter_mvn_test_savings_pass() {
        let input = include_str!("../../../tests/fixtures/mvn_test_pass_raw.txt");
        let output = filter_mvn_test(input);

        let input_tokens = count_tokens(input);
        let output_tokens = count_tokens(&output);
        let savings = 100.0 - (output_tokens as f64 / input_tokens as f64 * 100.0);

        assert!(
            savings >= 85.0,
            "Maven test (pass) filter: expected >=85% savings, got {:.1}% ({} -> {} tokens)",
            savings,
            input_tokens,
            output_tokens
        );
    }

    #[test]
    fn test_filter_mvn_test_savings_fail() {
        let input = include_str!("../../../tests/fixtures/mvn_test_fail_raw.txt");
        let output = filter_mvn_test(input);

        let input_tokens = count_tokens(input);
        let output_tokens = count_tokens(&output);
        let savings = 100.0 - (output_tokens as f64 / input_tokens as f64 * 100.0);

        assert!(
            savings >= 70.0,
            "Maven test (fail) filter: expected >=70% savings, got {:.1}% ({} -> {} tokens)",
            savings,
            input_tokens,
            output_tokens
        );
    }

    #[test]
    fn test_filter_mvn_test_empty() {
        let output = filter_mvn_test("");
        assert!(output.contains("mvn test:"));
    }

    #[test]
    fn test_filter_mvn_test_reports_compile_errors() {
        // Compilation fails, so no test runs. The diagnostics are the only
        // actionable content and must survive; previously this whole fixture
        // collapsed to "mvn test: BUILD FAILURE".
        let input = include_str!("../../../tests/fixtures/mvn_compile_fail_raw.txt");
        let output = filter_mvn_test(input);

        assert!(
            output.starts_with("mvn test: 3 build errors"),
            "expected deduped compile-error summary, got: {}",
            output
        );
        assert!(output.contains("cannot find symbol"));
        assert!(output.contains("package javax.ws.rs does not exist"));
        assert!(
            !output.contains("BUILD FAILURE"),
            "the error list replaces the bare status line"
        );
    }

    #[test]
    fn test_each_failure_listed_once() {
        // Surefire reports each failure twice: once in the per-test
        // "<<< FAILURE!" block, once in the "Failures:" summary. The two must
        // merge into one entry carrying both the message and the location.
        let input = include_str!("../../../tests/fixtures/mvn_test_fail_raw.txt");
        let output = filter_mvn_test(input);

        assert_eq!(
            output
                .matches("UserServiceTest.testUpdateUserProfile FAILED")
                .count(),
            1,
            "failure listed more than once:\n{}",
            output
        );
        assert_eq!(
            output.matches("FAILED\n").count(),
            2,
            "expected exactly 2 failure entries for 2 failures:\n{}",
            output
        );
        // The merged entry keeps the summary's location and the block's message.
        assert!(output.contains("Expected user name to be \"John Updated\" but was \"John\""));
        assert!(output.contains("at UserServiceTest.testUpdateUserProfile:89"));
    }

    #[test]
    fn test_same_method_name_in_two_packages_stays_distinct() {
        // compact_test_name() drops the package, so both of these compact to
        // "FooTest.testX". They are different tests and must not merge.
        let mut failures = Vec::new();
        upsert_failure(
            &mut failures,
            MavenTestFailure {
                test_name: "FooTest.testX".to_string(),
                fqn: "com.a.FooTest.testX".to_string(),
                message: "a failed".to_string(),
                location: String::new(),
                stack_lines: Vec::new(),
            },
        );
        upsert_failure(
            &mut failures,
            MavenTestFailure {
                test_name: "FooTest.testX".to_string(),
                fqn: "com.b.FooTest.testX".to_string(),
                message: "b failed".to_string(),
                location: String::new(),
                stack_lines: Vec::new(),
            },
        );
        assert_eq!(failures.len(), 2);

        // Same test arriving from the two channels does merge: the summary
        // channel carries no fqn, so it attaches to the existing entry.
        upsert_failure(
            &mut failures,
            MavenTestFailure {
                test_name: "FooTest.testX".to_string(),
                fqn: String::new(),
                message: String::new(),
                location: "FooTest.testX:12".to_string(),
                stack_lines: Vec::new(),
            },
        );
        assert_eq!(failures.len(), 2);
        assert_eq!(failures[0].location, "FooTest.testX:12");
        assert_eq!(failures[0].message, "a failed");
    }

    #[test]
    fn test_compile_error_re_matches_windows_paths() {
        assert!(COMPILE_ERROR_RE.is_match(r"[ERROR] /src/main/java/A.java:[1,2] boom"));
        assert!(COMPILE_ERROR_RE.is_match(r"[ERROR] C:\src\main\java\A.java:[1,2] boom"));
        assert!(!COMPILE_ERROR_RE.is_match("[ERROR] Tests run: 4, Failures: 1"));
    }

    // ============================================================
    // Utility tests
    // ============================================================

    // ============================================================
    // Snapshot tests
    // ============================================================

    #[test]
    fn test_snapshot_mvn_build_success() {
        let input = include_str!("../../../tests/fixtures/mvn_compile_success_raw.txt");
        insta::assert_snapshot!(filter_mvn_build(input));
    }

    #[test]
    fn test_snapshot_mvn_build_fail() {
        let input = include_str!("../../../tests/fixtures/mvn_compile_fail_raw.txt");
        insta::assert_snapshot!(filter_mvn_build(input));
    }

    #[test]
    fn test_snapshot_mvn_test_pass() {
        let input = include_str!("../../../tests/fixtures/mvn_test_pass_raw.txt");
        insta::assert_snapshot!(filter_mvn_test(input));
    }

    #[test]
    fn test_snapshot_mvn_test_fail() {
        let input = include_str!("../../../tests/fixtures/mvn_test_fail_raw.txt");
        insta::assert_snapshot!(filter_mvn_test(input));
    }

    #[test]
    fn test_compact_test_name() {
        assert_eq!(
            compact_test_name("com.edeal.frontline.UserServiceTest.testFoo"),
            "UserServiceTest.testFoo"
        );
        assert_eq!(
            compact_test_name("SimpleTest.testBar"),
            "SimpleTest.testBar"
        );
    }

    #[test]
    fn test_is_noise_line() {
        assert!(is_noise_line("[INFO] Scanning for projects..."));
        assert!(is_noise_line("[INFO] Downloading org.apache:foo:1.0"));
        assert!(is_noise_line("[INFO] Downloaded org.apache:foo:1.0"));
        assert!(is_noise_line(
            "[INFO] --- maven-compiler-plugin:3.11.0:compile ---"
        ));
        assert!(is_noise_line(
            "[INFO] Nothing to compile - all classes are up to date."
        ));
        assert!(is_noise_line("[INFO] "));
        assert!(is_noise_line("[INFO] ---"));
        assert!(is_noise_line("[INFO] Using encoding: UTF-8"));

        assert!(!is_noise_line("[ERROR] Compilation failed"));
        assert!(!is_noise_line("[INFO] BUILD SUCCESS"));
        assert!(!is_noise_line("[WARNING] Using deprecated API"));
    }
}
