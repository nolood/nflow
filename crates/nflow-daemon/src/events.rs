use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use uuid::Uuid;

/// Events that can be broadcast to subscribed clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// A work item's status changed.
    StatusChange {
        item_id: String,
        item_type: String,
        old_status: String,
        new_status: String,
        project_id: String,
    },
    /// Output from a running agent process.
    AgentOutput { task_id: String, line: String },
    /// A story completed its execution lifecycle.
    StoryCompleted {
        story_id: String,
        project_id: String,
        branch_name: String,
        mr_url: Option<String>,
    },
    /// Decomposition (plan generation) completed — work items are now in the database.
    DecompositionCompleted {
        session_id: String,
        project_id: String,
        wave_number: u32,
    },
    /// A pipeline stage changed status.
    PipelineStageChange {
        pipeline_run_id: String,
        project_id: String,
        stage_type: String,
        iteration: u32,
        new_status: String,
    },
    /// A pipeline run completed.
    PipelineCompleted {
        pipeline_run_id: String,
        project_id: String,
        status: String,
        iterations: u32,
    },
    /// Output from a pipeline agent process.
    PipelineAgentOutput {
        pipeline_run_id: String,
        stage_type: String,
        iteration: u32,
        line: String,
    },
}

/// A client subscription identified by a unique ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClientId(pub Uuid);

impl ClientId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ClientId {
    fn default() -> Self {
        Self::new()
    }
}

/// The event bus manages broadcast of events to subscribed clients.
///
/// Internally uses a tokio broadcast channel. Subscribers receive events via
/// a broadcast receiver. When a client disconnects, its subscription is
/// automatically removed.
pub struct EventBus {
    /// The broadcast sender — cloned to send events.
    sender: broadcast::Sender<Event>,
    /// Tracks active subscriptions (client_id → bool).
    subscriptions: Mutex<HashMap<ClientId, bool>>,
}

impl EventBus {
    /// Create a new event bus with the given channel capacity.
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self {
            sender,
            subscriptions: Mutex::new(HashMap::new()),
        }
    }

    /// Subscribe a client to receive events.
    ///
    /// Returns a broadcast receiver that the client can use to receive events.
    pub fn subscribe(&self, client_id: ClientId) -> broadcast::Receiver<Event> {
        let rx = self.sender.subscribe();
        self.subscriptions.lock().unwrap().insert(client_id, true);
        rx
    }

    /// Remove a client's subscription.
    pub fn unsubscribe(&self, client_id: &ClientId) {
        self.subscriptions.lock().unwrap().remove(client_id);
    }

    /// Check if a client is subscribed.
    pub fn is_subscribed(&self, client_id: &ClientId) -> bool {
        self.subscriptions.lock().unwrap().contains_key(client_id)
    }

    /// Broadcast an event to all subscribed clients.
    ///
    /// Returns the number of receivers that will receive the event.
    /// Returns 0 if there are no active subscribers.
    pub fn broadcast(&self, event: Event) -> usize {
        self.sender.send(event).unwrap_or(0)
    }

    /// Get the number of active subscriptions.
    pub fn subscription_count(&self) -> usize {
        self.subscriptions.lock().unwrap().len()
    }

    /// Get a clone of the sender for use by other components.
    pub fn sender(&self) -> broadcast::Sender<Event> {
        self.sender.clone()
    }
}

/// Shared event bus wrapped in an Arc for use across async tasks.
pub type SharedEventBus = Arc<EventBus>;

/// Create a new shared event bus.
pub fn new_event_bus(capacity: usize) -> SharedEventBus {
    Arc::new(EventBus::new(capacity))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_id_unique() {
        let id1 = ClientId::new();
        let id2 = ClientId::new();
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_client_id_default() {
        let id1 = ClientId::default();
        let id2 = ClientId::default();
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_event_bus_subscribe_unsubscribe() {
        let bus = EventBus::new(16);
        let client = ClientId::new();

        assert_eq!(bus.subscription_count(), 0);
        assert!(!bus.is_subscribed(&client));

        let _rx = bus.subscribe(client);
        assert_eq!(bus.subscription_count(), 1);
        assert!(bus.is_subscribed(&client));

        bus.unsubscribe(&client);
        assert_eq!(bus.subscription_count(), 0);
        assert!(!bus.is_subscribed(&client));
    }

    #[test]
    fn test_event_bus_multiple_subscribers() {
        let bus = EventBus::new(16);
        let c1 = ClientId::new();
        let c2 = ClientId::new();
        let c3 = ClientId::new();

        let _r1 = bus.subscribe(c1);
        let _r2 = bus.subscribe(c2);
        let _r3 = bus.subscribe(c3);
        assert_eq!(bus.subscription_count(), 3);

        bus.unsubscribe(&c2);
        assert_eq!(bus.subscription_count(), 2);
        assert!(bus.is_subscribed(&c1));
        assert!(!bus.is_subscribed(&c2));
        assert!(bus.is_subscribed(&c3));
    }

    #[tokio::test]
    async fn test_event_bus_broadcast_status_change() {
        let bus = EventBus::new(16);
        let client = ClientId::new();
        let mut rx = bus.subscribe(client);

        let event = Event::StatusChange {
            item_id: "T1".to_string(),
            item_type: "task".to_string(),
            old_status: "pending".to_string(),
            new_status: "in_progress".to_string(),
            project_id: "p1".to_string(),
        };

        let count = bus.broadcast(event);
        assert_eq!(count, 1);

        let received = rx.recv().await.unwrap();
        match received {
            Event::StatusChange {
                item_id,
                new_status,
                ..
            } => {
                assert_eq!(item_id, "T1");
                assert_eq!(new_status, "in_progress");
            }
            _ => panic!("expected StatusChange event"),
        }
    }

    #[tokio::test]
    async fn test_event_bus_broadcast_agent_output() {
        let bus = EventBus::new(16);
        let client = ClientId::new();
        let mut rx = bus.subscribe(client);

        let event = Event::AgentOutput {
            task_id: "W1-T1".to_string(),
            line: "Running cargo test...".to_string(),
        };

        bus.broadcast(event);

        let received = rx.recv().await.unwrap();
        match received {
            Event::AgentOutput { task_id, line } => {
                assert_eq!(task_id, "W1-T1");
                assert_eq!(line, "Running cargo test...");
            }
            _ => panic!("expected AgentOutput event"),
        }
    }

    #[tokio::test]
    async fn test_event_bus_broadcast_story_completed() {
        let bus = EventBus::new(16);
        let client = ClientId::new();
        let mut rx = bus.subscribe(client);

        let event = Event::StoryCompleted {
            story_id: "S1".to_string(),
            project_id: "p1".to_string(),
            branch_name: "feature/s1".to_string(),
            mr_url: Some("https://github.com/org/repo/pull/42".to_string()),
        };

        bus.broadcast(event);

        let received = rx.recv().await.unwrap();
        match received {
            Event::StoryCompleted {
                story_id, mr_url, ..
            } => {
                assert_eq!(story_id, "S1");
                assert_eq!(
                    mr_url,
                    Some("https://github.com/org/repo/pull/42".to_string())
                );
            }
            _ => panic!("expected StoryCompleted event"),
        }
    }

    #[tokio::test]
    async fn test_event_bus_multiple_receivers() {
        let bus = EventBus::new(16);
        let c1 = ClientId::new();
        let c2 = ClientId::new();
        let mut rx1 = bus.subscribe(c1);
        let mut rx2 = bus.subscribe(c2);

        let event = Event::AgentOutput {
            task_id: "T1".to_string(),
            line: "hello".to_string(),
        };

        let count = bus.broadcast(event);
        assert_eq!(count, 2);

        let e1 = rx1.recv().await.unwrap();
        let e2 = rx2.recv().await.unwrap();
        match (&e1, &e2) {
            (Event::AgentOutput { line: l1, .. }, Event::AgentOutput { line: l2, .. }) => {
                assert_eq!(l1, "hello");
                assert_eq!(l2, "hello");
            }
            _ => panic!("expected AgentOutput events"),
        }
    }

    #[test]
    fn test_event_bus_broadcast_no_subscribers() {
        let bus = EventBus::new(16);
        let count = bus.broadcast(Event::AgentOutput {
            task_id: "T1".to_string(),
            line: "nobody listening".to_string(),
        });
        assert_eq!(count, 0);
    }

    #[test]
    fn test_event_bus_unsubscribe_nonexistent() {
        let bus = EventBus::new(16);
        let client = ClientId::new();
        // Should not panic
        bus.unsubscribe(&client);
        assert_eq!(bus.subscription_count(), 0);
    }

    #[test]
    fn test_event_serialization_status_change() {
        let event = Event::StatusChange {
            item_id: "T1".to_string(),
            item_type: "task".to_string(),
            old_status: "pending".to_string(),
            new_status: "in_progress".to_string(),
            project_id: "p1".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"status_change""#));
        assert!(json.contains(r#""item_id":"T1""#));

        let deserialized: Event = serde_json::from_str(&json).unwrap();
        match deserialized {
            Event::StatusChange { item_id, .. } => assert_eq!(item_id, "T1"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_event_serialization_agent_output() {
        let event = Event::AgentOutput {
            task_id: "W1-T1".to_string(),
            line: "output line".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"agent_output""#));
        assert!(json.contains(r#""task_id":"W1-T1""#));
    }

    #[test]
    fn test_event_serialization_story_completed() {
        let event = Event::StoryCompleted {
            story_id: "S1".to_string(),
            project_id: "p1".to_string(),
            branch_name: "feature/s1".to_string(),
            mr_url: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"story_completed""#));
        assert!(json.contains(r#""mr_url":null"#));
    }

    #[test]
    fn test_new_event_bus() {
        let bus = new_event_bus(32);
        assert_eq!(bus.subscription_count(), 0);
    }

    #[test]
    fn test_event_bus_sender() {
        let bus = EventBus::new(16);
        let client = ClientId::new();
        let mut rx = bus.subscribe(client);

        // Use the sender directly
        let sender = bus.sender();
        sender
            .send(Event::AgentOutput {
                task_id: "T1".to_string(),
                line: "via sender".to_string(),
            })
            .unwrap();

        let received = rx.try_recv().unwrap();
        match received {
            Event::AgentOutput { line, .. } => assert_eq!(line, "via sender"),
            _ => panic!("expected AgentOutput"),
        }
    }

    #[tokio::test]
    async fn test_disconnect_removes_subscription() {
        let bus = new_event_bus(16);
        let client = ClientId::new();

        let _rx = bus.subscribe(client);
        assert_eq!(bus.subscription_count(), 1);

        // Simulate disconnect by dropping rx and unsubscribing
        bus.unsubscribe(&client);
        assert_eq!(bus.subscription_count(), 0);
    }
}
