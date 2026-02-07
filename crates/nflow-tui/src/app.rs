use crate::error::Result;
use crate::socket_client::{ResponseStatus, SocketClient};

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

    /// Move selection up.
    pub fn select_prev(&mut self) {
        if !self.items.is_empty() && self.selected > 0 {
            self.selected -= 1;
        }
    }

    /// Move selection down.
    pub fn select_next(&mut self) {
        if !self.items.is_empty() && self.selected < self.items.len() - 1 {
            self.selected += 1;
        }
    }

    /// Get the currently selected item (used by action handlers in future stories).
    #[allow(dead_code)]
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
}

impl PlanTreeState {
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            selected: 0,
            hide_verify: false,
            wave_number: None,
            wave_status: None,
        }
    }

    /// Get visible nodes (respecting collapsed state and verify filter).
    pub fn visible_nodes(&self) -> Vec<(usize, &PlanTreeNode)> {
        let mut result = Vec::new();
        let mut skip_depth: Option<u8> = None;

        for (i, node) in self.nodes.iter().enumerate() {
            // Skip children of collapsed nodes
            if let Some(sd) = skip_depth {
                if node.depth > sd {
                    continue;
                }
                skip_depth = None;
            }

            // Filter out verify tasks if hide_verify is on
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
    pub fn select_prev(&mut self) {
        let visible = self.visible_nodes();
        if visible.is_empty() {
            return;
        }
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    /// Move selection down.
    pub fn select_next(&mut self) {
        let visible = self.visible_nodes();
        if visible.is_empty() {
            return;
        }
        if self.selected < visible.len() - 1 {
            self.selected += 1;
        }
    }

    /// Toggle collapse on the selected node.
    pub fn toggle_collapse(&mut self) {
        let visible = self.visible_nodes();
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

        self.wave_number = wave_number;
        self.wave_status = wave_status.clone();

        let mut nodes = Vec::new();

        // Add wave root node
        let wave_label = wave_number
            .map(|w| format!("W{}", w))
            .unwrap_or_else(|| "Wave".to_string());
        let has_epics = data
            .get("epics")
            .and_then(|v| v.as_array())
            .is_some_and(|a| !a.is_empty());
        nodes.push(PlanTreeNode {
            short_id: wave_label,
            title: wave_status.unwrap_or_default(),
            status: String::new(),
            depth: 0,
            collapsed: false,
            has_children: has_epics,
            depends_on: Vec::new(),
            progress: None,
            kind: None,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmAction {
    ApprovePlan,
    DiscardPlan,
}

impl ConfirmAction {
    pub fn message(&self) -> &'static str {
        match self {
            ConfirmAction::ApprovePlan => "Approve current draft wave?",
            ConfirmAction::DiscardPlan => "Discard current draft wave?",
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
        let mut result = Vec::new();
        let mut skip_depth: Option<u8> = None;

        for (i, node) in self.nodes.iter().enumerate() {
            if let Some(sd) = skip_depth {
                if node.depth > sd {
                    continue;
                }
                skip_depth = None;
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
    pub fn select_prev(&mut self) {
        let visible = self.visible_nodes();
        if visible.is_empty() {
            return;
        }
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    /// Move selection down.
    pub fn select_next(&mut self) {
        let visible = self.visible_nodes();
        if visible.is_empty() {
            return;
        }
        if self.selected < visible.len() - 1 {
            self.selected += 1;
        }
    }

    /// Toggle collapse on the selected node.
    pub fn toggle_collapse(&mut self) {
        let visible = self.visible_nodes();
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

                let wave_label = wave_number
                    .map(|w| format!("W{}", w))
                    .unwrap_or_else(|| "Wave".to_string());
                let has_epics = wave
                    .get("epics")
                    .and_then(|v| v.as_array())
                    .is_some_and(|a| !a.is_empty());

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

/// Overlay that can be displayed on top of the current view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overlay {
    Help,
    ProjectSwitcher,
    Filter,
}

/// The active view in the TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Specs,
    Plan,
    Execute,
    Logs,
}

impl View {
    pub fn label(&self) -> &'static str {
        match self {
            View::Specs => "Specs",
            View::Plan => "Plan",
            View::Execute => "Execute",
            View::Logs => "Logs",
        }
    }

    pub fn all() -> &'static [View] {
        &[View::Specs, View::Plan, View::Execute, View::Logs]
    }

    pub fn index(&self) -> usize {
        match self {
            View::Specs => 0,
            View::Plan => 1,
            View::Execute => 2,
            View::Logs => 3,
        }
    }

    pub fn from_index(index: usize) -> Self {
        match index {
            0 => View::Specs,
            1 => View::Plan,
            2 => View::Execute,
            3 => View::Logs,
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
    /// Current wave number (from daemon status).
    pub current_wave: Option<u32>,
    /// Number of active agents (from daemon status).
    pub active_agent_count: u32,
    /// Specs list view state.
    pub specs_list: SpecsListState,
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
    /// Event subscription state for real-time updates.
    pub event_subscription: EventSubscriptionState,
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
            current_wave: None,
            active_agent_count: 0,
            specs_list: SpecsListState::new(),
            spec_dialogue: None,
            spec_pager: None,
            plan_tree: PlanTreeState::new(),
            plan_generate: None,
            plan_feedback: None,
            plan_confirm: None,
            plan_detail: None,
            execute_tree: ExecuteTreeState::new(),
            execute_output: ExecuteOutputState::new(),
            event_subscription: EventSubscriptionState::new(),
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
        self.overlay = None;
    }

    /// Returns true if any overlay is currently shown.
    pub fn has_overlay(&self) -> bool {
        self.overlay.is_some()
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
        assert_eq!(app.current_view, View::Specs);
    }

    #[test]
    fn test_view_cycle_backward() {
        let mut app = App::new("test".to_string());
        assert_eq!(app.current_view, View::Specs);

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
        app.toggle_overlay(Overlay::Filter);
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
