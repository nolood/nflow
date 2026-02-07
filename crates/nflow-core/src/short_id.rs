use std::collections::HashMap;
use std::fmt;

use uuid::Uuid;

use crate::error::{NflowError, Result};
use crate::work_item::{ItemType, TaskKind, WorkItem};

/// A wave-prefixed display ID, e.g. "W1-S1" or "W1-T1v".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayId {
    pub wave: u32,
    pub short_id: String,
}

impl fmt::Display for DisplayId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "W{}-{}", self.wave, self.short_id)
    }
}

/// Generates sequential short IDs scoped per decomposition session.
///
/// Short IDs follow the pattern:
/// - Epics: E1, E2, E3, ...
/// - Stories: S1, S2, S3, ...
/// - Tasks (impl): T1, T2, T3, ...
/// - Tasks (verify): T1v, T2v, T3v, ... (auto-generated from impl short_id)
pub struct ShortIdGenerator {
    epic_counter: u32,
    story_counter: u32,
    task_counter: u32,
}

impl ShortIdGenerator {
    pub fn new() -> Self {
        Self {
            epic_counter: 0,
            story_counter: 0,
            task_counter: 0,
        }
    }

    /// Generate the next short ID for an epic.
    pub fn next_epic(&mut self) -> String {
        self.epic_counter += 1;
        format!("E{}", self.epic_counter)
    }

    /// Generate the next short ID for a story.
    pub fn next_story(&mut self) -> String {
        self.story_counter += 1;
        format!("S{}", self.story_counter)
    }

    /// Generate the next short ID for an impl task.
    pub fn next_task(&mut self) -> String {
        self.task_counter += 1;
        format!("T{}", self.task_counter)
    }

    /// Generate the verify short ID from an impl task's short ID.
    /// Simply appends 'v' to the impl short_id.
    pub fn verify_id(impl_short_id: &str) -> String {
        format!("{impl_short_id}v")
    }
}

impl Default for ShortIdGenerator {
    fn default() -> Self {
        Self::new()
    }
}

/// Registry that maps short IDs to UUIDs, scoped by wave number.
///
/// Supports resolving both wave-prefixed ("W1-S1") and bare ("S1") short IDs.
/// Bare IDs are resolved when they are unambiguous (exist in exactly one wave).
pub struct ShortIdRegistry {
    /// wave_number -> (short_id -> uuid)
    waves: HashMap<u32, HashMap<String, Uuid>>,
}

impl ShortIdRegistry {
    pub fn new() -> Self {
        Self {
            waves: HashMap::new(),
        }
    }

    /// Register a work item's short ID under a wave number.
    pub fn register(&mut self, wave: u32, short_id: &str, uuid: Uuid) {
        self.waves
            .entry(wave)
            .or_default()
            .insert(short_id.to_string(), uuid);
    }

    /// Bulk-register all work items from a wave.
    pub fn register_items(&mut self, wave: u32, items: &[WorkItem]) {
        for item in items {
            self.register(wave, &item.short_id, item.id);
        }
    }

    /// Build a display ID for a work item given its wave number.
    pub fn display_id(wave: u32, short_id: &str) -> DisplayId {
        DisplayId {
            wave,
            short_id: short_id.to_string(),
        }
    }

    /// Resolve a short ID with an explicit wave prefix.
    ///
    /// Returns the UUID for the given wave+short_id combination.
    pub fn resolve_with_wave(&self, wave: u32, short_id: &str) -> Result<Uuid> {
        let wave_map = self
            .waves
            .get(&wave)
            .ok_or_else(|| NflowError::NotFound(format!("wave W{wave} not found")))?;
        wave_map.get(short_id).copied().ok_or_else(|| {
            NflowError::NotFound(format!("short ID '{short_id}' not found in wave W{wave}"))
        })
    }

    /// Resolve a short ID that may or may not have a wave prefix.
    ///
    /// Accepts formats:
    /// - "W1-S1" — explicit wave prefix
    /// - "S1" — bare short ID, resolved if unambiguous across all waves
    ///
    /// When `context_wave` is provided, bare IDs are resolved in that wave first.
    pub fn resolve(&self, input: &str, context_wave: Option<u32>) -> Result<Uuid> {
        if let Some(parsed) = parse_wave_prefixed(input) {
            return self.resolve_with_wave(parsed.wave, &parsed.short_id);
        }

        // Bare short ID — try context wave first
        if let Some(wave) = context_wave {
            if let Some(wave_map) = self.waves.get(&wave) {
                if let Some(uuid) = wave_map.get(input) {
                    return Ok(*uuid);
                }
            }
        }

        // Search all waves for the bare short ID
        let mut matches: Vec<(u32, Uuid)> = Vec::new();
        for (&wave, wave_map) in &self.waves {
            if let Some(&uuid) = wave_map.get(input) {
                matches.push((wave, uuid));
            }
        }

        match matches.len() {
            0 => Err(NflowError::NotFound(format!(
                "short ID '{input}' not found in any wave"
            ))),
            1 => Ok(matches[0].1),
            _ => {
                let wave_list: Vec<String> = matches.iter().map(|(w, _)| format!("W{w}")).collect();
                Err(NflowError::InvalidParams(format!(
                    "short ID '{input}' is ambiguous — found in waves: {}. Use wave prefix (e.g., W{}-{input})",
                    wave_list.join(", "),
                    matches[0].0,
                )))
            }
        }
    }

    /// Get all registered wave numbers.
    pub fn waves(&self) -> Vec<u32> {
        let mut waves: Vec<u32> = self.waves.keys().copied().collect();
        waves.sort();
        waves
    }
}

impl Default for ShortIdRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse a wave-prefixed string like "W1-S1" into its components.
/// Returns None if the input doesn't match the wave-prefix pattern.
pub fn parse_wave_prefixed(input: &str) -> Option<DisplayId> {
    let input = input.trim();
    // Must start with 'W' or 'w', followed by digits, then '-', then the short ID
    let rest = input
        .strip_prefix('W')
        .or_else(|| input.strip_prefix('w'))?;
    let dash_pos = rest.find('-')?;
    let wave_str = &rest[..dash_pos];
    let short_id = &rest[dash_pos + 1..];

    if short_id.is_empty() {
        return None;
    }

    let wave: u32 = wave_str.parse().ok()?;
    Some(DisplayId {
        wave,
        short_id: short_id.to_string(),
    })
}

/// Parse a bare short ID to determine its item type.
/// Returns (prefix_char, number, is_verify).
///
/// Examples:
/// - "E1" -> ('E', 1, false)
/// - "S3" -> ('S', 3, false)
/// - "T2" -> ('T', 2, false)
/// - "T2v" -> ('T', 2, true)
pub fn parse_short_id(input: &str) -> Option<(char, u32, bool)> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }

    let first = input.chars().next()?;
    if !matches!(first, 'E' | 'S' | 'T' | 'e' | 's' | 't') {
        return None;
    }

    let rest = &input[1..];
    let (num_str, is_verify) = if let Some(stripped) = rest.strip_suffix('v') {
        (stripped, true)
    } else {
        (rest, false)
    };

    let number: u32 = num_str.parse().ok()?;
    Some((first.to_ascii_uppercase(), number, is_verify))
}

/// Generate short IDs for a set of work items from a decomposition session.
///
/// Items should be passed in their natural order (epics first, then stories, then tasks).
/// This function assigns short IDs to items that don't already have one set.
/// Returns the items with short IDs populated.
pub fn assign_short_ids(items: &mut [WorkItem]) {
    let mut gen = ShortIdGenerator::new();

    for item in items.iter_mut() {
        if !item.short_id.is_empty() {
            // Already assigned — update generator counters to avoid collisions
            if let Some((prefix, num, _)) = parse_short_id(&item.short_id) {
                match prefix {
                    'E' => {
                        if num > gen.epic_counter {
                            gen.epic_counter = num;
                        }
                    }
                    'S' => {
                        if num > gen.story_counter {
                            gen.story_counter = num;
                        }
                    }
                    'T' => {
                        if num > gen.task_counter {
                            gen.task_counter = num;
                        }
                    }
                    _ => {}
                }
            }
            continue;
        }

        let short_id = match item.item_type {
            ItemType::Epic => gen.next_epic(),
            ItemType::Story => gen.next_story(),
            ItemType::Task => match item.kind {
                Some(TaskKind::Verify) => {
                    // Verify tasks derive their ID from the paired impl task
                    // This should be handled by auto_generate_verify_tasks
                    // but as a fallback, generate a task ID with 'v' suffix
                    let base = gen.next_task();
                    format!("{base}v")
                }
                _ => gen.next_task(),
            },
        };
        item.short_id = short_id;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work_item::{auto_generate_verify_tasks, WorkItem};

    fn make_epic(session_id: Uuid, short_id: &str) -> WorkItem {
        WorkItem::new_epic(session_id, "Epic".into(), "Desc".into(), short_id.into(), 0)
    }

    fn make_story(epic_id: Uuid, session_id: Uuid, short_id: &str) -> WorkItem {
        WorkItem::new_story(
            epic_id,
            session_id,
            "Story".into(),
            "Desc".into(),
            "AC".into(),
            short_id.into(),
            0,
        )
    }

    fn make_task(story_id: Uuid, session_id: Uuid, short_id: &str, sort_order: i32) -> WorkItem {
        WorkItem::new_task(
            story_id,
            session_id,
            "Task".into(),
            "Desc".into(),
            "AC".into(),
            short_id.into(),
            sort_order,
        )
    }

    // --- ShortIdGenerator ---

    #[test]
    fn generator_produces_sequential_epic_ids() {
        let mut gen = ShortIdGenerator::new();
        assert_eq!(gen.next_epic(), "E1");
        assert_eq!(gen.next_epic(), "E2");
        assert_eq!(gen.next_epic(), "E3");
    }

    #[test]
    fn generator_produces_sequential_story_ids() {
        let mut gen = ShortIdGenerator::new();
        assert_eq!(gen.next_story(), "S1");
        assert_eq!(gen.next_story(), "S2");
        assert_eq!(gen.next_story(), "S3");
    }

    #[test]
    fn generator_produces_sequential_task_ids() {
        let mut gen = ShortIdGenerator::new();
        assert_eq!(gen.next_task(), "T1");
        assert_eq!(gen.next_task(), "T2");
        assert_eq!(gen.next_task(), "T3");
    }

    #[test]
    fn generator_counters_are_independent() {
        let mut gen = ShortIdGenerator::new();
        assert_eq!(gen.next_epic(), "E1");
        assert_eq!(gen.next_story(), "S1");
        assert_eq!(gen.next_task(), "T1");
        assert_eq!(gen.next_epic(), "E2");
        assert_eq!(gen.next_story(), "S2");
        assert_eq!(gen.next_task(), "T2");
    }

    #[test]
    fn verify_id_appends_v() {
        assert_eq!(ShortIdGenerator::verify_id("T1"), "T1v");
        assert_eq!(ShortIdGenerator::verify_id("T42"), "T42v");
    }

    // --- DisplayId ---

    #[test]
    fn display_id_format() {
        let d = DisplayId {
            wave: 1,
            short_id: "S1".into(),
        };
        assert_eq!(d.to_string(), "W1-S1");
    }

    #[test]
    fn display_id_verify_format() {
        let d = DisplayId {
            wave: 2,
            short_id: "T3v".into(),
        };
        assert_eq!(d.to_string(), "W2-T3v");
    }

    // --- parse_wave_prefixed ---

    #[test]
    fn parse_wave_prefixed_valid() {
        let parsed = parse_wave_prefixed("W1-S1").unwrap();
        assert_eq!(parsed.wave, 1);
        assert_eq!(parsed.short_id, "S1");
    }

    #[test]
    fn parse_wave_prefixed_verify_task() {
        let parsed = parse_wave_prefixed("W2-T3v").unwrap();
        assert_eq!(parsed.wave, 2);
        assert_eq!(parsed.short_id, "T3v");
    }

    #[test]
    fn parse_wave_prefixed_lowercase() {
        let parsed = parse_wave_prefixed("w1-E1").unwrap();
        assert_eq!(parsed.wave, 1);
        assert_eq!(parsed.short_id, "E1");
    }

    #[test]
    fn parse_wave_prefixed_with_whitespace() {
        let parsed = parse_wave_prefixed("  W1-S1  ").unwrap();
        assert_eq!(parsed.wave, 1);
        assert_eq!(parsed.short_id, "S1");
    }

    #[test]
    fn parse_wave_prefixed_no_prefix_returns_none() {
        assert!(parse_wave_prefixed("S1").is_none());
    }

    #[test]
    fn parse_wave_prefixed_no_dash_returns_none() {
        assert!(parse_wave_prefixed("W1S1").is_none());
    }

    #[test]
    fn parse_wave_prefixed_no_number_returns_none() {
        assert!(parse_wave_prefixed("W-S1").is_none());
    }

    #[test]
    fn parse_wave_prefixed_empty_short_id_returns_none() {
        assert!(parse_wave_prefixed("W1-").is_none());
    }

    #[test]
    fn parse_wave_prefixed_invalid_wave_number() {
        assert!(parse_wave_prefixed("Wabc-S1").is_none());
    }

    // --- parse_short_id ---

    #[test]
    fn parse_short_id_epic() {
        let (prefix, num, is_verify) = parse_short_id("E1").unwrap();
        assert_eq!(prefix, 'E');
        assert_eq!(num, 1);
        assert!(!is_verify);
    }

    #[test]
    fn parse_short_id_story() {
        let (prefix, num, is_verify) = parse_short_id("S42").unwrap();
        assert_eq!(prefix, 'S');
        assert_eq!(num, 42);
        assert!(!is_verify);
    }

    #[test]
    fn parse_short_id_task() {
        let (prefix, num, is_verify) = parse_short_id("T3").unwrap();
        assert_eq!(prefix, 'T');
        assert_eq!(num, 3);
        assert!(!is_verify);
    }

    #[test]
    fn parse_short_id_verify_task() {
        let (prefix, num, is_verify) = parse_short_id("T2v").unwrap();
        assert_eq!(prefix, 'T');
        assert_eq!(num, 2);
        assert!(is_verify);
    }

    #[test]
    fn parse_short_id_lowercase() {
        let (prefix, num, _) = parse_short_id("e5").unwrap();
        assert_eq!(prefix, 'E');
        assert_eq!(num, 5);
    }

    #[test]
    fn parse_short_id_invalid_prefix() {
        assert!(parse_short_id("X1").is_none());
    }

    #[test]
    fn parse_short_id_empty() {
        assert!(parse_short_id("").is_none());
    }

    #[test]
    fn parse_short_id_no_number() {
        assert!(parse_short_id("E").is_none());
    }

    #[test]
    fn parse_short_id_non_numeric() {
        assert!(parse_short_id("Eabc").is_none());
    }

    // --- ShortIdRegistry ---

    #[test]
    fn registry_register_and_resolve_with_wave() {
        let mut reg = ShortIdRegistry::new();
        let uuid = Uuid::new_v4();
        reg.register(1, "S1", uuid);

        assert_eq!(reg.resolve_with_wave(1, "S1").unwrap(), uuid);
    }

    #[test]
    fn registry_resolve_with_wave_not_found() {
        let reg = ShortIdRegistry::new();
        let err = reg.resolve_with_wave(1, "S1").unwrap_err();
        assert!(matches!(err, NflowError::NotFound(_)));
    }

    #[test]
    fn registry_resolve_with_wave_wrong_id() {
        let mut reg = ShortIdRegistry::new();
        reg.register(1, "S1", Uuid::new_v4());

        let err = reg.resolve_with_wave(1, "S2").unwrap_err();
        assert!(matches!(err, NflowError::NotFound(_)));
    }

    #[test]
    fn registry_resolve_wave_prefixed_input() {
        let mut reg = ShortIdRegistry::new();
        let uuid = Uuid::new_v4();
        reg.register(1, "S1", uuid);

        assert_eq!(reg.resolve("W1-S1", None).unwrap(), uuid);
    }

    #[test]
    fn registry_resolve_bare_id_unambiguous() {
        let mut reg = ShortIdRegistry::new();
        let uuid = Uuid::new_v4();
        reg.register(1, "S1", uuid);

        assert_eq!(reg.resolve("S1", None).unwrap(), uuid);
    }

    #[test]
    fn registry_resolve_bare_id_with_context_wave() {
        let mut reg = ShortIdRegistry::new();
        let uuid1 = Uuid::new_v4();
        let uuid2 = Uuid::new_v4();
        reg.register(1, "S1", uuid1);
        reg.register(2, "S1", uuid2);

        // Context wave 1 resolves to uuid1
        assert_eq!(reg.resolve("S1", Some(1)).unwrap(), uuid1);
        // Context wave 2 resolves to uuid2
        assert_eq!(reg.resolve("S1", Some(2)).unwrap(), uuid2);
    }

    #[test]
    fn registry_resolve_bare_id_ambiguous_no_context() {
        let mut reg = ShortIdRegistry::new();
        reg.register(1, "S1", Uuid::new_v4());
        reg.register(2, "S1", Uuid::new_v4());

        let err = reg.resolve("S1", None).unwrap_err();
        assert!(matches!(err, NflowError::InvalidParams(_)));
        let msg = err.to_string();
        assert!(msg.contains("ambiguous"));
        assert!(msg.contains("W1"));
        assert!(msg.contains("W2"));
    }

    #[test]
    fn registry_resolve_bare_id_not_found() {
        let reg = ShortIdRegistry::new();
        let err = reg.resolve("S1", None).unwrap_err();
        assert!(matches!(err, NflowError::NotFound(_)));
    }

    #[test]
    fn registry_resolve_bare_id_context_wave_miss_falls_through() {
        let mut reg = ShortIdRegistry::new();
        let uuid = Uuid::new_v4();
        reg.register(2, "S1", uuid);

        // Context wave 1 doesn't have S1, but wave 2 does — should find it
        assert_eq!(reg.resolve("S1", Some(1)).unwrap(), uuid);
    }

    #[test]
    fn registry_register_items() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id, "E1");
        let story = make_story(epic.id, session_id, "S1");
        let task = make_task(story.id, session_id, "T1", 0);

        let mut reg = ShortIdRegistry::new();
        reg.register_items(1, &[epic.clone(), story.clone(), task.clone()]);

        assert_eq!(reg.resolve("W1-E1", None).unwrap(), epic.id);
        assert_eq!(reg.resolve("W1-S1", None).unwrap(), story.id);
        assert_eq!(reg.resolve("W1-T1", None).unwrap(), task.id);
    }

    #[test]
    fn registry_register_items_with_verify_tasks() {
        let session_id = Uuid::new_v4();
        let story_id = Uuid::new_v4();
        let task = make_task(story_id, session_id, "T1", 0);
        let verify_tasks = auto_generate_verify_tasks(&[task.clone()]);

        let mut reg = ShortIdRegistry::new();
        reg.register_items(1, &[task.clone()]);
        reg.register_items(1, &verify_tasks);

        assert_eq!(reg.resolve("W1-T1", None).unwrap(), task.id);
        assert_eq!(reg.resolve("W1-T1v", None).unwrap(), verify_tasks[0].id);
    }

    #[test]
    fn registry_display_id() {
        let d = ShortIdRegistry::display_id(1, "S1");
        assert_eq!(d.to_string(), "W1-S1");
    }

    #[test]
    fn registry_waves_returns_sorted() {
        let mut reg = ShortIdRegistry::new();
        reg.register(3, "E1", Uuid::new_v4());
        reg.register(1, "E1", Uuid::new_v4());
        reg.register(2, "E1", Uuid::new_v4());

        assert_eq!(reg.waves(), vec![1, 2, 3]);
    }

    #[test]
    fn registry_waves_empty() {
        let reg = ShortIdRegistry::new();
        assert!(reg.waves().is_empty());
    }

    // --- assign_short_ids ---

    #[test]
    fn assign_short_ids_to_empty_items() {
        let session_id = Uuid::new_v4();
        let epic_id = Uuid::new_v4();
        let story_id = Uuid::new_v4();

        let mut items = vec![
            WorkItem::new_epic(session_id, "E".into(), "D".into(), String::new(), 0),
            WorkItem::new_story(
                epic_id,
                session_id,
                "S".into(),
                "D".into(),
                "AC".into(),
                String::new(),
                0,
            ),
            WorkItem::new_task(
                story_id,
                session_id,
                "T".into(),
                "D".into(),
                "AC".into(),
                String::new(),
                0,
            ),
        ];

        assign_short_ids(&mut items);

        assert_eq!(items[0].short_id, "E1");
        assert_eq!(items[1].short_id, "S1");
        assert_eq!(items[2].short_id, "T1");
    }

    #[test]
    fn assign_short_ids_preserves_existing() {
        let session_id = Uuid::new_v4();
        let epic_id = Uuid::new_v4();

        let mut items = vec![
            WorkItem::new_epic(session_id, "E".into(), "D".into(), "E1".into(), 0),
            WorkItem::new_epic(session_id, "E".into(), "D".into(), String::new(), 1),
            WorkItem::new_story(
                epic_id,
                session_id,
                "S".into(),
                "D".into(),
                "AC".into(),
                "S1".into(),
                0,
            ),
            WorkItem::new_story(
                epic_id,
                session_id,
                "S".into(),
                "D".into(),
                "AC".into(),
                String::new(),
                1,
            ),
        ];

        assign_short_ids(&mut items);

        assert_eq!(items[0].short_id, "E1"); // preserved
        assert_eq!(items[1].short_id, "E2"); // assigned, skipped counter past E1
        assert_eq!(items[2].short_id, "S1"); // preserved
        assert_eq!(items[3].short_id, "S2"); // assigned, skipped counter past S1
    }

    #[test]
    fn assign_short_ids_multiple_of_each() {
        let session_id = Uuid::new_v4();
        let epic_id = Uuid::new_v4();
        let story_id = Uuid::new_v4();

        let mut items = vec![
            WorkItem::new_epic(session_id, "E".into(), "D".into(), String::new(), 0),
            WorkItem::new_epic(session_id, "E".into(), "D".into(), String::new(), 1),
            WorkItem::new_story(
                epic_id,
                session_id,
                "S".into(),
                "D".into(),
                "AC".into(),
                String::new(),
                0,
            ),
            WorkItem::new_story(
                epic_id,
                session_id,
                "S".into(),
                "D".into(),
                "AC".into(),
                String::new(),
                1,
            ),
            WorkItem::new_task(
                story_id,
                session_id,
                "T".into(),
                "D".into(),
                "AC".into(),
                String::new(),
                0,
            ),
            WorkItem::new_task(
                story_id,
                session_id,
                "T".into(),
                "D".into(),
                "AC".into(),
                String::new(),
                2,
            ),
        ];

        assign_short_ids(&mut items);

        assert_eq!(items[0].short_id, "E1");
        assert_eq!(items[1].short_id, "E2");
        assert_eq!(items[2].short_id, "S1");
        assert_eq!(items[3].short_id, "S2");
        assert_eq!(items[4].short_id, "T1");
        assert_eq!(items[5].short_id, "T2");
    }

    // --- Integration: full workflow ---

    #[test]
    fn full_workflow_generate_register_resolve() {
        let session_id = Uuid::new_v4();

        // Generate items
        let mut gen = ShortIdGenerator::new();
        let epic = make_epic(session_id, &gen.next_epic());
        let story = make_story(epic.id, session_id, &gen.next_story());
        let t1 = make_task(story.id, session_id, &gen.next_task(), 0);
        let verify_tasks = auto_generate_verify_tasks(&[t1.clone()]);

        assert_eq!(epic.short_id, "E1");
        assert_eq!(story.short_id, "S1");
        assert_eq!(t1.short_id, "T1");
        assert_eq!(verify_tasks[0].short_id, "T1v");

        // Register in wave 1
        let mut reg = ShortIdRegistry::new();
        reg.register_items(1, &[epic.clone(), story.clone(), t1.clone()]);
        reg.register_items(1, &verify_tasks);

        // Resolve with various input formats
        assert_eq!(reg.resolve("W1-E1", None).unwrap(), epic.id);
        assert_eq!(reg.resolve("W1-S1", None).unwrap(), story.id);
        assert_eq!(reg.resolve("W1-T1", None).unwrap(), t1.id);
        assert_eq!(reg.resolve("W1-T1v", None).unwrap(), verify_tasks[0].id);

        // Bare IDs (unambiguous, only one wave)
        assert_eq!(reg.resolve("E1", None).unwrap(), epic.id);
        assert_eq!(reg.resolve("S1", None).unwrap(), story.id);
        assert_eq!(reg.resolve("T1", None).unwrap(), t1.id);
        assert_eq!(reg.resolve("T1v", None).unwrap(), verify_tasks[0].id);

        // Display IDs
        assert_eq!(ShortIdRegistry::display_id(1, "E1").to_string(), "W1-E1");
        assert_eq!(ShortIdRegistry::display_id(1, "T1v").to_string(), "W1-T1v");
    }

    #[test]
    fn multi_wave_resolution() {
        let session_id = Uuid::new_v4();

        let epic_w1 = make_epic(session_id, "E1");
        let epic_w2 = make_epic(session_id, "E1");

        let mut reg = ShortIdRegistry::new();
        reg.register(1, "E1", epic_w1.id);
        reg.register(2, "E1", epic_w2.id);

        // Wave-prefixed resolves correctly
        assert_eq!(reg.resolve("W1-E1", None).unwrap(), epic_w1.id);
        assert_eq!(reg.resolve("W2-E1", None).unwrap(), epic_w2.id);

        // Bare ID is ambiguous
        assert!(reg.resolve("E1", None).is_err());

        // Context wave disambiguates
        assert_eq!(reg.resolve("E1", Some(1)).unwrap(), epic_w1.id);
        assert_eq!(reg.resolve("E1", Some(2)).unwrap(), epic_w2.id);
    }
}
