use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, Row};
use uuid::Uuid;

use nflow_core::pipeline::{
    PipelineMode, PipelineRun, PipelineStage, PipelineStageType, PipelineStatus, StageStatus,
};

use super::Result;

// ─── Status Conversions ──────────────────────────────────

pub fn pipeline_mode_to_str(m: PipelineMode) -> &'static str {
    match m {
        PipelineMode::Manual => "manual",
        PipelineMode::Auto => "auto",
    }
}

pub fn pipeline_mode_from_str(s: &str) -> PipelineMode {
    match s {
        "manual" => PipelineMode::Manual,
        _ => PipelineMode::Auto,
    }
}

fn pipeline_status_to_str(s: PipelineStatus) -> &'static str {
    match s {
        PipelineStatus::Pending => "pending",
        PipelineStatus::Running => "running",
        PipelineStatus::WaitingForApproval => "waiting_for_approval",
        PipelineStatus::WaitingForFinalApproval => "waiting_for_final_approval",
        PipelineStatus::Completed => "completed",
        PipelineStatus::Failed => "failed",
        PipelineStatus::Cancelled => "cancelled",
    }
}

fn pipeline_status_from_str(s: &str) -> PipelineStatus {
    match s {
        "running" => PipelineStatus::Running,
        "waiting_for_approval" => PipelineStatus::WaitingForApproval,
        "waiting_for_final_approval" => PipelineStatus::WaitingForFinalApproval,
        "completed" => PipelineStatus::Completed,
        "failed" => PipelineStatus::Failed,
        "cancelled" => PipelineStatus::Cancelled,
        _ => PipelineStatus::Pending,
    }
}

fn stage_type_to_str(s: PipelineStageType) -> &'static str {
    match s {
        PipelineStageType::Plan => "plan",
        PipelineStageType::Implement => "implement",
        PipelineStageType::Review => "review",
    }
}

fn stage_type_from_str(s: &str) -> PipelineStageType {
    match s {
        "implement" => PipelineStageType::Implement,
        "review" => PipelineStageType::Review,
        _ => PipelineStageType::Plan,
    }
}

fn stage_status_to_str(s: StageStatus) -> &'static str {
    match s {
        StageStatus::Pending => "pending",
        StageStatus::Running => "running",
        StageStatus::Completed => "completed",
        StageStatus::Failed => "failed",
    }
}

fn stage_status_from_str(s: &str) -> StageStatus {
    match s {
        "running" => StageStatus::Running,
        "completed" => StageStatus::Completed,
        "failed" => StageStatus::Failed,
        _ => StageStatus::Pending,
    }
}

fn parse_datetime(s: &str) -> DateTime<Utc> {
    s.parse::<DateTime<Utc>>().unwrap_or_else(|_| Utc::now())
}

fn parse_uuid(s: &str) -> Uuid {
    Uuid::parse_str(s).unwrap_or_else(|_| Uuid::nil())
}

// ─── Row Mappers ─────────────────────────────────────────

fn row_to_pipeline_run(row: &Row<'_>) -> rusqlite::Result<PipelineRun> {
    let id_str: String = row.get("id")?;
    let project_id_str: String = row.get("project_id")?;
    let name: String = row.get("name")?;
    let goal: String = row.get("goal")?;
    let mode_str: String = row.get("mode")?;
    let status_str: String = row.get("status")?;
    let current_stage_str: Option<String> = row.get("current_stage")?;
    let iteration: u32 = row.get("iteration")?;
    let max_iterations: u32 = row.get("max_iterations")?;
    let created_at_str: String = row.get("created_at")?;
    let updated_at_str: String = row.get("updated_at")?;

    Ok(PipelineRun {
        id: parse_uuid(&id_str),
        project_id: parse_uuid(&project_id_str),
        name,
        goal,
        mode: pipeline_mode_from_str(&mode_str),
        status: pipeline_status_from_str(&status_str),
        current_stage: current_stage_str.map(|s| stage_type_from_str(&s)),
        iteration,
        max_iterations,
        created_at: parse_datetime(&created_at_str),
        updated_at: parse_datetime(&updated_at_str),
    })
}

fn row_to_pipeline_stage(row: &Row<'_>) -> rusqlite::Result<PipelineStage> {
    let id_str: String = row.get("id")?;
    let pipeline_run_id_str: String = row.get("pipeline_run_id")?;
    let stage_type_str: String = row.get("stage_type")?;
    let iteration: u32 = row.get("iteration")?;
    let status_str: String = row.get("status")?;
    let input_context: Option<String> = row.get("input_context")?;
    let output_result: Option<String> = row.get("output_result")?;
    let agent_run_id_str: Option<String> = row.get("agent_run_id")?;
    let started_at_str: Option<String> = row.get("started_at")?;
    let finished_at_str: Option<String> = row.get("finished_at")?;
    let created_at_str: String = row.get("created_at")?;

    Ok(PipelineStage {
        id: parse_uuid(&id_str),
        pipeline_run_id: parse_uuid(&pipeline_run_id_str),
        stage_type: stage_type_from_str(&stage_type_str),
        iteration,
        status: stage_status_from_str(&status_str),
        input_context,
        output_result,
        agent_run_id: agent_run_id_str.map(|s| parse_uuid(&s)),
        started_at: started_at_str.map(|s| parse_datetime(&s)),
        finished_at: finished_at_str.map(|s| parse_datetime(&s)),
        created_at: parse_datetime(&created_at_str),
    })
}

// ─── Pipeline Run Operations ─────────────────────────────

pub fn insert_pipeline_run(conn: &Connection, run: &PipelineRun) -> Result<()> {
    conn.execute(
        "INSERT INTO pipeline_runs (id, project_id, name, goal, mode, status, current_stage, iteration, max_iterations, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            run.id.to_string(),
            run.project_id.to_string(),
            run.name,
            run.goal,
            pipeline_mode_to_str(run.mode),
            pipeline_status_to_str(run.status),
            run.current_stage.map(|s| stage_type_to_str(s)),
            run.iteration,
            run.max_iterations,
            run.created_at.to_rfc3339(),
            run.updated_at.to_rfc3339(),
        ],
    )?;
    Ok(())
}

pub fn get_pipeline_run(conn: &Connection, id: &Uuid) -> Result<Option<PipelineRun>> {
    let mut stmt = conn.prepare(
        "SELECT id, project_id, name, goal, mode, status, current_stage, iteration, max_iterations, created_at, updated_at
         FROM pipeline_runs WHERE id = ?1",
    )?;
    let mut rows = stmt.query_map(params![id.to_string()], row_to_pipeline_run)?;
    match rows.next() {
        Some(row) => Ok(Some(row?)),
        None => Ok(None),
    }
}

pub fn list_pipeline_runs(conn: &Connection, project_id: &Uuid) -> Result<Vec<PipelineRun>> {
    let mut stmt = conn.prepare(
        "SELECT id, project_id, name, goal, mode, status, current_stage, iteration, max_iterations, created_at, updated_at
         FROM pipeline_runs WHERE project_id = ?1 ORDER BY created_at DESC",
    )?;
    let rows = stmt.query_map(params![project_id.to_string()], row_to_pipeline_run)?;
    let mut runs = Vec::new();
    for row in rows {
        runs.push(row?);
    }
    Ok(runs)
}

pub fn get_active_pipeline_run(
    conn: &Connection,
    project_id: &Uuid,
) -> Result<Option<PipelineRun>> {
    let mut stmt = conn.prepare(
        "SELECT id, project_id, name, goal, mode, status, current_stage, iteration, max_iterations, created_at, updated_at
         FROM pipeline_runs WHERE project_id = ?1 AND status = 'running' LIMIT 1",
    )?;
    let mut rows = stmt.query_map(params![project_id.to_string()], row_to_pipeline_run)?;
    match rows.next() {
        Some(row) => Ok(Some(row?)),
        None => Ok(None),
    }
}

pub fn update_pipeline_run(
    conn: &Connection,
    id: &Uuid,
    status: &str,
    current_stage: Option<&str>,
    iteration: u32,
) -> Result<()> {
    conn.execute(
        "UPDATE pipeline_runs SET status = ?1, current_stage = ?2, iteration = ?3, updated_at = ?4 WHERE id = ?5",
        params![
            status,
            current_stage,
            iteration,
            Utc::now().to_rfc3339(),
            id.to_string(),
        ],
    )?;
    Ok(())
}

// ─── Pipeline Stage Operations ───────────────────────────

pub fn insert_pipeline_stage(conn: &Connection, stage: &PipelineStage) -> Result<()> {
    conn.execute(
        "INSERT INTO pipeline_stages (id, pipeline_run_id, stage_type, iteration, status, input_context, output_result, agent_run_id, started_at, finished_at, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            stage.id.to_string(),
            stage.pipeline_run_id.to_string(),
            stage_type_to_str(stage.stage_type),
            stage.iteration,
            stage_status_to_str(stage.status),
            stage.input_context,
            stage.output_result,
            stage.agent_run_id.map(|id| id.to_string()),
            stage.started_at.map(|dt| dt.to_rfc3339()),
            stage.finished_at.map(|dt| dt.to_rfc3339()),
            stage.created_at.to_rfc3339(),
        ],
    )?;
    Ok(())
}

pub fn list_pipeline_stages(conn: &Connection, run_id: &Uuid) -> Result<Vec<PipelineStage>> {
    let mut stmt = conn.prepare(
        "SELECT id, pipeline_run_id, stage_type, iteration, status, input_context, output_result, agent_run_id, started_at, finished_at, created_at
         FROM pipeline_stages WHERE pipeline_run_id = ?1 ORDER BY iteration, created_at",
    )?;
    let rows = stmt.query_map(params![run_id.to_string()], row_to_pipeline_stage)?;
    let mut stages = Vec::new();
    for row in rows {
        stages.push(row?);
    }
    Ok(stages)
}

pub fn update_pipeline_stage(
    conn: &Connection,
    id: &Uuid,
    status: &str,
    output_result: Option<&str>,
    agent_run_id: Option<&Uuid>,
    finished_at: Option<DateTime<Utc>>,
) -> Result<()> {
    conn.execute(
        "UPDATE pipeline_stages SET status = ?1, output_result = ?2, agent_run_id = ?3, finished_at = ?4 WHERE id = ?5",
        params![
            status,
            output_result,
            agent_run_id.map(|id| id.to_string()),
            finished_at.map(|dt| dt.to_rfc3339()),
            id.to_string(),
        ],
    )?;
    Ok(())
}

/// Mark a pipeline stage as started (sets status to running and started_at timestamp).
pub fn update_pipeline_stage_started(
    conn: &Connection,
    id: &Uuid,
    agent_run_id: Option<&Uuid>,
) -> Result<()> {
    conn.execute(
        "UPDATE pipeline_stages SET status = 'running', started_at = ?1, agent_run_id = ?2 WHERE id = ?3",
        params![
            Utc::now().to_rfc3339(),
            agent_run_id.map(|id| id.to_string()),
            id.to_string(),
        ],
    )?;
    Ok(())
}

/// Find pipeline stages matching criteria for log streaming.
pub fn find_pipeline_stages(
    conn: &Connection,
    run_id: &Uuid,
    stage_type: Option<&str>,
    iteration: Option<u32>,
) -> Result<Vec<PipelineStage>> {
    let base = "SELECT id, pipeline_run_id, stage_type, iteration, status, input_context, output_result, agent_run_id, started_at, finished_at, created_at FROM pipeline_stages WHERE pipeline_run_id = ?1";

    let query = match (stage_type, iteration) {
        (Some(_), Some(_)) => format!(
            "{} AND stage_type = ?2 AND iteration = ?3 ORDER BY iteration, created_at",
            base
        ),
        (Some(_), None) => format!(
            "{} AND stage_type = ?2 ORDER BY iteration, created_at",
            base
        ),
        (None, Some(_)) => format!("{} AND iteration = ?2 ORDER BY created_at", base),
        (None, None) => format!("{} ORDER BY iteration, created_at", base),
    };

    let mut stmt = conn.prepare(&query)?;
    let rows = match (stage_type, iteration) {
        (Some(st), Some(it)) => {
            stmt.query_map(params![run_id.to_string(), st, it], row_to_pipeline_stage)?
        }
        (Some(st), None) => {
            stmt.query_map(params![run_id.to_string(), st], row_to_pipeline_stage)?
        }
        (None, Some(it)) => {
            stmt.query_map(params![run_id.to_string(), it], row_to_pipeline_stage)?
        }
        (None, None) => stmt.query_map(params![run_id.to_string()], row_to_pipeline_stage)?,
    };

    let mut stages = Vec::new();
    for row in rows {
        stages.push(row?);
    }
    Ok(stages)
}
