use crate::envelope::{Envelope, Level};
use crate::hub::Hub;
use serde_json::{Map, Value};
use std::sync::Arc;
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::Layer;

/// The `tracing` bridge of RFC section 4: importing the crate is a Rust
/// component's messaging ability. Every `info!`/`event!` becomes an envelope,
/// its fields the payload, its `topic`/`env`/`durable`/`session` fields the
/// closed core. No field means the envelope is a plain log line under the
/// event's target.
pub struct BusLayer {
    hub: Arc<Hub>,
    src: String,
}

impl BusLayer {
    pub fn new(hub: Arc<Hub>, src: impl Into<String>) -> Self {
        Self {
            hub,
            src: src.into(),
        }
    }
}

impl<S: Subscriber> Layer<S> for BusLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        let metadata = event.metadata();
        let level = match *metadata.level() {
            tracing::Level::TRACE => Level::Trace,
            tracing::Level::DEBUG => Level::Debug,
            tracing::Level::INFO => Level::Info,
            tracing::Level::WARN => Level::Warn,
            tracing::Level::ERROR => Level::Error,
        };
        let topic = visitor
            .topic
            .unwrap_or_else(|| format!("internal/{}", metadata.target()));
        let mut envelope = Envelope::new(topic, level, Value::Object(visitor.payload));
        envelope.src = self.src.clone();
        envelope.env = visitor.env;
        envelope.session = visitor.session;
        envelope.durable = visitor.durable;
        envelope.model_visible = visitor.model_visible;
        self.hub.emit(envelope);
    }
}

#[derive(Default)]
struct FieldVisitor {
    topic: Option<String>,
    env: Option<String>,
    session: Option<String>,
    durable: bool,
    model_visible: bool,
    payload: Map<String, Value>,
}

impl FieldVisitor {
    fn string(&mut self, field: &Field, value: String) {
        match field.name() {
            "topic" => self.topic = Some(value),
            "env" => self.env = Some(value),
            "session" => self.session = Some(value),
            name => {
                self.payload.insert(name.to_string(), Value::String(value));
            }
        }
    }
}

impl Visit for FieldVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.string(field, value.to_string());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        match field.name() {
            "durable" => self.durable = value,
            "model_visible" => self.model_visible = value,
            name => {
                self.payload.insert(name.to_string(), Value::Bool(value));
            }
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.payload
            .insert(field.name().to_string(), Value::from(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.payload
            .insert(field.name().to_string(), Value::from(value));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.payload
            .insert(field.name().to_string(), Value::from(value));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.string(field, format!("{value:?}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::Filter;
    use crate::hub::SubOpts;
    use tracing_subscriber::prelude::*;

    #[tokio::test]
    async fn tracing_events_become_envelopes() {
        let hub = Hub::new();
        let sub = hub.subscribe(Filter::all(), SubOpts::default());
        let layer = BusLayer::new(hub.clone(), "test");
        let dispatch = tracing_subscriber::registry().with(layer).into();
        tracing::dispatcher::with_default(&dispatch, || {
            tracing::info!(topic = "worker/step", env = "root", n = 7, "stepped");
        });
        let batch = sub.recv().await.expect("batch");
        let envelope = &batch[0].envelope;
        assert_eq!(envelope.topic, "worker/step");
        assert_eq!(envelope.env.as_deref(), Some("root"));
        assert_eq!(envelope.src, "test");
        assert_eq!(envelope.payload["n"], 7);
    }
}
