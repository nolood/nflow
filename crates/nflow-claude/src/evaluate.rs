/// Result of evaluating a task's stream output and exit status.
#[derive(Debug, Clone, PartialEq)]
pub enum TaskResult {
    Success,
    Failed { reason: String },
}

/// Evaluate whether an impl task succeeded.
///
/// Success requires all three conditions:
/// - `exit_code == 0`
/// - `head_after != head_before` (a new commit was made)
/// - `commit_message` contains `[{short_id}]`
pub fn evaluate_impl_result(
    exit_code: i32,
    head_before: &str,
    head_after: &str,
    commit_message: &str,
    short_id: &str,
) -> TaskResult {
    if exit_code != 0 {
        return TaskResult::Failed {
            reason: format!("non-zero exit code: {exit_code}"),
        };
    }

    if head_after == head_before {
        return TaskResult::Failed {
            reason: "no new commit produced".to_string(),
        };
    }

    let expected_tag = format!("[{short_id}]");
    if !commit_message.contains(&expected_tag) {
        return TaskResult::Failed {
            reason: format!("commit message missing [{short_id}] tag: {commit_message:?}"),
        };
    }

    TaskResult::Success
}

/// Evaluate whether a verify task succeeded.
///
/// `result_text` should be the `result` field from the final `{type: "result"}` stream-json event.
///
/// - Success: `exit_code == 0` AND `result_text` contains "VERIFICATION PASSED"
/// - Failed: `result_text` contains "VERIFICATION FAILED", or neither marker is present (ambiguous = failure)
/// - Failed: `exit_code != 0` regardless of markers
pub fn evaluate_verify_result(exit_code: i32, result_text: &str) -> TaskResult {
    if exit_code != 0 {
        return TaskResult::Failed {
            reason: format!("non-zero exit code: {exit_code}"),
        };
    }

    if result_text.contains("VERIFICATION PASSED") {
        return TaskResult::Success;
    }

    if result_text.contains("VERIFICATION FAILED") {
        return TaskResult::Failed {
            reason: "verification explicitly failed".to_string(),
        };
    }

    TaskResult::Failed {
        reason: "ambiguous result: neither VERIFICATION PASSED nor VERIFICATION FAILED found"
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- evaluate_impl_result tests ---

    #[test]
    fn impl_success() {
        let result =
            evaluate_impl_result(0, "abc123", "def456", "feat: [T1] implement login", "T1");
        assert_eq!(result, TaskResult::Success);
    }

    #[test]
    fn impl_fail_non_zero_exit() {
        let result =
            evaluate_impl_result(1, "abc123", "def456", "feat: [T1] implement login", "T1");
        assert_eq!(
            result,
            TaskResult::Failed {
                reason: "non-zero exit code: 1".to_string()
            }
        );
    }

    #[test]
    fn impl_fail_negative_exit_code() {
        let result =
            evaluate_impl_result(-1, "abc123", "def456", "feat: [T1] implement login", "T1");
        assert_eq!(
            result,
            TaskResult::Failed {
                reason: "non-zero exit code: -1".to_string()
            }
        );
    }

    #[test]
    fn impl_fail_no_new_commit() {
        let result =
            evaluate_impl_result(0, "abc123", "abc123", "feat: [T1] implement login", "T1");
        assert_eq!(
            result,
            TaskResult::Failed {
                reason: "no new commit produced".to_string()
            }
        );
    }

    #[test]
    fn impl_fail_wrong_commit_message() {
        let result = evaluate_impl_result(0, "abc123", "def456", "feat: implement login", "T1");
        match &result {
            TaskResult::Failed { reason } => {
                assert!(reason.contains("[T1]"));
                assert!(reason.contains("commit message missing"));
            }
            _ => panic!("expected Failed, got: {result:?}"),
        }
    }

    #[test]
    fn impl_fail_exit_code_checked_first() {
        // Even if head changed and message is correct, non-zero exit = failure
        let result = evaluate_impl_result(2, "abc123", "def456", "feat: [T1] login", "T1");
        match &result {
            TaskResult::Failed { reason } => {
                assert!(reason.contains("non-zero exit code"));
            }
            _ => panic!("expected Failed, got: {result:?}"),
        }
    }

    #[test]
    fn impl_success_with_wave_prefixed_id() {
        let result = evaluate_impl_result(0, "aaa", "bbb", "feat: [W1-T3] add feature", "W1-T3");
        assert_eq!(result, TaskResult::Success);
    }

    #[test]
    fn impl_success_with_verify_id() {
        let result = evaluate_impl_result(0, "aaa", "bbb", "feat: [T1v] verify login", "T1v");
        assert_eq!(result, TaskResult::Success);
    }

    #[test]
    fn impl_success_tag_anywhere_in_message() {
        let result = evaluate_impl_result(
            0,
            "aaa",
            "bbb",
            "this has [T5] somewhere in the middle of the message",
            "T5",
        );
        assert_eq!(result, TaskResult::Success);
    }

    #[test]
    fn impl_empty_heads_equal() {
        let result = evaluate_impl_result(0, "", "", "feat: [T1] login", "T1");
        assert_eq!(
            result,
            TaskResult::Failed {
                reason: "no new commit produced".to_string()
            }
        );
    }

    // --- evaluate_verify_result tests ---

    #[test]
    fn verify_success() {
        let result = evaluate_verify_result(0, "All checks passed. VERIFICATION PASSED");
        assert_eq!(result, TaskResult::Success);
    }

    #[test]
    fn verify_fail_non_zero_exit() {
        let result = evaluate_verify_result(1, "VERIFICATION PASSED");
        assert_eq!(
            result,
            TaskResult::Failed {
                reason: "non-zero exit code: 1".to_string()
            }
        );
    }

    #[test]
    fn verify_fail_explicit() {
        let result = evaluate_verify_result(0, "Tests failed. VERIFICATION FAILED");
        assert_eq!(
            result,
            TaskResult::Failed {
                reason: "verification explicitly failed".to_string()
            }
        );
    }

    #[test]
    fn verify_fail_ambiguous() {
        let result = evaluate_verify_result(0, "Some output without clear markers");
        assert_eq!(
            result,
            TaskResult::Failed {
                reason:
                    "ambiguous result: neither VERIFICATION PASSED nor VERIFICATION FAILED found"
                        .to_string()
            }
        );
    }

    #[test]
    fn verify_fail_empty_result() {
        let result = evaluate_verify_result(0, "");
        assert_eq!(
            result,
            TaskResult::Failed {
                reason:
                    "ambiguous result: neither VERIFICATION PASSED nor VERIFICATION FAILED found"
                        .to_string()
            }
        );
    }

    #[test]
    fn verify_fail_exit_code_checked_first() {
        // Non-zero exit code should fail even if markers are absent
        let result = evaluate_verify_result(127, "");
        match &result {
            TaskResult::Failed { reason } => {
                assert!(reason.contains("non-zero exit code: 127"));
            }
            _ => panic!("expected Failed, got: {result:?}"),
        }
    }

    #[test]
    fn verify_passed_marker_case_sensitive() {
        // "verification passed" (lowercase) should NOT match
        let result = evaluate_verify_result(0, "verification passed");
        assert_eq!(
            result,
            TaskResult::Failed {
                reason:
                    "ambiguous result: neither VERIFICATION PASSED nor VERIFICATION FAILED found"
                        .to_string()
            }
        );
    }

    #[test]
    fn verify_both_markers_present_passed_wins() {
        // If both markers are present, PASSED is checked first
        let result = evaluate_verify_result(0, "VERIFICATION PASSED but also VERIFICATION FAILED");
        assert_eq!(result, TaskResult::Success);
    }

    #[test]
    fn task_result_clone_and_debug() {
        let result = TaskResult::Success;
        let cloned = result.clone();
        assert_eq!(result, cloned);
        let debug = format!("{result:?}");
        assert!(debug.contains("Success"));
    }

    #[test]
    fn task_result_failed_debug() {
        let result = TaskResult::Failed {
            reason: "test reason".to_string(),
        };
        let debug = format!("{result:?}");
        assert!(debug.contains("Failed"));
        assert!(debug.contains("test reason"));
    }
}
