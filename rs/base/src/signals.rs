use anyhow::Result;
use tokio::signal::unix::{signal, Signal, SignalKind};

pub struct Signals {
    term: Signal,
    interrupt: Signal,
}

impl Signals {
    pub fn install() -> Result<Self> {
        Ok(Self {
            term: signal(SignalKind::terminate())?,
            interrupt: signal(SignalKind::interrupt())?,
        })
    }

    pub async fn recv(&mut self) {
        tokio::select! {
            _ = self.term.recv() => {},
            _ = self.interrupt.recv() => {},
        }
    }
}
