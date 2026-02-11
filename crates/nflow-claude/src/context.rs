use std::collections::HashMap;

use nflow_core::project::Project;
use nflow_core::spec::Spec;
use nflow_core::work_item::{TaskKind, WorkItem, WorkItemStatus};

/// Build context variables for the task_execution.md template.
///
/// Variables: project_name, short_id, task_title, task_description,
/// acceptance_criteria, previous_error.
///
/// `completed_tasks` formatted as: `- [{short_id}] {title}: {commit_hash}`
/// `previous_error` is included only on retry (empty string on first attempt).
pub fn build_task_context(
    task: &WorkItem,
    project_name: &str,
    completed_tasks: &[WorkItem],
    previous_error: &str,
) -> HashMap<String, String> {
    let mut vars = HashMap::new();
    vars.insert("project_name".to_string(), project_name.to_string());
    vars.insert("short_id".to_string(), task.short_id.clone());
    vars.insert("task_title".to_string(), task.title.clone());
    vars.insert("task_description".to_string(), task.description.clone());
    vars.insert(
        "acceptance_criteria".to_string(),
        task.acceptance_criteria.clone(),
    );

    let error_section = if previous_error.is_empty() {
        String::new()
    } else {
        format!("## Previous Error\n\nThe previous attempt failed with:\n\n{previous_error}")
    };
    vars.insert("previous_error".to_string(), error_section);

    if !completed_tasks.is_empty() {
        let list = format_completed_tasks(completed_tasks);
        let desc = format!("{}\n\n## Completed Tasks\n\n{list}", task.description);
        vars.insert("task_description".to_string(), desc);
    }

    vars
}

/// Build context variables for the verify_task.md template.
///
/// Variables: project_name, short_id, task_title, task_description, acceptance_criteria.
pub fn build_verify_context(
    verify_task: &WorkItem,
    impl_task: &WorkItem,
    project_name: &str,
) -> HashMap<String, String> {
    let mut vars = HashMap::new();
    vars.insert("project_name".to_string(), project_name.to_string());
    vars.insert("short_id".to_string(), verify_task.short_id.clone());
    vars.insert("task_title".to_string(), impl_task.title.clone());
    vars.insert(
        "task_description".to_string(),
        impl_task.description.clone(),
    );
    vars.insert(
        "acceptance_criteria".to_string(),
        impl_task.acceptance_criteria.clone(),
    );
    vars
}

/// Build context variables for the mr_body.md template.
///
/// Variables: story_id, story_title, story_description, tasks_list, acceptance_criteria.
///
/// `tasks_list` formatted as:
/// - `{commit_hash} {title}` for completed tasks
/// - `[SKIPPED] {title}` for skipped/cancelled tasks
pub fn build_mr_context(
    story: &WorkItem,
    tasks: &[WorkItem],
    _project: &Project,
) -> HashMap<String, String> {
    let mut vars = HashMap::new();
    vars.insert("story_id".to_string(), story.short_id.clone());
    vars.insert("story_title".to_string(), story.title.clone());
    vars.insert("story_description".to_string(), story.description.clone());
    vars.insert("tasks_list".to_string(), format_tasks_list(tasks));
    vars.insert(
        "acceptance_criteria".to_string(),
        story.acceptance_criteria.clone(),
    );
    vars
}

/// Build context variables for the spec_session.md template.
///
/// Variables: project_name, project_path, spec_file_path, additional_context.
pub fn build_spec_context(spec: &Spec, project: &Project) -> HashMap<String, String> {
    let mut vars = HashMap::new();
    vars.insert("project_name".to_string(), project.name.clone());
    vars.insert("project_path".to_string(), project.path.clone());
    vars.insert("spec_file_path".to_string(), spec.file_path.clone());
    vars.insert("additional_context".to_string(), String::new());
    vars
}

/// Build the decomposition prompt by concatenating spec file contents.
///
/// Each spec is formatted as a section with its name and content.
pub fn build_decompose_prompt(specs: &[(Spec, String)]) -> String {
    specs
        .iter()
        .map(|(spec, content)| format!("### {}\n\n{content}", spec.name))
        .collect::<Vec<_>>()
        .join("\n\n---\n\n")
}

/// Format completed tasks as a bulleted list.
///
/// Format: `- [{short_id}] {title}: {commit_hash}`
fn format_completed_tasks(tasks: &[WorkItem]) -> String {
    tasks
        .iter()
        .filter(|t| t.status == WorkItemStatus::Done && t.kind == Some(TaskKind::Impl))
        .map(|t| {
            let hash = t.commit_hash.as_deref().unwrap_or("no-hash");
            format!("- [{}] {}: {hash}", t.short_id, t.title)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Format tasks list for MR body.
///
/// Format: `- {commit_hash} {title}` or `- [SKIPPED] {title}`
fn format_tasks_list(tasks: &[WorkItem]) -> String {
    tasks
        .iter()
        .filter(|t| t.kind == Some(TaskKind::Impl))
        .map(|t| {
            if t.status == WorkItemStatus::Done {
                if let Some(hash) = &t.commit_hash {
                    format!("- {hash} {}", t.title)
                } else {
                    format!("- [SKIPPED] {}", t.title)
                }
            } else {
                format!("- [SKIPPED] {}", t.title)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use nflow_core::project::GitProvider;
    use nflow_core::work_item::ItemType;
    use uuid::Uuid;

    fn make_project() -> Project {
        let now = Utc::now();
        Project {
            id: Uuid::new_v4(),
            name: "test-project".to_string(),
            path: "/home/user/test-project".to_string(),
            base_branch: "main".to_string(),
            git_provider: GitProvider::Github,
            execution_enabled: true,
            created_at: now,
            updated_at: now,
        }
    }

    fn make_spec(project_id: Uuid) -> Spec {
        Spec::new(
            project_id,
            "auth-spec".to_string(),
            "specs/auth.md".to_string(),
        )
    }

    fn make_task(short_id: &str, title: &str, sort_order: i32) -> WorkItem {
        let session_id = Uuid::new_v4();
        let story_id = Uuid::new_v4();
        WorkItem::new_task(
            story_id,
            session_id,
            title.to_string(),
            "Task description".to_string(),
            "Task AC".to_string(),
            short_id.to_string(),
            sort_order,
        )
    }

    fn make_story(short_id: &str) -> WorkItem {
        let session_id = Uuid::new_v4();
        let epic_id = Uuid::new_v4();
        WorkItem::new_story(
            epic_id,
            session_id,
            "Add authentication".to_string(),
            "Implement JWT-based auth".to_string(),
            "- Users can log in\n- Tokens expire".to_string(),
            short_id.to_string(),
            0,
        )
    }

    fn make_done_task(short_id: &str, title: &str, commit_hash: &str) -> WorkItem {
        let mut task = make_task(short_id, title, 0);
        task.task_start().unwrap();
        task.task_complete(Some(commit_hash.to_string())).unwrap();
        task
    }

    // --- build_task_context tests ---

    #[test]
    fn task_context_has_all_required_variables() {
        let task = make_task("T1", "Add login", 0);
        let ctx = build_task_context(&task, "nflow", &[], "");

        assert_eq!(ctx["project_name"], "nflow");
        assert_eq!(ctx["short_id"], "T1");
        assert_eq!(ctx["task_title"], "Add login");
        assert_eq!(ctx["task_description"], "Task description");
        assert_eq!(ctx["acceptance_criteria"], "Task AC");
        assert_eq!(ctx["previous_error"], "");
    }

    #[test]
    fn task_context_previous_error_empty_on_first_attempt() {
        let task = make_task("T1", "Add login", 0);
        let ctx = build_task_context(&task, "nflow", &[], "");

        assert!(ctx["previous_error"].is_empty());
    }

    #[test]
    fn task_context_previous_error_included_on_retry() {
        let task = make_task("T1", "Add login", 0);
        let ctx = build_task_context(&task, "nflow", &[], "compilation error on line 42");

        assert!(ctx["previous_error"].contains("compilation error on line 42"));
        assert!(ctx["previous_error"].contains("Previous Error"));
    }

    #[test]
    fn task_context_completed_tasks_formatted_correctly() {
        let task = make_task("T3", "Add logout", 4);
        let done1 = make_done_task("T1", "Add login", "abc123");
        let done2 = make_done_task("T2", "Add signup", "def456");
        let completed = vec![done1, done2];

        let ctx = build_task_context(&task, "nflow", &completed, "");

        assert!(ctx["task_description"].contains("## Completed Tasks"));
        assert!(ctx["task_description"].contains("- [T1] Add login: abc123"));
        assert!(ctx["task_description"].contains("- [T2] Add signup: def456"));
    }

    #[test]
    fn task_context_no_completed_tasks_no_section() {
        let task = make_task("T1", "Add login", 0);
        let ctx = build_task_context(&task, "nflow", &[], "");

        assert!(!ctx["task_description"].contains("Completed Tasks"));
    }

    // --- build_verify_context tests ---

    #[test]
    fn verify_context_has_all_required_variables() {
        let impl_task = make_task("T1", "Add login", 0);
        let now = Utc::now();
        let verify_task = WorkItem {
            id: Uuid::new_v4(),
            parent_id: impl_task.parent_id,
            decomposition_session_id: impl_task.decomposition_session_id,
            item_type: ItemType::Task,
            kind: Some(TaskKind::Verify),
            title: "Verify: Add login".to_string(),
            description: "Verify that 'Add login' was implemented correctly.".to_string(),
            acceptance_criteria: String::new(),
            status: WorkItemStatus::Pending,
            short_id: "T1v".to_string(),
            sort_order: 1,
            branch_name: None,
            worktree_path: None,
            mr_url: None,
            commit_hash: None,
            error_message: None,
            created_at: now,
            updated_at: now,
        };

        let ctx = build_verify_context(&verify_task, &impl_task, "nflow");

        assert_eq!(ctx["project_name"], "nflow");
        assert_eq!(ctx["short_id"], "T1v");
        assert_eq!(ctx["task_title"], "Add login");
        assert_eq!(ctx["task_description"], "Task description");
        assert_eq!(ctx["acceptance_criteria"], "Task AC");
    }

    #[test]
    fn verify_context_uses_impl_task_details() {
        let impl_task = make_task("T2", "Implement auth middleware", 2);
        let now = Utc::now();
        let verify_task = WorkItem {
            id: Uuid::new_v4(),
            parent_id: impl_task.parent_id,
            decomposition_session_id: impl_task.decomposition_session_id,
            item_type: ItemType::Task,
            kind: Some(TaskKind::Verify),
            title: "Verify: Implement auth middleware".to_string(),
            description: "Verify implementation.".to_string(),
            acceptance_criteria: String::new(),
            status: WorkItemStatus::Pending,
            short_id: "T2v".to_string(),
            sort_order: 3,
            branch_name: None,
            worktree_path: None,
            mr_url: None,
            commit_hash: None,
            error_message: None,
            created_at: now,
            updated_at: now,
        };

        let ctx = build_verify_context(&verify_task, &impl_task, "nflow");

        // Title, description, AC come from impl_task
        assert_eq!(ctx["task_title"], "Implement auth middleware");
        // But short_id comes from verify_task
        assert_eq!(ctx["short_id"], "T2v");
    }

    // --- build_mr_context tests ---

    #[test]
    fn mr_context_has_all_required_variables() {
        let story = make_story("W1-S3");
        let project = make_project();
        let t1 = make_done_task("T1", "Add login", "abc123");
        let t2 = make_done_task("T2", "Add logout", "def456");

        let ctx = build_mr_context(&story, &[t1, t2], &project);

        assert_eq!(ctx["story_id"], "W1-S3");
        assert_eq!(ctx["story_title"], "Add authentication");
        assert_eq!(ctx["story_description"], "Implement JWT-based auth");
        assert!(ctx["tasks_list"].contains("abc123 Add login"));
        assert!(ctx["tasks_list"].contains("def456 Add logout"));
        assert_eq!(
            ctx["acceptance_criteria"],
            "- Users can log in\n- Tokens expire"
        );
    }

    #[test]
    fn mr_context_tasks_list_with_skipped() {
        let story = make_story("S1");
        let project = make_project();
        let t1 = make_done_task("T1", "Add login", "abc123");
        let mut t2 = make_task("T2", "Add logout", 2);
        t2.task_cancel().unwrap();

        let ctx = build_mr_context(&story, &[t1, t2], &project);

        assert!(ctx["tasks_list"].contains("abc123 Add login"));
        assert!(ctx["tasks_list"].contains("[SKIPPED] Add logout"));
    }

    #[test]
    fn mr_context_tasks_list_done_without_commit() {
        let story = make_story("S1");
        let project = make_project();
        let mut t1 = make_task("T1", "Add login", 0);
        t1.task_start().unwrap();
        t1.task_complete(None).unwrap();

        let ctx = build_mr_context(&story, &[t1], &project);

        assert!(ctx["tasks_list"].contains("[SKIPPED] Add login"));
    }

    // --- build_spec_context tests ---

    #[test]
    fn spec_context_has_all_required_variables() {
        let project = make_project();
        let spec = make_spec(project.id);

        let ctx = build_spec_context(&spec, &project);

        assert_eq!(ctx["project_name"], "test-project");
        assert_eq!(ctx["project_path"], "/home/user/test-project");
        assert_eq!(ctx["spec_file_path"], "specs/auth.md");
        assert_eq!(ctx["additional_context"], "");
    }

    // --- build_decompose_prompt tests ---

    #[test]
    fn decompose_prompt_single_spec() {
        let project = make_project();
        let spec = make_spec(project.id);
        let content = "# Auth Spec\n\nUsers should be able to log in.";

        let result = build_decompose_prompt(&[(spec, content.to_string())]);

        assert!(result.contains("### auth-spec"));
        assert!(result.contains("Users should be able to log in."));
    }

    #[test]
    fn decompose_prompt_multiple_specs() {
        let project = make_project();
        let spec1 = Spec::new(project.id, "auth".to_string(), "specs/auth.md".to_string());
        let spec2 = Spec::new(
            project.id,
            "payments".to_string(),
            "specs/payments.md".to_string(),
        );

        let result = build_decompose_prompt(&[
            (spec1, "Auth content".to_string()),
            (spec2, "Payments content".to_string()),
        ]);

        assert!(result.contains("### auth"));
        assert!(result.contains("Auth content"));
        assert!(result.contains("---"));
        assert!(result.contains("### payments"));
        assert!(result.contains("Payments content"));
    }

    #[test]
    fn decompose_prompt_empty_specs() {
        let result = build_decompose_prompt(&[]);
        assert!(result.is_empty());
    }

    // --- format_completed_tasks tests ---

    #[test]
    fn format_completed_tasks_correct_format() {
        let t1 = make_done_task("T1", "Add login", "abc123");
        let t2 = make_done_task("T2", "Add signup", "def456");

        let result = format_completed_tasks(&[t1, t2]);

        assert_eq!(
            result,
            "- [T1] Add login: abc123\n- [T2] Add signup: def456"
        );
    }

    #[test]
    fn format_completed_tasks_skips_non_done() {
        let t1 = make_done_task("T1", "Add login", "abc123");
        let t2 = make_task("T2", "Add signup", 2); // still pending

        let result = format_completed_tasks(&[t1, t2]);

        assert!(result.contains("T1"));
        assert!(!result.contains("T2"));
    }

    #[test]
    fn format_completed_tasks_skips_verify_tasks() {
        let t1 = make_done_task("T1", "Add login", "abc123");
        let mut t2 = make_task("T1v", "Verify: Add login", 1);
        t2.kind = Some(TaskKind::Verify);
        t2.task_start().unwrap();
        t2.task_complete(None).unwrap();

        let result = format_completed_tasks(&[t1, t2]);

        assert!(result.contains("T1"));
        assert!(!result.contains("T1v"));
    }

    #[test]
    fn format_completed_tasks_empty() {
        let result = format_completed_tasks(&[]);
        assert!(result.is_empty());
    }

    #[test]
    fn format_completed_tasks_no_commit_hash() {
        let mut task = make_task("T1", "Add login", 0);
        task.task_start().unwrap();
        task.task_complete(None).unwrap();

        let result = format_completed_tasks(&[task]);

        assert!(result.contains("no-hash"));
    }

    // --- format_tasks_list tests ---

    #[test]
    fn format_tasks_list_with_commits() {
        let t1 = make_done_task("T1", "Add login", "abc123");
        let t2 = make_done_task("T2", "Add logout", "def456");

        let result = format_tasks_list(&[t1, t2]);

        assert_eq!(result, "- abc123 Add login\n- def456 Add logout");
    }

    #[test]
    fn format_tasks_list_with_skipped() {
        let t1 = make_done_task("T1", "Add login", "abc123");
        let mut t2 = make_task("T2", "Add logout", 2);
        t2.task_cancel().unwrap();

        let result = format_tasks_list(&[t1, t2]);

        assert_eq!(result, "- abc123 Add login\n- [SKIPPED] Add logout");
    }

    #[test]
    fn format_tasks_list_filters_verify_tasks() {
        let t1 = make_done_task("T1", "Add login", "abc123");
        let now = Utc::now();
        let verify = WorkItem {
            id: Uuid::new_v4(),
            parent_id: t1.parent_id,
            decomposition_session_id: t1.decomposition_session_id,
            item_type: ItemType::Task,
            kind: Some(TaskKind::Verify),
            title: "Verify: Add login".to_string(),
            description: String::new(),
            acceptance_criteria: String::new(),
            status: WorkItemStatus::Done,
            short_id: "T1v".to_string(),
            sort_order: 1,
            branch_name: None,
            worktree_path: None,
            mr_url: None,
            commit_hash: None,
            error_message: None,
            created_at: now,
            updated_at: now,
        };

        let result = format_tasks_list(&[t1, verify]);

        assert!(result.contains("abc123 Add login"));
        assert!(!result.contains("Verify"));
    }

    #[test]
    fn format_tasks_list_empty() {
        let result = format_tasks_list(&[]);
        assert!(result.is_empty());
    }
}
