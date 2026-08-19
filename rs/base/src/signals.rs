use crate::rpc::Cmd;
use anyhow::Result;
use tokio::signal::unix::{signal, Signal, SignalKind};
use tokio::sync::mpsc;

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

    /// SIGUSR1 is the kill switch's third carrier (RFC section 5): same halt
    /// as the STOP file and the `kill` frame, sent from a shell that has
    /// nothing but a pid.
    pub fn kill_switch(cmds: mpsc::UnboundedSender<Cmd>) -> Result<()> {
        let mut usr1 = signal(SignalKind::user_defined1())?;
        tokio::spawn(async move {
            while usr1.recv().await.is_some() {
                if cmds
                    .send(Cmd::Kill {
                        on: true,
                        reason: "SIGUSR1".to_string(),
                        reply: None,
                    })
                    .is_err()
                {
                    return;
                }
            }
        });
        Ok(())
    }

    pub async fn recv(&mut self) {
        tokio::select! {
            _ = self.term.recv() => {},
            _ = self.interrupt.recv() => {},
        }
    }
}
