//! The window's link to whoever runs the mixer: this process on demand, or
//! the Windows service through its pipe. A worker thread sends the window's
//! requests and, between them, polls the state about 30 times a second, so
//! the window never waits on the pipe.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::platform::control;
use crate::platform::remote;
use crate::platform::service::{ServiceState, service_state};
use crate::protocol::{Reply, Request, Snapshot, Tone};

/// Where the mixer runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// In this process, until the window quits.
    OnDemand,
    /// In the Windows service, which the window manages.
    Service,
}

/// What the window knows, shared with the worker and background jobs.
/// A status line: its tone and text.
pub type StatusLine = (Tone, String);

pub struct Shared {
    pub mode: Mode,
    pub service_state: ServiceState,
    /// Whether the service answered the last request.
    pub connected: bool,
    /// The service's version, which may differ from the window's.
    pub version: String,
    /// A long operation in progress, shown in the header.
    pub busy: Option<String>,
    pub snapshot: Snapshot,
    /// Errors to show the user, oldest first.
    pub errors: Vec<String>,
    /// The window's own message for the status line, and the Windows Sound
    /// status it covers: it shows until that status changes.
    pub notice: Option<(StatusLine, Option<StatusLine>)>,
    /// Bumped by each new snapshot.
    pub generation: u64,
}

pub type SharedState = Arc<Mutex<Shared>>;

pub fn lock(shared: &SharedState) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(|p| p.into_inner())
}

pub fn new_shared(mode: Mode) -> SharedState {
    Arc::new(Mutex::new(Shared {
        mode,
        service_state: service_state(),
        connected: mode == Mode::Service,
        version: crate::VERSION.to_string(),
        busy: None,
        snapshot: Snapshot::default(),
        errors: Vec::new(),
        notice: None,
        generation: 0,
    }))
}

/// Shows an error to the user; safe from any thread.
pub fn report_error(shared: &SharedState, error: impl Into<String>) {
    lock(shared).errors.push(error.into());
    repaint();
}

/// Sets the window's own status-line message; safe from any thread.
pub fn set_notice(shared: &SharedState, tone: Tone, text: &str) {
    let mut s = lock(shared);
    let covers = s.snapshot.status.clone();
    s.notice = Some(((tone, text.to_string()), covers));
    drop(s);
    repaint();
}

static CONTEXT: std::sync::OnceLock<eframe::egui::Context> = std::sync::OnceLock::new();

pub fn set_context(ctx: &eframe::egui::Context) {
    let _ = CONTEXT.set(ctx.clone());
}

/// Asks the window to redraw; safe from any thread.
pub fn repaint() {
    if let Some(ctx) = CONTEXT.get() {
        ctx.request_repaint();
    }
}

/// Sends a request to the mixer wherever it runs and records the reply.
pub fn send_now(shared: &SharedState, request: &Request) -> Result<Reply, String> {
    let mode = lock(shared).mode;
    let reply = match mode {
        Mode::OnDemand => Ok(control::reply(request)),
        Mode::Service => remote::call(request).map_err(|err| format!("Could not reach the Windows service:\n{err}")),
    };
    let mut s = lock(shared);
    match &reply {
        Ok(reply) => record(&mut s, reply),
        Err(_) => s.connected = false,
    }
    drop(s);
    repaint();
    reply
}

fn record(s: &mut Shared, reply: &Reply) {
    s.snapshot = reply.snapshot.clone();
    s.version = reply.version.clone();
    s.connected = true;
    s.generation += 1;
    // The notice gives way once the status it covered changes.
    if s.notice.as_ref().is_some_and(|(_, covers)| *covers != s.snapshot.status) {
        s.notice = None;
    }
}

/// The worker thread's end: requests from the window.
pub struct Link {
    jobs: Sender<Request>,
}

impl Link {
    pub fn start(shared: &SharedState) -> Link {
        let (jobs, receiver) = channel();
        let shared = Arc::clone(shared);
        std::thread::Builder::new()
            .name("mixer link".into())
            .spawn(move || work(&shared, &receiver))
            .expect("start the link");
        Link { jobs }
    }

    /// A link whose requests go to the returned receiver, for tests.
    #[cfg(test)]
    pub fn detached() -> (Link, std::sync::mpsc::Receiver<Request>) {
        let (jobs, receiver) = channel();
        (Link { jobs }, receiver)
    }

    /// Queues a request; its error, if any, is shown to the user.
    pub fn send(&self, request: Request) {
        let _ = self.jobs.send(request);
    }
}

/// How often the state is polled while nothing else is sent.
const POLL: Duration = Duration::from_millis(33);
/// How often the service's state is checked with the service manager.
const SERVICE_CHECK: Duration = Duration::from_secs(1);

fn work(shared: &SharedState, jobs: &Receiver<Request>) {
    let mut next_service_check = Instant::now();
    loop {
        match jobs.recv_timeout(POLL) {
            Ok(request) => {
                // Coalesce: a fader drag sends a request per frame, and only
                // the latest setting of each control matters.
                let mut batch = vec![request];
                batch.extend(jobs.try_iter());
                for request in coalesce(batch) {
                    match send_now(shared, &request) {
                        Ok(reply) => {
                            if let Err(err) = reply.result() {
                                report_error(shared, err);
                            }
                        }
                        Err(err) => report_error(shared, err),
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if lock(shared).busy.is_none() {
                    let _ = send_now(shared, &Request::Status);
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
        if Instant::now() >= next_service_check {
            next_service_check = Instant::now() + SERVICE_CHECK;
            let state = service_state();
            let mut s = lock(shared);
            if s.service_state != state {
                s.service_state = state;
                drop(s);
                repaint();
            }
        }
    }
}

/// Drops requests a later one in the batch makes moot: earlier settings of
/// the same strip or bus.
fn coalesce(batch: Vec<Request>) -> Vec<Request> {
    use crate::protocol::MixerCommand;
    let key = |request: &Request| match request {
        Request::Mixer { command: MixerCommand::Strip { id, .. } } => Some(format!("strip {id}")),
        Request::Mixer { command: MixerCommand::Bus { id, .. } } => Some(format!("bus {id}")),
        _ => None,
    };
    let mut kept: Vec<Request> = Vec::new();
    for request in batch {
        if let Some(k) = key(&request) {
            kept.retain(|earlier| key(earlier).as_deref() != Some(k.as_str()));
        }
        kept.push(request);
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{MixerCommand, StripParams};

    #[test]
    fn only_the_latest_setting_of_a_control_is_sent() {
        let strip = |id: &str, gain_db: f32| Request::Mixer {
            command: MixerCommand::Strip { id: id.into(), params: StripParams { gain_db, ..Default::default() } },
        };
        let route = Request::Mixer { command: MixerCommand::Route { strip: "H1".into(), bus: "A1".into(), on: true } };
        let kept = coalesce(vec![strip("H1", -1.0), strip("H2", -2.0), route.clone(), strip("H1", -3.0)]);
        assert_eq!(kept, vec![strip("H2", -2.0), route, strip("H1", -3.0)]);
    }
}
