//! The connection to the server, kept alive on its own thread.
//!
//! The UI thread never waits for the network: it takes what has arrived with [`Link::poll`] and
//! queues what it wants sent. When the connection breaks, the thread reconnects with growing
//! pauses and asks the server to resume where it left off.

use mdaw_engine::clock;
use mdaw_net::{ClockEstimate, ClockSync, JoinInfo};
use mdaw_proto::{ClientMsg, Datagram, Hello, PROTO_VERSION, RefuseReason, Resume, ServerMsg, Welcome};
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::thread::JoinHandle;
use std::time::Duration;
use tokio::sync::{mpsc, watch};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const RETRY_MIN: Duration = Duration::from_millis(250);
const RETRY_MAX: Duration = Duration::from_secs(4);
/// A new connection measures the server's clock with a quick burst of pings, then keeps the
/// estimate fresh at a slower pace.
const PING_BURST: u32 = 5;
const PING_BURST_EVERY: Duration = Duration::from_millis(20);
const PING_EVERY: Duration = Duration::from_secs(1);
/// How long leaving waits for the goodbye to reach the wire.
const CLOSE_TIMEOUT: Duration = Duration::from_millis(150);

/// Called from the network thread whenever something waits in [`Link::poll`].
pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// Who this client is to the server.
#[derive(Clone, Debug)]
pub struct Who {
    pub user_key: [u8; 16],
    pub name: String,
}

#[derive(Debug)]
pub enum LinkEvent {
    /// A connection is up. Messages queued from now on must carry `epoch`.
    Welcome { epoch: u64, welcome: Box<Welcome> },
    /// From the connection of the last `Welcome`, in the order the server sent them.
    Msg(ServerMsg),
    /// A better estimate of the server's clock on the connection of the last `Welcome`,
    /// against [`mdaw_engine::clock::now_micros`]. The first follows the welcome within
    /// moments.
    Clock(ClockEstimate),
    /// The connection broke, or could not be made. Another attempt follows.
    Down(String),
    /// The server turned this client away. No further attempt follows.
    Refused(RefuseReason),
}

enum Out {
    Msg(u64, ClientMsg),
    Rejoin,
}

pub struct Link {
    events: std_mpsc::Receiver<LinkEvent>,
    out: mpsc::UnboundedSender<Out>,
    stop: watch::Sender<bool>,
    thread: Option<JoinHandle<()>>,
}

impl Link {
    /// Starts connecting. The outcome arrives through [`Link::poll`].
    pub fn connect(join: JoinInfo, who: Who, waker: Waker) -> std::io::Result<Self> {
        let (events_tx, events) = std_mpsc::channel();
        let (out, out_rx) = mpsc::unbounded_channel();
        let (stop, stop_rx) = watch::channel(false);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let task = Task {
            join,
            who,
            events: events_tx,
            waker,
            out: out_rx,
            stop: stop_rx,
            resume: None,
            epoch: 0,
        };
        let thread = std::thread::Builder::new()
            .name("mdaw-link".into())
            .spawn(move || runtime.block_on(task.run()))?;
        Ok(Self {
            events,
            out,
            stop,
            thread: Some(thread),
        })
    }

    /// The next thing that happened, if any.
    pub fn poll(&self) -> Option<LinkEvent> {
        self.events.try_recv().ok()
    }

    /// Waits for the next thing to happen.
    pub fn wait(&self, timeout: Duration) -> Option<LinkEvent> {
        self.events.recv_timeout(timeout).ok()
    }

    /// Queues a message for the connection announced with `epoch`. It is dropped if that
    /// connection is gone by the time it would be written.
    pub fn send(&self, epoch: u64, msg: ClientMsg) {
        let _ = self.out.send(Out::Msg(epoch, msg));
    }

    /// Drops the connection and joins again from scratch, to receive the whole document.
    pub fn rejoin(&self) {
        let _ = self.out.send(Out::Rejoin);
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Task {
    join: JoinInfo,
    who: Who,
    events: std_mpsc::Sender<LinkEvent>,
    waker: Waker,
    out: mpsc::UnboundedReceiver<Out>,
    stop: watch::Receiver<bool>,
    /// How to continue after the connection breaks. `None` before the first welcome.
    resume: Option<Resume>,
    /// Counts the connections that got as far as a welcome.
    epoch: u64,
}

/// How one connection ended.
enum End {
    /// The link is being dropped.
    Stop,
    Rejoin,
    Refused(RefuseReason),
    Lost(String),
}

impl Task {
    async fn run(mut self) {
        let mut pause = RETRY_MIN;
        loop {
            let before = self.epoch;
            match self.connection().await {
                End::Stop => return,
                End::Rejoin => {
                    self.resume = None;
                    pause = RETRY_MIN;
                }
                End::Refused(reason) => {
                    self.emit(LinkEvent::Refused(reason));
                    return;
                }
                End::Lost(reason) => {
                    if !self.emit(LinkEvent::Down(reason)) {
                        return;
                    }
                    if self.epoch != before {
                        // It worked a moment ago: try again soon.
                        pause = RETRY_MIN;
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(pause) => {}
                        _ = stopped(&mut self.stop) => return,
                    }
                    pause = (pause * 2).min(RETRY_MAX);
                }
            }
        }
    }

    /// Hands an event to the UI thread. False when nobody listens any more.
    fn emit(&self, event: LinkEvent) -> bool {
        let delivered = self.events.send(event).is_ok();
        (self.waker)();
        delivered
    }

    async fn connection(&mut self) -> End {
        let connecting = tokio::time::timeout(CONNECT_TIMEOUT, mdaw_net::connect(&self.join));
        let (endpoint, connection) = tokio::select! {
            made = connecting => match made {
                Ok(Ok(made)) => made,
                Ok(Err(err)) => return End::Lost(format!("{err:#}")),
                Err(_) => return End::Lost("the server did not answer".into()),
            },
            _ = stopped(&mut self.stop) => return End::Stop,
        };
        let end = self.talk(&connection).await;
        let _ = tokio::time::timeout(CLOSE_TIMEOUT, mdaw_net::close(&endpoint, &connection)).await;
        end
    }

    async fn talk(&mut self, connection: &quinn::Connection) -> End {
        let lost = |err: anyhow::Error| End::Lost(format!("{err:#}"));

        let hello = ClientMsg::Hello(Hello {
            proto_version: PROTO_VERSION,
            token: self.join.token.clone(),
            user_key: self.who.user_key,
            name: self.who.name.clone(),
            resume: self.resume,
        });
        let greeting = async {
            let (mut send, mut recv) = connection.open_bi().await?;
            mdaw_net::write_frame(&mut send, &hello).await?;
            let answer: Option<ServerMsg> = mdaw_net::read_frame(&mut recv).await?;
            anyhow::Ok((send, recv, answer))
        };
        let (mut send, mut recv, answer) = tokio::select! {
            greeted = tokio::time::timeout(CONNECT_TIMEOUT, greeting) => match greeted {
                Ok(Ok(greeted)) => greeted,
                Ok(Err(err)) => return lost(err),
                Err(_) => return End::Lost("the server did not answer".into()),
            },
            _ = stopped(&mut self.stop) => return End::Stop,
        };
        let welcome = match answer {
            Some(ServerMsg::Welcome(welcome)) => welcome,
            Some(ServerMsg::Refused(reason)) => return End::Refused(reason),
            _ => return End::Lost("the server answered with something unexpected".into()),
        };

        self.epoch += 1;
        let epoch = self.epoch;
        self.resume = Some(Resume {
            prev_actor: welcome.actor,
            session_secret: welcome.session_secret,
            last_seq: welcome.head_seq,
        });
        if !self.emit(LinkEvent::Welcome { epoch, welcome }) {
            return End::Stop;
        }

        let Task {
            events,
            waker,
            out,
            stop,
            resume,
            ..
        } = self;
        let (events, waker) = (&*events, &*waker);
        // Each loop owns its stream for the whole connection: a frame read or written halfway
        // cannot be picked up again, so neither is ever cancelled in favour of the other.
        let reading = async {
            loop {
                match mdaw_net::read_frame::<ServerMsg>(&mut recv).await {
                    Ok(Some(msg)) => {
                        if let (ServerMsg::Event(event), Some(resume)) = (&msg, resume.as_mut()) {
                            resume.last_seq = event.seq;
                        }
                        let delivered = events.send(LinkEvent::Msg(msg)).is_ok();
                        waker();
                        if !delivered {
                            return End::Stop;
                        }
                    }
                    Ok(None) => return End::Lost("the server closed the connection".into()),
                    Err(err) => return End::Lost(format!("{err:#}")),
                }
            }
        };
        let writing = async {
            loop {
                match out.recv().await {
                    Some(Out::Msg(for_epoch, msg)) if for_epoch == epoch => {
                        if let Err(err) = mdaw_net::write_frame(&mut send, &msg).await {
                            return End::Lost(format!("{err:#}"));
                        }
                    }
                    // Meant for a connection that is gone; its sender has been told to resend.
                    Some(Out::Msg(..)) => {}
                    Some(Out::Rejoin) => return End::Rejoin,
                    None => return End::Stop,
                }
            }
        };
        // Datagrams get lost, so the two halves of the clock exchange do not wait for each
        // other. Neither ever ends: a dead connection is the reading loop's to report.
        let pinging = async {
            let mut sent = 0;
            loop {
                let t0 = clock::now_micros();
                mdaw_net::send_datagram(connection, &Datagram::Ping { t0 });
                sent += 1;
                let pause = if sent < PING_BURST {
                    PING_BURST_EVERY
                } else {
                    PING_EVERY
                };
                tokio::time::sleep(pause).await;
            }
        };
        let timing = async {
            let mut sync = ClockSync::new();
            loop {
                match mdaw_net::read_datagram(connection).await {
                    Ok(Some(Datagram::Pong { t0, server_micros })) => {
                        if let Some(estimate) = sync.on_pong(t0, server_micros, clock::now_micros()) {
                            let _ = events.send(LinkEvent::Clock(estimate));
                            waker();
                        }
                    }
                    Ok(_) => {}
                    Err(_) => std::future::pending().await,
                }
            }
        };
        tokio::select! {
            end = reading => end,
            end = writing => end,
            _ = stopped(stop) => End::Stop,
            () = pinging => unreachable!(),
            () = timing => unreachable!(),
        }
    }
}

/// Resolves when the link is being dropped.
async fn stopped(stop: &mut watch::Receiver<bool>) {
    // An error means the sender is gone, which is the same thing.
    let _ = stop.wait_for(|stop| *stop).await;
}
