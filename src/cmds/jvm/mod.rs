//! JVM ecosystem command filter modules (Gradle, Maven).

pub mod gradle_cmd;
pub mod maven_cmd;

/// Reword a build filter's "nothing to report" line when the command failed.
///
/// The build filters collapse to `<label>: ok` when every line turned out to be
/// noise. That is the right answer on a zero exit and a lie on any other: a run
/// that failed with only filtered-out output printed `ok` while the exit status
/// said the opposite, and the text is what an agent reads.
///
/// The filters themselves stay pure - they never see the exit code - so this is
/// applied by the `run_*` wrappers, which do.
pub(crate) fn reword_if_failed(filtered: String, label: &str, exit_code: i32) -> String {
    if exit_code != 0 && filtered == format!("{}: ok", label) {
        format!(
            "{} failed (exit {}, no diagnostic output)",
            label, exit_code
        )
    } else {
        filtered
    }
}

#[cfg(test)]
mod tests {
    use super::reword_if_failed;

    #[test]
    fn keeps_ok_on_success() {
        assert_eq!(
            reword_if_failed("Gradle build: ok".to_string(), "Gradle build", 0),
            "Gradle build: ok"
        );
    }

    #[test]
    fn rewords_silent_failure() {
        assert_eq!(
            reword_if_failed("Gradle build: ok".to_string(), "Gradle build", 1),
            "Gradle build failed (exit 1, no diagnostic output)"
        );
        assert_eq!(
            reword_if_failed("mvn: ok".to_string(), "mvn", 137),
            "mvn failed (exit 137, no diagnostic output)"
        );
    }

    #[test]
    fn leaves_real_output_alone() {
        // A failure that did produce diagnostics must pass through untouched -
        // the exit code is not a licence to rewrite the body.
        let body = "[ERROR] /src/A.java:[1,2] cannot find symbol\nBUILD FAILURE (2 s)";
        assert_eq!(reword_if_failed(body.to_string(), "mvn", 1), body);
    }
}
