use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, Row};
use uuid::Uuid;

use nflow_core::spec::SpecQuestion;

use super::Result;

fn parse_datetime(s: &str) -> DateTime<Utc> {
    s.parse::<DateTime<Utc>>().unwrap_or_else(|_| Utc::now())
}

fn parse_uuid(s: &str) -> Uuid {
    Uuid::parse_str(s).unwrap_or_else(|_| Uuid::nil())
}

fn row_to_spec_question(row: &Row<'_>) -> rusqlite::Result<SpecQuestion> {
    let id_str: String = row.get("id")?;
    let spec_id_str: String = row.get("spec_id")?;
    let question: String = row.get("question")?;
    let options: Option<String> = row.get("options")?;
    let answered: bool = row.get("answered")?;
    let answer: Option<String> = row.get("answer")?;
    let created_at_str: String = row.get("created_at")?;
    let answered_at_str: Option<String> = row.get("answered_at")?;

    Ok(SpecQuestion {
        id: parse_uuid(&id_str),
        spec_id: parse_uuid(&spec_id_str),
        question,
        options,
        answered,
        answer,
        created_at: parse_datetime(&created_at_str),
        answered_at: answered_at_str.map(|s| parse_datetime(&s)),
    })
}

pub fn insert_spec_question(conn: &Connection, question: &SpecQuestion) -> Result<()> {
    conn.execute(
        "INSERT INTO spec_questions (id, spec_id, question, options, answered, answer, created_at, answered_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            question.id.to_string(),
            question.spec_id.to_string(),
            question.question,
            question.options,
            question.answered,
            question.answer,
            question.created_at.to_rfc3339(),
            question.answered_at.map(|dt| dt.to_rfc3339()),
        ],
    )?;
    Ok(())
}

pub fn get_pending_questions(
    conn: &Connection,
    spec_id: &Uuid,
) -> Result<Vec<SpecQuestion>> {
    let mut stmt = conn.prepare(
        "SELECT id, spec_id, question, options, answered, answer, created_at, answered_at
         FROM spec_questions WHERE spec_id = ?1 AND answered = 0 ORDER BY created_at",
    )?;
    let rows = stmt.query_map(params![spec_id.to_string()], row_to_spec_question)?;
    let mut questions = Vec::new();
    for row in rows {
        questions.push(row?);
    }
    Ok(questions)
}

pub fn get_all_questions(
    conn: &Connection,
    spec_id: &Uuid,
) -> Result<Vec<SpecQuestion>> {
    let mut stmt = conn.prepare(
        "SELECT id, spec_id, question, options, answered, answer, created_at, answered_at
         FROM spec_questions WHERE spec_id = ?1 ORDER BY created_at",
    )?;
    let rows = stmt.query_map(params![spec_id.to_string()], row_to_spec_question)?;
    let mut questions = Vec::new();
    for row in rows {
        questions.push(row?);
    }
    Ok(questions)
}

pub fn answer_question(
    conn: &Connection,
    id: &Uuid,
    answer: &str,
) -> Result<bool> {
    let rows_affected = conn.execute(
        "UPDATE spec_questions SET answered = 1, answer = ?1, answered_at = ?2 WHERE id = ?3 AND answered = 0",
        params![
            answer,
            Utc::now().to_rfc3339(),
            id.to_string(),
        ],
    )?;
    Ok(rows_affected > 0)
}

pub fn delete_questions_for_spec(conn: &Connection, spec_id: &Uuid) -> Result<()> {
    conn.execute(
        "DELETE FROM spec_questions WHERE spec_id = ?1",
        params![spec_id.to_string()],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{projects::insert_project, specs::insert_spec, test_conn};
    use nflow_core::project::{GitProvider, Project};
    use nflow_core::spec::Spec;

    fn make_project(conn: &Connection) -> Uuid {
        let now = Utc::now();
        let project = Project {
            id: Uuid::new_v4(),
            name: "test-project".to_string(),
            path: "/home/user/test".to_string(),
            base_branch: "main".to_string(),
            git_provider: GitProvider::Github,
            execution_enabled: true,
            created_at: now,
            updated_at: now,
        };
        insert_project(conn, &project).unwrap();
        project.id
    }

    fn make_spec_in_db(conn: &Connection, project_id: Uuid) -> Uuid {
        let spec = Spec::new(project_id, "test-spec".to_string(), "/specs/test.md".to_string());
        insert_spec(conn, &spec).unwrap();
        spec.id
    }

    #[test]
    fn test_insert_and_get_pending() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let spec_id = make_spec_in_db(&conn, pid);

        let q = SpecQuestion::new(spec_id, "What framework?".to_string(), None);
        insert_spec_question(&conn, &q).unwrap();

        let pending = get_pending_questions(&conn, &spec_id).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].question, "What framework?");
        assert!(!pending[0].answered);
    }

    #[test]
    fn test_answer_question() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let spec_id = make_spec_in_db(&conn, pid);

        let q = SpecQuestion::new(spec_id, "What DB?".to_string(), None);
        let qid = q.id;
        insert_spec_question(&conn, &q).unwrap();

        let updated = answer_question(&conn, &qid, "SQLite").unwrap();
        assert!(updated);

        let pending = get_pending_questions(&conn, &spec_id).unwrap();
        assert!(pending.is_empty());

        let all = get_all_questions(&conn, &spec_id).unwrap();
        assert_eq!(all.len(), 1);
        assert!(all[0].answered);
        assert_eq!(all[0].answer.as_deref(), Some("SQLite"));
    }

    #[test]
    fn test_answer_already_answered() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let spec_id = make_spec_in_db(&conn, pid);

        let q = SpecQuestion::new(spec_id, "What DB?".to_string(), None);
        let qid = q.id;
        insert_spec_question(&conn, &q).unwrap();

        answer_question(&conn, &qid, "SQLite").unwrap();
        let updated = answer_question(&conn, &qid, "Postgres").unwrap();
        assert!(!updated);
    }

    #[test]
    fn test_delete_questions_for_spec() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let spec_id = make_spec_in_db(&conn, pid);

        insert_spec_question(&conn, &SpecQuestion::new(spec_id, "Q1".to_string(), None)).unwrap();
        insert_spec_question(&conn, &SpecQuestion::new(spec_id, "Q2".to_string(), None)).unwrap();

        delete_questions_for_spec(&conn, &spec_id).unwrap();

        let all = get_all_questions(&conn, &spec_id).unwrap();
        assert!(all.is_empty());
    }

    #[test]
    fn test_options_roundtrip() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let spec_id = make_spec_in_db(&conn, pid);

        let opts = serde_json::json!([{"label": "CLI"}, {"label": "Web"}]).to_string();
        let q = SpecQuestion::new(spec_id, "App type?".to_string(), Some(opts.clone()));
        insert_spec_question(&conn, &q).unwrap();

        let pending = get_pending_questions(&conn, &spec_id).unwrap();
        assert_eq!(pending[0].options.as_deref(), Some(opts.as_str()));
    }
}
