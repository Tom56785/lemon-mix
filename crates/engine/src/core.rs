//! The engine: the routing graph, mixed one bus frame at a time.
//!
//! The engine's state sits behind one lock, taken by whoever needs frames
//! processed: the engine's clock thread, which keeps hardware buses fed and
//! meters moving, and capture endpoints that need a virtual bus's frame
//! before the clock thread got to it. Processing a frame is a few tens of
//! microseconds of arithmetic and memory copies.
//!
//! Control never takes that lock: settings travel as commands over a channel
//! the engine drains before each batch, and structural changes arrive as a
//! fully built layout, so the audio path neither allocates nor frees.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cables::audio::{DelayLine, PACKET_BYTES};
use cables::event::Event;
use cables::logging::Logger;
use cables::usbip::busclock::current_bus_time;

use crate::denoise::Denoiser;
use crate::dsp::{Compressor, Ducker, Eq3, Gate, Limiter, declick};
use crate::meters::{MeterBank, RmsFollower};
use crate::params::{BusParams, DuckParams, StripParams};
use crate::ring::BusRing;
use crate::{FRAME_LEN, Frame, decode_frame, encode_frame};

/// A physical capture device, after its resampler, as the engine sees it.
pub trait InputPort: Send + Sync {
    /// Fills one frame of 48 kHz stereo audio; silence when there is none.
    fn pull(&self, frame: &mut Frame);
}

/// A physical render device, before its resampler, as the engine sees it.
pub trait OutputPort: Send + Sync {
    /// Queues one frame of 48 kHz stereo audio, already clamped to full scale.
    fn push(&self, frame: &Frame);
}

/// Where a strip's audio comes from.
pub enum StripSource {
    /// A cable's playback side. Frame F of the strip is the cable's playback
    /// packet of frame F minus the cable's latency.
    Virtual { line: Arc<DelayLine>, playback_active: Arc<AtomicBool> },
    /// A physical capture device, or none chosen yet.
    Hardware(Option<Arc<dyn InputPort>>),
    /// Another bus's output, by index, as it left that bus one frame
    /// (1 ms) earlier: a bus fed back in without a virtual cable. The delay
    /// keeps the order of processing simple; the host keeps routes from
    /// making a loop.
    Bus(usize),
}

/// Where a bus's mix goes.
pub enum BusSink {
    /// A cable's recording side.
    Virtual(Arc<BusRing>),
    /// A physical render device, or none chosen yet.
    Hardware(Option<Arc<dyn OutputPort>>),
}

pub struct StripSpec {
    pub source: StripSource,
    pub params: StripParams,
    /// The buses this strip feeds, by index in the topology's buses.
    pub routes: Vec<usize>,
    /// The strips that duck this one while they play, by index.
    pub duck_sources: Vec<usize>,
    /// Identifies the strip across layouts, so one that carries on into a
    /// new layout keeps its moment: its ducking, gate, compressor and filter
    /// state. 0 is a strip with nothing to carry.
    pub identity: u64,
}

pub struct BusSpec {
    pub sink: BusSink,
    pub params: BusParams,
}

/// The strips and buses, in the order the engine indexes them: strip routes
/// and meters refer to these indices. There may be any number of each.
#[derive(Default)]
pub struct Topology {
    pub strips: Vec<StripSpec>,
    pub buses: Vec<BusSpec>,
}

/// What control sends the engine.
pub enum Command {
    /// Replaces the strips and buses.
    Layout(Box<Layout>),
    /// New settings for the strip at this index.
    Strip(usize, StripParams),
    /// New settings for the bus at this index.
    Bus(usize, BusParams),
    /// Connects (true) or disconnects the strip and the bus at these indices.
    Route(usize, usize, bool),
    /// Makes the second strip duck the first (true), or not, by index.
    DuckSource(usize, usize, bool),
}

/// The engine's working state for a topology, built off the audio path.
pub struct Layout {
    strips: Vec<StripState>,
    buses: Vec<BusState>,
    /// Sized for this layout's strips and buses.
    meters: Arc<MeterBank>,
    /// Each strip's side-chain level from the last frame, gathered before
    /// a frame so strips can read each other's. Sized with the layout.
    levels: Vec<f32>,
}

struct StripState {
    source: StripSource,
    identity: u64,
    params: StripParams,
    /// Whether this strip feeds each bus, by bus index. Sized with the
    /// layout, so routing changes never allocate.
    routes: Vec<bool>,
    denoise: Denoiser,
    eq: Eq3,
    gate: Gate,
    comp: Compressor,
    /// The gains being applied towards each bus, ramped towards the target.
    sends: Vec<[f32; 2]>,
    /// The last sample of the previous frame, and the scheduled frame of the
    /// playback packet it came from, to find and smooth cuts.
    last: [f32; 2],
    last_own: Option<u64>,
    rms: RmsFollower,
    /// Whether each strip ducks this one, by strip index. Sized with the
    /// layout.
    duck_sources: Vec<bool>,
    ducker: Ducker,
    /// The ducking gain applied this frame.
    duck: f32,
    /// The strip's level as heard, after its fader, mute and solo but
    /// before its own ducking: what ducks other strips.
    level: f32,
}

struct BusState {
    sink: BusSink,
    params: BusParams,
    gain: f32,
    limiter: Limiter,
    rms: RmsFollower,
    mix: Frame,
    /// The last frame this bus put out, for strips fed from it.
    out: Frame,
}

/// The most frames processed in one batch. After a longer stall the engine
/// skips ahead rather than spend the next interval working through audio no
/// one will hear in time.
pub const MAX_CATCH_UP_FRAMES: u64 = 250;

/// Counters for the log, updated by the engine and read by the health report.
#[derive(Default)]
pub struct EngineStats {
    pub frames: AtomicU64,
    pub batches: AtomicU64,
    /// Batches of more than one frame: the coarse clock jumped, or the clock
    /// thread was late.
    pub catch_up_batches: AtomicU64,
    /// The largest batch since the health report last looked.
    pub max_batch: AtomicU64,
    /// Frames skipped after a stall longer than `MAX_CATCH_UP_FRAMES`.
    pub skipped: AtomicU64,
    /// Frames processed by capture endpoints rather than the clock thread.
    pub pulled: AtomicU64,
}

pub struct Engine {
    core: Mutex<Core>,
    commands: Sender<Command>,
    garbage: Mutex<Receiver<Box<Layout>>>,
    /// The meters of the latest topology.
    meters: Mutex<Arc<MeterBank>>,
    pub stats: EngineStats,
}

struct Core {
    layout: Box<Layout>,
    commands: Receiver<Command>,
    garbage: Sender<Box<Layout>>,
    /// The next frame to process; `None` until the first.
    next: Option<u64>,
    frame: Frame,
    packet: [u8; PACKET_BYTES],
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

impl Default for Engine {
    fn default() -> Engine {
        Engine::new()
    }
}

impl Engine {
    pub fn new() -> Engine {
        let (commands, receiver) = channel();
        let (garbage_tx, garbage_rx) = channel();
        let meters = Arc::new(MeterBank::new(0, 0));
        Engine {
            core: Mutex::new(Core {
                layout: Box::new(Layout {
                    strips: Vec::new(),
                    buses: Vec::new(),
                    meters: Arc::clone(&meters),
                    levels: Vec::new(),
                }),
                commands: receiver,
                garbage: garbage_tx,
                next: None,
                frame: [0.0; FRAME_LEN],
                packet: [0; PACKET_BYTES],
            }),
            commands,
            garbage: Mutex::new(garbage_rx),
            meters: Mutex::new(meters),
            stats: EngineStats::default(),
        }
    }

    /// The meters of the latest topology, by strip and bus index. A new
    /// topology comes with new meters, which read silence until the engine
    /// takes it up.
    pub fn meters(&self) -> Arc<MeterBank> {
        Arc::clone(&lock(&self.meters))
    }

    /// Replaces the strips and buses. The layout is built here, on the
    /// caller's thread; the engine swaps it in before its next batch, and the
    /// layout it replaces is freed on a later call.
    pub fn set_topology(&self, topology: Topology) {
        // Free the layouts the engine has finished with.
        for old in lock(&self.garbage).try_iter() {
            drop(old);
        }
        let buses = topology.buses.len();
        let meters = Arc::new(MeterBank::new(topology.strips.len(), buses));
        let bus_states: Vec<BusState> = topology
            .buses
            .into_iter()
            .map(|spec| {
                let params = spec.params.sanitized();
                let mut limiter = Limiter::default();
                limiter.set(params.limiter);
                BusState {
                    sink: spec.sink,
                    gain: params.gain(),
                    params,
                    limiter,
                    rms: RmsFollower::default(),
                    mix: [0.0; FRAME_LEN],
                    out: [0.0; FRAME_LEN],
                }
            })
            .collect();
        let strip_count = topology.strips.len();
        let strip_states: Vec<StripState> = topology
            .strips
            .into_iter()
            .enumerate()
            .map(|(strip_index, spec)| {
                let mut routes = vec![false; buses];
                for bus in spec.routes {
                    if let Some(route) = routes.get_mut(bus) {
                        *route = true;
                    }
                }
                let mut duck_sources = vec![false; strip_count];
                for source in spec.duck_sources {
                    if let Some(on) = duck_sources.get_mut(source) {
                        *on = true;
                    }
                }
                let mut state = StripState {
                    source: spec.source,
                    identity: spec.identity,
                    params: StripParams::default(),
                    routes,
                    denoise: Denoiser::staggered(strip_index),
                    eq: Eq3::default(),
                    gate: Gate::default(),
                    comp: Compressor::default(),
                    sends: vec![[0.0; 2]; buses],
                    last: [0.0; 2],
                    last_own: None,
                    rms: RmsFollower::default(),
                    duck_sources,
                    ducker: Ducker::new(DuckParams::default()),
                    duck: 1.0,
                    level: 0.0,
                };
                state.apply(spec.params.sanitized());
                // A new layout starts at its settings rather than ramping
                // from silence, so audio already routed keeps flowing.
                let sends: Vec<[f32; 2]> = (0..buses).map(|bus| state.params_target(bus, false)).collect();
                state.sends = sends;
                state
            })
            .collect();
        *lock(&self.meters) = Arc::clone(&meters);
        let layout = Layout { strips: strip_states, buses: bus_states, meters, levels: vec![0.0; strip_count] };
        let _ = self.commands.send(Command::Layout(Box::new(layout)));
    }

    pub fn set_strip(&self, index: usize, params: StripParams) {
        let _ = self.commands.send(Command::Strip(index, params.sanitized()));
    }

    /// Connects or disconnects strip `strip` and bus `bus`, by index; the
    /// send fades in or out across the next frame.
    pub fn set_route(&self, strip: usize, bus: usize, on: bool) {
        let _ = self.commands.send(Command::Route(strip, bus, on));
    }

    /// Makes strip `source` duck strip `strip` while it plays (`on`), or not,
    /// by index. A strip does not duck itself.
    pub fn set_duck_source(&self, strip: usize, source: usize, on: bool) {
        let _ = self.commands.send(Command::DuckSource(strip, source, on));
    }

    pub fn set_bus(&self, index: usize, params: BusParams) {
        let _ = self.commands.send(Command::Bus(index, params.sanitized()));
    }

    /// Processes every frame up to and including `last` that has not been
    /// processed yet, in order. The first call processes `last` alone.
    pub fn process_until(&self, last: u64) {
        self.process(last, false);
    }

    /// As `process_until`, for a capture endpoint that needs the frame now.
    pub fn pull_until(&self, last: u64) {
        self.process(last, true);
    }

    fn process(&self, last: u64, pulled: bool) {
        let mut core = lock(&self.core);
        core.apply_commands();
        let next = *core.next.get_or_insert(last);
        if last < next {
            return;
        }
        let mut start = next;
        let pending = last - next + 1;
        if pending > MAX_CATCH_UP_FRAMES {
            start = last + 1 - MAX_CATCH_UP_FRAMES;
            self.stats.skipped.fetch_add(start - next, Ordering::Relaxed);
        }
        for frame in start..=last {
            core.process_frame(frame);
        }
        core.next = Some(last + 1);
        drop(core);
        let count = last + 1 - start;
        self.stats.frames.fetch_add(count, Ordering::Relaxed);
        self.stats.batches.fetch_add(1, Ordering::Relaxed);
        if count > 1 {
            self.stats.catch_up_batches.fetch_add(1, Ordering::Relaxed);
        }
        self.stats.max_batch.fetch_max(count, Ordering::Relaxed);
        if pulled {
            self.stats.pulled.fetch_add(count, Ordering::Relaxed);
        }
    }

    /// Starts the clock thread, which processes each frame once the coarse
    /// bus clock shows it has elapsed. `stall_test` holds it, as the USB/IP
    /// server holds its completions, for that long once a minute.
    pub fn start_clock(self: &Arc<Self>, logger: &Logger, stall_test: Option<Duration>) -> Clock {
        let stop = Arc::new(AtomicBool::new(false));
        let wake = Arc::new(Event::new());
        let (engine, flag, event, logger) = (Arc::clone(self), Arc::clone(&stop), Arc::clone(&wake), logger.clone());
        let thread = std::thread::Builder::new()
            .name("mix engine".into())
            .spawn(move || {
                // Before the frames start: denoising's first block would
                // otherwise allocate on this thread.
                crate::denoise::prepare_thread();
                let _audio_thread = cables::enter_audio_thread(&logger);
                let started = Instant::now();
                while !flag.load(Ordering::Relaxed) {
                    if let Some(length) = stall_test {
                        let elapsed = started.elapsed();
                        let window = elapsed.as_secs() / 60;
                        let into = elapsed - Duration::from_secs(window * 60);
                        if window > 0 && into < length {
                            event.wait(Some(Instant::now() + (length - into)));
                            continue;
                        }
                    }
                    let now = current_bus_time();
                    if now.frame() > 0 {
                        engine.process_until(now.frame() - 1);
                    }
                    // Wake just after the next frame starts. The coarse clock
                    // may show it a little later; then this wakes again soon.
                    let next = now.frame_start(now.frame() + 1) + Duration::from_micros(50);
                    let soonest = Instant::now() + Duration::from_micros(200);
                    event.wait(Some(next.max(soonest).min(Instant::now() + Duration::from_millis(2))));
                }
            })
            .expect("start the engine clock thread");
        Clock { stop, wake, thread: Some(thread) }
    }
}

/// The engine's clock thread; stops when dropped.
pub struct Clock {
    stop: Arc<AtomicBool>,
    wake: Arc<Event>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Clock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.wake.signal();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl StripState {
    /// Takes up the moment of the same strip in the layout before.
    fn carry_from(&mut self, old: &mut StripState) {
        self.denoise.carry_from(&mut old.denoise);
        self.eq.carry_from(&old.eq);
        self.gate.carry_from(&old.gate);
        self.comp.carry_from(&old.comp);
        self.ducker.carry_from(&old.ducker);
        self.duck = old.duck;
        self.level = old.level;
        self.rms = old.rms;
        (self.last, self.last_own) = (old.last, old.last_own);
        // The sends start at the strip's settings; ducked, they start ducked.
        for send in &mut self.sends {
            send[0] *= self.duck;
            send[1] *= self.duck;
        }
    }

    fn apply(&mut self, params: StripParams) {
        if params.denoise != self.params.denoise {
            self.denoise.set(params.denoise);
        }
        if params.eq != self.params.eq {
            self.eq.set(params.eq);
        }
        if params.gate != self.params.gate || params.gate_options != self.params.gate_options {
            self.gate.set(params.gate, params.gate_options);
        }
        if params.comp != self.params.comp || params.comp_options != self.params.comp_options {
            self.comp.set(params.comp, params.comp_options);
        }
        if params.duck != self.params.duck {
            self.ducker.set(params.duck);
        }
        self.params = params;
    }

    /// The gains this strip should apply towards `bus`.
    fn params_target(&self, bus: usize, any_solo: bool) -> [f32; 2] {
        let p = &self.params;
        let routed = self.routes.get(bus).copied().unwrap_or(false);
        if !routed || p.mute || (any_solo && !p.solo) { [0.0; 2] } else { p.channel_gains() }
    }
}

impl Core {
    fn apply_commands(&mut self) {
        while let Ok(command) = self.commands.try_recv() {
            match command {
                Command::Layout(mut layout) => {
                    // Strips that carry on keep their moment, so a rebuild (a
                    // device plugged in, a strip added) is not heard. A plain
                    // search: there are few strips, and it allocates nothing.
                    for strip in layout.strips.iter_mut().filter(|s| s.identity != 0) {
                        if let Some(old) = self.layout.strips.iter_mut().find(|o| o.identity == strip.identity) {
                            strip.carry_from(old);
                        }
                    }
                    let old = std::mem::replace(&mut self.layout, layout);
                    let _ = self.garbage.send(old);
                }
                Command::Strip(index, params) => {
                    if let Some(strip) = self.layout.strips.get_mut(index) {
                        strip.apply(params);
                    }
                }
                Command::Bus(index, params) => {
                    if let Some(bus) = self.layout.buses.get_mut(index) {
                        bus.limiter.set(params.limiter);
                        bus.params = params;
                    }
                }
                Command::Route(strip, bus, on) => {
                    if let Some(route) = self.layout.strips.get_mut(strip).and_then(|s| s.routes.get_mut(bus)) {
                        *route = on;
                    }
                }
                Command::DuckSource(strip, source, on) => {
                    if let Some(duck) = self.layout.strips.get_mut(strip).and_then(|s| s.duck_sources.get_mut(source)) {
                        *duck = on && source != strip;
                    }
                }
            }
        }
    }

    fn process_frame(&mut self, frame_number: u64) {
        let Core { layout, frame, packet, .. } = self;
        let Layout { strips, buses, meters, levels } = &mut **layout;
        for bus in buses.iter_mut() {
            bus.mix = [0.0; FRAME_LEN];
        }
        let any_solo = strips.iter().any(|s| s.params.solo);
        // Ducking reads the last frame's levels, so every strip sees the same
        // ones whatever the order (and a strip that ducks another is heard a
        // millisecond before it lowers it).
        for (level, strip) in levels.iter_mut().zip(strips.iter()) {
            *level = strip.level;
        }

        for (index, strip) in strips.iter_mut().enumerate() {
            // The source.
            let cut = match &strip.source {
                StripSource::Virtual { line, playback_active } => {
                    let own = line.read_frame(frame_number, packet, playback_active.load(Ordering::Relaxed));
                    decode_frame(packet, frame);
                    let continuous = match (strip.last_own, own) {
                        (Some(previous), Some(own)) => own == previous + 1,
                        (None, None) => true,
                        _ => false,
                    };
                    strip.last_own = own;
                    !continuous
                }
                StripSource::Hardware(Some(port)) => {
                    port.pull(frame);
                    false
                }
                StripSource::Hardware(None) => {
                    *frame = [0.0; FRAME_LEN];
                    false
                }
                StripSource::Bus(bus) => {
                    *frame = buses.get(*bus).map_or([0.0; FRAME_LEN], |b| b.out);
                    false
                }
            };
            let last = strip.last;
            strip.last = [frame[FRAME_LEN - 2], frame[FRAME_LEN - 1]];
            if cut && last != [0.0; 2] {
                declick(frame, last);
            }

            // Processing: noise suppression, EQ, gate, compressor, mono. The
            // gate works best on sound already denoised.
            let mut reduction = 1.0f32;
            if strip.denoise.active() {
                strip.denoise.process(frame);
            }
            if strip.eq.active() {
                strip.eq.process(frame);
            }
            if strip.gate.active() {
                reduction *= strip.gate.process(frame);
            }
            if strip.comp.active() {
                reduction *= strip.comp.process(frame);
            }
            if strip.params.mono {
                for pair in frame.as_chunks_mut::<2>().0 {
                    let mid = (pair[0] + pair[1]) * 0.5;
                    pair[0] = mid;
                    pair[1] = mid;
                }
            }
            let (peak, rms) = strip.rms.measure(frame);

            // The level others are ducked by: as heard, before this strip's
            // own ducking, so two strips ducking each other do not pump.
            let heard = !strip.params.mute && !(any_solo && !strip.params.solo);
            let gains = strip.params.channel_gains();
            strip.level = if heard { (peak[0] * gains[0]).max(peak[1] * gains[1]) } else { 0.0 };
            let threshold = strip.ducker.threshold();
            let triggered = strip.duck_sources.iter().zip(levels.iter()).any(|(&on, &level)| on && level >= threshold);
            strip.duck = strip.ducker.step(triggered);
            if let Some(meter) = meters.strips.get(index) {
                meter.record(peak, rms, reduction);
                meter.record_duck(strip.duck);
            }

            // The sends, ramped across the frame when they change.
            for (bus_index, bus) in buses.iter_mut().enumerate() {
                let [left, right] = strip.params_target(bus_index, any_solo);
                let target = [left * strip.duck, right * strip.duck];
                let Some(send) = strip.sends.get_mut(bus_index) else {
                    continue;
                };
                mix_into(&mut bus.mix, frame, *send, target);
                *send = target;
            }
        }

        for (index, bus) in buses.iter_mut().enumerate() {
            let target = bus.params.gain();
            let mix = &mut bus.mix;
            if bus.gain != target || target != 1.0 {
                ramp(mix, [bus.gain; 2], [target; 2]);
            }
            bus.gain = target;
            if bus.params.mono {
                for pair in mix.as_chunks_mut::<2>().0 {
                    let mid = (pair[0] + pair[1]) * 0.5;
                    pair[0] = mid;
                    pair[1] = mid;
                }
            }
            let reduction = if bus.limiter.active() { bus.limiter.process(mix) } else { 1.0 };
            for sample in mix.iter_mut() {
                *sample = sample.clamp(-1.0, 1.0);
            }
            bus.out = *mix;
            let (peak, rms) = bus.rms.measure(mix);
            if let Some(meter) = meters.buses.get(index) {
                meter.record(peak, rms, reduction);
            }
            match &bus.sink {
                BusSink::Virtual(ring) => {
                    encode_frame(mix, packet);
                    ring.write(frame_number, packet);
                }
                BusSink::Hardware(Some(port)) => port.push(mix),
                BusSink::Hardware(None) => {}
            }
        }
    }
}

/// Adds `src` scaled by a gain ramping linearly from `from` to `to` across
/// the frame. A constant gain multiplies every sample by the same value, so
/// unity adds the source unchanged.
fn mix_into(mix: &mut Frame, src: &Frame, from: [f32; 2], to: [f32; 2]) {
    if from == to {
        if from == [0.0; 2] {
            return;
        }
        for (m, s) in mix.as_chunks_mut::<2>().0.iter_mut().zip(src.as_chunks::<2>().0.iter()) {
            m[0] += s[0] * from[0];
            m[1] += s[1] * from[1];
        }
        return;
    }
    let n = crate::FRAME_SAMPLES as f32;
    for (i, (m, s)) in mix.as_chunks_mut::<2>().0.iter_mut().zip(src.as_chunks::<2>().0.iter()).enumerate() {
        let w = (i + 1) as f32 / n;
        m[0] += s[0] * (from[0] + (to[0] - from[0]) * w);
        m[1] += s[1] * (from[1] + (to[1] - from[1]) * w);
    }
}

/// Scales a frame in place by a gain ramping from `from` to `to`.
fn ramp(frame: &mut Frame, from: [f32; 2], to: [f32; 2]) {
    let n = crate::FRAME_SAMPLES as f32;
    for (i, pair) in frame.as_chunks_mut::<2>().0.iter_mut().enumerate() {
        let w = (i + 1) as f32 / n;
        pair[0] *= from[0] + (to[0] - from[0]) * w;
        pair[1] *= from[1] + (to[1] - from[1]) * w;
    }
}

#[cfg(test)]
mod tests;
