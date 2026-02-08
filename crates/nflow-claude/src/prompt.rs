use std::collections::HashMap;
use std::path::Path;

use crate::error::ClaudeError;

/// Embedded prompt templates (compiled into the binary).
const EMBEDDED_SPEC_SESSION: &str = include_str!("../../../prompts/spec_session.md");
const EMBEDDED_DECOMPOSE: &str = include_str!("../../../prompts/decompose.md");
const EMBEDDED_TASK_EXECUTION: &str = include_str!("../../../prompts/task_execution.md");
const EMBEDDED_VERIFY_TASK: &str = include_str!("../../../prompts/verify_task.md");
const EMBEDDED_MR_BODY: &str = include_str!("../../../prompts/mr_body.md");

/// Known template names.
const KNOWN_TEMPLATES: &[(&str, &str)] = &[
    ("spec_session", EMBEDDED_SPEC_SESSION),
    ("decompose", EMBEDDED_DECOMPOSE),
    ("task_execution", EMBEDDED_TASK_EXECUTION),
    ("verify_task", EMBEDDED_VERIFY_TASK),
    ("mr_body", EMBEDDED_MR_BODY),
];

/// Get the embedded template content for a given template name.
fn get_embedded(name: &str) -> Option<&'static str> {
    KNOWN_TEMPLATES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, content)| *content)
}

/// Load a prompt template by name.
///
/// Checks the override directory first (if provided), falling back to
/// the embedded template compiled into the binary.
///
/// Override files should be named `{name}.md` in the override directory.
///
/// Returns an error if the template name is unknown and no override file exists.
pub fn load_template(name: &str, override_dir: Option<&Path>) -> Result<String, ClaudeError> {
    // Check override directory first
    if let Some(dir) = override_dir {
        let override_path = dir.join(format!("{name}.md"));
        if override_path.is_file() {
            return std::fs::read_to_string(&override_path).map_err(ClaudeError::Io);
        }
    }

    // Fall back to embedded template
    get_embedded(name)
        .map(|s| s.to_string())
        .ok_or_else(|| ClaudeError::TemplateNotFound {
            name: name.to_string(),
        })
}

/// Render a prompt template by substituting `{variable}` placeholders.
///
/// Variables are identified by the pattern `{identifier}` where identifier
/// consists of lowercase letters, digits, and underscores.
///
/// - Known variables (present in `vars`) are replaced with their values.
/// - Unknown `{identifier}` patterns that look like variables but are not in
///   `vars` cause an error (missing required variable).
/// - Braces containing content that doesn't match the variable pattern
///   (e.g., spaces, uppercase, special chars) are left as-is.
pub fn render_template(template: &str, vars: &HashMap<&str, &str>) -> Result<String, ClaudeError> {
    let mut result = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '{' {
            // Try to parse a variable name: [a-z0-9_]+
            let mut var_name = String::new();
            let mut found_close = false;

            // Peek ahead to collect potential variable name
            let mut lookahead: Vec<char> = Vec::new();
            while let Some(&next) = chars.peek() {
                if next == '}' {
                    found_close = true;
                    chars.next(); // consume '}'
                    break;
                }
                if next.is_ascii_lowercase() || next.is_ascii_digit() || next == '_' {
                    var_name.push(next);
                    lookahead.push(next);
                    chars.next();
                } else {
                    // Not a valid variable character — not a variable reference
                    break;
                }
            }

            if found_close && !var_name.is_empty() {
                // This is a valid {variable} pattern
                if let Some(value) = vars.get(var_name.as_str()) {
                    result.push_str(value);
                } else {
                    // Unresolved variable — error
                    return Err(ClaudeError::UnresolvedVariable { variable: var_name });
                }
            } else {
                // Not a variable reference — output the original characters
                result.push('{');
                for c in lookahead {
                    result.push(c);
                }
                if found_close {
                    result.push('}');
                }
            }
        } else {
            result.push(ch);
        }
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // --- Embedded template tests ---

    #[test]
    fn embedded_spec_session_template_exists() {
        assert!(!EMBEDDED_SPEC_SESSION.is_empty());
        assert!(EMBEDDED_SPEC_SESSION.contains("{project_name}"));
    }

    #[test]
    fn embedded_decompose_template_exists() {
        assert!(!EMBEDDED_DECOMPOSE.is_empty());
        assert!(EMBEDDED_DECOMPOSE.contains("{project_name}"));
        assert!(EMBEDDED_DECOMPOSE.contains("{specs_content}"));
    }

    #[test]
    fn embedded_task_execution_template_exists() {
        assert!(!EMBEDDED_TASK_EXECUTION.is_empty());
        assert!(EMBEDDED_TASK_EXECUTION.contains("{short_id}"));
        assert!(EMBEDDED_TASK_EXECUTION.contains("{task_title}"));
    }

    #[test]
    fn embedded_verify_task_template_exists() {
        assert!(!EMBEDDED_VERIFY_TASK.is_empty());
        assert!(EMBEDDED_VERIFY_TASK.contains("{short_id}"));
        assert!(EMBEDDED_VERIFY_TASK.contains("VERIFICATION PASSED"));
    }

    #[test]
    fn embedded_mr_body_template_exists() {
        assert!(!EMBEDDED_MR_BODY.is_empty());
        assert!(EMBEDDED_MR_BODY.contains("{story_id}"));
        assert!(EMBEDDED_MR_BODY.contains("{tasks_list}"));
    }

    // --- load_template tests ---

    #[test]
    fn load_template_returns_embedded_when_no_override() {
        let content = load_template("spec_session", None).unwrap();
        assert_eq!(content, EMBEDDED_SPEC_SESSION);
    }

    #[test]
    fn load_template_returns_embedded_for_all_known_names() {
        for (name, _) in KNOWN_TEMPLATES {
            let content = load_template(name, None).unwrap();
            assert!(!content.is_empty());
        }
    }

    #[test]
    fn load_template_errors_for_unknown_name() {
        let err = load_template("nonexistent", None).unwrap_err();
        assert!(matches!(err, ClaudeError::TemplateNotFound { .. }));
    }

    #[test]
    fn load_template_uses_override_when_file_exists() {
        let dir = TempDir::new().unwrap();
        let override_content = "Custom spec template for {project_name}";
        fs::write(dir.path().join("spec_session.md"), override_content).unwrap();

        let content = load_template("spec_session", Some(dir.path())).unwrap();
        assert_eq!(content, override_content);
    }

    #[test]
    fn load_template_falls_back_to_embedded_when_override_missing() {
        let dir = TempDir::new().unwrap();
        // No override file created

        let content = load_template("spec_session", Some(dir.path())).unwrap();
        assert_eq!(content, EMBEDDED_SPEC_SESSION);
    }

    #[test]
    fn load_template_override_dir_with_unknown_name_and_file() {
        let dir = TempDir::new().unwrap();
        let override_content = "Custom unknown template";
        fs::write(dir.path().join("custom_prompt.md"), override_content).unwrap();

        let content = load_template("custom_prompt", Some(dir.path())).unwrap();
        assert_eq!(content, override_content);
    }

    #[test]
    fn load_template_unknown_name_no_override_errors() {
        let dir = TempDir::new().unwrap();
        let err = load_template("custom_prompt", Some(dir.path())).unwrap_err();
        assert!(matches!(err, ClaudeError::TemplateNotFound { .. }));
    }

    // --- render_template tests ---

    #[test]
    fn render_template_substitutes_single_variable() {
        let mut vars = HashMap::new();
        vars.insert("name", "Alice");
        let result = render_template("Hello {name}!", &vars).unwrap();
        assert_eq!(result, "Hello Alice!");
    }

    #[test]
    fn render_template_substitutes_multiple_variables() {
        let mut vars = HashMap::new();
        vars.insert("project_name", "nflow");
        vars.insert("project_path", "/home/user/nflow");
        let result = render_template("Project: {project_name} at {project_path}", &vars).unwrap();
        assert_eq!(result, "Project: nflow at /home/user/nflow");
    }

    #[test]
    fn render_template_substitutes_repeated_variable() {
        let mut vars = HashMap::new();
        vars.insert("id", "S1");
        let result = render_template("[{id}] title [{id}]", &vars).unwrap();
        assert_eq!(result, "[S1] title [S1]");
    }

    #[test]
    fn render_template_errors_on_unresolved_variable() {
        let vars = HashMap::new();
        let err = render_template("Hello {name}!", &vars).unwrap_err();
        match err {
            ClaudeError::UnresolvedVariable { variable } => {
                assert_eq!(variable, "name");
            }
            _ => panic!("expected UnresolvedVariable error"),
        }
    }

    #[test]
    fn render_template_errors_on_partially_unresolved() {
        let mut vars = HashMap::new();
        vars.insert("known", "value");
        let err = render_template("{known} and {unknown}", &vars).unwrap_err();
        match err {
            ClaudeError::UnresolvedVariable { variable } => {
                assert_eq!(variable, "unknown");
            }
            _ => panic!("expected UnresolvedVariable error"),
        }
    }

    #[test]
    fn render_template_ignores_non_variable_braces() {
        let mut vars = HashMap::new();
        vars.insert("name", "test");
        // Uppercase, spaces, special chars in braces should be left as-is
        let result = render_template("{name} {Not a var} {WITH SPACES}", &vars).unwrap();
        assert_eq!(result, "test {Not a var} {WITH SPACES}");
    }

    #[test]
    fn render_template_ignores_empty_braces() {
        let vars = HashMap::new();
        let result = render_template("empty {} braces", &vars).unwrap();
        assert_eq!(result, "empty {} braces");
    }

    #[test]
    fn render_template_ignores_unclosed_brace() {
        let vars = HashMap::new();
        let result = render_template("unclosed { brace", &vars).unwrap();
        assert_eq!(result, "unclosed { brace");
    }

    #[test]
    fn render_template_handles_adjacent_braces() {
        let mut vars = HashMap::new();
        vars.insert("a", "1");
        vars.insert("b", "2");
        let result = render_template("{a}{b}", &vars).unwrap();
        assert_eq!(result, "12");
    }

    #[test]
    fn render_template_no_variables_in_template() {
        let vars = HashMap::new();
        let result = render_template("No variables here.", &vars).unwrap();
        assert_eq!(result, "No variables here.");
    }

    #[test]
    fn render_template_empty_template() {
        let vars = HashMap::new();
        let result = render_template("", &vars).unwrap();
        assert_eq!(result, "");
    }

    #[test]
    fn render_template_variable_with_underscores_and_digits() {
        let mut vars = HashMap::new();
        vars.insert("short_id", "W1-T3");
        vars.insert("task_title", "Add auth");
        vars.insert("var2", "two");
        let result = render_template("{short_id}: {task_title} ({var2})", &vars).unwrap();
        assert_eq!(result, "W1-T3: Add auth (two)");
    }

    #[test]
    fn render_template_extra_variables_ignored() {
        let mut vars = HashMap::new();
        vars.insert("name", "Alice");
        vars.insert("unused_var", "this should not matter");
        vars.insert("another_extra", "also irrelevant");
        let result = render_template("Hello {name}!", &vars).unwrap();
        assert_eq!(result, "Hello Alice!");
    }

    #[test]
    fn render_template_variable_value_with_braces() {
        let mut vars = HashMap::new();
        vars.insert("code", "fn main() { println!(\"hello\"); }");
        let result = render_template("Code: {code}", &vars).unwrap();
        assert_eq!(result, "Code: fn main() { println!(\"hello\"); }");
    }

    #[test]
    fn render_template_multiline() {
        let mut vars = HashMap::new();
        vars.insert("title", "My Task");
        vars.insert("desc", "Do the thing\nand the other thing");
        let template = "# {title}\n\n{desc}\n";
        let result = render_template(template, &vars).unwrap();
        assert_eq!(result, "# My Task\n\nDo the thing\nand the other thing\n");
    }

    #[test]
    fn render_full_spec_session_template() {
        let mut vars = HashMap::new();
        vars.insert("project_name", "nflow");
        vars.insert("project_path", "/home/user/nflow");
        vars.insert("additional_context", "");
        let result = render_template(EMBEDDED_SPEC_SESSION, &vars).unwrap();
        assert!(result.contains("nflow"));
        assert!(result.contains("/home/user/nflow"));
        assert!(!result.contains("{project_name}"));
    }

    #[test]
    fn render_full_mr_body_template() {
        let mut vars = HashMap::new();
        vars.insert("story_id", "W1-S3");
        vars.insert("story_title", "Add authentication");
        vars.insert("story_description", "Implement JWT-based auth");
        vars.insert("tasks_list", "- abc123 Add login\n- [SKIPPED] Add logout");
        vars.insert("acceptance_criteria", "- Users can log in\n- Tokens expire");
        let result = render_template(EMBEDDED_MR_BODY, &vars).unwrap();
        assert!(result.contains("W1-S3"));
        assert!(result.contains("Add authentication"));
        assert!(result.contains("abc123 Add login"));
    }
}
