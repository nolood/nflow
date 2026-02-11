use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ─── Enums ───────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PipelineStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl fmt::Display for PipelineStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PipelineStatus::Pending => write!(f, "pending"),
            PipelineStatus::Running => write!(f, "running"),
            PipelineStatus::Completed => write!(f, "completed"),
            PipelineStatus::Failed => write!(f, "failed"),
            PipelineStatus::Cancelled => write!(f, "cancelled"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PipelineStageType {
    Plan,
    Implement,
    Review,
}

impl fmt::Display for PipelineStageType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PipelineStageType::Plan => write!(f, "plan"),
            PipelineStageType::Implement => write!(f, "implement"),
            PipelineStageType::Review => write!(f, "review"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StageStatus {
    Pending,
    Running,
    Completed,
    Failed,
}

impl fmt::Display for StageStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StageStatus::Pending => write!(f, "pending"),
            StageStatus::Running => write!(f, "running"),
            StageStatus::Completed => write!(f, "completed"),
            StageStatus::Failed => write!(f, "failed"),
        }
    }
}

// ─── Core Structs ────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineRun {
    pub id: Uuid,
    pub project_id: Uuid,
    pub name: String,
    pub goal: String,
    pub status: PipelineStatus,
    pub current_stage: Option<PipelineStageType>,
    pub iteration: u32,
    pub max_iterations: u32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl PipelineRun {
    pub fn new(project_id: Uuid, name: String, goal: String, max_iterations: u32) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            project_id,
            name,
            goal,
            status: PipelineStatus::Pending,
            current_stage: None,
            iteration: 0,
            max_iterations,
            created_at: now,
            updated_at: now,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineStage {
    pub id: Uuid,
    pub pipeline_run_id: Uuid,
    pub stage_type: PipelineStageType,
    pub iteration: u32,
    pub status: StageStatus,
    pub input_context: Option<String>,
    pub output_result: Option<String>,
    pub agent_run_id: Option<Uuid>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

impl PipelineStage {
    pub fn new(pipeline_run_id: Uuid, stage_type: PipelineStageType, iteration: u32) -> Self {
        Self {
            id: Uuid::new_v4(),
            pipeline_run_id,
            stage_type,
            iteration,
            status: StageStatus::Pending,
            input_context: None,
            output_result: None,
            agent_run_id: None,
            started_at: None,
            finished_at: None,
            created_at: Utc::now(),
        }
    }
}

// ─── Structured Handoff Types ────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageOutput {
    pub success: bool,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<PlanOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub implementation: Option<ImplementOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review: Option<ReviewOutput>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanOutput {
    pub steps: Vec<PlanStep>,
    pub files_to_modify: Vec<String>,
    pub rationale: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanStep {
    pub description: String,
    pub file_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImplementOutput {
    pub files_changed: Vec<String>,
    pub changes_summary: String,
    pub tests_passed: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewOutput {
    pub passed: bool,
    pub issues: Vec<ReviewIssue>,
    pub feedback: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewIssue {
    pub severity: String,
    pub description: String,
    pub file_path: Option<String>,
}

// ─── Context Accumulation ────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineContext {
    pub goal: String,
    pub iteration: u32,
    pub history: Vec<IterationRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IterationRecord {
    pub iteration: u32,
    pub plan: Option<PlanOutput>,
    pub implementation: Option<ImplementOutput>,
    pub review: Option<ReviewOutput>,
}

// ─── State Machine ───────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipelineAction {
    StartStage(PipelineStageType),
    LoopBack { feedback: String },
    Complete,
    MaxIterationsReached,
}

/// Determine the next action after a stage completes.
///
/// State machine:
/// - Plan completed → start Implement
/// - Implement completed → start Review
/// - Review passed → Complete
/// - Review failed + iterations left → LoopBack
/// - Review failed + no iterations → MaxIterationsReached
pub fn next_action(
    run: &PipelineRun,
    completed_stage: PipelineStageType,
    output: &StageOutput,
) -> PipelineAction {
    match completed_stage {
        PipelineStageType::Plan => PipelineAction::StartStage(PipelineStageType::Implement),
        PipelineStageType::Implement => PipelineAction::StartStage(PipelineStageType::Review),
        PipelineStageType::Review => {
            let passed = output
                .review
                .as_ref()
                .map(|r| r.passed)
                .unwrap_or(output.success);

            if passed {
                PipelineAction::Complete
            } else if run.iteration >= run.max_iterations {
                PipelineAction::MaxIterationsReached
            } else {
                let feedback = output
                    .review
                    .as_ref()
                    .map(|r| r.feedback.clone())
                    .unwrap_or_else(|| output.summary.clone());
                PipelineAction::LoopBack { feedback }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_run(iteration: u32, max_iterations: u32) -> PipelineRun {
        let now = Utc::now();
        PipelineRun {
            id: Uuid::new_v4(),
            project_id: Uuid::new_v4(),
            name: "test".to_string(),
            goal: "test goal".to_string(),
            status: PipelineStatus::Running,
            current_stage: Some(PipelineStageType::Plan),
            iteration,
            max_iterations,
            created_at: now,
            updated_at: now,
        }
    }

    fn success_output() -> StageOutput {
        StageOutput {
            success: true,
            summary: "done".to_string(),
            plan: None,
            implementation: None,
            review: None,
        }
    }

    fn review_passed_output() -> StageOutput {
        StageOutput {
            success: true,
            summary: "review passed".to_string(),
            plan: None,
            implementation: None,
            review: Some(ReviewOutput {
                passed: true,
                issues: vec![],
                feedback: "looks good".to_string(),
            }),
        }
    }

    fn review_failed_output() -> StageOutput {
        StageOutput {
            success: true,
            summary: "review failed".to_string(),
            plan: None,
            implementation: None,
            review: Some(ReviewOutput {
                passed: false,
                issues: vec![ReviewIssue {
                    severity: "error".to_string(),
                    description: "tests fail".to_string(),
                    file_path: Some("src/main.rs".to_string()),
                }],
                feedback: "fix the tests".to_string(),
            }),
        }
    }

    // ─── Constructor tests ───────────────────────────────

    #[test]
    fn pipeline_run_new_sets_defaults() {
        let project_id = Uuid::new_v4();
        let run = PipelineRun::new(
            project_id,
            "test-pipeline".to_string(),
            "build feature X".to_string(),
            5,
        );

        assert_eq!(run.project_id, project_id);
        assert_eq!(run.name, "test-pipeline");
        assert_eq!(run.goal, "build feature X");
        assert_eq!(run.status, PipelineStatus::Pending);
        assert_eq!(run.current_stage, None);
        assert_eq!(run.iteration, 0);
        assert_eq!(run.max_iterations, 5);
    }

    #[test]
    fn pipeline_stage_new_sets_defaults() {
        let pipeline_run_id = Uuid::new_v4();
        let stage = PipelineStage::new(pipeline_run_id, PipelineStageType::Plan, 1);

        assert_eq!(stage.pipeline_run_id, pipeline_run_id);
        assert_eq!(stage.stage_type, PipelineStageType::Plan);
        assert_eq!(stage.iteration, 1);
        assert_eq!(stage.status, StageStatus::Pending);
        assert!(stage.input_context.is_none());
        assert!(stage.output_result.is_none());
        assert!(stage.agent_run_id.is_none());
        assert!(stage.started_at.is_none());
        assert!(stage.finished_at.is_none());
    }

    // ─── next_action tests ───────────────────────────────

    #[test]
    fn plan_completed_starts_implement() {
        let run = make_run(1, 5);
        let output = success_output();
        assert_eq!(
            next_action(&run, PipelineStageType::Plan, &output),
            PipelineAction::StartStage(PipelineStageType::Implement)
        );
    }

    #[test]
    fn implement_completed_starts_review() {
        let run = make_run(1, 5);
        let output = success_output();
        assert_eq!(
            next_action(&run, PipelineStageType::Implement, &output),
            PipelineAction::StartStage(PipelineStageType::Review)
        );
    }

    #[test]
    fn review_passed_completes() {
        let run = make_run(1, 5);
        let output = review_passed_output();
        assert_eq!(
            next_action(&run, PipelineStageType::Review, &output),
            PipelineAction::Complete
        );
    }

    #[test]
    fn review_failed_with_iterations_left_loops_back() {
        let run = make_run(1, 5);
        let output = review_failed_output();
        let action = next_action(&run, PipelineStageType::Review, &output);
        match action {
            PipelineAction::LoopBack { feedback } => {
                assert_eq!(feedback, "fix the tests");
            }
            other => panic!("expected LoopBack, got {:?}", other),
        }
    }

    #[test]
    fn review_failed_at_max_iterations() {
        let run = make_run(5, 5);
        let output = review_failed_output();
        assert_eq!(
            next_action(&run, PipelineStageType::Review, &output),
            PipelineAction::MaxIterationsReached
        );
    }

    #[test]
    fn review_without_review_output_uses_success_flag() {
        let run = make_run(1, 5);
        let output = StageOutput {
            success: true,
            summary: "ok".to_string(),
            plan: None,
            implementation: None,
            review: None,
        };
        assert_eq!(
            next_action(&run, PipelineStageType::Review, &output),
            PipelineAction::Complete
        );
    }

    #[test]
    fn review_without_review_output_failure_loops_back() {
        let run = make_run(1, 5);
        let output = StageOutput {
            success: false,
            summary: "something went wrong".to_string(),
            plan: None,
            implementation: None,
            review: None,
        };
        let action = next_action(&run, PipelineStageType::Review, &output);
        match action {
            PipelineAction::LoopBack { feedback } => {
                assert_eq!(feedback, "something went wrong");
            }
            other => panic!("expected LoopBack, got {:?}", other),
        }
    }

    #[test]
    fn review_without_review_output_defaults_to_complete() {
        let run = make_run(1, 5);
        let output = StageOutput {
            success: true,
            summary: "generic success".to_string(),
            plan: None,
            implementation: None,
            review: None,
        };
        assert_eq!(
            next_action(&run, PipelineStageType::Review, &output),
            PipelineAction::Complete
        );
    }

    // ─── Display trait tests ─────────────────────────────

    #[test]
    fn pipeline_status_display() {
        assert_eq!(PipelineStatus::Pending.to_string(), "pending");
        assert_eq!(PipelineStatus::Running.to_string(), "running");
        assert_eq!(PipelineStatus::Completed.to_string(), "completed");
        assert_eq!(PipelineStatus::Failed.to_string(), "failed");
        assert_eq!(PipelineStatus::Cancelled.to_string(), "cancelled");
    }

    #[test]
    fn pipeline_stage_type_display() {
        assert_eq!(PipelineStageType::Plan.to_string(), "plan");
        assert_eq!(PipelineStageType::Implement.to_string(), "implement");
        assert_eq!(PipelineStageType::Review.to_string(), "review");
    }

    #[test]
    fn stage_status_display() {
        assert_eq!(StageStatus::Pending.to_string(), "pending");
        assert_eq!(StageStatus::Running.to_string(), "running");
        assert_eq!(StageStatus::Completed.to_string(), "completed");
        assert_eq!(StageStatus::Failed.to_string(), "failed");
    }

    // ─── Serialization tests ─────────────────────────────

    #[test]
    fn stage_output_serializes() {
        let output = review_passed_output();
        let json = serde_json::to_string(&output).unwrap();
        assert!(json.contains("\"success\":true"));
        assert!(json.contains("\"passed\":true"));

        let parsed: StageOutput = serde_json::from_str(&json).unwrap();
        assert!(parsed.success);
        assert!(parsed.review.unwrap().passed);
    }

    #[test]
    fn pipeline_context_round_trips() {
        let ctx = PipelineContext {
            goal: "add auth".to_string(),
            iteration: 2,
            history: vec![IterationRecord {
                iteration: 1,
                plan: Some(PlanOutput {
                    steps: vec![PlanStep {
                        description: "add login".to_string(),
                        file_path: Some("src/auth.rs".to_string()),
                    }],
                    files_to_modify: vec!["src/auth.rs".to_string()],
                    rationale: "need auth".to_string(),
                }),
                implementation: None,
                review: Some(ReviewOutput {
                    passed: false,
                    issues: vec![],
                    feedback: "missing tests".to_string(),
                }),
            }],
        };
        let json = serde_json::to_string(&ctx).unwrap();
        let parsed: PipelineContext = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.goal, "add auth");
        assert_eq!(parsed.iteration, 2);
        assert_eq!(parsed.history.len(), 1);
        assert_eq!(parsed.history[0].iteration, 1);
    }
}
