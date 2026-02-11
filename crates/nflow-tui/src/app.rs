use std::collections::VecDeque;
use std::time::Instant;

use crate::error::Result;
use crate::socket_client::{ResponseStatus, SocketClient};

/// Current phase of the plan decomposition process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecompositionPhase {
    /// No decomposition in progress.
    Idle,
    /// Decomposition request sent, waiting for first event.
    Starting,
    /// Claude is analyzing specs (receiving text events).
    Analyzing,
    /// Claude is building work items (receiving tool_use/input_json_delta events).
    BuildingItems,
    /// Decomposition finished successfully.
    Completed(DecompositionSummary),
    /// Decomposition failed with an error message.
    Failed(String),
}

/// Summary of a completed decomposition for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecompositionSummary {
    pub epic_count: u32,
    pub story_count: u32,
    pub task_count: u32,
}

/// A single spec entry for display in the specs list view.
#[derive(Debug, Clone)]
pub struct SpecItem {
    pub name: String,
    pub status: String,
    #[allow(dead_code)] // Used by future spec dialogue/resume views
    pub session_active: bool,
    pub created_at: String,
}

/// State for the specs list view.
#[derive(Debug)]
pub struct SpecsListState {
    pub items: Vec<SpecItem>,
    pub selected: usize,
}

impl SpecsListState {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            selected: 0,
        }
    }

    /// Get visible items with optional text filter.
    pub fn visible_items_filtered(&self, filter: &str) -> Vec<(usize, &SpecItem)> {
        if filter.is_empty() {
            return self.items.iter().enumerate().collect();
        }
        let filter_lower = filter.to_lowercase();
        self.items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.name.to_lowercase().contains(&filter_lower))
            .collect()
    }

    /// Move selection up.
    #[allow(dead_code)]
    pub fn select_prev(&mut self) {
        self.select_prev_filtered("");
    }

    /// Move selection up with filter.
    pub fn select_prev_filtered(&mut self, filter: &str) {
        let visible = self.visible_items_filtered(filter);
        if !visible.is_empty() && self.selected > 0 {
            self.selected -= 1;
        }
    }

    /// Move selection down.
    #[allow(dead_code)]
    pub fn select_next(&mut self) {
        self.select_next_filtered("");
    }

    /// Move selection down with filter.
    pub fn select_next_filtered(&mut self, filter: &str) {
        let visible = self.visible_items_filtered(filter);
        if !visible.is_empty() && self.selected < visible.len() - 1 {
            self.selected += 1;
        }
    }

    /// Get the currently selected item.
    pub fn selected_item(&self) -> Option<&SpecItem> {
        self.items.get(self.selected)
    }

    /// Update specs from daemon response data.
    pub fn update_from_response(&mut self, data: &serde_json::Value) {
        if let Some(specs) = data.get("specs").and_then(|v| v.as_array()) {
            self.items = specs
                .iter()
                .map(|s| SpecItem {
                    name: s
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    status: s
                        .get("status")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_string(),
                    session_active: s
                        .get("session_active")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                    created_at: s
                        .get("created_at")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                })
                .collect();

            // Clamp selection to new bounds
            if self.items.is_empty() {
                self.selected = 0;
            } else if self.selected >= self.items.len() {
                self.selected = self.items.len() - 1;
            }
        }
    }
}

/// A single message in the spec dialogue chat.
#[derive(Debug, Clone)]
pub struct DialogueMessage {
    /// Who sent this message ("Claude" or "You").
    pub sender: String,
    /// The message text.
    pub text: String,
}

/// State of the spec dialogue streaming session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogueSessionState {
    /// Streaming output from Claude (input disabled).
    Streaming,
    /// Waiting for user input (input enabled).
    WaitingForInput,
    /// Session completed or ended.
    Completed,
}

/// State for the spec dialogue sub-view.
#[derive(Debug)]
pub struct SpecDialogueState {
    /// Chat messages history.
    pub messages: Vec<DialogueMessage>,
    /// Current user input buffer.
    pub input: String,
    /// Current session state (streaming, waiting for input, completed).
    pub session_state: DialogueSessionState,
    /// The spec ID for this dialogue (set once streaming starts).
    pub spec_id: Option<String>,
    /// The spec name being dialogued.
    pub spec_name: String,
    /// Streaming request ID (to validate response lines).
    pub request_id: Option<String>,
    /// Scroll offset for the message area (0 = bottom, auto-scroll).
    pub scroll_offset: u16,
}

impl SpecDialogueState {
    /// Create a new dialogue state for the given spec.
    pub fn new(spec_name: String) -> Self {
        Self {
            messages: Vec::new(),
            input: String::new(),
            session_state: DialogueSessionState::Streaming,
            spec_id: None,
            spec_name,
            request_id: None,
            scroll_offset: 0,
        }
    }

    /// Add a message from Claude.
    pub fn add_claude_message(&mut self, text: String) {
        if let Some(last) = self.messages.last_mut() {
            if last.sender == "Claude" {
                // Append to existing Claude message for contiguous text events
                last.text.push_str(&text);
                self.scroll_offset = 0; // Auto-scroll
                return;
            }
        }
        self.messages.push(DialogueMessage {
            sender: "Claude".to_string(),
            text,
        });
        self.scroll_offset = 0;
    }

    /// Add a message from the user.
    pub fn add_user_message(&mut self, text: String) {
        self.messages.push(DialogueMessage {
            sender: "You".to_string(),
            text,
        });
        self.scroll_offset = 0;
    }

    /// Scroll up in the message area.
    pub fn scroll_up(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_add(1);
    }

    /// Scroll down in the message area.
    pub fn scroll_down(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_sub(1);
    }
}

/// State for the spec content pager sub-view.
#[derive(Debug)]
pub struct SpecPagerState {
    /// The spec name displayed in the header.
    pub spec_name: String,
    /// The full spec content (plain text / markdown).
    #[allow(dead_code)] // Kept for potential future use (e.g., search)
    pub content: String,
    /// Lines of content split for rendering.
    pub lines: Vec<String>,
    /// Current scroll offset (0 = top of document).
    pub scroll_offset: u16,
    /// Total number of lines in the content.
    pub total_lines: u16,
}

impl SpecPagerState {
    /// Create a new pager state with the given spec name and content.
    pub fn new(spec_name: String, content: String) -> Self {
        let lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
        let total_lines = lines.len() as u16;
        Self {
            spec_name,
            content,
            lines,
            scroll_offset: 0,
            total_lines,
        }
    }

    /// Scroll down by one line.
    pub fn scroll_down(&mut self, visible_height: u16) {
        let max_scroll = self.total_lines.saturating_sub(visible_height);
        if self.scroll_offset < max_scroll {
            self.scroll_offset += 1;
        }
    }

    /// Scroll up by one line.
    pub fn scroll_up(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_sub(1);
    }

    /// Scroll down by a page (half the visible height).
    pub fn page_down(&mut self, visible_height: u16) {
        let half = visible_height / 2;
        let max_scroll = self.total_lines.saturating_sub(visible_height);
        self.scroll_offset = (self.scroll_offset + half).min(max_scroll);
    }

    /// Scroll up by a page (half the visible height).
    pub fn page_up(&mut self, visible_height: u16) {
        let half = visible_height / 2;
        self.scroll_offset = self.scroll_offset.saturating_sub(half);
    }

    /// Jump to the top of the document.
    pub fn scroll_to_top(&mut self) {
        self.scroll_offset = 0;
    }

    /// Jump to the bottom of the document.
    pub fn scroll_to_bottom(&mut self, visible_height: u16) {
        self.scroll_offset = self.total_lines.saturating_sub(visible_height);
    }
}

/// A node in the plan tree view.
#[derive(Debug, Clone)]
pub struct PlanTreeNode {
    /// Display short_id (e.g., "W1-E1", "W1-S1", "W1-T1").
    pub short_id: String,
    /// Title of the work item.
    pub title: String,
    /// Status string (e.g., "pending", "ready", "in_progress", "done", "failed", "cancelled").
    pub status: String,
    /// Node depth level: 0=wave, 1=epic, 2=story, 3=task.
    pub depth: u8,
    /// Whether this node is collapsed (children hidden).
    pub collapsed: bool,
    /// Whether this node has children.
    pub has_children: bool,
    /// Dependencies as display IDs (stories only).
    pub depends_on: Vec<String>,
    /// Progress string for stories (e.g., "2/5").
    pub progress: Option<String>,
    /// Task kind: "impl" or "verify" (tasks only).
    pub kind: Option<String>,
    /// Error message for failed items.
    pub error_message: Option<String>,
}

/// State for the plan tree view.
#[derive(Debug)]
pub struct PlanTreeState {
    /// All tree nodes (flattened).
    pub nodes: Vec<PlanTreeNode>,
    /// Current selected index in the visible nodes list.
    pub selected: usize,
    /// Whether to hide verify tasks.
    pub hide_verify: bool,
    /// Wave number being displayed.
    pub wave_number: Option<u32>,
    /// Wave status (e.g., "in_progress", "approved").
    pub wave_status: Option<String>,
    /// Spec names for this wave.
    pub spec_names: Vec<String>,
}

impl PlanTreeState {
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            selected: 0,
            hide_verify: false,
            wave_number: None,
            wave_status: None,
            spec_names: Vec::new(),
        }
    }

    /// Get visible nodes (respecting collapsed state and verify filter).
    pub fn visible_nodes(&self) -> Vec<(usize, &PlanTreeNode)> {
        self.visible_nodes_filtered("")
    }

    /// Get visible nodes with optional text filter.
    /// If filter is non-empty, only show nodes that match or have matching descendants.
    pub fn visible_nodes_filtered(&self, filter: &str) -> Vec<(usize, &PlanTreeNode)> {
        // First pass: determine which nodes match (directly or via descendants)
        let matching = if filter.is_empty() {
            vec![true; self.nodes.len()]
        } else {
            let filter_lower = filter.to_lowercase();
            let mut matches = vec![false; self.nodes.len()];

            // Mark direct matches
            for (i, node) in self.nodes.iter().enumerate() {
                if node.title.to_lowercase().contains(&filter_lower) {
                    matches[i] = true;
                }
            }

            // Propagate matches upward: if a child matches, mark all its ancestors
            // For each matching node, walk backwards to find and mark all ancestors
            let mut result = matches.clone();
            for (i, &is_match) in matches.iter().enumerate() {
                if is_match {
                    let mut depth = self.nodes[i].depth;
                    for j in (0..i).rev() {
                        if self.nodes[j].depth < depth {
                            result[j] = true;
                            depth = self.nodes[j].depth;
                            if depth == 0 {
                                break;
                            }
                        }
                    }
                }
            }
            result
        };

        // Second pass: normal visibility logic (collapsed, verify filter) + matching filter
        let mut result = Vec::new();
        let mut skip_depth: Option<u8> = None;

        for (i, node) in self.nodes.iter().enumerate() {
            if let Some(sd) = skip_depth {
                if node.depth > sd {
                    continue;
                }
                skip_depth = None;
            }

            // Filter out non-matching nodes
            if !matching[i] {
                continue;
            }

            if self.hide_verify && node.depth == 3 {
                if let Some(ref kind) = node.kind {
                    if kind == "verify" {
                        continue;
                    }
                }
            }

            result.push((i, node));

            if node.collapsed && node.has_children {
                skip_depth = Some(node.depth);
            }
        }

        result
    }

    /// Move selection up.
    #[allow(dead_code)]
    pub fn select_prev(&mut self) {
        self.select_prev_filtered("");
    }

    /// Move selection up with filter.
    pub fn select_prev_filtered(&mut self, filter: &str) {
        let visible = self.visible_nodes_filtered(filter);
        if visible.is_empty() {
            return;
        }
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    /// Move selection down.
    #[allow(dead_code)]
    pub fn select_next(&mut self) {
        self.select_next_filtered("");
    }

    /// Move selection down with filter.
    pub fn select_next_filtered(&mut self, filter: &str) {
        let visible = self.visible_nodes_filtered(filter);
        if visible.is_empty() {
            return;
        }
        if self.selected < visible.len() - 1 {
            self.selected += 1;
        }
    }

    /// Toggle collapse on the selected node.
    #[allow(dead_code)]
    pub fn toggle_collapse(&mut self) {
        self.toggle_collapse_filtered("");
    }

    /// Toggle collapse on the selected node with filter.
    pub fn toggle_collapse_filtered(&mut self, filter: &str) {
        let visible = self.visible_nodes_filtered(filter);
        if let Some(&(real_idx, _)) = visible.get(self.selected) {
            if self.nodes[real_idx].has_children {
                self.nodes[real_idx].collapsed = !self.nodes[real_idx].collapsed;
            }
        }
    }

    /// Toggle verify task visibility.
    pub fn toggle_verify_visibility(&mut self) {
        self.hide_verify = !self.hide_verify;
        // Clamp selection
        let visible = self.visible_nodes();
        if !visible.is_empty() && self.selected >= visible.len() {
            self.selected = visible.len() - 1;
        }
    }

    /// Returns true if the tree has any epic nodes (depth 1).
    pub fn has_epics(&self) -> bool {
        self.nodes.iter().any(|n| n.depth == 1)
    }

    /// Update tree from daemon plan.show response data.
    pub fn update_from_response(&mut self, data: &serde_json::Value) {
        let wave_number = data
            .get("wave_number")
            .and_then(|v| v.as_u64())
            .map(|n| n as u32);
        let wave_status = data
            .get("status")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let spec_names: Vec<String> = data
            .get("spec_names")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        self.wave_number = wave_number;
        self.wave_status = wave_status.clone();
        self.spec_names = spec_names.clone();

        let mut nodes = Vec::new();

        // Add wave root node
        let wave_label = wave_number
            .map(|w| format!("W{}", w))
            .unwrap_or_else(|| "Wave".to_string());
        let has_epics = data
            .get("epics")
            .and_then(|v| v.as_array())
            .is_some_and(|a| !a.is_empty());
        let wave_title = if spec_names.is_empty() {
            wave_status.unwrap_or_default()
        } else {
            format!(
                "{} [{}]",
                wave_status.unwrap_or_default(),
                spec_names.join(", ")
            )
        };
        let wave_error_message = data
            .get("error_message")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        nodes.push(PlanTreeNode {
            short_id: wave_label,
            title: wave_title,
            status: String::new(),
            depth: 0,
            collapsed: false,
            has_children: has_epics,
            depends_on: Vec::new(),
            progress: None,
            kind: None,
            error_message: wave_error_message,
        });

        if let Some(epics) = data.get("epics").and_then(|v| v.as_array()) {
            for epic in epics {
                let epic_short_id = epic
                    .get("short_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let epic_title = epic
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let epic_status = epic
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let stories = epic.get("stories").and_then(|v| v.as_array());
                let has_stories = stories.is_some_and(|s| !s.is_empty());

                let epic_error_message = epic
                    .get("error_message")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                nodes.push(PlanTreeNode {
                    short_id: epic_short_id,
                    title: epic_title,
                    status: epic_status,
                    depth: 1,
                    collapsed: false,
                    has_children: has_stories,
                    depends_on: Vec::new(),
                    progress: None,
                    kind: None,
                    error_message: epic_error_message,
                });

                if let Some(stories) = stories {
                    for story in stories {
                        let story_short_id = story
                            .get("short_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let story_title = story
                            .get("title")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let story_status = story
                            .get("status")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let story_progress = story
                            .get("progress")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                        let story_deps: Vec<String> = story
                            .get("depends_on")
                            .and_then(|v| v.as_array())
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                    .collect()
                            })
                            .unwrap_or_default();
                        let tasks = story.get("tasks").and_then(|v| v.as_array());
                        let has_tasks = tasks.is_some_and(|t| !t.is_empty());

                        let story_error_message = story
                            .get("error_message")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                        nodes.push(PlanTreeNode {
                            short_id: story_short_id,
                            title: story_title,
                            status: story_status,
                            depth: 2,
                            collapsed: false,
                            has_children: has_tasks,
                            depends_on: story_deps,
                            progress: story_progress,
                            kind: None,
                            error_message: story_error_message,
                        });

                        if let Some(tasks) = tasks {
                            for task in tasks {
                                let task_short_id = task
                                    .get("short_id")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let task_title = task
                                    .get("title")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let task_status = task
                                    .get("status")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let task_kind = task
                                    .get("kind")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string());

                                let task_error_message = task
                                    .get("error_message")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string());
                                nodes.push(PlanTreeNode {
                                    short_id: task_short_id,
                                    title: task_title,
                                    status: task_status,
                                    depth: 3,
                                    collapsed: false,
                                    has_children: false,
                                    depends_on: Vec::new(),
                                    progress: None,
                                    kind: task_kind,
                                    error_message: task_error_message,
                                });
                            }
                        }
                    }
                }
            }
        }

        self.nodes = nodes;

        // Clamp selection
        let visible = self.visible_nodes();
        if !visible.is_empty() && self.selected >= visible.len() {
            self.selected = visible.len() - 1;
        }
    }

    /// Clear the plan tree state.
    pub fn clear(&mut self) {
        self.nodes.clear();
        self.selected = 0;
        self.wave_number = None;
        self.wave_status = None;
    }
}

/// A selectable spec entry for the generate dialog.
#[derive(Debug, Clone)]
pub struct GenerateSpecItem {
    pub name: String,
    pub selected: bool,
}

/// State for the plan generate dialog.
#[derive(Debug)]
pub struct PlanGenerateState {
    /// Available specs to select from.
    pub specs: Vec<GenerateSpecItem>,
    /// Currently highlighted item index.
    pub cursor: usize,
    /// Whether to include codebase context (--with-codebase).
    pub with_codebase: bool,
}

impl PlanGenerateState {
    pub fn new(spec_names: Vec<String>) -> Self {
        let specs = spec_names
            .into_iter()
            .map(|name| GenerateSpecItem {
                name,
                selected: false,
            })
            .collect();
        Self {
            specs,
            cursor: 0,
            with_codebase: false,
        }
    }

    pub fn select_prev(&mut self) {
        if !self.specs.is_empty() && self.cursor > 0 {
            self.cursor -= 1;
        }
    }

    pub fn select_next(&mut self) {
        if !self.specs.is_empty() && self.cursor < self.specs.len() - 1 {
            self.cursor += 1;
        }
    }

    pub fn toggle_selection(&mut self) {
        if let Some(spec) = self.specs.get_mut(self.cursor) {
            spec.selected = !spec.selected;
        }
    }
}

/// State for the spec name input (when creating a new spec).
#[derive(Debug)]
pub struct SpecNameInputState {
    /// User's spec name text input.
    pub input: String,
}

impl SpecNameInputState {
    pub fn new() -> Self {
        Self {
            input: String::new(),
        }
    }
}

/// State for the plan feedback input.
#[derive(Debug)]
pub struct PlanFeedbackState {
    /// User's feedback text input.
    pub input: String,
}

impl PlanFeedbackState {
    pub fn new() -> Self {
        Self {
            input: String::new(),
        }
    }
}

/// Action to confirm in the confirmation popup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmAction {
    ApprovePlan,
    DiscardPlan,
    StopStory(String),
    CancelStory(String),
    EscalateStory(String),
}

impl ConfirmAction {
    pub fn message(&self) -> String {
        match self {
            ConfirmAction::ApprovePlan => "Approve current draft wave?".to_string(),
            ConfirmAction::DiscardPlan => "Discard current draft wave?".to_string(),
            ConfirmAction::StopStory(id) => format!("Stop story {}?", id),
            ConfirmAction::CancelStory(id) => format!("Cancel story {}?", id),
            ConfirmAction::EscalateStory(id) => {
                format!("Escalate story {}? (stops & shows worktree path)", id)
            }
        }
    }
}

/// State for the confirmation popup.
#[derive(Debug, Clone)]
pub struct PlanConfirmState {
    pub action: ConfirmAction,
}

/// A detail entry for the plan node detail popup.
#[derive(Debug, Clone)]
pub struct PlanDetailState {
    /// Node short ID.
    pub short_id: String,
    /// Node title.
    pub title: String,
    /// Node status.
    pub status: String,
    /// Description (same as title for now; reserved for future detailed descriptions).
    #[allow(dead_code)]
    pub description: String,
    /// Dependencies (stories only).
    pub depends_on: Vec<String>,
    /// Progress (stories only).
    pub progress: Option<String>,
    /// Task kind (tasks only).
    pub kind: Option<String>,
    /// Node depth.
    pub depth: u8,
    /// Error message for failed items.
    pub error_message: Option<String>,
}

/// State for daemon event subscription.
#[derive(Debug)]
pub struct EventSubscriptionState {
    /// Whether we are currently subscribed to daemon events.
    pub subscribed: bool,
}

impl EventSubscriptionState {
    pub fn new() -> Self {
        Self { subscribed: false }
    }
}

/// A node in the execute tree view (similar to PlanTreeNode but with execution-specific fields).
#[derive(Debug, Clone)]
pub struct ExecuteTreeNode {
    /// Display short_id (e.g., "W1-E1", "W1-S1", "W1-T1").
    pub short_id: String,
    /// Title of the work item.
    pub title: String,
    /// Status string (e.g., "pending", "ready", "in_progress", "done", "failed", "cancelled").
    pub status: String,
    /// Node depth level: 0=wave, 1=epic, 2=story, 3=task.
    pub depth: u8,
    /// Whether this node is collapsed (children hidden).
    pub collapsed: bool,
    /// Whether this node has children.
    pub has_children: bool,
    /// Task kind: "impl" or "verify" (tasks only).
    pub kind: Option<String>,
    /// Progress string for stories (e.g., "2/5").
    pub progress: Option<String>,
    /// MR URL for completed stories.
    pub mr_url: Option<String>,
    /// Error message for failed items.
    pub error_message: Option<String>,
}

/// State for the execute tree view (left pane).
#[derive(Debug)]
pub struct ExecuteTreeState {
    /// All tree nodes (flattened).
    pub nodes: Vec<ExecuteTreeNode>,
    /// Current selected index in the visible nodes list.
    pub selected: usize,
    /// Whether to hide verify tasks.
    pub hide_verify: bool,
}

impl ExecuteTreeState {
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            selected: 0,
            hide_verify: false,
        }
    }

    /// Get visible nodes (respecting collapsed state and verify filter).
    pub fn visible_nodes(&self) -> Vec<(usize, &ExecuteTreeNode)> {
        self.visible_nodes_filtered("")
    }

    /// Get visible nodes with optional text filter.
    /// If filter is non-empty, only show nodes that match or have matching descendants.
    pub fn visible_nodes_filtered(&self, filter: &str) -> Vec<(usize, &ExecuteTreeNode)> {
        // First pass: determine which nodes match (directly or via descendants)
        let matching = if filter.is_empty() {
            vec![true; self.nodes.len()]
        } else {
            let filter_lower = filter.to_lowercase();
            let mut matches = vec![false; self.nodes.len()];

            // Mark direct matches
            for (i, node) in self.nodes.iter().enumerate() {
                if node.title.to_lowercase().contains(&filter_lower) {
                    matches[i] = true;
                }
            }

            // Propagate matches upward: if a child matches, mark all its ancestors
            // For each matching node, walk backwards to find and mark all ancestors
            let mut result = matches.clone();
            for (i, &is_match) in matches.iter().enumerate() {
                if is_match {
                    let mut depth = self.nodes[i].depth;
                    for j in (0..i).rev() {
                        if self.nodes[j].depth < depth {
                            result[j] = true;
                            depth = self.nodes[j].depth;
                            if depth == 0 {
                                break;
                            }
                        }
                    }
                }
            }
            result
        };

        // Second pass: normal visibility logic (collapsed, verify filter) + matching filter
        let mut result = Vec::new();
        let mut skip_depth: Option<u8> = None;

        for (i, node) in self.nodes.iter().enumerate() {
            if let Some(sd) = skip_depth {
                if node.depth > sd {
                    continue;
                }
                skip_depth = None;
            }

            // Filter out non-matching nodes
            if !matching[i] {
                continue;
            }

            if self.hide_verify && node.depth == 3 {
                if let Some(ref kind) = node.kind {
                    if kind == "verify" {
                        continue;
                    }
                }
            }

            result.push((i, node));

            if node.collapsed && node.has_children {
                skip_depth = Some(node.depth);
            }
        }

        result
    }

    /// Move selection up.
    #[allow(dead_code)]
    pub fn select_prev(&mut self) {
        self.select_prev_filtered("");
    }

    /// Move selection up with filter.
    pub fn select_prev_filtered(&mut self, filter: &str) {
        let visible = self.visible_nodes_filtered(filter);
        if visible.is_empty() {
            return;
        }
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    /// Move selection down.
    #[allow(dead_code)]
    pub fn select_next(&mut self) {
        self.select_next_filtered("");
    }

    /// Move selection down with filter.
    pub fn select_next_filtered(&mut self, filter: &str) {
        let visible = self.visible_nodes_filtered(filter);
        if visible.is_empty() {
            return;
        }
        if self.selected < visible.len() - 1 {
            self.selected += 1;
        }
    }

    /// Toggle collapse on the selected node.
    #[allow(dead_code)]
    pub fn toggle_collapse(&mut self) {
        self.toggle_collapse_filtered("");
    }

    /// Toggle collapse on the selected node with filter.
    pub fn toggle_collapse_filtered(&mut self, filter: &str) {
        let visible = self.visible_nodes_filtered(filter);
        if let Some(&(real_idx, _)) = visible.get(self.selected) {
            if self.nodes[real_idx].has_children {
                self.nodes[real_idx].collapsed = !self.nodes[real_idx].collapsed;
            }
        }
    }

    /// Toggle verify task visibility.
    pub fn toggle_verify_visibility(&mut self) {
        self.hide_verify = !self.hide_verify;
        let visible = self.visible_nodes();
        if !visible.is_empty() && self.selected >= visible.len() {
            self.selected = visible.len() - 1;
        }
    }

    /// Get the short_id of the currently selected task (depth 3), if any.
    pub fn selected_task_id(&self) -> Option<String> {
        let visible = self.visible_nodes();
        if let Some(&(real_idx, _)) = visible.get(self.selected) {
            let node = &self.nodes[real_idx];
            if node.depth == 3 {
                return Some(node.short_id.clone());
            }
        }
        None
    }

    /// Get the status of the currently selected task, if any.
    pub fn selected_task_status(&self) -> Option<String> {
        let visible = self.visible_nodes();
        if let Some(&(real_idx, _)) = visible.get(self.selected) {
            let node = &self.nodes[real_idx];
            if node.depth == 3 {
                return Some(node.status.clone());
            }
        }
        None
    }

    /// Get the short_id of the currently selected story (depth 2), if any.
    /// If a task (depth 3) is selected, returns its parent story.
    pub fn selected_story_id(&self) -> Option<String> {
        let visible = self.visible_nodes();
        if let Some(&(real_idx, _)) = visible.get(self.selected) {
            let node = &self.nodes[real_idx];
            if node.depth == 2 {
                return Some(node.short_id.clone());
            }
            // If a task is selected, find its parent story
            if node.depth == 3 {
                // Walk backwards in the nodes array to find the parent story
                for i in (0..real_idx).rev() {
                    if self.nodes[i].depth == 2 {
                        return Some(self.nodes[i].short_id.clone());
                    }
                }
            }
        }
        None
    }

    /// Get the status of the currently selected story (depth 2), if any.
    /// If a task (depth 3) is selected, returns its parent story's status.
    pub fn selected_story_status(&self) -> Option<String> {
        let visible = self.visible_nodes();
        if let Some(&(real_idx, _)) = visible.get(self.selected) {
            let node = &self.nodes[real_idx];
            if node.depth == 2 {
                return Some(node.status.clone());
            }
            if node.depth == 3 {
                for i in (0..real_idx).rev() {
                    if self.nodes[i].depth == 2 {
                        return Some(self.nodes[i].status.clone());
                    }
                }
            }
        }
        None
    }

    /// Get the depth, short_id, and status of the currently selected node.
    #[allow(dead_code)]
    pub fn selected_node_info(&self) -> Option<(u8, String, String)> {
        let visible = self.visible_nodes();
        if let Some(&(real_idx, _)) = visible.get(self.selected) {
            let node = &self.nodes[real_idx];
            return Some((node.depth, node.short_id.clone(), node.status.clone()));
        }
        None
    }

    /// Find and select the first in_progress task. Returns true if found.
    pub fn auto_select_running_task(&mut self) -> bool {
        let visible = self.visible_nodes();
        for (vi, &(_, node)) in visible.iter().enumerate() {
            if node.depth == 3 && node.status == "in_progress" {
                self.selected = vi;
                return true;
            }
        }
        false
    }

    /// Update a node's status by matching its short_id.
    /// Returns true if a node was found and updated.
    #[allow(dead_code)] // Used when daemon provides short_id in events
    pub fn update_node_status(&mut self, short_id: &str, new_status: &str) -> bool {
        for node in &mut self.nodes {
            if node.short_id == short_id {
                node.status = new_status.to_string();
                return true;
            }
        }
        false
    }

    /// Set an MR URL on a story node by short_id.
    #[allow(dead_code)] // Used when daemon provides short_id in events
    pub fn set_story_mr_url(&mut self, short_id: &str, mr_url: &str) {
        for node in &mut self.nodes {
            if node.short_id == short_id && node.depth == 2 {
                node.mr_url = Some(mr_url.to_string());
                return;
            }
        }
    }

    /// Count tasks by status for the running counts display.
    /// Returns (running, total_tasks, done, failed).
    pub fn count_task_stats(&self) -> (u32, u32, u32, u32) {
        let mut running = 0u32;
        let mut total = 0u32;
        let mut done = 0u32;
        let mut failed = 0u32;

        for node in &self.nodes {
            if node.depth == 3 {
                total += 1;
                match node.status.as_str() {
                    "in_progress" => running += 1,
                    "done" => done += 1,
                    "failed" => failed += 1,
                    _ => {}
                }
            }
        }

        (running, total, done, failed)
    }

    /// Update tree from daemon exec.status response data.
    pub fn update_from_response(&mut self, data: &serde_json::Value) {
        let mut nodes = Vec::new();

        if let Some(waves) = data.get("waves").and_then(|v| v.as_array()) {
            for wave in waves {
                let wave_number = wave
                    .get("wave_number")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u32);
                let wave_status = wave
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                if wave_status == "discarded" {
                    continue;
                }

                let wave_label = wave_number
                    .map(|w| format!("W{}", w))
                    .unwrap_or_else(|| "Wave".to_string());
                let has_epics = wave
                    .get("epics")
                    .and_then(|v| v.as_array())
                    .is_some_and(|a| !a.is_empty());

                let wave_error_message = wave
                    .get("error_message")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                nodes.push(ExecuteTreeNode {
                    short_id: wave_label,
                    title: wave_status,
                    status: String::new(),
                    depth: 0,
                    collapsed: false,
                    has_children: has_epics,
                    kind: None,
                    progress: None,
                    mr_url: None,
                    error_message: wave_error_message,
                });

                if let Some(epics) = wave.get("epics").and_then(|v| v.as_array()) {
                    for epic in epics {
                        let epic_short_id = epic
                            .get("short_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let epic_title = epic
                            .get("title")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let epic_status = epic
                            .get("status")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let stories = epic.get("stories").and_then(|v| v.as_array());
                        let has_stories = stories.is_some_and(|s| !s.is_empty());

                        let epic_error_message = epic
                            .get("error_message")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                        nodes.push(ExecuteTreeNode {
                            short_id: epic_short_id,
                            title: epic_title,
                            status: epic_status,
                            depth: 1,
                            collapsed: false,
                            has_children: has_stories,
                            kind: None,
                            progress: None,
                            mr_url: None,
                            error_message: epic_error_message,
                        });

                        if let Some(stories) = stories {
                            for story in stories {
                                let story_short_id = story
                                    .get("short_id")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let story_title = story
                                    .get("title")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let story_status = story
                                    .get("status")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let story_progress = story
                                    .get("progress")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string());
                                let tasks = story.get("tasks").and_then(|v| v.as_array());
                                let has_tasks = tasks.is_some_and(|t| !t.is_empty());

                                let story_error_message = story
                                    .get("error_message")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string());
                                nodes.push(ExecuteTreeNode {
                                    short_id: story_short_id,
                                    title: story_title,
                                    status: story_status,
                                    depth: 2,
                                    collapsed: false,
                                    has_children: has_tasks,
                                    kind: None,
                                    progress: story_progress,
                                    mr_url: None,
                                    error_message: story_error_message,
                                });

                                if let Some(tasks) = tasks {
                                    for task in tasks {
                                        let task_short_id = task
                                            .get("short_id")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string();
                                        let task_title = task
                                            .get("title")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string();
                                        let task_status = task
                                            .get("status")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string();
                                        let task_kind = task
                                            .get("kind")
                                            .and_then(|v| v.as_str())
                                            .map(|s| s.to_string());

                                        let task_error_message = task
                                            .get("error_message")
                                            .and_then(|v| v.as_str())
                                            .map(|s| s.to_string());
                                        nodes.push(ExecuteTreeNode {
                                            short_id: task_short_id,
                                            title: task_title,
                                            status: task_status,
                                            depth: 3,
                                            collapsed: false,
                                            has_children: false,
                                            kind: task_kind,
                                            progress: None,
                                            mr_url: None,
                                            error_message: task_error_message,
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        self.nodes = nodes;

        // Clamp selection
        let visible = self.visible_nodes();
        if !visible.is_empty() && self.selected >= visible.len() {
            self.selected = visible.len() - 1;
        }
    }
}

/// State for the execute output pane (right pane).
#[derive(Debug)]
pub struct ExecuteOutputState {
    /// Short ID of the currently displayed task (if any).
    pub task_id: Option<String>,
    /// Log lines for the currently displayed task.
    pub lines: Vec<String>,
    /// Whether we're streaming live output.
    pub is_streaming: bool,
    /// Streaming request ID (for validating response lines).
    pub request_id: Option<String>,
    /// Current scroll offset (0 = bottom for streaming, 0 = top for historical).
    pub scroll_offset: u16,
    /// Total number of lines.
    pub total_lines: u16,
}

impl ExecuteOutputState {
    pub fn new() -> Self {
        Self {
            task_id: None,
            lines: Vec::new(),
            is_streaming: false,
            request_id: None,
            scroll_offset: 0,
            total_lines: 0,
        }
    }

    /// Set the displayed task and its log lines (historical).
    pub fn set_historical(&mut self, task_id: String, lines: Vec<String>) {
        self.task_id = Some(task_id);
        self.total_lines = lines.len() as u16;
        self.lines = lines;
        self.is_streaming = false;
        self.request_id = None;
        self.scroll_offset = 0;
    }

    /// Start streaming output for a task.
    pub fn start_streaming(&mut self, task_id: String, request_id: String) {
        self.task_id = Some(task_id);
        self.lines.clear();
        self.total_lines = 0;
        self.is_streaming = true;
        self.request_id = Some(request_id);
        self.scroll_offset = 0;
    }

    /// Append a line of streaming output.
    pub fn append_line(&mut self, line: String) {
        self.lines.push(line);
        self.total_lines = self.lines.len() as u16;
        // Auto-scroll when streaming (keep at bottom)
        self.scroll_offset = 0;
    }

    /// Stop streaming.
    pub fn stop_streaming(&mut self) {
        self.is_streaming = false;
        self.request_id = None;
    }

    /// Clear the output pane.
    #[allow(dead_code)] // Used by future execute view features (e.g., task completion)
    pub fn clear(&mut self) {
        self.task_id = None;
        self.lines.clear();
        self.total_lines = 0;
        self.is_streaming = false;
        self.request_id = None;
        self.scroll_offset = 0;
    }

    /// Scroll up in the output pane.
    pub fn scroll_up(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_add(1);
    }

    /// Scroll down in the output pane.
    pub fn scroll_down(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_sub(1);
    }
}

/// State for the decomposition output pane (plan view during decomposition).
#[derive(Debug)]
pub struct DecompositionOutputState {
    /// Log lines from Claude's decomposition streaming output.
    pub lines: Vec<String>,
    /// Whether we're streaming live output.
    pub is_streaming: bool,
    /// Streaming request ID (for validating response lines).
    pub request_id: Option<String>,
    /// Current scroll offset (0 = bottom for streaming).
    pub scroll_offset: u16,
    /// Total number of lines.
    pub total_lines: u16,
    /// Partial line accumulator for text events that don't end with newline.
    partial_line: String,
    /// Accumulator for tool input JSON fragments.
    tool_input_buffer: String,
}

impl DecompositionOutputState {
    pub fn new() -> Self {
        Self {
            lines: Vec::new(),
            is_streaming: false,
            request_id: None,
            scroll_offset: 0,
            total_lines: 0,
            partial_line: String::new(),
            tool_input_buffer: String::new(),
        }
    }

    /// Start streaming output for a decomposition.
    pub fn start_streaming(&mut self, request_id: String) {
        self.clear();
        self.is_streaming = true;
        self.request_id = Some(request_id);
        self.tool_input_buffer.clear();
    }

    /// Append a line of streaming output.
    pub fn append_line(&mut self, line: String) {
        self.lines.push(line);
        self.total_lines = self.lines.len() as u16;
        // Auto-scroll when streaming (keep at bottom)
        self.scroll_offset = 0;
    }

    /// Stop streaming.
    pub fn stop_streaming(&mut self) {
        self.is_streaming = false;
        self.request_id = None;
        // Flush any remaining partial line
        if !self.partial_line.is_empty() {
            self.lines.push(self.partial_line.clone());
            self.partial_line.clear();
            self.total_lines = self.lines.len() as u16;
        }
    }

    /// Clear the output pane.
    pub fn clear(&mut self) {
        self.lines.clear();
        self.total_lines = 0;
        self.is_streaming = false;
        self.request_id = None;
        self.scroll_offset = 0;
        self.partial_line.clear();
        self.tool_input_buffer.clear();
    }

    /// Scroll up in the output pane.
    pub fn scroll_up(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_add(1);
    }

    /// Scroll down in the output pane.
    pub fn scroll_down(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_sub(1);
    }

    /// Append a streaming event from the decomposition process.
    /// Handles text deltas, tool use, tool results, and errors.
    pub fn append_decomposition_event(&mut self, data: &serde_json::Value) {
        let event_type = data.get("type").and_then(|v| v.as_str()).unwrap_or("");

        match event_type {
            "text" => {
                // Claude's reasoning text - split by newlines and accumulate partials
                if let Some(text) = data.get("text").and_then(|v| v.as_str()) {
                    // If partial_line is not empty, prepend it to the text
                    let full_text = if self.partial_line.is_empty() {
                        text.to_string()
                    } else {
                        let mut combined = self.partial_line.clone();
                        combined.push_str(text);
                        self.partial_line.clear();
                        combined
                    };

                    // Split by newlines
                    let mut iter = full_text.split('\n').peekable();
                    while let Some(line) = iter.next() {
                        if iter.peek().is_some() {
                            // Not the last fragment, so it's a complete line
                            self.append_line(line.to_string());
                        } else {
                            // Last fragment - might be partial
                            if text.ends_with('\n') {
                                // Text ended with newline, so this is a complete line
                                self.append_line(line.to_string());
                            } else {
                                // Partial line - accumulate it
                                self.partial_line = line.to_string();
                            }
                        }
                    }
                }
            }
            "input_json_delta" => {
                // Accumulate partial JSON for tool input
                if let Some(partial) = data.get("partial_json").and_then(|v| v.as_str()) {
                    self.tool_input_buffer.push_str(partial);

                    // Remove previous typing indicator if it exists
                    if self
                        .lines
                        .last()
                        .is_some_and(|l| l == "  \u{22EF} typing...")
                    {
                        self.lines.pop();
                        self.total_lines = self.total_lines.saturating_sub(1);
                    }

                    // Show a minimal typing indicator instead of raw JSON
                    self.append_line("  \u{22EF} typing...".to_string());
                }
            }
            "tool_use" => {
                // Clear the tool input buffer (the complete tool_use event has arrived)
                self.tool_input_buffer.clear();

                // Flush any partial line first
                if !self.partial_line.is_empty() {
                    self.append_line(self.partial_line.clone());
                    self.partial_line.clear();
                }

                // Remove the typing indicator if present
                if self
                    .lines
                    .last()
                    .is_some_and(|l| l == "  \u{22EF} typing...")
                {
                    self.lines.pop();
                    self.total_lines = self.total_lines.saturating_sub(1);
                }

                let name = data
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let input = data.get("input");

                // Add empty line before tool block for breathing room
                if !self.lines.is_empty() {
                    self.append_line(String::new());
                }

                // Separator line with tool name
                self.append_line(format!(
                    "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500} Tool: {} \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}",
                    name
                ));

                // Extract and display key parameters based on tool name
                if let Some(input_obj) = input {
                    Self::format_tool_params(name, input_obj, &mut self.lines);
                    self.total_lines = self.lines.len() as u16;
                    self.scroll_offset = 0;
                }
            }
            "tool_result" => {
                let content = data.get("content").and_then(|v| v.as_str()).unwrap_or("");

                if content.is_empty() {
                    self.append_line("  \u{21B3} (no output)".to_string());
                } else {
                    let result_lines: Vec<&str> = content.lines().collect();
                    let max_display_lines: usize = 4;
                    let max_line_width = 120;

                    // Show first line with arrow indicator
                    if let Some(first) = result_lines.first() {
                        let truncated = truncate_str(first, max_line_width);
                        self.append_line(format!("  \u{21B3} {}", truncated));
                    }

                    // Show subsequent lines (up to max) with indentation
                    for line in result_lines
                        .iter()
                        .skip(1)
                        .take(max_display_lines.saturating_sub(1))
                    {
                        let truncated = truncate_str(line, max_line_width);
                        self.append_line(format!("    {}", truncated));
                    }

                    // If more lines exist, show a summary
                    if result_lines.len() > max_display_lines {
                        self.append_line(format!(
                            "    ... ({} more lines)",
                            result_lines.len() - max_display_lines
                        ));
                    }
                }
            }
            "result" => {
                // Flush any partial line first
                if !self.partial_line.is_empty() {
                    self.append_line(self.partial_line.clone());
                    self.partial_line.clear();
                }
                self.append_line("[Decomposition complete]".to_string());
            }
            "error" => {
                // Flush any partial line first
                if !self.partial_line.is_empty() {
                    self.append_line(self.partial_line.clone());
                    self.partial_line.clear();
                }
                let message = data
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error");
                self.append_line(format!("[Error: {}]", message));
            }
            "parse_error" => {
                // Silently ignore parse errors — they are internal diagnostics, not user-facing errors.
                // Most parse errors are from valid Claude events we don't need to handle (system, assistant, user turn markers,
                // stream metadata events like content_block_start/stop, message_start/stop, etc.)
                // No need to display these to the user.
            }
            _ => {
                // Unknown event type - ignore
            }
        }
    }

    /// Format tool parameters into human-readable indented lines.
    /// Pushes directly into the provided lines vector for efficiency.
    fn format_tool_params(tool_name: &str, input: &serde_json::Value, lines: &mut Vec<String>) {
        let max_value_len: usize = 120;

        let truncate = |s: &str| -> String { truncate_str(s, max_value_len) };

        let obj = match input.as_object() {
            Some(o) => o,
            None => {
                // Not an object — just show the raw value
                let raw = input.to_string();
                lines.push(format!("  {}", truncate(&raw)));
                return;
            }
        };

        // For known tools, extract the most important parameter first
        match tool_name {
            "Bash" | "bash" => {
                if let Some(cmd) = obj.get("command").and_then(|v| v.as_str()) {
                    lines.push(format!("  command: {}", truncate(cmd)));
                }
                if let Some(desc) = obj.get("description").and_then(|v| v.as_str()) {
                    lines.push(format!("  description: {}", truncate(desc)));
                }
            }
            "Read" | "read" => {
                if let Some(path) = obj.get("file_path").and_then(|v| v.as_str()) {
                    lines.push(format!("  file_path: {}", truncate(path)));
                }
                if let Some(offset) = obj.get("offset") {
                    lines.push(format!("  offset: {}", offset));
                }
                if let Some(limit) = obj.get("limit") {
                    lines.push(format!("  limit: {}", limit));
                }
            }
            "Write" | "write" => {
                if let Some(path) = obj.get("file_path").and_then(|v| v.as_str()) {
                    lines.push(format!("  file_path: {}", truncate(path)));
                }
            }
            "Edit" | "edit" => {
                if let Some(path) = obj.get("file_path").and_then(|v| v.as_str()) {
                    lines.push(format!("  file_path: {}", truncate(path)));
                }
                if let Some(old) = obj.get("old_string").and_then(|v| v.as_str()) {
                    let first_line = old.lines().next().unwrap_or("");
                    lines.push(format!("  old_string: {}", truncate(first_line)));
                }
            }
            "Glob" | "glob" => {
                if let Some(pattern) = obj.get("pattern").and_then(|v| v.as_str()) {
                    lines.push(format!("  pattern: {}", truncate(pattern)));
                }
                if let Some(path) = obj.get("path").and_then(|v| v.as_str()) {
                    lines.push(format!("  path: {}", truncate(path)));
                }
            }
            "Grep" | "grep" => {
                if let Some(pattern) = obj.get("pattern").and_then(|v| v.as_str()) {
                    lines.push(format!("  pattern: {}", truncate(pattern)));
                }
                if let Some(path) = obj.get("path").and_then(|v| v.as_str()) {
                    lines.push(format!("  path: {}", truncate(path)));
                }
            }
            _ => {
                // Generic: show all params as key: value, up to 5
                for (count, (key, val)) in obj.iter().enumerate() {
                    if count >= 5 {
                        lines.push(format!("  ... ({} more params)", obj.len() - 5));
                        break;
                    }
                    let display_val = match val {
                        serde_json::Value::String(s) => truncate(s),
                        serde_json::Value::Null => "null".to_string(),
                        serde_json::Value::Bool(b) => b.to_string(),
                        serde_json::Value::Number(n) => n.to_string(),
                        _ => {
                            let raw = val.to_string();
                            truncate(&raw)
                        }
                    };
                    lines.push(format!("  {}: {}", key, display_val));
                }
            }
        }
    }
}

/// A parsed log entry for display in the logs view.
#[derive(Debug, Clone)]
pub struct LogEntry {
    /// Timestamp string (if available).
    pub timestamp: Option<String>,
    /// Entry type: "text", "tool_call", "tool_result", "error".
    pub entry_type: String,
    /// The display text for this entry.
    pub text: String,
}

/// State for the full-screen logs view.
#[derive(Debug)]
pub struct LogsViewState {
    /// Short ID of the currently displayed task (if any).
    pub task_id: Option<String>,
    /// Task title for display.
    pub task_title: Option<String>,
    /// Task status (e.g., "in_progress", "done", "failed").
    pub task_status: Option<String>,
    /// Parsed log entries for display.
    pub entries: Vec<LogEntry>,
    /// Flat lines for rendering (derived from entries).
    pub lines: Vec<String>,
    /// Whether we're streaming live output.
    pub is_streaming: bool,
    /// Streaming request ID (for validating response lines).
    pub request_id: Option<String>,
    /// Current scroll offset (0 = bottom for streaming, 0 = top for historical).
    pub scroll_offset: u16,
    /// Total number of lines.
    pub total_lines: u16,
}

impl LogsViewState {
    pub fn new() -> Self {
        Self {
            task_id: None,
            task_title: None,
            task_status: None,
            entries: Vec::new(),
            lines: Vec::new(),
            is_streaming: false,
            request_id: None,
            scroll_offset: 0,
            total_lines: 0,
        }
    }

    /// Set the displayed task and its log lines (historical/completed task).
    pub fn set_historical(&mut self, task_id: String, status: String, lines: Vec<String>) {
        self.task_id = Some(task_id);
        self.task_status = Some(status);
        self.entries = parse_log_entries(&lines);
        self.lines = format_log_entries(&self.entries);
        self.total_lines = self.lines.len() as u16;
        self.is_streaming = false;
        self.request_id = None;
        self.scroll_offset = 0;
    }

    /// Start streaming output for a running task.
    pub fn start_streaming(&mut self, task_id: String, request_id: String) {
        self.task_id = Some(task_id);
        self.task_status = Some("in_progress".to_string());
        self.entries.clear();
        self.lines.clear();
        self.total_lines = 0;
        self.is_streaming = true;
        self.request_id = Some(request_id);
        self.scroll_offset = 0;
    }

    /// Append a line of streaming output.
    pub fn append_line(&mut self, line: String) {
        let entry = parse_single_log_entry(&line);
        let formatted = format_single_entry(&entry);
        self.entries.push(entry);
        self.lines.push(formatted);
        self.total_lines = self.lines.len() as u16;
        // Auto-scroll when streaming (keep at bottom)
        self.scroll_offset = 0;
    }

    /// Stop streaming.
    pub fn stop_streaming(&mut self) {
        self.is_streaming = false;
        self.request_id = None;
        if let Some(ref mut status) = self.task_status {
            if status == "in_progress" {
                *status = "done".to_string();
            }
        }
    }

    /// Clear the logs view state.
    pub fn clear(&mut self) {
        self.task_id = None;
        self.task_title = None;
        self.task_status = None;
        self.entries.clear();
        self.lines.clear();
        self.total_lines = 0;
        self.is_streaming = false;
        self.request_id = None;
        self.scroll_offset = 0;
    }

    /// Scroll down by one line (toward bottom of content).
    pub fn scroll_down(&mut self, visible_height: u16) {
        if self.is_streaming {
            // Streaming: offset 0 = bottom, increasing moves up
            self.scroll_offset = self.scroll_offset.saturating_sub(1);
        } else {
            // Historical: offset 0 = top, increasing moves down
            let max_scroll = self.total_lines.saturating_sub(visible_height);
            if self.scroll_offset < max_scroll {
                self.scroll_offset += 1;
            }
        }
    }

    /// Scroll up by one line (toward top of content).
    pub fn scroll_up(&mut self, visible_height: u16) {
        if self.is_streaming {
            // Streaming: increasing offset = moving up from bottom
            let max_scroll = self.total_lines.saturating_sub(visible_height);
            if self.scroll_offset < max_scroll {
                self.scroll_offset += 1;
            }
        } else {
            // Historical: decreasing offset = moving up
            self.scroll_offset = self.scroll_offset.saturating_sub(1);
        }
    }

    /// Scroll down by half a page.
    pub fn page_down(&mut self, visible_height: u16) {
        let half = visible_height / 2;
        if self.is_streaming {
            self.scroll_offset = self.scroll_offset.saturating_sub(half);
        } else {
            let max_scroll = self.total_lines.saturating_sub(visible_height);
            self.scroll_offset = (self.scroll_offset + half).min(max_scroll);
        }
    }

    /// Scroll up by half a page.
    pub fn page_up(&mut self, visible_height: u16) {
        let half = visible_height / 2;
        if self.is_streaming {
            let max_scroll = self.total_lines.saturating_sub(visible_height);
            self.scroll_offset = (self.scroll_offset + half).min(max_scroll);
        } else {
            self.scroll_offset = self.scroll_offset.saturating_sub(half);
        }
    }

    /// Jump to the top of the log.
    pub fn scroll_to_top(&mut self, visible_height: u16) {
        if self.is_streaming {
            let max_scroll = self.total_lines.saturating_sub(visible_height);
            self.scroll_offset = max_scroll;
        } else {
            self.scroll_offset = 0;
        }
    }

    /// Jump to the bottom of the log.
    pub fn scroll_to_bottom(&mut self, visible_height: u16) {
        if self.is_streaming {
            self.scroll_offset = 0;
        } else {
            self.scroll_offset = self.total_lines.saturating_sub(visible_height);
        }
    }
}

/// Parse raw log lines into structured LogEntry objects.
/// Detects tool calls, tool results, and timestamps.
fn parse_log_entries(lines: &[String]) -> Vec<LogEntry> {
    lines.iter().map(|l| parse_single_log_entry(l)).collect()
}

/// Parse a single log line into a LogEntry.
fn parse_single_log_entry(line: &str) -> LogEntry {
    // Detect tool call patterns: "Tool: <name>" or lines starting with tool-related prefixes
    let trimmed = line.trim();

    // Try to extract timestamp from beginning of line (e.g., "[2026-02-08T10:30:00Z]")
    let (timestamp, rest) = if trimmed.starts_with('[') {
        if let Some(end) = trimmed.find(']') {
            let ts = trimmed[1..end].to_string();
            let remainder = trimmed[end + 1..].trim();
            (Some(ts), remainder)
        } else {
            (None, trimmed)
        }
    } else {
        (None, trimmed)
    };

    // Classify the entry type
    let entry_type = if rest.starts_with("Tool call:")
        || rest.starts_with("tool_use:")
        || rest.starts_with("Running:")
    {
        "tool_call"
    } else if rest.starts_with("Tool result:")
        || rest.starts_with("tool_result:")
        || rest.starts_with("Result:")
    {
        "tool_result"
    } else if rest.starts_with("Error:") || rest.starts_with("error:") || rest.starts_with("ERR") {
        "error"
    } else {
        "text"
    };

    LogEntry {
        timestamp,
        entry_type: entry_type.to_string(),
        text: line.to_string(),
    }
}

/// Format log entries into display lines.
fn format_log_entries(entries: &[LogEntry]) -> Vec<String> {
    entries.iter().map(format_single_entry).collect()
}

/// Format a single log entry into a display line.
fn format_single_entry(entry: &LogEntry) -> String {
    if let Some(ref ts) = entry.timestamp {
        // Extract just the time portion for compact display
        let time_part = if let Some(t_pos) = ts.find('T') {
            let time_str = &ts[t_pos + 1..];
            // Truncate at seconds (drop fractional/timezone)
            // Safety: All slice positions come from find() on ASCII chars ('.', '+', 'T')
            // which always return valid UTF-8 boundaries.
            if let Some(dot) = time_str.find('.') {
                &time_str[..dot]
            } else if let Some(plus) = time_str.find('+') {
                &time_str[..plus]
            } else if let Some(stripped) = time_str.strip_suffix('Z') {
                stripped
            } else {
                time_str
            }
        } else {
            ts.as_str()
        };
        format!(
            "[{}] {}",
            time_part,
            entry
                .text
                .trim_start_matches(|c: char| c == '['
                    || c.is_ascii_digit()
                    || c == '-'
                    || c == 'T'
                    || c == ':'
                    || c == '.'
                    || c == 'Z'
                    || c == '+'
                    || c == ']')
                .trim_start()
        )
    } else {
        entry.text.clone()
    }
}

/// A single project entry for display in the project switcher.
#[derive(Debug, Clone)]
pub struct ProjectItem {
    pub name: String,
    pub path: String,
    pub active_agent_count: u32,
}

/// State for the project switcher overlay.
#[derive(Debug)]
pub struct ProjectSwitcherState {
    pub projects: Vec<ProjectItem>,
    pub selected: usize,
}

impl ProjectSwitcherState {
    pub fn new(projects: Vec<ProjectItem>) -> Self {
        Self {
            selected: 0,
            projects,
        }
    }

    pub fn select_prev(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    pub fn select_next(&mut self) {
        if !self.projects.is_empty() && self.selected < self.projects.len() - 1 {
            self.selected += 1;
        }
    }

    pub fn selected_project(&self) -> Option<&ProjectItem> {
        self.projects.get(self.selected)
    }
}

/// State for the filter/search bar.
#[derive(Debug)]
pub struct FilterState {
    /// Whether the filter input bar is active (user is typing).
    pub editing: bool,
    /// The current filter query text.
    pub query: String,
    /// The confirmed/applied filter query (what's actually filtering).
    pub applied_query: String,
}

impl FilterState {
    pub fn new() -> Self {
        Self {
            editing: false,
            query: String::new(),
            applied_query: String::new(),
        }
    }

    /// Start editing the filter (open the input bar).
    pub fn start_editing(&mut self) {
        self.editing = true;
        self.query = self.applied_query.clone();
    }

    /// Confirm the current query (Enter).
    pub fn confirm(&mut self) {
        self.applied_query = self.query.clone();
        self.editing = false;
    }

    /// Cancel editing and clear the filter (Esc).
    pub fn cancel(&mut self) {
        self.editing = false;
        self.query.clear();
        self.applied_query.clear();
    }

    /// Returns true if a filter is active (non-empty applied query).
    pub fn is_active(&self) -> bool {
        !self.applied_query.is_empty()
    }

    /// Returns true if the filter input bar is being edited.
    pub fn is_editing(&self) -> bool {
        self.editing
    }

    /// Check if a title matches the filter (case-insensitive).
    #[allow(dead_code)]
    pub fn matches(&self, title: &str) -> bool {
        if self.applied_query.is_empty() {
            return true;
        }
        let query_lower = self.applied_query.to_lowercase();
        title.to_lowercase().contains(&query_lower)
    }

    /// Check if a title matches the current editing query (for live preview).
    #[allow(dead_code)]
    pub fn matches_editing(&self, title: &str) -> bool {
        let q = if self.editing {
            &self.query
        } else {
            &self.applied_query
        };
        if q.is_empty() {
            return true;
        }
        title.to_lowercase().contains(&q.to_lowercase())
    }
}

/// A pipeline run item for display in the list.
#[derive(Debug, Clone)]
pub struct PipelineRunItem {
    pub id: String,
    pub name: String,
    pub status: String,
    pub current_stage: Option<String>,
    pub iteration: u32,
    pub max_iterations: u32,
    pub created_at: String,
}

/// State for the pipeline list view.
#[derive(Debug)]
pub struct PipelineListState {
    pub runs: Vec<PipelineRunItem>,
    pub selected: usize,
}

impl PipelineListState {
    pub fn new() -> Self {
        Self {
            runs: Vec::new(),
            selected: 0,
        }
    }

    pub fn select_prev(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    pub fn select_next(&mut self) {
        if !self.runs.is_empty() && self.selected < self.runs.len() - 1 {
            self.selected += 1;
        }
    }

    pub fn selected_item(&self) -> Option<&PipelineRunItem> {
        self.runs.get(self.selected)
    }
}

/// A pipeline stage item for display.
#[derive(Debug, Clone)]
pub struct PipelineStageItem {
    pub stage_type: String,
    pub iteration: u32,
    pub status: String,
}

/// State for the pipeline detail view.
#[derive(Debug)]
pub struct PipelineDetailState {
    pub run: PipelineRunItem,
    pub stages: Vec<PipelineStageItem>,
    pub selected_stage: usize,
    #[allow(dead_code)]
    pub output_lines: Vec<String>, // Deprecated: kept for backward compat, use stage_outputs
    pub stage_outputs: std::collections::HashMap<String, Vec<String>>, // Key: "plan_1", "implement_1", etc.
    pub is_streaming: bool,
    pub scroll_offset: u16,
    pub current_streaming_stage: Option<(String, u32)>, // Track current stage during streaming (stage_type, iteration)
}

impl PipelineDetailState {
    pub fn new(run: PipelineRunItem) -> Self {
        Self {
            run,
            stages: Vec::new(),
            selected_stage: 0,
            output_lines: Vec::new(),
            stage_outputs: std::collections::HashMap::new(),
            is_streaming: false,
            scroll_offset: 0,
            current_streaming_stage: None,
        }
    }

    /// Generate a unique key for a stage: "stage_type_iteration"
    pub fn stage_key(stage_type: &str, iteration: u32) -> String {
        format!("{}_{}", stage_type, iteration)
    }

    /// Append output to a specific stage's output buffer.
    pub fn append_stage_output(&mut self, stage_type: &str, iteration: u32, text: String) {
        let key = Self::stage_key(stage_type, iteration);
        self.stage_outputs.entry(key).or_default().push(text);
    }

    /// Get the output lines for the currently selected stage.
    pub fn current_stage_output(&self) -> &[String] {
        if let Some(stage) = self.stages.get(self.selected_stage) {
            let key = Self::stage_key(&stage.stage_type, stage.iteration);
            if let Some(lines) = self.stage_outputs.get(&key) {
                return lines;
            }
        }
        &[]
    }

    pub fn select_prev_stage(&mut self) {
        if self.selected_stage > 0 {
            self.selected_stage -= 1;
            self.scroll_offset = 0; // Reset scroll when changing stages
        }
    }

    pub fn select_next_stage(&mut self) {
        if !self.stages.is_empty() && self.selected_stage < self.stages.len() - 1 {
            self.selected_stage += 1;
            self.scroll_offset = 0; // Reset scroll when changing stages
        }
    }

    #[allow(dead_code)]
    pub fn append_output(&mut self, line: String) {
        self.output_lines.push(line);
    }

    pub fn scroll_up(&mut self) {
        if self.scroll_offset > 0 {
            self.scroll_offset -= 1;
        }
    }

    pub fn scroll_down(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_add(1);
    }
}

/// State for the new pipeline dialog.
#[derive(Debug)]
pub struct PipelineNewState {
    pub name_input: String,
    pub goal_input: String,
    /// 0 = name, 1 = goal
    pub focused_field: usize,
}

impl PipelineNewState {
    pub fn new() -> Self {
        Self {
            name_input: String::new(),
            goal_input: String::new(),
            focused_field: 0,
        }
    }

    pub fn toggle_focus(&mut self) {
        self.focused_field = if self.focused_field == 0 { 1 } else { 0 };
    }
}

/// A pending pipeline question shown in the TUI.
#[derive(Debug, Clone)]
pub struct PendingQuestion {
    pub question_id: String,
    pub pipeline_run_id: String,
    pub question: String,
    pub context: Option<String>,
}

/// State for the pipeline question overlay dialog.
#[derive(Debug, Clone)]
pub struct PipelineQuestionOverlayState {
    pub questions: Vec<PendingQuestion>,
    pub current_index: usize,
    pub answer_input: String,
}

impl PipelineQuestionOverlayState {
    pub fn new(questions: Vec<PendingQuestion>) -> Self {
        Self {
            questions,
            current_index: 0,
            answer_input: String::new(),
        }
    }

    pub fn current_question(&self) -> Option<&PendingQuestion> {
        self.questions.get(self.current_index)
    }

    pub fn advance(&mut self) {
        self.answer_input.clear();
        self.current_index += 1;
    }

    pub fn is_done(&self) -> bool {
        self.current_index >= self.questions.len()
    }

    pub fn remaining(&self) -> usize {
        self.questions.len().saturating_sub(self.current_index)
    }
}

/// Whether the approval overlay is for plan approval or final approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalKind {
    Plan,
    Final,
}

/// State for the pipeline approval overlay (plan or final).
#[derive(Debug, Clone)]
pub struct PipelineApprovalOverlayState {
    pub pipeline_run_id: String,
    pub kind: ApprovalKind,
    pub summary: String,
    /// true when user pressed 'r' and is typing rejection feedback
    pub rejecting: bool,
    pub feedback_input: String,
    pub scroll_offset: u16,
}

impl PipelineApprovalOverlayState {
    pub fn new(pipeline_run_id: String, kind: ApprovalKind, summary: String) -> Self {
        Self {
            pipeline_run_id,
            kind,
            summary,
            rejecting: false,
            feedback_input: String::new(),
            scroll_offset: 0,
        }
    }
}

/// Overlay that can be displayed on top of the current view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overlay {
    Help,
    ProjectSwitcher,
    PipelineQuestion,
    PipelineApproval,
}

/// The active view in the TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Specs,
    Plan,
    Execute,
    Logs,
    Pipeline,
}

impl View {
    pub fn label(&self) -> &'static str {
        match self {
            View::Specs => "Specs",
            View::Plan => "Plan",
            View::Execute => "Execute",
            View::Logs => "Logs",
            View::Pipeline => "Pipeline",
        }
    }

    pub fn all() -> &'static [View] {
        &[
            View::Specs,
            View::Plan,
            View::Execute,
            View::Logs,
            View::Pipeline,
        ]
    }

    pub fn index(&self) -> usize {
        match self {
            View::Specs => 0,
            View::Plan => 1,
            View::Execute => 2,
            View::Logs => 3,
            View::Pipeline => 4,
        }
    }

    pub fn from_index(index: usize) -> Self {
        match index {
            0 => View::Specs,
            1 => View::Plan,
            2 => View::Execute,
            3 => View::Logs,
            4 => View::Pipeline,
            _ => View::Specs,
        }
    }
}

/// Connection state to the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonState {
    Connected,
    Disconnected,
    Connecting,
}

impl DaemonState {
    pub fn label(&self) -> &'static str {
        match self {
            DaemonState::Connected => "Connected",
            DaemonState::Disconnected => "Disconnected",
            DaemonState::Connecting => "Connecting...",
        }
    }
}

/// Main application state for the TUI.
pub struct App {
    /// Current active view.
    pub current_view: View,
    /// Project name.
    pub project: String,
    /// Daemon connection state.
    pub daemon_state: DaemonState,
    /// Whether the app should exit.
    pub should_quit: bool,
    /// Status message for the status bar.
    pub status_message: String,
    /// Currently active overlay (if any).
    pub overlay: Option<Overlay>,
    /// Project switcher state (populated when overlay is open).
    pub project_switcher: Option<ProjectSwitcherState>,
    /// Filter/search state for the filter bar.
    pub filter_state: FilterState,
    /// Current wave number (from daemon status).
    pub current_wave: Option<u32>,
    /// Number of active agents (from daemon status).
    pub active_agent_count: u32,
    /// Specs list view state.
    pub specs_list: SpecsListState,
    /// Active spec name input state (if prompting for new spec name).
    pub spec_name_input: Option<SpecNameInputState>,
    /// Active spec dialogue state (if in dialogue sub-view).
    pub spec_dialogue: Option<SpecDialogueState>,
    /// Active spec pager state (if in pager sub-view).
    pub spec_pager: Option<SpecPagerState>,
    /// Plan tree view state.
    pub plan_tree: PlanTreeState,
    /// Active plan generate dialog state (if open).
    pub plan_generate: Option<PlanGenerateState>,
    /// Active plan feedback input state (if open).
    pub plan_feedback: Option<PlanFeedbackState>,
    /// Active plan confirmation popup state (if open).
    pub plan_confirm: Option<PlanConfirmState>,
    /// Active plan node detail popup state (if open).
    pub plan_detail: Option<PlanDetailState>,
    /// Execute tree view state (left pane).
    pub execute_tree: ExecuteTreeState,
    /// Execute output pane state (right pane).
    pub execute_output: ExecuteOutputState,
    /// Active execute confirmation popup state (if open).
    pub execute_confirm: Option<PlanConfirmState>,
    /// Event subscription state for real-time updates.
    pub event_subscription: EventSubscriptionState,
    /// Full-screen logs view state.
    pub logs_view: LogsViewState,
    /// Current decomposition phase (replaces decomposition_in_progress bool).
    pub decomposition_phase: DecompositionPhase,
    /// Decomposition output pane state (plan view during decomposition).
    pub decomposition_output: DecompositionOutputState,
    /// Whether the user is viewing the decomposition output in full-screen mode.
    pub viewing_decomposition_output: bool,
    /// When the completion/failure banner was first shown (for auto-dismiss).
    pub completion_shown_at: Option<Instant>,
    /// Whether the user dismissed the inline streaming preview via Esc.
    pub streaming_preview_dismissed: bool,
    /// Timestamp of the last received decomposition streaming event (for stale detection).
    pub decomposition_last_event_at: Option<Instant>,
    /// Pipeline list view state.
    pub pipeline_list: PipelineListState,
    /// Pipeline detail view state (if viewing a specific run).
    pub pipeline_detail: Option<PipelineDetailState>,
    /// Pipeline new dialog state (if creating a new pipeline run).
    pub pipeline_new: Option<PipelineNewState>,
    /// Streaming request ID for an active pipeline (if streaming).
    pub pipeline_streaming_request_id: Option<String>,
    /// Ring buffer for pipeline agent output (capacity 1000).
    pub pipeline_output_buffer: VecDeque<String>,
    /// Scroll position for the pipeline output buffer.
    pub pipeline_output_scroll: usize,
    /// Whether the output pane auto-scrolls to the bottom.
    pub pipeline_auto_scroll: bool,
    /// Pending pipeline questions (accumulated for badge display and overlay).
    pub pipeline_pending_questions: Vec<PendingQuestion>,
    /// Pipeline question overlay dialog state (if open).
    pub pipeline_question_overlay: Option<PipelineQuestionOverlayState>,
    /// Pipeline approval overlay state (plan or final approval).
    pub pipeline_approval_overlay: Option<PipelineApprovalOverlayState>,
}

/// Truncate a string to at most `max_chars` characters, appending "..." if truncated.
/// Safe for multi-byte UTF-8 characters.
fn truncate_str(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_chars).collect();
        format!("{}...", truncated)
    }
}

impl App {
    /// Create a new App with the given project name.
    pub fn new(project: String) -> Self {
        Self {
            current_view: View::Specs,
            project,
            daemon_state: DaemonState::Disconnected,
            should_quit: false,
            status_message: String::new(),
            overlay: None,
            project_switcher: None,
            filter_state: FilterState::new(),
            current_wave: None,
            active_agent_count: 0,
            specs_list: SpecsListState::new(),
            spec_name_input: None,
            spec_dialogue: None,
            spec_pager: None,
            plan_tree: PlanTreeState::new(),
            plan_generate: None,
            plan_feedback: None,
            plan_confirm: None,
            plan_detail: None,
            execute_tree: ExecuteTreeState::new(),
            execute_output: ExecuteOutputState::new(),
            execute_confirm: None,
            event_subscription: EventSubscriptionState::new(),
            logs_view: LogsViewState::new(),
            decomposition_phase: DecompositionPhase::Idle,
            decomposition_output: DecompositionOutputState::new(),
            viewing_decomposition_output: false,
            completion_shown_at: None,
            streaming_preview_dismissed: false,
            decomposition_last_event_at: None,
            pipeline_list: PipelineListState::new(),
            pipeline_detail: None,
            pipeline_new: None,
            pipeline_streaming_request_id: None,
            pipeline_output_buffer: VecDeque::with_capacity(1000),
            pipeline_output_scroll: 0,
            pipeline_auto_scroll: true,
            pipeline_pending_questions: Vec::new(),
            pipeline_question_overlay: None,
            pipeline_approval_overlay: None,
        }
    }

    /// Switch to the next view (Tab).
    pub fn next_view(&mut self) {
        let idx = self.current_view.index();
        let next = (idx + 1) % View::all().len();
        self.current_view = View::from_index(next);
    }

    /// Switch to the previous view (Shift+Tab).
    pub fn prev_view(&mut self) {
        let idx = self.current_view.index();
        let prev = if idx == 0 {
            View::all().len() - 1
        } else {
            idx - 1
        };
        self.current_view = View::from_index(prev);
    }

    /// Switch to a specific view by number key (1-4).
    pub fn switch_view(&mut self, view: View) {
        self.current_view = view;
    }

    /// Mark daemon as connected and update status.
    pub fn set_connected(&mut self) {
        self.daemon_state = DaemonState::Connected;
        self.status_message.clear();
    }

    /// Mark daemon as disconnected.
    pub fn set_disconnected(&mut self, message: &str) {
        self.daemon_state = DaemonState::Disconnected;
        self.status_message = message.to_string();
    }

    /// Toggle an overlay. If the same overlay is already open, close it.
    /// If a different overlay is open, switch to the new one.
    pub fn toggle_overlay(&mut self, overlay: Overlay) {
        if self.overlay == Some(overlay) {
            self.overlay = None;
        } else {
            self.overlay = Some(overlay);
        }
    }

    /// Close any open overlay.
    pub fn close_overlay(&mut self) {
        if self.overlay == Some(Overlay::ProjectSwitcher) {
            self.project_switcher = None;
        }
        self.overlay = None;
    }

    /// Returns true if any overlay is currently shown.
    pub fn has_overlay(&self) -> bool {
        self.overlay.is_some()
    }

    /// Returns true if the filter input bar is being edited.
    pub fn in_filter_editing(&self) -> bool {
        self.filter_state.is_editing()
    }

    /// Open the project switcher overlay with the given project list.
    pub fn open_project_switcher(&mut self, projects: Vec<ProjectItem>) {
        // Pre-select the current project in the list
        let selected = projects
            .iter()
            .position(|p| p.name == self.project)
            .unwrap_or(0);
        let mut state = ProjectSwitcherState::new(projects);
        state.selected = selected;
        self.project_switcher = Some(state);
        self.overlay = Some(Overlay::ProjectSwitcher);
    }

    /// Switch to a different project, resetting all view state.
    pub async fn switch_project(&mut self, name: String, client: &mut SocketClient) {
        self.project = name;
        // Reset all view state
        self.specs_list = SpecsListState::new();
        self.spec_name_input = None;
        self.spec_dialogue = None;
        self.spec_pager = None;
        self.plan_tree = PlanTreeState::new();
        self.plan_generate = None;
        self.plan_feedback = None;
        self.plan_confirm = None;
        self.plan_detail = None;
        self.execute_tree = ExecuteTreeState::new();
        self.execute_output = ExecuteOutputState::new();
        self.execute_confirm = None;
        self.logs_view = LogsViewState::new();
        self.pipeline_list = PipelineListState::new();
        self.pipeline_detail = None;
        self.pipeline_new = None;
        self.pipeline_streaming_request_id = None;
        self.pipeline_output_buffer.clear();
        self.pipeline_output_scroll = 0;
        self.pipeline_auto_scroll = true;
        self.current_wave = None;
        self.active_agent_count = 0;
        // Close overlay
        self.project_switcher = None;
        self.overlay = None;
        // Reload data for the new project
        self.connect(client).await.ok();
        self.fetch_plan(client).await.ok();
        self.fetch_execute(client).await.ok();
        self.status_message = format!("Switched to project: {}", self.project);
    }

    /// Fetch the project list from the daemon.
    pub async fn fetch_projects(&mut self, client: &mut SocketClient) -> Result<Vec<ProjectItem>> {
        let resp = client
            .send_command("project.list", serde_json::json!({}))
            .await?;

        if resp.status == ResponseStatus::Ok {
            let projects = resp
                .data
                .get("projects")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .map(|p| ProjectItem {
                            name: p
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            path: p
                                .get("path")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            active_agent_count: p
                                .get("active_agent_count")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0)
                                as u32,
                        })
                        .collect()
                })
                .unwrap_or_default();
            Ok(projects)
        } else {
            Ok(Vec::new())
        }
    }

    /// Returns true if a spec dialogue is active.
    pub fn in_dialogue(&self) -> bool {
        self.spec_dialogue.is_some()
    }

    /// Enter spec dialogue mode for a given spec.
    pub fn enter_dialogue(&mut self, spec_name: String) {
        self.spec_dialogue = Some(SpecDialogueState::new(spec_name));
    }

    /// Exit spec dialogue mode and return to specs list.
    pub fn exit_dialogue(&mut self) {
        self.spec_dialogue = None;
    }

    /// Returns true if the spec content pager is active.
    pub fn in_pager(&self) -> bool {
        self.spec_pager.is_some()
    }

    /// Enter spec pager mode with the given spec name and content.
    pub fn enter_pager(&mut self, spec_name: String, content: String) {
        self.spec_pager = Some(SpecPagerState::new(spec_name, content));
    }

    /// Exit spec pager mode and return to specs list.
    pub fn exit_pager(&mut self) {
        self.spec_pager = None;
    }

    /// Returns true if any plan sub-view is active (generate, feedback, detail, confirm).
    pub fn in_plan_sub_view(&self) -> bool {
        self.plan_generate.is_some()
            || self.plan_feedback.is_some()
            || self.plan_confirm.is_some()
            || self.plan_detail.is_some()
    }

    /// Open the spec name input popup.
    pub fn open_spec_name_input(&mut self) {
        self.spec_name_input = Some(SpecNameInputState::new());
    }

    /// Close the spec name input popup.
    pub fn close_spec_name_input(&mut self) {
        self.spec_name_input = None;
    }

    /// Open the plan generate dialog with available approved specs.
    pub fn open_generate_dialog(&mut self) {
        // Gather approved spec names from the specs list
        let approved_specs: Vec<String> = self
            .specs_list
            .items
            .iter()
            .filter(|s| s.status == "approved")
            .map(|s| s.name.clone())
            .collect();
        self.plan_generate = Some(PlanGenerateState::new(approved_specs));
    }

    /// Open the plan feedback input.
    pub fn open_feedback_input(&mut self) {
        self.plan_feedback = Some(PlanFeedbackState::new());
    }

    /// Open a confirmation popup for the given action.
    pub fn open_confirm(&mut self, action: ConfirmAction) {
        self.plan_confirm = Some(PlanConfirmState { action });
    }

    /// Open the detail popup for the currently selected plan node.
    pub fn open_plan_detail(&mut self) {
        let visible = self.plan_tree.visible_nodes();
        if let Some(&(real_idx, _)) = visible.get(self.plan_tree.selected) {
            let node = &self.plan_tree.nodes[real_idx];
            self.plan_detail = Some(PlanDetailState {
                short_id: node.short_id.clone(),
                title: node.title.clone(),
                status: node.status.clone(),
                description: node.title.clone(),
                depends_on: node.depends_on.clone(),
                progress: node.progress.clone(),
                kind: node.kind.clone(),
                depth: node.depth,
                error_message: node.error_message.clone(),
            });
        }
    }

    /// Close any open plan sub-view.
    pub fn close_plan_sub_view(&mut self) {
        self.plan_generate = None;
        self.plan_feedback = None;
        self.plan_confirm = None;
        self.plan_detail = None;
    }

    /// Attempt to connect to the daemon and fetch initial state.
    pub async fn connect(&mut self, client: &mut SocketClient) -> Result<()> {
        self.daemon_state = DaemonState::Connecting;

        // Verify connection by sending a status command
        let resp = client
            .send_command(
                "exec.status",
                serde_json::json!({ "project_name": &self.project }),
            )
            .await;

        match resp {
            Ok(r) if r.status == ResponseStatus::Ok => {
                self.set_connected();
                // Parse wave and agent count from status response
                if let Some(wave) = r.data.get("current_wave").and_then(|v| v.as_u64()) {
                    self.current_wave = Some(wave as u32);
                }
                if let Some(count) = r.data.get("active_agents").and_then(|v| v.as_u64()) {
                    self.active_agent_count = count as u32;
                }
            }
            Ok(r) => {
                // Connected but command failed — still connected to daemon
                let msg = r.data.get("message").and_then(|v| v.as_str()).unwrap_or("");
                // NOT_FOUND for project is ok — daemon is running, project just not initialized
                self.set_connected();
                if !msg.is_empty() {
                    self.status_message = msg.to_string();
                }
            }
            Err(e) => {
                self.set_disconnected(&e.to_string());
                return Err(e);
            }
        }

        // Fetch specs list
        self.fetch_specs(client).await.ok();

        Ok(())
    }

    /// Fetch the plan tree from the daemon.
    pub async fn fetch_plan(&mut self, client: &mut SocketClient) -> Result<()> {
        let resp = client
            .send_command(
                "plan.show",
                serde_json::json!({ "project_name": &self.project }),
            )
            .await?;

        if resp.status == ResponseStatus::Ok {
            self.plan_tree.update_from_response(&resp.data);
        }

        Ok(())
    }

    /// Returns true if the execute output pane is streaming.
    pub fn in_execute_streaming(&self) -> bool {
        self.execute_output.is_streaming
    }

    /// Returns true if decomposition output is streaming.
    pub fn in_decomposition_streaming(&self) -> bool {
        self.decomposition_output.is_streaming
    }

    /// Returns true if decomposition is actively running (Starting, Analyzing, or BuildingItems).
    pub fn is_decomposition_active(&self) -> bool {
        matches!(
            self.decomposition_phase,
            DecompositionPhase::Starting
                | DecompositionPhase::Analyzing
                | DecompositionPhase::BuildingItems
        )
    }

    /// Count plan items from the plan tree by depth (1=epic, 2=story, 3=task).
    pub fn count_plan_items(&self) -> DecompositionSummary {
        let mut epic_count = 0u32;
        let mut story_count = 0u32;
        let mut task_count = 0u32;
        for node in &self.plan_tree.nodes {
            match node.depth {
                1 => epic_count += 1,
                2 => story_count += 1,
                3 => task_count += 1,
                _ => {}
            }
        }
        DecompositionSummary {
            epic_count,
            story_count,
            task_count,
        }
    }

    /// Returns true if any execute sub-view (confirm popup) is active.
    pub fn in_execute_sub_view(&self) -> bool {
        self.execute_confirm.is_some()
    }

    /// Open a confirmation popup for an execute action.
    pub fn open_execute_confirm(&mut self, action: ConfirmAction) {
        self.execute_confirm = Some(PlanConfirmState { action });
    }

    /// Close the execute confirmation popup.
    pub fn close_execute_confirm(&mut self) {
        self.execute_confirm = None;
    }

    /// Returns true if the logs view is actively streaming.
    pub fn in_logs_streaming(&self) -> bool {
        self.current_view == View::Logs && self.logs_view.is_streaming
    }

    /// Returns true if the logs view has a task loaded (active sub-view).
    pub fn in_logs_view(&self) -> bool {
        self.current_view == View::Logs && self.logs_view.task_id.is_some()
    }

    /// Enter the logs view for a specific task, switching to Logs tab.
    #[allow(dead_code)] // Used by future integration (e.g., double-click from execute view)
    pub fn enter_logs_for_task(&mut self, task_id: String, task_status: String) {
        self.logs_view.task_id = Some(task_id);
        self.logs_view.task_status = Some(task_status);
        self.current_view = View::Logs;
    }

    /// Exit the logs view back to Execute view.
    pub fn exit_logs_view(&mut self) {
        self.logs_view.clear();
        self.current_view = View::Execute;
    }

    /// Returns true if viewing pipeline detail.
    pub fn in_pipeline_detail(&self) -> bool {
        self.pipeline_detail.is_some()
    }

    /// Returns true if the new pipeline dialog is open.
    pub fn in_pipeline_new(&self) -> bool {
        self.pipeline_new.is_some()
    }

    /// Open the new pipeline dialog.
    pub fn open_pipeline_new(&mut self) {
        self.pipeline_new = Some(PipelineNewState::new());
    }

    /// Close the new pipeline dialog.
    pub fn close_pipeline_new(&mut self) {
        self.pipeline_new = None;
    }

    /// Returns true if the pipeline question overlay is open.
    pub fn in_pipeline_question_overlay(&self) -> bool {
        self.pipeline_question_overlay.is_some()
    }

    /// Open the pipeline question overlay with current pending questions.
    pub fn open_pipeline_question_overlay(&mut self) {
        if !self.pipeline_pending_questions.is_empty() {
            let questions = self.pipeline_pending_questions.clone();
            self.pipeline_question_overlay = Some(PipelineQuestionOverlayState::new(questions));
            self.overlay = Some(Overlay::PipelineQuestion);
        }
    }

    /// Close the pipeline question overlay (dismiss temporarily).
    pub fn close_pipeline_question_overlay(&mut self) {
        self.pipeline_question_overlay = None;
        if self.overlay == Some(Overlay::PipelineQuestion) {
            self.overlay = None;
        }
    }

    /// Remove a question from the pending list by question_id (after answering).
    pub fn remove_pending_question(&mut self, question_id: &str) {
        self.pipeline_pending_questions.retain(|q| q.question_id != question_id);
    }

    /// Get the number of pending questions for a specific pipeline run.
    pub fn pending_question_count_for_run(&self, run_id: &str) -> usize {
        self.pipeline_pending_questions.iter().filter(|q| q.pipeline_run_id == run_id).count()
    }

    /// Returns true if the pipeline approval overlay is open.
    pub fn in_pipeline_approval_overlay(&self) -> bool {
        self.pipeline_approval_overlay.is_some()
    }

    /// Open the pipeline approval overlay.
    pub fn open_pipeline_approval_overlay(
        &mut self,
        pipeline_run_id: String,
        kind: ApprovalKind,
        summary: String,
    ) {
        self.pipeline_approval_overlay = Some(PipelineApprovalOverlayState::new(
            pipeline_run_id,
            kind,
            summary,
        ));
        self.overlay = Some(Overlay::PipelineApproval);
    }

    /// Close the pipeline approval overlay.
    pub fn close_pipeline_approval_overlay(&mut self) {
        self.pipeline_approval_overlay = None;
        if self.overlay == Some(Overlay::PipelineApproval) {
            self.overlay = None;
        }
    }

    /// Returns true if a pipeline streaming session is active.
    pub fn in_pipeline_streaming(&self) -> bool {
        self.pipeline_streaming_request_id.is_some()
    }

    /// Fetch the execute tree from the daemon.
    pub async fn fetch_execute(&mut self, client: &mut SocketClient) -> Result<()> {
        let resp = client
            .send_command(
                "exec.status",
                serde_json::json!({ "project_name": &self.project }),
            )
            .await?;

        if resp.status == ResponseStatus::Ok {
            self.execute_tree.update_from_response(&resp.data);
            // Auto-select the first running task on data load
            self.execute_tree.auto_select_running_task();
        }

        Ok(())
    }

    /// Subscribe to daemon events for real-time updates.
    pub async fn subscribe_events(&mut self, client: &mut SocketClient) -> Result<()> {
        if self.event_subscription.subscribed {
            return Ok(());
        }

        let resp = client
            .send_command("subscribe", serde_json::json!({}))
            .await?;

        if resp.status == ResponseStatus::Ok {
            self.event_subscription.subscribed = true;
        }

        Ok(())
    }

    /// Returns true if subscribed to daemon events.
    pub fn is_event_subscribed(&self) -> bool {
        self.event_subscription.subscribed
    }

    /// Apply a StatusChange event to the execute tree.
    /// The event contains item_id (UUID), but we need to find the node by matching.
    /// Since we don't have UUIDs in the tree (only short_ids), we'll need to
    /// refresh the execute tree to pick up status changes.
    pub fn apply_status_change(&mut self, _item_id: &str, new_status: &str, item_type: &str) {
        // For now, mark that we need to refresh.
        // The event_loop will call fetch_execute() to reload tree state.
        // We set the status message to show the event.
        self.status_message = format!("{} status → {}", item_type, new_status);
    }

    /// Apply an AgentOutput event to the execute output pane.
    pub fn apply_agent_output(&mut self, task_id: &str, line: &str) {
        // Only append if we're currently viewing this task's output
        if let Some(ref current_task) = self.execute_output.task_id {
            // The task_id in events is a UUID, but our display uses short_ids.
            // We can't match directly. However, if we're streaming output for a task,
            // we can check if this is the currently selected task by matching
            // the task_id against any known mapping.
            // For now, if we're in streaming mode, accept all output for the current task.
            // The daemon's exec.log already handles task-specific filtering.
            let _ = current_task; // We'll rely on the exec.log streaming for task output.
        }

        // Store the line in a buffer that can be used when the task is selected
        // For now, just ignore - the exec.log streaming handles per-task output.
        // AgentOutput events are primarily useful for updating the output pane
        // when a task is auto-selected (e.g., first in_progress task).
        let _ = (task_id, line);
    }

    /// Apply a StoryCompleted event — store the MR URL and refresh tree.
    pub fn apply_story_completed(&mut self, _story_id: &str, mr_url: Option<&str>) {
        if let Some(url) = mr_url {
            self.status_message = format!("MR created: {}", url);
        }
    }

    /// Fetch the specs list from the daemon.
    pub async fn fetch_specs(&mut self, client: &mut SocketClient) -> Result<()> {
        let resp = client
            .send_command(
                "spec.list",
                serde_json::json!({ "project_name": &self.project }),
            )
            .await?;

        if resp.status == ResponseStatus::Ok {
            self.specs_list.update_from_response(&resp.data);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_view_cycle() {
        let mut app = App::new("test".to_string());
        assert_eq!(app.current_view, View::Specs);

        app.next_view();
        assert_eq!(app.current_view, View::Plan);

        app.next_view();
        assert_eq!(app.current_view, View::Execute);

        app.next_view();
        assert_eq!(app.current_view, View::Logs);

        app.next_view();
        assert_eq!(app.current_view, View::Pipeline);

        app.next_view();
        assert_eq!(app.current_view, View::Specs);
    }

    #[test]
    fn test_view_cycle_backward() {
        let mut app = App::new("test".to_string());
        assert_eq!(app.current_view, View::Specs);

        app.prev_view();
        assert_eq!(app.current_view, View::Pipeline);

        app.prev_view();
        assert_eq!(app.current_view, View::Logs);

        app.prev_view();
        assert_eq!(app.current_view, View::Execute);
    }

    #[test]
    fn test_switch_view_by_number() {
        let mut app = App::new("test".to_string());
        app.switch_view(View::Execute);
        assert_eq!(app.current_view, View::Execute);

        app.switch_view(View::Specs);
        assert_eq!(app.current_view, View::Specs);
    }

    #[test]
    fn test_daemon_state_transitions() {
        let mut app = App::new("test".to_string());
        assert_eq!(app.daemon_state, DaemonState::Disconnected);

        app.set_connected();
        assert_eq!(app.daemon_state, DaemonState::Connected);
        assert!(app.status_message.is_empty());

        app.set_disconnected("connection lost");
        assert_eq!(app.daemon_state, DaemonState::Disconnected);
        assert_eq!(app.status_message, "connection lost");
    }

    #[test]
    fn test_view_labels() {
        assert_eq!(View::Specs.label(), "Specs");
        assert_eq!(View::Plan.label(), "Plan");
        assert_eq!(View::Execute.label(), "Execute");
        assert_eq!(View::Logs.label(), "Logs");
    }

    #[test]
    fn test_daemon_state_labels() {
        assert_eq!(DaemonState::Connected.label(), "Connected");
        assert_eq!(DaemonState::Disconnected.label(), "Disconnected");
        assert_eq!(DaemonState::Connecting.label(), "Connecting...");
    }

    #[test]
    fn test_toggle_overlay() {
        let mut app = App::new("test".to_string());
        assert_eq!(app.overlay, None);

        app.toggle_overlay(Overlay::Help);
        assert_eq!(app.overlay, Some(Overlay::Help));

        // Toggle same overlay closes it
        app.toggle_overlay(Overlay::Help);
        assert_eq!(app.overlay, None);
    }

    #[test]
    fn test_toggle_overlay_switches() {
        let mut app = App::new("test".to_string());
        app.toggle_overlay(Overlay::Help);
        assert_eq!(app.overlay, Some(Overlay::Help));

        // Toggle different overlay switches to it
        app.toggle_overlay(Overlay::ProjectSwitcher);
        assert_eq!(app.overlay, Some(Overlay::ProjectSwitcher));
    }

    #[test]
    fn test_close_overlay() {
        let mut app = App::new("test".to_string());
        app.toggle_overlay(Overlay::Help);
        assert!(app.has_overlay());

        app.close_overlay();
        assert!(!app.has_overlay());
        assert_eq!(app.overlay, None);
    }

    #[test]
    fn test_initial_wave_and_agents() {
        let app = App::new("test".to_string());
        assert_eq!(app.current_wave, None);
        assert_eq!(app.active_agent_count, 0);
    }

    // --- SpecsListState tests ---

    #[test]
    fn test_specs_list_initial_state() {
        let state = SpecsListState::new();
        assert!(state.items.is_empty());
        assert_eq!(state.selected, 0);
        assert!(state.selected_item().is_none());
    }

    #[test]
    fn test_specs_list_navigation() {
        let mut state = SpecsListState::new();
        state.items = vec![
            SpecItem {
                name: "a".to_string(),
                status: "draft".to_string(),
                session_active: false,
                created_at: "2026-01-01".to_string(),
            },
            SpecItem {
                name: "b".to_string(),
                status: "approved".to_string(),
                session_active: false,
                created_at: "2026-01-02".to_string(),
            },
        ];

        assert_eq!(state.selected, 0);
        assert_eq!(state.selected_item().unwrap().name, "a");

        state.select_next();
        assert_eq!(state.selected, 1);
        assert_eq!(state.selected_item().unwrap().name, "b");

        // Can't go past end
        state.select_next();
        assert_eq!(state.selected, 1);

        state.select_prev();
        assert_eq!(state.selected, 0);

        // Can't go past start
        state.select_prev();
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn test_specs_list_navigation_empty() {
        let mut state = SpecsListState::new();
        // Navigation on empty list should not panic
        state.select_next();
        assert_eq!(state.selected, 0);
        state.select_prev();
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn test_specs_list_update_from_response() {
        let mut state = SpecsListState::new();
        let data = serde_json::json!({
            "specs": [
                {
                    "name": "auth-spec",
                    "status": "draft",
                    "session_active": true,
                    "created_at": "2026-02-07T15:30:00+00:00"
                },
                {
                    "name": "payment-spec",
                    "status": "approved",
                    "session_active": false,
                    "created_at": "2026-02-07T16:00:00+00:00"
                }
            ]
        });

        state.update_from_response(&data);
        assert_eq!(state.items.len(), 2);
        assert_eq!(state.items[0].name, "auth-spec");
        assert_eq!(state.items[0].status, "draft");
        assert!(state.items[0].session_active);
        assert_eq!(state.items[1].name, "payment-spec");
        assert_eq!(state.items[1].status, "approved");
    }

    #[test]
    fn test_specs_list_update_clamps_selection() {
        let mut state = SpecsListState::new();
        state.items = vec![
            SpecItem {
                name: "a".to_string(),
                status: "draft".to_string(),
                session_active: false,
                created_at: "".to_string(),
            },
            SpecItem {
                name: "b".to_string(),
                status: "draft".to_string(),
                session_active: false,
                created_at: "".to_string(),
            },
            SpecItem {
                name: "c".to_string(),
                status: "draft".to_string(),
                session_active: false,
                created_at: "".to_string(),
            },
        ];
        state.selected = 2;

        // Update with fewer items should clamp selection
        let data = serde_json::json!({
            "specs": [
                { "name": "only-one", "status": "draft", "session_active": false, "created_at": "" }
            ]
        });
        state.update_from_response(&data);
        assert_eq!(state.items.len(), 1);
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn test_specs_list_update_empty_response() {
        let mut state = SpecsListState::new();
        let data = serde_json::json!({ "specs": [] });
        state.update_from_response(&data);
        assert!(state.items.is_empty());
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn test_initial_specs_list() {
        let app = App::new("test".to_string());
        assert!(app.specs_list.items.is_empty());
        assert_eq!(app.specs_list.selected, 0);
    }

    // --- SpecDialogueState tests ---

    #[test]
    fn test_dialogue_initial_state() {
        let state = SpecDialogueState::new("test-spec".to_string());
        assert!(state.messages.is_empty());
        assert!(state.input.is_empty());
        assert_eq!(state.session_state, DialogueSessionState::Streaming);
        assert_eq!(state.spec_name, "test-spec");
        assert!(state.spec_id.is_none());
        assert!(state.request_id.is_none());
        assert_eq!(state.scroll_offset, 0);
    }

    #[test]
    fn test_dialogue_add_claude_message() {
        let mut state = SpecDialogueState::new("test".to_string());
        state.add_claude_message("Hello!".to_string());
        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.messages[0].sender, "Claude");
        assert_eq!(state.messages[0].text, "Hello!");
    }

    #[test]
    fn test_dialogue_add_user_message() {
        let mut state = SpecDialogueState::new("test".to_string());
        state.add_user_message("My answer".to_string());
        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.messages[0].sender, "You");
        assert_eq!(state.messages[0].text, "My answer");
    }

    #[test]
    fn test_dialogue_claude_message_coalescing() {
        let mut state = SpecDialogueState::new("test".to_string());
        state.add_claude_message("Part 1".to_string());
        state.add_claude_message(" Part 2".to_string());
        // Contiguous Claude messages should be merged
        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.messages[0].text, "Part 1 Part 2");
    }

    #[test]
    fn test_dialogue_message_interleaving() {
        let mut state = SpecDialogueState::new("test".to_string());
        state.add_claude_message("Question?".to_string());
        state.add_user_message("Answer!".to_string());
        state.add_claude_message("Thanks!".to_string());
        assert_eq!(state.messages.len(), 3);
        assert_eq!(state.messages[0].sender, "Claude");
        assert_eq!(state.messages[1].sender, "You");
        assert_eq!(state.messages[2].sender, "Claude");
    }

    #[test]
    fn test_dialogue_scroll() {
        let mut state = SpecDialogueState::new("test".to_string());
        assert_eq!(state.scroll_offset, 0);

        state.scroll_up();
        assert_eq!(state.scroll_offset, 1);

        state.scroll_up();
        assert_eq!(state.scroll_offset, 2);

        state.scroll_down();
        assert_eq!(state.scroll_offset, 1);

        state.scroll_down();
        assert_eq!(state.scroll_offset, 0);

        // Can't go below 0
        state.scroll_down();
        assert_eq!(state.scroll_offset, 0);
    }

    #[test]
    fn test_dialogue_auto_scroll_on_new_message() {
        let mut state = SpecDialogueState::new("test".to_string());
        state.scroll_offset = 5;
        state.add_claude_message("New message".to_string());
        // Auto-scroll resets offset to 0
        assert_eq!(state.scroll_offset, 0);
    }

    // --- SpecPagerState tests ---

    #[test]
    fn test_pager_initial_state() {
        let state = SpecPagerState::new("test-spec".to_string(), "line1\nline2\nline3".to_string());
        assert_eq!(state.spec_name, "test-spec");
        assert_eq!(state.lines.len(), 3);
        assert_eq!(state.total_lines, 3);
        assert_eq!(state.scroll_offset, 0);
    }

    #[test]
    fn test_pager_scroll_down() {
        let mut state = SpecPagerState::new("test".to_string(), "a\nb\nc\nd\ne".to_string());
        assert_eq!(state.scroll_offset, 0);

        state.scroll_down(3); // 5 lines, 3 visible → max_scroll = 2
        assert_eq!(state.scroll_offset, 1);

        state.scroll_down(3);
        assert_eq!(state.scroll_offset, 2);

        // Can't scroll past max
        state.scroll_down(3);
        assert_eq!(state.scroll_offset, 2);
    }

    #[test]
    fn test_pager_scroll_up() {
        let mut state = SpecPagerState::new("test".to_string(), "a\nb\nc\nd\ne".to_string());
        state.scroll_offset = 2;

        state.scroll_up();
        assert_eq!(state.scroll_offset, 1);

        state.scroll_up();
        assert_eq!(state.scroll_offset, 0);

        // Can't go below 0
        state.scroll_up();
        assert_eq!(state.scroll_offset, 0);
    }

    #[test]
    fn test_pager_page_down() {
        let mut state = SpecPagerState::new(
            "test".to_string(),
            (0..20)
                .map(|i| format!("line {}", i))
                .collect::<Vec<_>>()
                .join("\n"),
        );

        state.page_down(10); // half of 10 = 5
        assert_eq!(state.scroll_offset, 5);

        state.page_down(10);
        assert_eq!(state.scroll_offset, 10); // max_scroll = 20-10 = 10
    }

    #[test]
    fn test_pager_page_up() {
        let mut state = SpecPagerState::new(
            "test".to_string(),
            (0..20)
                .map(|i| format!("line {}", i))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        state.scroll_offset = 10;

        state.page_up(10); // half of 10 = 5
        assert_eq!(state.scroll_offset, 5);

        state.page_up(10);
        assert_eq!(state.scroll_offset, 0);
    }

    #[test]
    fn test_pager_scroll_to_top() {
        let mut state = SpecPagerState::new("test".to_string(), "a\nb\nc".to_string());
        state.scroll_offset = 2;

        state.scroll_to_top();
        assert_eq!(state.scroll_offset, 0);
    }

    #[test]
    fn test_pager_scroll_to_bottom() {
        let mut state = SpecPagerState::new("test".to_string(), "a\nb\nc\nd\ne".to_string());

        state.scroll_to_bottom(3); // 5 lines, 3 visible → scroll to 2
        assert_eq!(state.scroll_offset, 2);
    }

    #[test]
    fn test_pager_empty_content() {
        let state = SpecPagerState::new("test".to_string(), String::new());
        assert_eq!(state.total_lines, 0);
        assert!(state.lines.is_empty());
    }

    #[test]
    fn test_app_pager_lifecycle() {
        let mut app = App::new("test".to_string());
        assert!(!app.in_pager());
        assert!(app.spec_pager.is_none());

        app.enter_pager("my-spec".to_string(), "content here".to_string());
        assert!(app.in_pager());
        assert_eq!(app.spec_pager.as_ref().unwrap().spec_name, "my-spec");

        app.exit_pager();
        assert!(!app.in_pager());
        assert!(app.spec_pager.is_none());
    }

    // --- PlanTreeState tests ---

    #[test]
    fn test_plan_tree_initial_state() {
        let state = PlanTreeState::new();
        assert!(state.nodes.is_empty());
        assert_eq!(state.selected, 0);
        assert!(!state.hide_verify);
        assert!(state.wave_number.is_none());
    }

    #[test]
    fn test_plan_tree_update_from_response() {
        let mut state = PlanTreeState::new();
        let data = serde_json::json!({
            "wave_number": 1,
            "status": "approved",
            "epics": [
                {
                    "short_id": "W1-E1",
                    "title": "Auth Epic",
                    "status": "pending",
                    "stories": [
                        {
                            "short_id": "W1-S1",
                            "title": "Login Story",
                            "status": "ready",
                            "progress": "0/2",
                            "depends_on": [],
                            "tasks": [
                                {
                                    "short_id": "W1-T1",
                                    "title": "Implement login",
                                    "status": "pending",
                                    "kind": "impl"
                                },
                                {
                                    "short_id": "W1-T1v",
                                    "title": "Verify login",
                                    "status": "pending",
                                    "kind": "verify"
                                }
                            ]
                        }
                    ]
                }
            ]
        });

        state.update_from_response(&data);
        assert_eq!(state.wave_number, Some(1));
        assert_eq!(state.wave_status.as_deref(), Some("approved"));
        // wave + epic + story + 2 tasks = 5
        assert_eq!(state.nodes.len(), 5);
        assert_eq!(state.nodes[0].depth, 0); // wave
        assert_eq!(state.nodes[1].depth, 1); // epic
        assert_eq!(state.nodes[2].depth, 2); // story
        assert_eq!(state.nodes[3].depth, 3); // task impl
        assert_eq!(state.nodes[4].depth, 3); // task verify
    }

    #[test]
    fn test_plan_tree_navigation() {
        let mut state = PlanTreeState::new();
        let data = serde_json::json!({
            "wave_number": 1,
            "status": "approved",
            "epics": [{
                "short_id": "W1-E1",
                "title": "Epic",
                "status": "pending",
                "stories": [{
                    "short_id": "W1-S1",
                    "title": "Story",
                    "status": "pending",
                    "progress": "0/1",
                    "depends_on": [],
                    "tasks": [{
                        "short_id": "W1-T1",
                        "title": "Task",
                        "status": "pending",
                        "kind": "impl"
                    }]
                }]
            }]
        });
        state.update_from_response(&data);

        assert_eq!(state.selected, 0);
        state.select_next();
        assert_eq!(state.selected, 1);
        state.select_next();
        assert_eq!(state.selected, 2);
        state.select_next();
        assert_eq!(state.selected, 3);
        // Can't go past end
        state.select_next();
        assert_eq!(state.selected, 3);

        state.select_prev();
        assert_eq!(state.selected, 2);
        state.selected = 0;
        // Can't go below 0
        state.select_prev();
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn test_plan_tree_collapse() {
        let mut state = PlanTreeState::new();
        let data = serde_json::json!({
            "wave_number": 1,
            "status": "approved",
            "epics": [{
                "short_id": "W1-E1",
                "title": "Epic",
                "status": "pending",
                "stories": [{
                    "short_id": "W1-S1",
                    "title": "Story",
                    "status": "pending",
                    "progress": "0/1",
                    "depends_on": [],
                    "tasks": [{
                        "short_id": "W1-T1",
                        "title": "Task",
                        "status": "pending",
                        "kind": "impl"
                    }]
                }]
            }]
        });
        state.update_from_response(&data);

        // All 4 nodes visible
        assert_eq!(state.visible_nodes().len(), 4);

        // Collapse the epic (index 1 in visible)
        state.selected = 1;
        state.toggle_collapse();
        assert!(state.nodes[1].collapsed);
        // Now only wave + epic visible (story and task hidden)
        assert_eq!(state.visible_nodes().len(), 2);

        // Uncollapse
        state.toggle_collapse();
        assert!(!state.nodes[1].collapsed);
        assert_eq!(state.visible_nodes().len(), 4);
    }

    #[test]
    fn test_plan_tree_toggle_verify() {
        let mut state = PlanTreeState::new();
        let data = serde_json::json!({
            "wave_number": 1,
            "status": "approved",
            "epics": [{
                "short_id": "W1-E1",
                "title": "Epic",
                "status": "pending",
                "stories": [{
                    "short_id": "W1-S1",
                    "title": "Story",
                    "status": "pending",
                    "progress": "0/2",
                    "depends_on": [],
                    "tasks": [
                        {
                            "short_id": "W1-T1",
                            "title": "Impl",
                            "status": "pending",
                            "kind": "impl"
                        },
                        {
                            "short_id": "W1-T1v",
                            "title": "Verify",
                            "status": "pending",
                            "kind": "verify"
                        }
                    ]
                }]
            }]
        });
        state.update_from_response(&data);

        // 5 nodes visible: wave, epic, story, impl task, verify task
        assert_eq!(state.visible_nodes().len(), 5);

        state.toggle_verify_visibility();
        assert!(state.hide_verify);
        // Verify task hidden: 4 visible
        assert_eq!(state.visible_nodes().len(), 4);

        state.toggle_verify_visibility();
        assert!(!state.hide_verify);
        assert_eq!(state.visible_nodes().len(), 5);
    }

    #[test]
    fn test_plan_tree_empty_response() {
        let mut state = PlanTreeState::new();
        let data = serde_json::json!({
            "wave_number": 1,
            "status": "in_progress",
            "epics": []
        });
        state.update_from_response(&data);
        // Only wave node
        assert_eq!(state.nodes.len(), 1);
        assert_eq!(state.nodes[0].depth, 0);
    }

    #[test]
    fn test_plan_tree_story_dependencies() {
        let mut state = PlanTreeState::new();
        let data = serde_json::json!({
            "wave_number": 1,
            "status": "approved",
            "epics": [{
                "short_id": "W1-E1",
                "title": "Epic",
                "status": "pending",
                "stories": [{
                    "short_id": "W1-S1",
                    "title": "Story",
                    "status": "pending",
                    "progress": "0/0",
                    "depends_on": ["W1-S2", "W1-S3"],
                    "tasks": []
                }]
            }]
        });
        state.update_from_response(&data);

        // Story node at index 2
        assert_eq!(state.nodes[2].depends_on, vec!["W1-S2", "W1-S3"]);
    }

    #[test]
    fn test_plan_tree_selection_clamps_on_verify_toggle() {
        let mut state = PlanTreeState::new();
        let data = serde_json::json!({
            "wave_number": 1,
            "status": "approved",
            "epics": [{
                "short_id": "W1-E1",
                "title": "Epic",
                "status": "pending",
                "stories": [{
                    "short_id": "W1-S1",
                    "title": "Story",
                    "status": "pending",
                    "progress": "0/2",
                    "depends_on": [],
                    "tasks": [
                        { "short_id": "W1-T1", "title": "Impl", "status": "pending", "kind": "impl" },
                        { "short_id": "W1-T1v", "title": "Verify", "status": "pending", "kind": "verify" }
                    ]
                }]
            }]
        });
        state.update_from_response(&data);

        // Select the last item (verify task at visible index 4)
        state.selected = 4;
        state.toggle_verify_visibility();
        // Selection should be clamped to 3 (last visible is impl task)
        assert!(state.selected <= 3);
    }

    #[test]
    fn test_plan_tree_navigation_empty() {
        let mut state = PlanTreeState::new();
        // Navigation on empty tree should not panic
        state.select_next();
        assert_eq!(state.selected, 0);
        state.select_prev();
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn test_plan_tree_toggle_collapse_no_children() {
        let mut state = PlanTreeState::new();
        let data = serde_json::json!({
            "wave_number": 1,
            "status": "approved",
            "epics": [{
                "short_id": "W1-E1",
                "title": "Epic",
                "status": "pending",
                "stories": [{
                    "short_id": "W1-S1",
                    "title": "Story",
                    "status": "pending",
                    "progress": "0/1",
                    "depends_on": [],
                    "tasks": [{
                        "short_id": "W1-T1",
                        "title": "Task",
                        "status": "pending",
                        "kind": "impl"
                    }]
                }]
            }]
        });
        state.update_from_response(&data);

        // Select a leaf node (task)
        state.selected = 3;
        state.toggle_collapse();
        // Task has no children — collapsed should remain false
        assert!(!state.nodes[3].collapsed);
    }

    #[test]
    fn test_app_plan_tree_initial() {
        let app = App::new("test".to_string());
        assert!(app.plan_tree.nodes.is_empty());
        assert_eq!(app.plan_tree.selected, 0);
        assert!(!app.plan_tree.hide_verify);
    }

    #[test]
    fn test_dialogue_ctrl_c_still_quits_with_plan_tree() {
        // Ensure plan_tree field doesn't break App construction
        let app = App::new("test".to_string());
        assert!(app.plan_tree.wave_number.is_none());
    }

    #[test]
    fn test_app_dialogue_lifecycle() {
        let mut app = App::new("test".to_string());
        assert!(!app.in_dialogue());
        assert!(app.spec_dialogue.is_none());

        app.enter_dialogue("my-spec".to_string());
        assert!(app.in_dialogue());
        assert_eq!(app.spec_dialogue.as_ref().unwrap().spec_name, "my-spec");

        app.exit_dialogue();
        assert!(!app.in_dialogue());
        assert!(app.spec_dialogue.is_none());
    }

    // --- PlanGenerateState tests ---

    #[test]
    fn test_generate_state_initial() {
        let state = PlanGenerateState::new(vec!["a".to_string(), "b".to_string()]);
        assert_eq!(state.specs.len(), 2);
        assert_eq!(state.cursor, 0);
        assert!(!state.with_codebase);
        assert!(!state.specs[0].selected);
        assert!(!state.specs[1].selected);
    }

    #[test]
    fn test_generate_state_navigation() {
        let mut state = PlanGenerateState::new(vec!["a".to_string(), "b".to_string()]);
        assert_eq!(state.cursor, 0);

        state.select_next();
        assert_eq!(state.cursor, 1);

        state.select_next();
        assert_eq!(state.cursor, 1); // Can't go past end

        state.select_prev();
        assert_eq!(state.cursor, 0);

        state.select_prev();
        assert_eq!(state.cursor, 0); // Can't go below 0
    }

    #[test]
    fn test_generate_state_toggle_selection() {
        let mut state = PlanGenerateState::new(vec!["a".to_string(), "b".to_string()]);

        state.toggle_selection();
        assert!(state.specs[0].selected);
        assert!(!state.specs[1].selected);

        state.toggle_selection();
        assert!(!state.specs[0].selected);

        state.select_next();
        state.toggle_selection();
        assert!(state.specs[1].selected);
    }

    #[test]
    fn test_generate_state_empty() {
        let mut state = PlanGenerateState::new(vec![]);
        // Should not panic
        state.select_next();
        state.select_prev();
        state.toggle_selection();
    }

    // --- PlanFeedbackState tests ---

    #[test]
    fn test_feedback_state_initial() {
        let state = PlanFeedbackState::new();
        assert!(state.input.is_empty());
    }

    // --- ConfirmAction tests ---

    #[test]
    fn test_confirm_action_messages() {
        assert!(!ConfirmAction::ApprovePlan.message().is_empty());
        assert!(!ConfirmAction::DiscardPlan.message().is_empty());
    }

    // --- Plan sub-view lifecycle ---

    #[test]
    fn test_plan_sub_view_detection() {
        let mut app = App::new("test".to_string());
        assert!(!app.in_plan_sub_view());

        app.plan_generate = Some(PlanGenerateState::new(vec![]));
        assert!(app.in_plan_sub_view());
        app.plan_generate = None;

        app.plan_feedback = Some(PlanFeedbackState::new());
        assert!(app.in_plan_sub_view());
        app.plan_feedback = None;

        app.plan_confirm = Some(PlanConfirmState {
            action: ConfirmAction::ApprovePlan,
        });
        assert!(app.in_plan_sub_view());
        app.plan_confirm = None;

        app.plan_detail = Some(PlanDetailState {
            short_id: "W1-E1".to_string(),
            title: "Test".to_string(),
            status: "pending".to_string(),
            description: "Test".to_string(),
            depends_on: vec![],
            progress: None,
            kind: None,
            depth: 1,
            error_message: None,
        });
        assert!(app.in_plan_sub_view());
    }

    #[test]
    fn test_close_plan_sub_view_clears_all() {
        let mut app = App::new("test".to_string());
        app.plan_generate = Some(PlanGenerateState::new(vec![]));
        app.plan_feedback = Some(PlanFeedbackState::new());
        app.plan_confirm = Some(PlanConfirmState {
            action: ConfirmAction::ApprovePlan,
        });
        app.plan_detail = Some(PlanDetailState {
            short_id: "".to_string(),
            title: "".to_string(),
            status: "".to_string(),
            description: "".to_string(),
            depends_on: vec![],
            progress: None,
            kind: None,
            depth: 0,
            error_message: None,
        });

        app.close_plan_sub_view();
        assert!(app.plan_generate.is_none());
        assert!(app.plan_feedback.is_none());
        assert!(app.plan_confirm.is_none());
        assert!(app.plan_detail.is_none());
    }

    #[test]
    fn test_open_plan_detail_from_tree() {
        let mut app = App::new("test".to_string());
        app.switch_view(View::Plan);
        let data = serde_json::json!({
            "wave_number": 1,
            "status": "approved",
            "epics": [{
                "short_id": "W1-E1",
                "title": "Epic Title",
                "status": "pending",
                "stories": [{
                    "short_id": "W1-S1",
                    "title": "Story Title",
                    "status": "ready",
                    "progress": "1/3",
                    "depends_on": ["W1-S2"],
                    "tasks": []
                }]
            }]
        });
        app.plan_tree.update_from_response(&data);

        // Select the story node (index 2 in visible)
        app.plan_tree.selected = 2;
        app.open_plan_detail();

        assert!(app.plan_detail.is_some());
        let detail = app.plan_detail.as_ref().unwrap();
        assert_eq!(detail.short_id, "W1-S1");
        assert_eq!(detail.title, "Story Title");
        assert_eq!(detail.status, "ready");
        assert_eq!(detail.progress.as_deref(), Some("1/3"));
        assert_eq!(detail.depends_on, vec!["W1-S2"]);
        assert_eq!(detail.depth, 2);
    }

    #[test]
    fn test_open_generate_dialog_filters_approved() {
        let mut app = App::new("test".to_string());
        app.specs_list.items = vec![
            SpecItem {
                name: "draft-spec".to_string(),
                status: "draft".to_string(),
                session_active: false,
                created_at: "".to_string(),
            },
            SpecItem {
                name: "approved-spec".to_string(),
                status: "approved".to_string(),
                session_active: false,
                created_at: "".to_string(),
            },
            SpecItem {
                name: "decomposed-spec".to_string(),
                status: "decomposed".to_string(),
                session_active: false,
                created_at: "".to_string(),
            },
        ];

        app.open_generate_dialog();
        let gen = app.plan_generate.as_ref().unwrap();
        assert_eq!(gen.specs.len(), 1);
        assert_eq!(gen.specs[0].name, "approved-spec");
    }

    // --- EventSubscriptionState tests ---

    #[test]
    fn test_event_subscription_initial_state() {
        let state = EventSubscriptionState::new();
        assert!(!state.subscribed);
    }

    #[test]
    fn test_app_event_subscription_initial() {
        let app = App::new("test".to_string());
        assert!(!app.is_event_subscribed());
        assert!(!app.event_subscription.subscribed);
    }

    // --- ExecuteTreeState count_task_stats tests ---

    fn make_execute_tree_with_tasks() -> ExecuteTreeState {
        let mut tree = ExecuteTreeState::new();
        let data = serde_json::json!({
            "waves": [{
                "wave_number": 1,
                "status": "in_progress",
                "epics": [{
                    "short_id": "W1-E1",
                    "title": "Epic",
                    "status": "in_progress",
                    "stories": [{
                        "short_id": "W1-S1",
                        "title": "Story",
                        "status": "in_progress",
                        "progress": "2/4",
                        "tasks": [
                            { "short_id": "W1-T1", "title": "Task 1", "status": "done", "kind": "impl" },
                            { "short_id": "W1-T1v", "title": "Verify 1", "status": "done", "kind": "verify" },
                            { "short_id": "W1-T2", "title": "Task 2", "status": "in_progress", "kind": "impl" },
                            { "short_id": "W1-T2v", "title": "Verify 2", "status": "pending", "kind": "verify" }
                        ]
                    }, {
                        "short_id": "W1-S2",
                        "title": "Story 2",
                        "status": "failed",
                        "progress": "1/2",
                        "tasks": [
                            { "short_id": "W1-T3", "title": "Task 3", "status": "failed", "kind": "impl" },
                            { "short_id": "W1-T3v", "title": "Verify 3", "status": "pending", "kind": "verify" }
                        ]
                    }]
                }]
            }]
        });
        tree.update_from_response(&data);
        tree
    }

    #[test]
    fn test_count_task_stats() {
        let tree = make_execute_tree_with_tasks();
        let (running, total, done, failed) = tree.count_task_stats();
        assert_eq!(running, 1); // W1-T2
        assert_eq!(total, 6); // 6 tasks
        assert_eq!(done, 2); // W1-T1, W1-T1v
        assert_eq!(failed, 1); // W1-T3
    }

    #[test]
    fn test_count_task_stats_empty() {
        let tree = ExecuteTreeState::new();
        let (running, total, done, failed) = tree.count_task_stats();
        assert_eq!(running, 0);
        assert_eq!(total, 0);
        assert_eq!(done, 0);
        assert_eq!(failed, 0);
    }

    #[test]
    fn test_update_node_status() {
        let mut tree = make_execute_tree_with_tasks();
        assert!(tree.update_node_status("W1-T2", "done"));
        let node = tree.nodes.iter().find(|n| n.short_id == "W1-T2").unwrap();
        assert_eq!(node.status, "done");
    }

    #[test]
    fn test_update_node_status_not_found() {
        let mut tree = make_execute_tree_with_tasks();
        assert!(!tree.update_node_status("W1-T999", "done"));
    }

    #[test]
    fn test_set_story_mr_url() {
        let mut tree = make_execute_tree_with_tasks();
        tree.set_story_mr_url("W1-S1", "https://gitlab.com/mr/123");
        let node = tree.nodes.iter().find(|n| n.short_id == "W1-S1").unwrap();
        assert_eq!(node.mr_url.as_deref(), Some("https://gitlab.com/mr/123"));
    }

    #[test]
    fn test_set_story_mr_url_only_stories() {
        let mut tree = make_execute_tree_with_tasks();
        // Should not set MR URL on a task node
        tree.set_story_mr_url("W1-T1", "https://example.com");
        let node = tree.nodes.iter().find(|n| n.short_id == "W1-T1").unwrap();
        assert!(node.mr_url.is_none());
    }

    #[test]
    fn test_execute_tree_node_mr_url_default() {
        let tree = make_execute_tree_with_tasks();
        // All nodes should have mr_url = None by default
        for node in &tree.nodes {
            assert!(node.mr_url.is_none());
        }
    }

    // --- Event application tests ---

    #[test]
    fn test_apply_status_change_sets_message() {
        let mut app = App::new("test".to_string());
        app.apply_status_change("some-uuid", "in_progress", "task");
        assert!(app.status_message.contains("task"));
        assert!(app.status_message.contains("in_progress"));
    }

    #[test]
    fn test_apply_story_completed_with_mr_url() {
        let mut app = App::new("test".to_string());
        app.apply_story_completed("some-uuid", Some("https://gitlab.com/mr/42"));
        assert!(app.status_message.contains("https://gitlab.com/mr/42"));
    }

    #[test]
    fn test_apply_story_completed_without_mr_url() {
        let mut app = App::new("test".to_string());
        app.apply_story_completed("some-uuid", None);
        // No MR URL — status message should be empty (not modified)
        assert!(app.status_message.is_empty());
    }
}
