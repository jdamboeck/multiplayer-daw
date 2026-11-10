//! The network side: accepts connections and turns their streams into hub commands.

use crate::clock::Clock;
use crate::hub::{Hub, Sink};
use crate::store::Store;
use anyhow::{Context, Result, bail};
use mdaw_model::ActorId;
use mdaw_net::{Identity, JoinInfo, read_frame, write_frame};
use mdaw_proto::{ClientMsg, Datagram, Hello, RefuseReason, ServerMsg};
use quinn::{Connection, Endpoint, VarInt};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;
use std::thread::JoinHandle;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

/// Messages a client may have waiting to be written before it counts as too slow.
const OUTBOX: usize = 4096;

/// How long a new connection has to say hello.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

pub struct Config {
    /// Where the project lives. Created if missing.
    pub data_dir: PathBuf,
    pub listen: SocketAddr,
    /// The host name or address clients should use, for the join string. Defaults to the listen
    /// address, or to localhost when listening on all interfaces.
    pub host: Option<String>,
}

enum Command {
    Join {
        hello: Hello,
        sink: Sink,
        reply: oneshot::Sender<Result<ActorId, RefuseReason>>,
    },
    Msg {
        actor: ActorId,
        msg: ClientMsg,
    },
    Leave {
        actor: ActorId,
    },
    Stop,
}

/// A running server. Stops when dropped.
pub struct Server {
    join: JoinInfo,
    endpoint: Endpoint,
    clock: Clock,
    runtime: Option<tokio::runtime::Runtime>,
    commands: std_mpsc::Sender<Command>,
    hub: Option<JoinHandle<()>>,
}

impl Server {
    pub fn start(config: Config) -> Result<Self> {
        if !config.data_dir.exists() {
            std::fs::create_dir_all(&config.data_dir)
                .with_context(|| format!("creating {}", config.data_dir.display()))?;
            // The directory holds the private key and the join token.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&config.data_dir, std::fs::Permissions::from_mode(0o700))?;
            }
        }
        let identity = Identity::load_or_create(&config.data_dir)?;
        let hub = Hub::open(Store::open(&config.data_dir.join("project.db"))?)?;
        let token = hub.token().to_string();
        let clock = hub.clock();

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("mdaw-net")
            .enable_all()
            .build()?;
        let endpoint = {
            let _in_runtime = runtime.enter();
            mdaw_net::listen(config.listen, &identity)?
        };
        let addr = endpoint.local_addr()?;
        let host = match config.host {
            Some(host) => host,
            None if addr.ip().is_unspecified() => "127.0.0.1".into(),
            None => addr.ip().to_string(),
        };
        let join = JoinInfo {
            host,
            port: addr.port(),
            fingerprint: identity.fingerprint(),
            token,
        };

        let (commands, inbox) = std_mpsc::channel();
        let hub = std::thread::Builder::new()
            .name("mdaw-project".into())
            .spawn(move || run_hub(hub, inbox))?;
        runtime.spawn(accept(endpoint.clone(), commands.clone(), clock));
        Ok(Self {
            join,
            endpoint,
            clock,
            runtime: Some(runtime),
            commands,
            hub: Some(hub),
        })
    }

    /// What clients need to connect.
    pub fn join_info(&self) -> &JoinInfo {
        &self.join
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.endpoint.local_addr()?)
    }

    /// The clock the shared transport runs on.
    pub fn clock(&self) -> Clock {
        self.clock
    }

    /// Disconnects everyone and waits until the project is on disk.
    pub fn stop(mut self) {
        self.shut_down();
    }

    fn shut_down(&mut self) {
        let Some(runtime) = self.runtime.take() else {
            return;
        };
        self.endpoint.close(VarInt::from_u32(0), b"server stopped");
        // Give the close frames a moment to leave, so clients notice at once and not by timeout.
        runtime.block_on(async {
            let _ = tokio::time::timeout(Duration::from_millis(500), self.endpoint.wait_idle()).await;
        });
        runtime.shutdown_timeout(Duration::from_secs(1));
        let _ = self.commands.send(Command::Stop);
        if let Some(hub) = self.hub.take() {
            let _ = hub.join();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shut_down();
    }
}

fn run_hub(mut hub: Hub, inbox: std_mpsc::Receiver<Command>) {
    while let Ok(command) = inbox.recv() {
        match command {
            Command::Join { hello, sink, reply } => {
                let _ = reply.send(hub.join(hello, sink));
            }
            Command::Msg { actor, msg } => hub.handle(actor, msg),
            Command::Leave { actor } => hub.leave(actor),
            Command::Stop => break,
        }
    }
}

async fn accept(endpoint: Endpoint, commands: std_mpsc::Sender<Command>, clock: Clock) {
    while let Some(incoming) = endpoint.accept().await {
        let commands = commands.clone();
        tokio::spawn(async move {
            // A connection that fails is the client's to retry; the server has nothing to do.
            if let Ok(connection) = incoming.await {
                let _ = serve(&connection, commands, clock).await;
                connection.close(VarInt::from_u32(0), b"");
            }
        });
    }
}

async fn serve(connection: &Connection, commands: std_mpsc::Sender<Command>, clock: Clock) -> Result<()> {
    let (mut send, mut recv) = connection.accept_bi().await?;
    let first = tokio::time::timeout(HELLO_TIMEOUT, read_frame::<ClientMsg>(&mut recv)).await??;
    let Some(ClientMsg::Hello(hello)) = first else {
        write_frame(&mut send, &ServerMsg::Refused(RefuseReason::Protocol)).await?;
        return finish(send).await;
    };

    let (outbox, mut outgoing) = mpsc::channel::<ServerMsg>(OUTBOX);
    let sink: Sink = Box::new(move |msg| outbox.try_send(msg).is_ok());
    let (reply, joined) = oneshot::channel();
    if commands.send(Command::Join { hello, sink, reply }).is_err() {
        bail!("the server is stopping");
    }
    let actor = match joined.await? {
        Ok(actor) => actor,
        Err(reason) => {
            write_frame(&mut send, &ServerMsg::Refused(reason)).await?;
            return finish(send).await;
        }
    };

    // The hub drops its end of the outbox to drop the client; then the writer ends first.
    let writer = async {
        while let Some(msg) = outgoing.recv().await {
            write_frame(&mut send, &msg).await?;
        }
        anyhow::Ok(())
    };
    let reader = async {
        while let Some(msg) = read_frame::<ClientMsg>(&mut recv).await? {
            if commands.send(Command::Msg { actor, msg }).is_err() {
                break;
            }
        }
        anyhow::Ok(())
    };
    // Pings are answered right here, without a detour through the project thread: the less
    // time passes between reading the clock and sending it, the better the clients' estimates.
    let clock_service = async {
        loop {
            if let Some(Datagram::Ping { t0 }) = mdaw_net::read_datagram(connection).await? {
                let server_micros = clock.micros();
                mdaw_net::send_datagram(connection, &Datagram::Pong { t0, server_micros });
            }
        }
    };
    let result: Result<()> = tokio::select! {
        result = writer => result,
        result = reader => result,
        result = clock_service => result,
    };
    let _ = commands.send(Command::Leave { actor });
    result
}

/// Ends a stream and waits until the peer has everything written to it.
async fn finish(mut send: quinn::SendStream) -> Result<()> {
    send.finish()?;
    let _ = tokio::time::timeout(Duration::from_secs(2), send.stopped()).await;
    Ok(())
}
