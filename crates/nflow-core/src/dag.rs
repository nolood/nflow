use std::collections::{HashMap, HashSet, VecDeque};

use uuid::Uuid;

use crate::error::{NflowError, Result};
use crate::work_item::{Dependency, ItemType, WorkItem, WorkItemStatus};

/// Directed Acyclic Graph of story dependencies.
///
/// Stores adjacency lists for forward edges (blockers → blocked) and reverse edges (blocked → blockers).
/// All nodes are story UUIDs within a single decomposition session.
#[derive(Debug, Clone)]
pub struct Dag {
    /// story_id → set of stories it blocks (forward edges)
    pub forward: HashMap<Uuid, HashSet<Uuid>>,
    /// story_id → set of stories that block it (reverse edges)
    pub reverse: HashMap<Uuid, HashSet<Uuid>>,
    /// All story IDs in the DAG
    pub nodes: HashSet<Uuid>,
}

/// Build a DAG from stories and dependencies.
///
/// Validates:
/// - All dependency references (blocker_id, blocked_id) exist in the story set
/// - All stories belong to the same decomposition session
/// - No circular dependencies
pub fn build_dag(stories: &[WorkItem], dependencies: &[Dependency]) -> Result<Dag> {
    // Collect story IDs and validate they are all stories in the same session
    let mut nodes = HashSet::new();
    let mut session_id: Option<Uuid> = None;

    for story in stories {
        if story.item_type != ItemType::Story {
            continue;
        }
        // Validate same decomposition session
        match session_id {
            None => session_id = Some(story.decomposition_session_id),
            Some(sid) => {
                if story.decomposition_session_id != sid {
                    return Err(NflowError::ValidationError(
                        "all stories must belong to the same decomposition session".into(),
                    ));
                }
            }
        }
        nodes.insert(story.id);
    }

    // Build adjacency lists
    let mut forward: HashMap<Uuid, HashSet<Uuid>> = HashMap::new();
    let mut reverse: HashMap<Uuid, HashSet<Uuid>> = HashMap::new();

    // Initialize all nodes
    for &node in &nodes {
        forward.entry(node).or_default();
        reverse.entry(node).or_default();
    }

    for dep in dependencies {
        // Validate references exist
        if !nodes.contains(&dep.blocker_id) {
            return Err(NflowError::ValidationError(format!(
                "dependency blocker {} not found in story set",
                dep.blocker_id
            )));
        }
        if !nodes.contains(&dep.blocked_id) {
            return Err(NflowError::ValidationError(format!(
                "dependency blocked {} not found in story set",
                dep.blocked_id
            )));
        }

        forward
            .entry(dep.blocker_id)
            .or_default()
            .insert(dep.blocked_id);
        reverse
            .entry(dep.blocked_id)
            .or_default()
            .insert(dep.blocker_id);
    }

    let dag = Dag {
        forward,
        reverse,
        nodes,
    };

    // Check for cycles
    detect_cycle(&dag)?;

    Ok(dag)
}

/// Topological sort using Kahn's algorithm.
/// Returns stories in valid execution order (dependencies before dependents).
pub fn topological_sort(dag: &Dag) -> Result<Vec<Uuid>> {
    let mut in_degree: HashMap<Uuid, usize> = HashMap::new();
    for &node in &dag.nodes {
        let blockers = dag.reverse.get(&node).map_or(0, |s| s.len());
        in_degree.insert(node, blockers);
    }

    let mut queue: VecDeque<Uuid> = in_degree
        .iter()
        .filter(|(_, &deg)| deg == 0)
        .map(|(&id, _)| id)
        .collect();

    // Sort queue for deterministic output
    let mut sorted_start: Vec<Uuid> = queue.drain(..).collect();
    sorted_start.sort();
    queue.extend(sorted_start);

    let mut result = Vec::with_capacity(dag.nodes.len());

    while let Some(node) = queue.pop_front() {
        result.push(node);
        if let Some(blocked) = dag.forward.get(&node) {
            let mut next: Vec<Uuid> = Vec::new();
            for &b in blocked {
                if let Some(deg) = in_degree.get_mut(&b) {
                    *deg -= 1;
                    if *deg == 0 {
                        next.push(b);
                    }
                }
            }
            next.sort();
            queue.extend(next);
        }
    }

    if result.len() != dag.nodes.len() {
        // Cycle detected — find it
        let remaining: Vec<Uuid> = dag
            .nodes
            .iter()
            .filter(|id| !result.contains(id))
            .copied()
            .collect();
        return Err(NflowError::CyclicDependency { cycle: remaining });
    }

    Ok(result)
}

/// Detect cycles in the DAG using DFS-based cycle detection.
/// Returns the cycle path if found.
fn detect_cycle(dag: &Dag) -> Result<()> {
    #[derive(Clone, Copy, PartialEq)]
    enum Color {
        White,
        Gray,
        Black,
    }

    let mut color: HashMap<Uuid, Color> = dag.nodes.iter().map(|&id| (id, Color::White)).collect();
    let mut parent: HashMap<Uuid, Uuid> = HashMap::new();

    fn dfs(
        node: Uuid,
        dag: &Dag,
        color: &mut HashMap<Uuid, Color>,
        parent: &mut HashMap<Uuid, Uuid>,
    ) -> Option<Vec<Uuid>> {
        color.insert(node, Color::Gray);

        if let Some(neighbors) = dag.forward.get(&node) {
            let mut sorted_neighbors: Vec<Uuid> = neighbors.iter().copied().collect();
            sorted_neighbors.sort();
            for next in sorted_neighbors {
                match color.get(&next) {
                    Some(Color::Gray) => {
                        // Found cycle — reconstruct
                        let mut cycle = vec![next, node];
                        let mut cur = node;
                        while let Some(&p) = parent.get(&cur) {
                            if p == next {
                                break;
                            }
                            cycle.push(p);
                            cur = p;
                        }
                        cycle.reverse();
                        return Some(cycle);
                    }
                    Some(Color::White) => {
                        parent.insert(next, node);
                        if let Some(cycle) = dfs(next, dag, color, parent) {
                            return Some(cycle);
                        }
                    }
                    _ => {}
                }
            }
        }

        color.insert(node, Color::Black);
        None
    }

    // Sort nodes for deterministic traversal
    let mut sorted_nodes: Vec<Uuid> = dag.nodes.iter().copied().collect();
    sorted_nodes.sort();

    for node in sorted_nodes {
        if color.get(&node) == Some(&Color::White) {
            if let Some(cycle) = dfs(node, dag, &mut color, &mut parent) {
                return Err(NflowError::CyclicDependency { cycle });
            }
        }
    }

    Ok(())
}

/// Find stories that are ready to execute: all blockers are in done or cancelled state.
///
/// Cancelled stories do not block dependents — they are treated the same as done for unblocking.
pub fn find_ready_stories(dag: &Dag, statuses: &HashMap<Uuid, WorkItemStatus>) -> Vec<Uuid> {
    let mut ready = Vec::new();
    for &node in &dag.nodes {
        let blockers = match dag.reverse.get(&node) {
            Some(b) => b,
            None => {
                ready.push(node);
                continue;
            }
        };

        if blockers.is_empty() {
            ready.push(node);
            continue;
        }

        let all_blockers_resolved = blockers.iter().all(|blocker_id| {
            matches!(
                statuses.get(blocker_id),
                Some(WorkItemStatus::Done) | Some(WorkItemStatus::Cancelled)
            )
        });

        if all_blockers_resolved {
            ready.push(node);
        }
    }
    ready.sort();
    ready
}

/// Find stories that are blocked: at least one blocker is not in done or cancelled state.
pub fn find_blocked_stories(dag: &Dag, statuses: &HashMap<Uuid, WorkItemStatus>) -> Vec<Uuid> {
    let mut blocked = Vec::new();
    for &node in &dag.nodes {
        let blockers = match dag.reverse.get(&node) {
            Some(b) if !b.is_empty() => b,
            _ => continue,
        };

        let any_blocker_incomplete = blockers.iter().any(|blocker_id| {
            !matches!(
                statuses.get(blocker_id),
                Some(WorkItemStatus::Done) | Some(WorkItemStatus::Cancelled)
            )
        });

        if any_blocker_incomplete {
            blocked.push(node);
        }
    }
    blocked.sort();
    blocked
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work_item::WorkItem;

    fn make_story(session_id: Uuid, short_id: &str) -> WorkItem {
        WorkItem::new_story(
            Uuid::new_v4(), // epic_id (parent)
            session_id,
            format!("Story {short_id}"),
            "Desc".into(),
            "AC".into(),
            short_id.into(),
            0,
        )
    }

    // --- build_dag: basic construction ---

    #[test]
    fn build_dag_no_dependencies() {
        let session_id = Uuid::new_v4();
        let s1 = make_story(session_id, "S1");
        let s2 = make_story(session_id, "S2");

        let dag = build_dag(&[s1.clone(), s2.clone()], &[]).unwrap();

        assert_eq!(dag.nodes.len(), 2);
        assert!(dag.nodes.contains(&s1.id));
        assert!(dag.nodes.contains(&s2.id));
        assert!(dag.forward[&s1.id].is_empty());
        assert!(dag.forward[&s2.id].is_empty());
    }

    #[test]
    fn build_dag_with_single_dependency() {
        let session_id = Uuid::new_v4();
        let s1 = make_story(session_id, "S1");
        let s2 = make_story(session_id, "S2");
        let dep = Dependency::new(s1.id, s2.id);

        let dag = build_dag(&[s1.clone(), s2.clone()], &[dep]).unwrap();

        assert!(dag.forward[&s1.id].contains(&s2.id));
        assert!(dag.reverse[&s2.id].contains(&s1.id));
        assert!(dag.forward[&s2.id].is_empty());
        assert!(dag.reverse[&s1.id].is_empty());
    }

    #[test]
    fn build_dag_with_chain() {
        let session_id = Uuid::new_v4();
        let s1 = make_story(session_id, "S1");
        let s2 = make_story(session_id, "S2");
        let s3 = make_story(session_id, "S3");

        let deps = vec![Dependency::new(s1.id, s2.id), Dependency::new(s2.id, s3.id)];

        let dag = build_dag(&[s1.clone(), s2.clone(), s3.clone()], &deps).unwrap();

        assert!(dag.forward[&s1.id].contains(&s2.id));
        assert!(dag.forward[&s2.id].contains(&s3.id));
        assert!(dag.reverse[&s3.id].contains(&s2.id));
        assert!(dag.reverse[&s2.id].contains(&s1.id));
    }

    #[test]
    fn build_dag_with_diamond() {
        let session_id = Uuid::new_v4();
        let s1 = make_story(session_id, "S1");
        let s2 = make_story(session_id, "S2");
        let s3 = make_story(session_id, "S3");
        let s4 = make_story(session_id, "S4");

        // S1 -> S2, S1 -> S3, S2 -> S4, S3 -> S4
        let deps = vec![
            Dependency::new(s1.id, s2.id),
            Dependency::new(s1.id, s3.id),
            Dependency::new(s2.id, s4.id),
            Dependency::new(s3.id, s4.id),
        ];

        let dag = build_dag(&[s1.clone(), s2.clone(), s3.clone(), s4.clone()], &deps).unwrap();

        assert!(dag.forward[&s1.id].contains(&s2.id));
        assert!(dag.forward[&s1.id].contains(&s3.id));
        assert!(dag.forward[&s2.id].contains(&s4.id));
        assert!(dag.forward[&s3.id].contains(&s4.id));
        assert_eq!(dag.reverse[&s4.id].len(), 2);
    }

    #[test]
    fn build_dag_empty() {
        let dag = build_dag(&[], &[]).unwrap();
        assert!(dag.nodes.is_empty());
    }

    // --- build_dag: validation errors ---

    #[test]
    fn build_dag_invalid_blocker_reference() {
        let session_id = Uuid::new_v4();
        let s1 = make_story(session_id, "S1");
        let phantom_id = Uuid::new_v4();
        let dep = Dependency::new(phantom_id, s1.id);

        let err = build_dag(&[s1], &[dep]).unwrap_err();
        assert!(matches!(err, NflowError::ValidationError(_)));
    }

    #[test]
    fn build_dag_invalid_blocked_reference() {
        let session_id = Uuid::new_v4();
        let s1 = make_story(session_id, "S1");
        let phantom_id = Uuid::new_v4();
        let dep = Dependency::new(s1.id, phantom_id);

        let err = build_dag(&[s1], &[dep]).unwrap_err();
        assert!(matches!(err, NflowError::ValidationError(_)));
    }

    #[test]
    fn build_dag_cross_session_stories_rejected() {
        let session1 = Uuid::new_v4();
        let session2 = Uuid::new_v4();
        let s1 = make_story(session1, "S1");
        let s2 = make_story(session2, "S2");

        let err = build_dag(&[s1, s2], &[]).unwrap_err();
        assert!(matches!(err, NflowError::ValidationError(_)));
    }

    // --- build_dag: cycle detection ---

    #[test]
    fn build_dag_detects_direct_cycle() {
        let session_id = Uuid::new_v4();
        let s1 = make_story(session_id, "S1");
        let s2 = make_story(session_id, "S2");

        let deps = vec![Dependency::new(s1.id, s2.id), Dependency::new(s2.id, s1.id)];

        let err = build_dag(&[s1, s2], &deps).unwrap_err();
        assert!(matches!(err, NflowError::CyclicDependency { .. }));
    }

    #[test]
    fn build_dag_detects_indirect_cycle() {
        let session_id = Uuid::new_v4();
        let s1 = make_story(session_id, "S1");
        let s2 = make_story(session_id, "S2");
        let s3 = make_story(session_id, "S3");

        // S1 -> S2 -> S3 -> S1
        let deps = vec![
            Dependency::new(s1.id, s2.id),
            Dependency::new(s2.id, s3.id),
            Dependency::new(s3.id, s1.id),
        ];

        let err = build_dag(&[s1, s2, s3], &deps).unwrap_err();
        match err {
            NflowError::CyclicDependency { cycle } => {
                assert!(cycle.len() >= 2, "cycle should contain at least 2 nodes");
            }
            _ => panic!("expected CyclicDependency error"),
        }
    }

    #[test]
    fn build_dag_self_loop_detected() {
        let session_id = Uuid::new_v4();
        let s1 = make_story(session_id, "S1");

        let deps = vec![Dependency::new(s1.id, s1.id)];

        let err = build_dag(&[s1], &deps).unwrap_err();
        assert!(matches!(err, NflowError::CyclicDependency { .. }));
    }

    // --- topological_sort ---

    #[test]
    fn topological_sort_linear_chain() {
        let session_id = Uuid::new_v4();
        let s1 = make_story(session_id, "S1");
        let s2 = make_story(session_id, "S2");
        let s3 = make_story(session_id, "S3");

        let deps = vec![Dependency::new(s1.id, s2.id), Dependency::new(s2.id, s3.id)];

        let dag = build_dag(&[s1.clone(), s2.clone(), s3.clone()], &deps).unwrap();
        let order = topological_sort(&dag).unwrap();

        assert_eq!(order.len(), 3);
        // s1 must come before s2, s2 before s3
        let pos1 = order.iter().position(|&id| id == s1.id).unwrap();
        let pos2 = order.iter().position(|&id| id == s2.id).unwrap();
        let pos3 = order.iter().position(|&id| id == s3.id).unwrap();
        assert!(pos1 < pos2);
        assert!(pos2 < pos3);
    }

    #[test]
    fn topological_sort_diamond() {
        let session_id = Uuid::new_v4();
        let s1 = make_story(session_id, "S1");
        let s2 = make_story(session_id, "S2");
        let s3 = make_story(session_id, "S3");
        let s4 = make_story(session_id, "S4");

        let deps = vec![
            Dependency::new(s1.id, s2.id),
            Dependency::new(s1.id, s3.id),
            Dependency::new(s2.id, s4.id),
            Dependency::new(s3.id, s4.id),
        ];

        let dag = build_dag(&[s1.clone(), s2.clone(), s3.clone(), s4.clone()], &deps).unwrap();
        let order = topological_sort(&dag).unwrap();

        assert_eq!(order.len(), 4);
        let pos1 = order.iter().position(|&id| id == s1.id).unwrap();
        let pos2 = order.iter().position(|&id| id == s2.id).unwrap();
        let pos3 = order.iter().position(|&id| id == s3.id).unwrap();
        let pos4 = order.iter().position(|&id| id == s4.id).unwrap();
        assert!(pos1 < pos2);
        assert!(pos1 < pos3);
        assert!(pos2 < pos4);
        assert!(pos3 < pos4);
    }

    #[test]
    fn topological_sort_no_dependencies() {
        let session_id = Uuid::new_v4();
        let s1 = make_story(session_id, "S1");
        let s2 = make_story(session_id, "S2");

        let dag = build_dag(&[s1.clone(), s2.clone()], &[]).unwrap();
        let order = topological_sort(&dag).unwrap();

        assert_eq!(order.len(), 2);
        assert!(order.contains(&s1.id));
        assert!(order.contains(&s2.id));
    }

    #[test]
    fn topological_sort_single_node() {
        let session_id = Uuid::new_v4();
        let s1 = make_story(session_id, "S1");

        let dag = build_dag(&[s1.clone()], &[]).unwrap();
        let order = topological_sort(&dag).unwrap();

        assert_eq!(order, vec![s1.id]);
    }

    #[test]
    fn topological_sort_empty_dag() {
        let dag = build_dag(&[], &[]).unwrap();
        let order = topological_sort(&dag).unwrap();
        assert!(order.is_empty());
    }

    // --- find_ready_stories ---

    #[test]
    fn find_ready_no_dependencies_all_ready() {
        let session_id = Uuid::new_v4();
        let s1 = make_story(session_id, "S1");
        let s2 = make_story(session_id, "S2");

        let dag = build_dag(&[s1.clone(), s2.clone()], &[]).unwrap();
        let statuses: HashMap<Uuid, WorkItemStatus> = [
            (s1.id, WorkItemStatus::Pending),
            (s2.id, WorkItemStatus::Pending),
        ]
        .into();

        let ready = find_ready_stories(&dag, &statuses);
        assert_eq!(ready.len(), 2);
    }

    #[test]
    fn find_ready_blocker_done() {
        let session_id = Uuid::new_v4();
        let s1 = make_story(session_id, "S1");
        let s2 = make_story(session_id, "S2");
        let dep = Dependency::new(s1.id, s2.id);

        let dag = build_dag(&[s1.clone(), s2.clone()], &[dep]).unwrap();
        let statuses: HashMap<Uuid, WorkItemStatus> = [
            (s1.id, WorkItemStatus::Done),
            (s2.id, WorkItemStatus::Pending),
        ]
        .into();

        let ready = find_ready_stories(&dag, &statuses);
        assert!(ready.contains(&s1.id));
        assert!(ready.contains(&s2.id));
    }

    #[test]
    fn find_ready_blocker_pending() {
        let session_id = Uuid::new_v4();
        let s1 = make_story(session_id, "S1");
        let s2 = make_story(session_id, "S2");
        let dep = Dependency::new(s1.id, s2.id);

        let dag = build_dag(&[s1.clone(), s2.clone()], &[dep]).unwrap();
        let statuses: HashMap<Uuid, WorkItemStatus> = [
            (s1.id, WorkItemStatus::Pending),
            (s2.id, WorkItemStatus::Pending),
        ]
        .into();

        let ready = find_ready_stories(&dag, &statuses);
        assert!(ready.contains(&s1.id));
        assert!(!ready.contains(&s2.id));
    }

    #[test]
    fn find_ready_cancelled_blocker_unblocks() {
        let session_id = Uuid::new_v4();
        let s1 = make_story(session_id, "S1");
        let s2 = make_story(session_id, "S2");
        let dep = Dependency::new(s1.id, s2.id);

        let dag = build_dag(&[s1.clone(), s2.clone()], &[dep]).unwrap();
        let statuses: HashMap<Uuid, WorkItemStatus> = [
            (s1.id, WorkItemStatus::Cancelled),
            (s2.id, WorkItemStatus::Pending),
        ]
        .into();

        let ready = find_ready_stories(&dag, &statuses);
        assert!(ready.contains(&s1.id));
        assert!(ready.contains(&s2.id));
    }

    #[test]
    fn find_ready_mixed_blockers() {
        let session_id = Uuid::new_v4();
        let s1 = make_story(session_id, "S1");
        let s2 = make_story(session_id, "S2");
        let s3 = make_story(session_id, "S3");

        // S3 depends on both S1 and S2
        let deps = vec![Dependency::new(s1.id, s3.id), Dependency::new(s2.id, s3.id)];

        let dag = build_dag(&[s1.clone(), s2.clone(), s3.clone()], &deps).unwrap();

        // S1 done, S2 still pending → S3 blocked
        let statuses: HashMap<Uuid, WorkItemStatus> = [
            (s1.id, WorkItemStatus::Done),
            (s2.id, WorkItemStatus::Pending),
            (s3.id, WorkItemStatus::Pending),
        ]
        .into();

        let ready = find_ready_stories(&dag, &statuses);
        assert!(ready.contains(&s1.id));
        assert!(ready.contains(&s2.id));
        assert!(!ready.contains(&s3.id));
    }

    #[test]
    fn find_ready_all_blockers_resolved() {
        let session_id = Uuid::new_v4();
        let s1 = make_story(session_id, "S1");
        let s2 = make_story(session_id, "S2");
        let s3 = make_story(session_id, "S3");

        let deps = vec![Dependency::new(s1.id, s3.id), Dependency::new(s2.id, s3.id)];

        let dag = build_dag(&[s1.clone(), s2.clone(), s3.clone()], &deps).unwrap();

        // Both blockers resolved (one done, one cancelled)
        let statuses: HashMap<Uuid, WorkItemStatus> = [
            (s1.id, WorkItemStatus::Done),
            (s2.id, WorkItemStatus::Cancelled),
            (s3.id, WorkItemStatus::Pending),
        ]
        .into();

        let ready = find_ready_stories(&dag, &statuses);
        assert!(ready.contains(&s3.id));
    }

    #[test]
    fn find_ready_blocker_in_progress() {
        let session_id = Uuid::new_v4();
        let s1 = make_story(session_id, "S1");
        let s2 = make_story(session_id, "S2");
        let dep = Dependency::new(s1.id, s2.id);

        let dag = build_dag(&[s1.clone(), s2.clone()], &[dep]).unwrap();
        let statuses: HashMap<Uuid, WorkItemStatus> = [
            (s1.id, WorkItemStatus::InProgress),
            (s2.id, WorkItemStatus::Pending),
        ]
        .into();

        let ready = find_ready_stories(&dag, &statuses);
        assert!(ready.contains(&s1.id));
        assert!(!ready.contains(&s2.id));
    }

    #[test]
    fn find_ready_blocker_failed() {
        let session_id = Uuid::new_v4();
        let s1 = make_story(session_id, "S1");
        let s2 = make_story(session_id, "S2");
        let dep = Dependency::new(s1.id, s2.id);

        let dag = build_dag(&[s1.clone(), s2.clone()], &[dep]).unwrap();
        let statuses: HashMap<Uuid, WorkItemStatus> = [
            (s1.id, WorkItemStatus::Failed),
            (s2.id, WorkItemStatus::Pending),
        ]
        .into();

        let ready = find_ready_stories(&dag, &statuses);
        assert!(ready.contains(&s1.id));
        assert!(!ready.contains(&s2.id));
    }

    // --- find_blocked_stories ---

    #[test]
    fn find_blocked_no_dependencies() {
        let session_id = Uuid::new_v4();
        let s1 = make_story(session_id, "S1");
        let s2 = make_story(session_id, "S2");

        let dag = build_dag(&[s1.clone(), s2.clone()], &[]).unwrap();
        let statuses: HashMap<Uuid, WorkItemStatus> = [
            (s1.id, WorkItemStatus::Pending),
            (s2.id, WorkItemStatus::Pending),
        ]
        .into();

        let blocked = find_blocked_stories(&dag, &statuses);
        assert!(blocked.is_empty());
    }

    #[test]
    fn find_blocked_with_pending_blocker() {
        let session_id = Uuid::new_v4();
        let s1 = make_story(session_id, "S1");
        let s2 = make_story(session_id, "S2");
        let dep = Dependency::new(s1.id, s2.id);

        let dag = build_dag(&[s1.clone(), s2.clone()], &[dep]).unwrap();
        let statuses: HashMap<Uuid, WorkItemStatus> = [
            (s1.id, WorkItemStatus::Pending),
            (s2.id, WorkItemStatus::Pending),
        ]
        .into();

        let blocked = find_blocked_stories(&dag, &statuses);
        assert!(blocked.contains(&s2.id));
        assert!(!blocked.contains(&s1.id));
    }

    #[test]
    fn find_blocked_resolved_blockers_not_blocked() {
        let session_id = Uuid::new_v4();
        let s1 = make_story(session_id, "S1");
        let s2 = make_story(session_id, "S2");
        let dep = Dependency::new(s1.id, s2.id);

        let dag = build_dag(&[s1.clone(), s2.clone()], &[dep]).unwrap();
        let statuses: HashMap<Uuid, WorkItemStatus> = [
            (s1.id, WorkItemStatus::Done),
            (s2.id, WorkItemStatus::Pending),
        ]
        .into();

        let blocked = find_blocked_stories(&dag, &statuses);
        assert!(blocked.is_empty());
    }

    #[test]
    fn find_blocked_cancelled_blocker_not_blocking() {
        let session_id = Uuid::new_v4();
        let s1 = make_story(session_id, "S1");
        let s2 = make_story(session_id, "S2");
        let dep = Dependency::new(s1.id, s2.id);

        let dag = build_dag(&[s1.clone(), s2.clone()], &[dep]).unwrap();
        let statuses: HashMap<Uuid, WorkItemStatus> = [
            (s1.id, WorkItemStatus::Cancelled),
            (s2.id, WorkItemStatus::Pending),
        ]
        .into();

        let blocked = find_blocked_stories(&dag, &statuses);
        assert!(blocked.is_empty());
    }

    #[test]
    fn find_blocked_partial_blockers() {
        let session_id = Uuid::new_v4();
        let s1 = make_story(session_id, "S1");
        let s2 = make_story(session_id, "S2");
        let s3 = make_story(session_id, "S3");

        let deps = vec![Dependency::new(s1.id, s3.id), Dependency::new(s2.id, s3.id)];

        let dag = build_dag(&[s1.clone(), s2.clone(), s3.clone()], &deps).unwrap();

        // S1 done but S2 still pending
        let statuses: HashMap<Uuid, WorkItemStatus> = [
            (s1.id, WorkItemStatus::Done),
            (s2.id, WorkItemStatus::Pending),
            (s3.id, WorkItemStatus::Pending),
        ]
        .into();

        let blocked = find_blocked_stories(&dag, &statuses);
        assert!(blocked.contains(&s3.id));
    }

    // --- Non-story items are filtered ---

    #[test]
    fn build_dag_skips_non_story_items() {
        let session_id = Uuid::new_v4();
        let epic = WorkItem::new_epic(session_id, "Epic".into(), "Desc".into(), "E1".into(), 0);
        let s1 = make_story(session_id, "S1");

        let dag = build_dag(&[epic, s1.clone()], &[]).unwrap();
        assert_eq!(dag.nodes.len(), 1);
        assert!(dag.nodes.contains(&s1.id));
    }

    // --- 5-story linear chain ---

    #[test]
    fn build_dag_5_stories_linear_chain_topological_sort() {
        let session_id = Uuid::new_v4();
        let s1 = make_story(session_id, "S1");
        let s2 = make_story(session_id, "S2");
        let s3 = make_story(session_id, "S3");
        let s4 = make_story(session_id, "S4");
        let s5 = make_story(session_id, "S5");

        // Linear chain: S1 -> S2 -> S3 -> S4 -> S5
        let deps = vec![
            Dependency::new(s1.id, s2.id),
            Dependency::new(s2.id, s3.id),
            Dependency::new(s3.id, s4.id),
            Dependency::new(s4.id, s5.id),
        ];

        let dag = build_dag(
            &[s1.clone(), s2.clone(), s3.clone(), s4.clone(), s5.clone()],
            &deps,
        )
        .unwrap();

        assert_eq!(dag.nodes.len(), 5);

        let order = topological_sort(&dag).unwrap();
        assert_eq!(order.len(), 5);

        let pos1 = order.iter().position(|&id| id == s1.id).unwrap();
        let pos2 = order.iter().position(|&id| id == s2.id).unwrap();
        let pos3 = order.iter().position(|&id| id == s3.id).unwrap();
        let pos4 = order.iter().position(|&id| id == s4.id).unwrap();
        let pos5 = order.iter().position(|&id| id == s5.id).unwrap();

        assert!(pos1 < pos2);
        assert!(pos2 < pos3);
        assert!(pos3 < pos4);
        assert!(pos4 < pos5);
    }

    // --- Cross-wave dependency ---

    #[test]
    fn build_dag_cross_wave_dependency_rejected() {
        // Stories from different decomposition sessions (waves) cannot be in the same DAG
        let wave1_session = Uuid::new_v4();
        let wave2_session = Uuid::new_v4();
        let s1 = make_story(wave1_session, "S1");
        let s2 = make_story(wave2_session, "S2");

        // Attempting to build a DAG with stories from different sessions should fail
        let err = build_dag(&[s1.clone(), s2.clone()], &[]).unwrap_err();
        assert!(matches!(err, NflowError::ValidationError(_)));

        // Also fails even with an explicit dependency between them
        let dep = Dependency::new(s1.id, s2.id);
        let err = build_dag(&[s1, s2], &[dep]).unwrap_err();
        assert!(matches!(err, NflowError::ValidationError(_)));
    }

    // --- Edge cases ---

    #[test]
    fn find_ready_empty_dag() {
        let dag = build_dag(&[], &[]).unwrap();
        let statuses: HashMap<Uuid, WorkItemStatus> = HashMap::new();
        let ready = find_ready_stories(&dag, &statuses);
        assert!(ready.is_empty());
    }

    #[test]
    fn find_blocked_empty_dag() {
        let dag = build_dag(&[], &[]).unwrap();
        let statuses: HashMap<Uuid, WorkItemStatus> = HashMap::new();
        let blocked = find_blocked_stories(&dag, &statuses);
        assert!(blocked.is_empty());
    }

    #[test]
    fn find_ready_and_blocked_are_complementary_for_dependent_stories() {
        let session_id = Uuid::new_v4();
        let s1 = make_story(session_id, "S1");
        let s2 = make_story(session_id, "S2");
        let s3 = make_story(session_id, "S3");

        // Chain: S1 -> S2 -> S3
        let deps = vec![Dependency::new(s1.id, s2.id), Dependency::new(s2.id, s3.id)];

        let dag = build_dag(&[s1.clone(), s2.clone(), s3.clone()], &deps).unwrap();
        let statuses: HashMap<Uuid, WorkItemStatus> = [
            (s1.id, WorkItemStatus::Done),
            (s2.id, WorkItemStatus::Pending),
            (s3.id, WorkItemStatus::Pending),
        ]
        .into();

        let ready = find_ready_stories(&dag, &statuses);
        let blocked = find_blocked_stories(&dag, &statuses);

        // S1 (done, no blockers) → ready
        // S2 (blocker S1 done) → ready
        // S3 (blocker S2 pending) → blocked
        assert!(ready.contains(&s1.id));
        assert!(ready.contains(&s2.id));
        assert!(!ready.contains(&s3.id));
        assert!(blocked.contains(&s3.id));
        assert!(!blocked.contains(&s1.id));
        assert!(!blocked.contains(&s2.id));
    }
}
