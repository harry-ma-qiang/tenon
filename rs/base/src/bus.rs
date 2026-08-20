use crate::blob::BlobFacade;
use crate::home::Home;
use crate::kv::KvFacade;
use crate::timer::TimerService;
use std::sync::{Arc, Mutex};
use tenon_bus::{Durable, Envelope, Hub};
use tenon_storage::{EnvelopeRow, Store};

/// The durable side of the hub, backed by the barebone state file. The hub's
/// single writer task is the only caller of `append_batch`, so one mutex over
/// one connection is enough; replay and `head` share it. This is the seam RFC
/// section 7 keeps open for an append-log + snapshot (openraft) replacement.
pub struct StoreDurable {
    store: Arc<Mutex<Store>>,
}

impl StoreDurable {
    pub fn new(store: Arc<Mutex<Store>>) -> Self {
        Self { store }
    }
}

impl Durable for StoreDurable {
    fn append_batch(&self, batch: &[Envelope]) -> Result<Vec<u64>, String> {
        let bodies: Vec<String> = batch
            .iter()
            .map(|env| String::from_utf8_lossy(&env.encode()).into_owned())
            .collect();
        let rows: Vec<EnvelopeRow<'_>> = batch
            .iter()
            .zip(bodies.iter())
            .map(|(env, body)| EnvelopeRow {
                event_id: &env.event_id,
                topic: &env.topic,
                env: env.env.as_deref(),
                ts: env.ts,
                body,
            })
            .collect();
        self.store
            .lock()
            .expect("durable store")
            .append_envelopes(&rows)
            .map_err(|error| error.to_string())
    }

    fn since(&self, after: i64, limit: i64) -> Result<Vec<(u64, Envelope)>, String> {
        let rows = self
            .store
            .lock()
            .expect("durable store")
            .envelopes_since(after, None, limit)
            .map_err(|error| error.to_string())?;
        Ok(rows
            .into_iter()
            .filter_map(|(offset, body)| {
                serde_json::from_str::<Envelope>(&body)
                    .ok()
                    .map(|env| (offset, env))
            })
            .collect())
    }

    fn head(&self) -> u64 {
        self.store
            .lock()
            .expect("durable store")
            .envelopes_head()
            .unwrap_or(0)
    }
}

/// The four facades of RFC section 3, bundled so `foreground` builds them once
/// and the server and the actor share the same handles. Each holds its own
/// shared connection to the barebone state file (WAL, so concurrent with base's
/// own writer).
#[derive(Clone)]
pub struct Facades {
    pub hub: Arc<Hub>,
    pub kv: Arc<KvFacade>,
    pub blob: Arc<BlobFacade>,
    pub timer: Arc<TimerService>,
    #[cfg(feature = "http")]
    pub secrets: Arc<crate::secret::Secrets>,
}

impl Facades {
    /// Opens the facade store, builds the durable hub, the kv facade with its
    /// expiry tick, the blob facade and the timer wheel (which reloads its
    /// persisted timers from kv). The tracing layer is installed process-wide so
    /// `info!` in any Rust component becomes an envelope.
    pub fn build(home: &Home) -> anyhow::Result<Facades> {
        let store = Arc::new(Mutex::new(Store::open(&home.state_file())?));
        let hub = Hub::with_durable(Arc::new(StoreDurable::new(store.clone())));
        let kv = KvFacade::new(store.clone(), hub.clone());
        let blob = Arc::new(BlobFacade::new(store));
        let timer = TimerService::new(kv.clone(), hub.clone());
        install_layer(&hub);
        #[cfg(feature = "http")]
        let secrets = crate::secret::Secrets::new(home.secrets_file(), hub.clone());
        Ok(Facades {
            hub,
            kv,
            blob,
            timer,
            #[cfg(feature = "http")]
            secrets,
        })
    }
}

fn install_layer(hub: &Arc<Hub>) {
    use tracing_subscriber::prelude::*;
    let layer = tenon_bus::BusLayer::new(hub.clone(), "base");
    let _ = tracing_subscriber::registry().with(layer).try_init();
}
