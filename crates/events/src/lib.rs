//! Engine event model + in-process publish/subscribe broker.
//!
//! Mirrors Moby's `api/types/events.Message` (pinned moby revision) plus the
//! daemon Events manager (`daemon/events/events.go`). Field names match the
//! wire format produced by the Engine API events endpoint:
//!
//! ```json
//! {"type":"container","action":"start","actor":{"ID":"...","attributes":{"name":"web"}},
//!  "scope":"local","time":1700000000,"timeNano":1700000000000000000}
//! ```
//!
//! Exact casing is preserved — `/events` payloads use lowercase `type`,
//! `action`, `scope`, while the nested `actor` carries an uppercase `"ID"`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use chrono::Utc;
use docker_errors::{Error, Result};
use serde::{Deserialize, Serialize};

/// Event subject categories as the daemon's events endpoint emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventType {
    Container,
    Image,
    Network,
    Volume,
    Daemon,
    Builder,
    Plugin,
}

impl EventType {
    pub fn as_str(self) -> &'static str {
        match self {
            EventType::Container => "container",
            EventType::Image => "image",
            EventType::Network => "network",
            EventType::Volume => "volume",
            EventType::Daemon => "daemon",
            EventType::Builder => "builder",
            EventType::Plugin => "plugin",
        }
    }
}

/// Actions this engine emits natively.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum KnownAction {
    Create,
    Destroy,
    Start,
    Die,
    Restart,
    Kill,
    Stop,
    Pause,
    Unpause,
    Pull,
    Push,
    Tag,
    Untag,
    Delete,
    Mount,
    Unmount,
}

impl KnownAction {
    pub fn as_str(self) -> &'static str {
        match self {
            KnownAction::Create => "create",
            KnownAction::Destroy => "destroy",
            KnownAction::Start => "start",
            KnownAction::Die => "die",
            KnownAction::Restart => "restart",
            KnownAction::Kill => "kill",
            KnownAction::Stop => "stop",
            KnownAction::Pause => "pause",
            KnownAction::Unpause => "unpause",
            KnownAction::Pull => "pull",
            KnownAction::Push => "push",
            KnownAction::Tag => "tag",
            KnownAction::Untag => "untag",
            KnownAction::Delete => "delete",
            KnownAction::Mount => "mount",
            KnownAction::Unmount => "unmount",
        }
    }
}

/// Action value: known + verbatim-unknown passthrough. The daemon forwards
/// upstream action strings verbatim, so unknown verbs must round-trip
/// losslessly instead of becoming parse errors (forward compatibility with
/// future upstream releases).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EventAction {
    Known(KnownAction),
    Unknown(String),
}

impl std::fmt::Display for EventAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EventAction::Known(k) => f.write_str(k.as_str()),
            EventAction::Unknown(s) => f.write_str(s),
        }
    }
}

/// Event scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventScope {
    Local,
    Swarm,
}

/// One engine event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    #[serde(rename = "type")]
    pub r#type: EventType,
    #[serde(rename = "action")]
    pub action: EventAction,
    pub actor: Actor,
    pub scope: EventScope,
    /// Unix seconds (daemon `Time`).
    pub time: i64,
    /// Unix nanoseconds (daemon `TimeNano`).
    #[serde(rename = "timeNano")]
    pub time_nano: i64,
}

/// Actor identification block. The wire uses lowercase `attributes` and an
/// uppercase `ID`, matching events.Message.Actor json tags upstream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Actor {
    #[serde(rename = "ID")]
    pub id: String,
    #[serde(rename = "attributes")]
    pub attributes: BTreeMap<String, String>,
}

/// Build an event stamped to *now* with `scope = local`.
pub fn event_now(r#type: EventType, action: EventAction, actor: Actor) -> Event {
    let now = Utc::now();
    Event {
        r#type,
        action,
        actor,
        scope: EventScope::Local,
        time: now.timestamp(),
        time_nano: now.timestamp_nanos_opt().unwrap_or_else(|| {
            now.timestamp() * 1_000_000_000 + now.timestamp_subsec_nanos() as i64
        }),
    }
}

/// Publishers/subscribers in-process broker.
///
/// Each subscriber owns a dedicated bounded queue. Publishing is
/// non-blocking: a full subscriber queue drops the event for that subscriber
/// only (upstream kills the event stream of slow listeners; docker-rs drops
/// events — this difference and its reasons live in docs/API_COMPATIBILITY.md).
/// Disconnected subscribers are pruned on publish.
#[derive(Debug, Default)]
pub struct EventBroker {
    inner: Mutex<BrokerState>,
}

#[derive(Debug, Default)]
struct BrokerState {
    subs: Vec<Subscriber>,
    next_id: u64,
}

#[derive(Debug)]
struct Subscriber {
    id: u64,
    tx: std::sync::mpsc::SyncSender<Event>,
}

const MAX_SUBSCRIBER_BUFFER: usize = 65536;

impl EventBroker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a subscription with a bounded buffer of `buffer` events.
    pub fn subscribe(self: &Arc<Self>, buffer: usize) -> Result<Subscription> {
        if buffer == 0 || buffer > MAX_SUBSCRIBER_BUFFER {
            return Err(Error::invalid_argument(format!(
                "subscribe buffer must be 1..={MAX}, got {buffer}",
                MAX = MAX_SUBSCRIBER_BUFFER
            )));
        }
        let (tx, rx) = std::sync::mpsc::sync_channel(buffer);
        let mut state = self.inner.lock().expect("event broker lock poisoned");
        let id = state.next_id;
        state.next_id += 1;
        state.subs.push(Subscriber { id, tx });
        Ok(Subscription {
            rx,
            id,
            broker: Arc::clone(self),
        })
    }

    /// Deliver an event to all connected, non-full subscribers. Returns how
    /// many subscribers received it.
    pub fn publish(&self, event: Event) -> usize {
        let mut delivered = 0;
        let mut state = self.inner.lock().expect("event broker lock poisoned");
        let mut dead: Vec<u64> = Vec::new();
        for sub in &state.subs {
            match sub.tx.try_send(event.clone()) {
                Ok(()) => delivered += 1,
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => dead.push(sub.id),
                Err(std::sync::mpsc::TrySendError::Full(_)) => (),
            }
        }
        state.subs.retain(|s| !dead.contains(&s.id));
        delivered
    }

    /// Number of connected subscriber channels.
    pub fn subscriber_count(&self) -> usize {
        self.inner
            .lock()
            .expect("event broker lock poisoned")
            .subs
            .len()
    }
}

/// Receive handle for an EventBroker subscription.
#[derive(Debug)]
pub struct Subscription {
    rx: std::sync::mpsc::Receiver<Event>,
    id: u64,
    #[allow(dead_code)]
    broker: Arc<EventBroker>,
}

impl Subscription {
    pub fn recv_blocking(&self) -> Option<Event> {
        self.rx.recv().ok()
    }

    pub fn try_recv(&self) -> Option<Event> {
        self.rx.try_recv().ok()
    }

    pub fn subscription_id(&self) -> u64 {
        self.id
    }
}

/// Attribute helpers used by subsystems to build actors.
pub mod attrs {
    use super::Actor;
    use std::collections::BTreeMap;

    /// Container actor with the standard attributes the daemon attaches
    /// (`image`, `name`, `exitCode`, `exitSignal`, ...).
    pub fn container(container_id: &str, image: &str, name: &str) -> Actor {
        let mut attributes = BTreeMap::new();
        attributes.insert("image".to_string(), image.to_string());
        attributes.insert("name".to_string(), name.to_string());
        Actor {
            id: container_id.to_string(),
            attributes,
        }
    }

    /// Image actor carrying the image reference as its sole attribute.
    pub fn image(name: &str) -> Actor {
        let mut attributes = BTreeMap::new();
        attributes.insert("name".to_string(), name.to_string());
        Actor {
            id: name.to_string(),
            attributes,
        }
    }

    /// Id-only actor (daemon-meta events).
    pub fn bare(id: &str) -> Actor {
        Actor {
            id: id.to_string(),
            attributes: BTreeMap::new(),
        }
    }

    /// Attach container exit metadata to a die/destroy event.
    pub fn with_exit(mut a: Actor, exit_code: i64, signal: Option<&str>) -> Actor {
        a.attributes
            .insert("exitCode".to_string(), exit_code.to_string());
        if let Some(sig) = signal {
            a.attributes
                .insert("exitSignal".to_string(), sig.to_string());
        }
        a
    }
}

/// Parse a serialized event from the documented wire shape.
pub fn parse_wire_event(json: &str) -> Result<Event> {
    serde_json::from_str(json)
        .map_err(|e| Error::invalid_argument(format!("event wire parse failed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn actor() -> Actor {
        attrs::container("abc123", "alpine:latest", "web-1")
    }

    #[test]
    fn wire_shape_matches_upstream_casing() {
        let ev = event_now(
            EventType::Container,
            EventAction::Known(KnownAction::Start),
            actor(),
        );
        let json = serde_json::to_value(&ev).unwrap();
        let obj = json.as_object().unwrap();
        assert!(obj.contains_key("type"), "lowercase type");
        assert!(obj.contains_key("action"), "lowercase action");
        assert!(obj.contains_key("timeNano"), "camelCase nanos");
        let wire_actor = obj.get("actor").unwrap().as_object().unwrap();
        assert!(wire_actor.contains_key("ID"), "uppercase ID in actor");

        let back: Event = serde_json::from_value(json).unwrap();
        assert_eq!(back.actor.id, "abc123");
        assert_eq!(back.actor.attributes.get("name").unwrap(), "web-1");
        assert_eq!(back.time_nano / 1_000_000_000, back.time);
    }

    #[test]
    fn unknown_actions_survive_roundtrip() {
        let ev = event_now(
            EventType::Builder,
            EventAction::Unknown("prune-deferred-widgets".into()),
            attrs::bare("0"),
        );
        let json = serde_json::to_string(&ev).unwrap();
        let back = parse_wire_event(&json).unwrap();
        assert_eq!(
            back.action,
            EventAction::Unknown("prune-deferred-widgets".into())
        );
        assert_eq!(back.r#type, EventType::Builder);
    }

    #[test]
    fn broker_fans_out_to_subscribers() {
        let broker = Arc::new(EventBroker::new());
        let s1 = broker.subscribe(8).unwrap();
        let s2 = broker.subscribe(8).unwrap();

        let delivered = broker.publish(event_now(
            EventType::Image,
            EventAction::Known(KnownAction::Pull),
            attrs::image("alpine"),
        ));
        assert_eq!(delivered, 2);

        let e1 = s1.recv_blocking().unwrap();
        let e2 = s2.recv_blocking().unwrap();
        assert_eq!(e1.actor.id, "alpine");
        assert_eq!(e2.actor.id, "alpine");
    }

    #[test]
    fn full_buffer_drops_for_slow_subscriber() {
        let broker = Arc::new(EventBroker::new());
        let slow = broker.subscribe(1).unwrap();

        assert_eq!(
            broker.publish(event_now(
                EventType::Container,
                EventAction::Known(KnownAction::Create),
                actor()
            )),
            1
        );
        // Subscriber buffer still holds event 1 unconsumed: second is dropped.
        assert_eq!(
            broker.publish(event_now(
                EventType::Container,
                EventAction::Known(KnownAction::Create),
                actor()
            )),
            0
        );
        let _ = slow.recv_blocking().unwrap();
        assert_eq!(
            broker.publish(event_now(
                EventType::Container,
                EventAction::Known(KnownAction::Start),
                actor()
            )),
            1
        );
    }

    #[test]
    fn closed_subscriber_pruned_on_publish() {
        let broker = Arc::new(EventBroker::new());
        {
            let _s = broker.subscribe(4).unwrap();
            assert_eq!(broker.subscriber_count(), 1);
        }
        let delivered = broker.publish(event_now(
            EventType::Daemon,
            EventAction::Known(KnownAction::Delete),
            attrs::bare("x"),
        ));
        assert_eq!(delivered, 0);
        assert_eq!(broker.subscriber_count(), 0);
    }

    #[test]
    fn wire_parse_from_doc_example() {
        let wire = json!({
            "type":"container",
            "action":"start",
            "actor":{"ID":"abc12","attributes":{"name":"web","image":"alpine"}},
            "scope":"local",
            "time":1700000000,
            "timeNano":1700000000000000000i64
        });
        let ev: Event = serde_json::from_value(wire).unwrap();
        assert_eq!(ev.r#type, EventType::Container);
        assert_eq!(ev.time, 1700000000);
        assert_eq!(ev.time_nano, 1700000000000000000);
        assert_eq!(ev.actor.id, "abc12");
        assert_eq!(ev.scope, EventScope::Local);
    }

    #[test]
    fn subscribe_buffer_bounds() {
        let broker = Arc::new(EventBroker::new());
        assert_eq!(
            broker.subscribe(0).unwrap_err().kind(),
            docker_errors::ErrorKind::InvalidArgument
        );
        assert_eq!(
            broker
                .subscribe(MAX_SUBSCRIBER_BUFFER + 1)
                .unwrap_err()
                .kind(),
            docker_errors::ErrorKind::InvalidArgument
        );
        let _ = broker.subscribe(64).unwrap();
        assert_eq!(broker.subscriber_count(), 1);
    }

    #[test]
    fn actor_helpers_carry_exit_metadata() {
        let a = attrs::with_exit(attrs::container("id", "img", "n"), 137, Some("SIGKILL"));
        assert_eq!(a.attributes.get("exitCode").unwrap(), "137");
        assert_eq!(a.attributes.get("exitSignal").unwrap(), "SIGKILL");
    }
}
