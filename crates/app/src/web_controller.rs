//! Serializes listener reconfiguration away from GTK and owns its shutdown.
use crate::{
    storage::Source,
    web_config::{self, Config},
    web_server,
};
use std::sync::{Arc, RwLock, mpsc};

type Reply = mpsc::Sender<Result<(), String>>;
enum Command {
    Apply(Config, Reply),
    Shutdown,
}
#[derive(Clone)]
pub struct Controller {
    commands: mpsc::Sender<Command>,
    pub sources: Arc<RwLock<Vec<Source>>>,
}
impl Controller {
    pub fn start() -> (Self, mpsc::Receiver<Result<(), String>>) {
        let (commands, rx) = mpsc::channel();
        let (startup, result) = mpsc::channel();
        let sources = Arc::new(RwLock::new(Vec::new()));
        let shared = sources.clone();
        let startup_failure = startup.clone();
        if std::thread::Builder::new()
            .name("lan-controller".into())
            .spawn(move || {
                let mut server = None;
                let initial = web_config::load()
                    .map_err(|e| e.to_string())
                    .and_then(|config| apply(&mut server, config, &shared));
                let _ = startup.send(initial);
                while let Ok(command) = rx.recv() {
                    match command {
                        Command::Apply(config, reply) => {
                            let _ = reply.send(apply(&mut server, config, &shared));
                        }
                        Command::Shutdown => break,
                    }
                }
            })
            .is_err()
        {
            let _ = startup_failure.send(Err("Could not start LAN configuration worker.".into()));
        }
        (Self { commands, sources }, result)
    }
    pub fn apply(&self, config: Config) -> mpsc::Receiver<Result<(), String>> {
        let (tx, rx) = mpsc::channel();
        if let Err(error) = self.commands.send(Command::Apply(config, tx)) {
            if let Command::Apply(_, reply) = error.0 {
                let _ = reply.send(Err("LAN configuration worker stopped.".into()));
            }
        }
        rx
    }
    pub fn shutdown(&self) {
        let _ = self.commands.send(Command::Shutdown);
    }
}
fn apply(
    server: &mut Option<web_server::Server>,
    config: Config,
    sources: &Arc<RwLock<Vec<Source>>>,
) -> Result<(), String> {
    // Drop and join before binding again, so changing settings on the same port works.
    drop(server.take());
    if config.enabled {
        *server = Some(web_server::start(config, sources.clone())?);
    }
    Ok(())
}
