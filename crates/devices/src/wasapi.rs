//! WASAPI: enumerating endpoints, event-driven streams in shared or
//! exclusive mode, and device change notifications.

use std::sync::Arc;
use std::sync::mpsc::{Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cables::logf;
use cables::logging::Logger;
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::*;
use windows::Win32::Media::KernelStreaming::{KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE};
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, STGM_READ,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;
use windows::core::{HSTRING, PCWSTR, implement};

use crate::manager::{DeviceChange, DeviceChoice, DeviceInfo, Direction, PortShared, PortStatus};
use crate::port::PortState;
use crate::ring::Stereo;

/// COM for the current thread, released on drop.
struct Com(bool);

fn com() -> Com {
    // SAFETY: initializes COM for this thread; a thread already in another
    // apartment keeps it, and is then not uninitialized here.
    let ok = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
    Com(ok)
}

impl Drop for Com {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: balances the successful CoInitializeEx above.
            unsafe { CoUninitialize() };
        }
    }
}

fn describe(err: &windows::core::Error) -> String {
    match err.code() {
        AUDCLNT_E_DEVICE_INVALIDATED => "the device was removed or its format changed".into(),
        AUDCLNT_E_DEVICE_IN_USE => "another application has the device in exclusive mode".into(),
        AUDCLNT_E_UNSUPPORTED_FORMAT => "the device does not support any usable format".into(),
        AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED => "exclusive mode is not allowed for this device".into(),
        AUDCLNT_E_SERVICE_NOT_RUNNING => "the Windows Audio service is not running".into(),
        code => format!("{} (0x{:08X})", err.message(), code.0 as u32),
    }
}

fn enumerator() -> Result<IMMDeviceEnumerator, String> {
    // SAFETY: creates the system's device enumerator.
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
        .map_err(|e| format!("device enumerator: {}", describe(&e)))
}

fn flow(direction: Direction) -> EDataFlow {
    match direction {
        Direction::Capture => eCapture,
        Direction::Render => eRender,
    }
}

fn device_id(device: &IMMDevice) -> String {
    // SAFETY: GetId returns a CoTaskMem string, freed after copying.
    unsafe {
        match device.GetId() {
            Ok(id) => {
                let text = id.to_string().unwrap_or_default();
                CoTaskMemFree(Some(id.0 as _));
                text
            }
            Err(_) => String::new(),
        }
    }
}

fn device_name(device: &IMMDevice) -> String {
    // SAFETY: reads a property from the device's property store.
    unsafe {
        device
            .OpenPropertyStore(STGM_READ)
            .and_then(|store| store.GetValue(&PKEY_Device_FriendlyName as *const PROPERTYKEY))
            .map(|value| value.to_string())
            .unwrap_or_else(|_| "Unknown device".into())
    }
}

pub(crate) fn enumerate(direction: Direction) -> Result<Vec<DeviceInfo>, String> {
    let _com = com();
    let enumerator = enumerator()?;
    // SAFETY: plain COM calls on interfaces this function holds.
    unsafe {
        let default = enumerator.GetDefaultAudioEndpoint(flow(direction), eConsole).ok().map(|d| device_id(&d));
        let collection = enumerator
            .EnumAudioEndpoints(flow(direction), DEVICE_STATE_ACTIVE)
            .map_err(|e| format!("list devices: {}", describe(&e)))?;
        let count = collection.GetCount().unwrap_or(0);
        let mut devices = Vec::new();
        for index in 0..count {
            let Ok(device) = collection.Item(index) else {
                continue;
            };
            let id = device_id(&device);
            devices.push(DeviceInfo {
                is_default: default.as_deref() == Some(id.as_str()),
                name: device_name(&device),
                id,
            });
        }
        devices.sort_by_key(|d| d.name.to_lowercase());
        Ok(devices)
    }
}

/// A sample format the streams convert to and from.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Sample {
    F32,
    I16,
    /// Packed 3-byte samples.
    I24,
    /// 32-bit containers, with 24 or 32 valid bits, left-justified.
    I32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Format {
    sample: Sample,
    channels: usize,
    rate: u32,
    /// Bytes per frame of all channels.
    block: usize,
}

impl Format {
    fn describe(&self) -> String {
        let sample = match self.sample {
            Sample::F32 => "32-bit float",
            Sample::I16 => "16-bit",
            Sample::I24 => "24-bit",
            Sample::I32 => "32-bit",
        };
        format!("{} Hz {sample} {} ch", self.rate, self.channels)
    }
}

/// Reads a WAVEFORMATEX (or WAVEFORMATEXTENSIBLE) the stream can handle.
///
/// # Safety
/// `wave` must point to a valid format structure, extensible if its tag says so.
unsafe fn parse_format(wave: *const WAVEFORMATEX) -> Option<Format> {
    // SAFETY: the caller guarantees the structure; packed fields are read unaligned.
    let base = unsafe { std::ptr::read_unaligned(wave) };
    let tag = base.wFormatTag as u32;
    let bits = base.wBitsPerSample;
    let (float, valid) = if tag == WAVE_FORMAT_EXTENSIBLE && base.cbSize >= 22 {
        // SAFETY: the tag and size say the structure is extensible.
        let ext = unsafe { std::ptr::read_unaligned(wave as *const WAVEFORMATEXTENSIBLE) };
        let sub = ext.SubFormat;
        // SAFETY: wValidBitsPerSample is the union's member for PCM formats.
        let valid = unsafe { ext.Samples.wValidBitsPerSample };
        if sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
            (true, valid)
        } else if sub == KSDATAFORMAT_SUBTYPE_PCM {
            (false, valid)
        } else {
            return None;
        }
    } else if tag == WAVE_FORMAT_IEEE_FLOAT {
        (true, bits)
    } else if tag == WAVE_FORMAT_PCM {
        (false, bits)
    } else {
        return None;
    };
    let sample = match (float, bits) {
        (true, 32) => Sample::F32,
        (false, 16) => Sample::I16,
        (false, 24) => Sample::I24,
        (false, 32) if valid == 0 || valid >= 16 => Sample::I32,
        _ => return None,
    };
    let channels = base.nChannels as usize;
    if channels == 0 {
        return None;
    }
    Some(Format { sample, channels, rate: base.nSamplesPerSec, block: base.nBlockAlign as usize })
}

fn make_format(sample: Sample, channels: usize, rate: u32) -> WAVEFORMATEXTENSIBLE {
    let (bits, valid, sub) = match sample {
        Sample::F32 => (32, 32, KSDATAFORMAT_SUBTYPE_IEEE_FLOAT),
        Sample::I16 => (16, 16, KSDATAFORMAT_SUBTYPE_PCM),
        Sample::I24 => (24, 24, KSDATAFORMAT_SUBTYPE_PCM),
        Sample::I32 => (32, 24, KSDATAFORMAT_SUBTYPE_PCM),
    };
    let block = channels as u16 * bits / 8;
    WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_EXTENSIBLE as u16,
            nChannels: channels as u16,
            nSamplesPerSec: rate,
            nAvgBytesPerSec: rate * block as u32,
            nBlockAlign: block,
            wBitsPerSample: bits,
            cbSize: 22,
        },
        Samples: WAVEFORMATEXTENSIBLE_0 { wValidBitsPerSample: valid },
        dwChannelMask: if channels == 1 { 0x4 } else { 0x3 },
        SubFormat: sub,
    }
}

/// Reads one interleaved device buffer into stereo samples: the first two
/// channels, or the one channel twice.
fn read_samples(format: &Format, data: &[u8], out: &mut [Stereo]) {
    let width = format.block / format.channels;
    let read = |bytes: &[u8]| -> f32 {
        match format.sample {
            Sample::F32 => f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            Sample::I16 => i16::from_le_bytes([bytes[0], bytes[1]]) as f32 / 32_768.0,
            Sample::I24 => cables::audio::decode_int24(bytes[0], bytes[1], bytes[2]) as f32 / 8_388_608.0,
            Sample::I32 => i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as f32 / 2_147_483_648.0,
        }
    };
    for (frame, sample) in data.chunks_exact(format.block).zip(out.iter_mut()) {
        let left = read(&frame[..width]);
        let right = if format.channels > 1 { read(&frame[width..2 * width]) } else { left };
        *sample = [left, right];
    }
}

/// Writes stereo samples into an interleaved device buffer: the first two
/// channels, silence on the others, or the mix of both on a mono device.
fn write_samples(format: &Format, samples: &[Stereo], data: &mut [u8]) {
    let width = format.block / format.channels;
    let write = |bytes: &mut [u8], value: f32| {
        let value = value.clamp(-1.0, 1.0);
        match format.sample {
            Sample::F32 => bytes[..4].copy_from_slice(&value.to_le_bytes()),
            Sample::I16 => bytes[..2].copy_from_slice(&((value * 32_767.0).round() as i16).to_le_bytes()),
            Sample::I24 => cables::audio::encode_int24(bytes, (value * 8_388_607.0).round() as i32),
            Sample::I32 => bytes[..4].copy_from_slice(&((value as f64 * 2_147_483_647.0).round() as i32).to_le_bytes()),
        }
    };
    for (frame, sample) in data.chunks_exact_mut(format.block).zip(samples) {
        frame.fill(0);
        if format.channels == 1 {
            write(&mut frame[..width], (sample[0] + sample[1]) * 0.5);
        } else {
            write(&mut frame[..width], sample[0]);
            write(&mut frame[width..2 * width], sample[1]);
        }
    }
}

/// An event handle, closed on drop.
struct EventHandle(HANDLE);

impl Drop for EventHandle {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateEventW and is closed once.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// An open, initialized audio client.
struct Stream {
    client: IAudioClient,
    format: Format,
    buffer_frames: usize,
    event: EventHandle,
    exclusive: bool,
}

/// 100 ns units, as WASAPI counts time.
const SHARED_BUFFER_HNS: i64 = 200_000; // 20 ms

fn open_device(choice: &DeviceChoice, direction: Direction) -> Result<IMMDevice, String> {
    let enumerator = enumerator()?;
    // SAFETY: COM calls on the enumerator; the ID string outlives the call.
    unsafe {
        if choice.is_default() {
            enumerator
                .GetDefaultAudioEndpoint(flow(direction), eConsole)
                .map_err(|e| format!("no default device: {}", describe(&e)))
        } else {
            let id = HSTRING::from(choice.endpoint.as_str());
            let device =
                enumerator.GetDevice(PCWSTR(id.as_ptr())).map_err(|_| "the device is not connected".to_string())?;
            let state = device.GetState().unwrap_or(DEVICE_STATE_NOTPRESENT);
            if state != DEVICE_STATE_ACTIVE {
                return Err("the device is not connected or is disabled".into());
            }
            Ok(device)
        }
    }
}

fn open_stream(device: &IMMDevice, exclusive: bool) -> Result<Stream, String> {
    // SAFETY: COM calls on interfaces this function holds; format pointers
    // point to live locals or to the mix format until it is freed.
    unsafe {
        let activate =
            || device.Activate::<IAudioClient>(CLSCTX_ALL, None).map_err(|e| format!("activate: {}", describe(&e)));
        let mut client = activate()?;
        let event = EventHandle(CreateEventW(None, false, false, None).map_err(|e| e.to_string())?);
        let format;
        if exclusive {
            let mix = client.GetMixFormat().map_err(|e| describe(&e))?;
            let mix_format = parse_format(mix);
            CoTaskMemFree(Some(mix as _));
            let channels = mix_format.map_or(2, |f| f.channels.max(1));
            let mut candidates = Vec::new();
            for rate in [48_000, mix_format.map_or(48_000, |f| f.rate), 44_100, 96_000] {
                for ch in [2, channels] {
                    for sample in [Sample::F32, Sample::I32, Sample::I24, Sample::I16] {
                        candidates.push((sample, ch, rate));
                    }
                }
            }
            let chosen = candidates.into_iter().find_map(|(sample, ch, rate)| {
                let wave = make_format(sample, ch, rate);
                let ptr = &wave as *const WAVEFORMATEXTENSIBLE as *const WAVEFORMATEX;
                (client.IsFormatSupported(AUDCLNT_SHAREMODE_EXCLUSIVE, ptr, None).0 == 0).then_some(wave)
            });
            let wave = chosen.ok_or("the device supports no usable exclusive-mode format")?;
            let ptr = &wave as *const WAVEFORMATEXTENSIBLE as *const WAVEFORMATEX;
            format = parse_format(ptr).ok_or("unsupported format")?;
            let (mut default_period, mut min_period) = (0i64, 0i64);
            client.GetDevicePeriod(Some(&mut default_period), Some(&mut min_period)).map_err(|e| describe(&e))?;
            let mut period = min_period.max(30_000); // at least 3 ms
            let flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
            let mut result = client.Initialize(AUDCLNT_SHAREMODE_EXCLUSIVE, flags, period, period, ptr, None);
            if let Err(err) = &result
                && err.code() == AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED
            {
                // The documented recovery: align the period to the buffer
                // size the device offered and start over with a new client.
                let frames = client.GetBufferSize().map_err(|e| describe(&e))? as i64;
                period = (10_000_000.0 * frames as f64 / format.rate as f64 + 0.5) as i64;
                client = activate()?;
                result = client.Initialize(AUDCLNT_SHAREMODE_EXCLUSIVE, flags, period, period, ptr, None);
            }
            result.map_err(|e| format!("exclusive mode: {}", describe(&e)))?;
        } else {
            let mix = client.GetMixFormat().map_err(|e| describe(&e))?;
            let parsed = parse_format(mix);
            let result = client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_NOPERSIST,
                SHARED_BUFFER_HNS,
                0,
                mix,
                None,
            );
            CoTaskMemFree(Some(mix as _));
            result.map_err(|e| format!("shared mode: {}", describe(&e)))?;
            format = parsed.ok_or("the device's mix format is not supported")?;
        }
        client.SetEventHandle(event.0).map_err(|e| describe(&e))?;
        let buffer_frames = client.GetBufferSize().map_err(|e| describe(&e))? as usize;
        Ok(Stream { client, format, buffer_frames, event, exclusive })
    }
}

/// How long a stream may go without a buffer event before it counts as
/// stalled.
const STALL_LIMIT: Duration = Duration::from_secs(3);

/// Opens the port's device and streams until the port stops, its generation
/// changes (`Ok`) or the device fails (`Err`).
pub(crate) fn run_stream(port: &PortShared, choice: &DeviceChoice, generation: u64) -> Result<(), String> {
    let _com = com();
    let device = open_device(choice, port.direction)?;
    let name = device_name(&device);
    let stream = open_stream(&device, choice.exclusive).map_err(|err| format!("{name}: {err}"))?;
    let _audio_thread = cables::enter_audio_thread(&port.logger);
    logf!(
        port.logger,
        "DEVICE {} opened \"{name}\" ({}, {} mode, buffer {} frames)",
        port.label(),
        stream.format.describe(),
        if stream.exclusive { "exclusive" } else { "shared" },
        stream.buffer_frames
    );
    *port.status.lock().unwrap_or_else(|p| p.into_inner()) =
        PortStatus { device_name: name.clone(), error: String::new(), exclusive: stream.exclusive };
    let result = match port.direction {
        Direction::Render => render(port, &stream, generation),
        Direction::Capture => capture(port, &stream, generation),
    };
    // SAFETY: stops the client this function started.
    unsafe {
        let _ = stream.client.Stop();
    }
    match &result {
        Ok(()) => logf!(port.logger, "DEVICE {} closed \"{name}\"", port.label()),
        Err(err) => logf!(port.logger, "DEVICE {} lost \"{name}\": {err}", port.label()),
    }
    result.map_err(|err| format!("{name}: {err}"))
}

fn render(port: &PortShared, stream: &Stream, generation: u64) -> Result<(), String> {
    let render_port = port.render.as_ref().ok_or("not a render port")?;
    let format = stream.format;
    // SAFETY: COM calls on the client this stream owns. Buffers WASAPI hands
    // out are written within their length and released before the next.
    unsafe {
        let client: IAudioRenderClient = stream.client.GetService().map_err(|e| describe(&e))?;
        let mut side = render_port.begin_stream(format.rate);
        // Start from a buffer of silence.
        let data = client.GetBuffer(stream.buffer_frames as u32).map_err(|e| describe(&e))?;
        std::ptr::write_bytes(data, 0, stream.buffer_frames * format.block);
        client
            .ReleaseBuffer(stream.buffer_frames as u32, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)
            .map_err(|e| describe(&e))?;
        stream.client.Start().map_err(|e| describe(&e))?;
        port.stats().set_state(PortState::Running);
        let mut samples = vec![[0f32; 2]; stream.buffer_frames];
        let mut last_event = Instant::now();
        loop {
            let signalled = WaitForSingleObject(stream.event.0, 100) == WAIT_OBJECT_0;
            if port.should_stop(generation) {
                return Ok(());
            }
            if !signalled {
                if last_event.elapsed() > STALL_LIMIT {
                    return Err("the device stopped asking for audio".into());
                }
                continue;
            }
            last_event = Instant::now();
            let available = if stream.exclusive {
                stream.buffer_frames
            } else {
                let padding = stream.client.GetCurrentPadding().map_err(|e| describe(&e))? as usize;
                stream.buffer_frames.saturating_sub(padding)
            };
            if available == 0 {
                continue;
            }
            side.render(render_port, &mut samples[..available]);
            let data = client.GetBuffer(available as u32).map_err(|e| describe(&e))?;
            let bytes = std::slice::from_raw_parts_mut(data, available * format.block);
            write_samples(&format, &samples[..available], bytes);
            client.ReleaseBuffer(available as u32, 0).map_err(|e| describe(&e))?;
        }
    }
}

fn capture(port: &PortShared, stream: &Stream, generation: u64) -> Result<(), String> {
    let capture_port = port.capture.as_ref().ok_or("not a capture port")?;
    let format = stream.format;
    // SAFETY: as for render; captured buffers are read within their length.
    unsafe {
        let client: IAudioCaptureClient = stream.client.GetService().map_err(|e| describe(&e))?;
        capture_port.begin_stream(format.rate);
        stream.client.Start().map_err(|e| describe(&e))?;
        port.stats().set_state(PortState::Running);
        let mut samples = vec![[0f32; 2]; stream.buffer_frames.max(4096)];
        let mut last_event = Instant::now();
        loop {
            let signalled = WaitForSingleObject(stream.event.0, 100) == WAIT_OBJECT_0;
            if port.should_stop(generation) {
                return Ok(());
            }
            if !signalled {
                if last_event.elapsed() > STALL_LIMIT {
                    return Err("the device stopped delivering audio".into());
                }
                continue;
            }
            last_event = Instant::now();
            loop {
                let packet = client.GetNextPacketSize().map_err(|e| describe(&e))?;
                if packet == 0 {
                    break;
                }
                let (mut data, mut frames, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
                client.GetBuffer(&mut data, &mut frames, &mut flags, None, None).map_err(|e| describe(&e))?;
                let frames = frames as usize;
                let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                let mut done = 0;
                while done < frames {
                    let n = (frames - done).min(samples.len());
                    if silent || data.is_null() {
                        samples[..n].fill([0.0; 2]);
                    } else {
                        let bytes = std::slice::from_raw_parts(data.add(done * format.block), n * format.block);
                        read_samples(&format, bytes, &mut samples[..n]);
                    }
                    capture_port.push(&samples[..n]);
                    done += n;
                }
                client.ReleaseBuffer(frames as u32).map_err(|e| describe(&e))?;
            }
        }
    }
}

#[implement(IMMNotificationClient)]
struct Client {
    callback: Arc<dyn Fn(DeviceChange) + Send + Sync>,
}

impl IMMNotificationClient_Impl for Client_Impl {
    fn OnDeviceStateChanged(&self, _: &PCWSTR, _: DEVICE_STATE) -> windows::core::Result<()> {
        (self.callback)(DeviceChange::Devices);
        Ok(())
    }

    fn OnDeviceAdded(&self, _: &PCWSTR) -> windows::core::Result<()> {
        (self.callback)(DeviceChange::Devices);
        Ok(())
    }

    fn OnDeviceRemoved(&self, _: &PCWSTR) -> windows::core::Result<()> {
        (self.callback)(DeviceChange::Devices);
        Ok(())
    }

    fn OnDefaultDeviceChanged(&self, flow: EDataFlow, role: ERole, _: &PCWSTR) -> windows::core::Result<()> {
        if role == eConsole {
            let direction = if flow == eCapture { Direction::Capture } else { Direction::Render };
            (self.callback)(DeviceChange::Default(direction));
        }
        Ok(())
    }

    fn OnPropertyValueChanged(&self, _: &PCWSTR, _: &PROPERTYKEY) -> windows::core::Result<()> {
        Ok(())
    }
}

/// Device change notifications, delivered to a callback while it lives.
pub(crate) struct Notifier {
    stop: Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl Notifier {
    pub(crate) fn start(callback: Box<dyn Fn(DeviceChange) + Send + Sync>) -> Result<Notifier, String> {
        let (stop, stopped) = channel::<()>();
        let (ready_tx, ready) = channel::<Result<(), String>>();
        let callback: Arc<dyn Fn(DeviceChange) + Send + Sync> = Arc::from(callback);
        let thread = std::thread::Builder::new()
            .name("device notifications".into())
            .spawn(move || {
                let _com = com();
                let registered = enumerator().and_then(|enumerator| {
                    let client: IMMNotificationClient = Client { callback }.into();
                    // SAFETY: registers a live client with a live enumerator.
                    unsafe { enumerator.RegisterEndpointNotificationCallback(&client) }
                        .map(|()| (enumerator, client))
                        .map_err(|e| describe(&e))
                });
                match registered {
                    Ok((enumerator, client)) => {
                        let _ = ready_tx.send(Ok(()));
                        let _ = stopped.recv();
                        // SAFETY: unregisters the client registered above.
                        unsafe {
                            let _ = enumerator.UnregisterEndpointNotificationCallback(&client);
                        }
                    }
                    Err(err) => {
                        let _ = ready_tx.send(Err(err));
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        ready.recv().map_err(|e| e.to_string())??;
        Ok(Notifier { stop, thread: Some(thread) })
    }
}

impl Drop for Notifier {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Measures the round trip from a playback device to a recording device:
/// plays clicks into `render_name` (for example a mixer input cable) and
/// times their arrival on `capture_name` (a mixer output bus), matched by
/// name. Returns a report.
pub fn measure_latency(
    render_name: &str,
    capture_name: &str,
    clicks: usize,
    logger: &Logger,
) -> Result<String, String> {
    use engine::FRAME_LEN;

    let find = |direction: Direction, name: &str| -> Result<DeviceInfo, String> {
        let devices = enumerate(direction)?;
        let lower = name.to_lowercase();
        devices.into_iter().find(|d| d.name.to_lowercase().contains(&lower)).ok_or_else(|| {
            format!(
                "no {} device matches \"{name}\"",
                if direction == Direction::Render { "playback" } else { "recording" }
            )
        })
    };
    let (render_device, capture_device) =
        (find(Direction::Render, render_name)?, find(Direction::Capture, capture_name)?);
    let manager = crate::DeviceManager::new(logger, 20.0);
    let output = manager.render("latency playback");
    let input = manager.capture("latency recording");
    output.configure(DeviceChoice { endpoint: render_device.id.clone(), exclusive: false });
    input.configure(DeviceChoice { endpoint: capture_device.id.clone(), exclusive: false });
    let (output_port, input_port) = (output.output_port().unwrap(), input.input_port().unwrap());

    // Run on the bus clock, as the engine does, until every click is timed.
    let event = cables::event::Event::new();
    let mut next = cables::usbip::busclock::current_bus_time().frame();
    let started = Instant::now();
    let period = 500u64; // frames between clicks
    let mut sent: Vec<u64> = Vec::new();
    let mut found: Vec<f64> = Vec::new();
    let mut frame_index = 0u64;
    let mut armed: Option<u64> = None;
    let mut frame = [0f32; FRAME_LEN];
    while found.len() < clicks {
        if started.elapsed() > Duration::from_secs(10 + clicks as u64) {
            return Err(format!(
                "timed out with {} of {clicks} clicks found; is \"{}\" routed to \"{}\"?",
                found.len(),
                render_device.name,
                capture_device.name
            ));
        }
        let now = cables::usbip::busclock::current_bus_time().frame();
        while next < now {
            next += 1;
            frame_index += 1;
            let warm = frame_index > 2000; // let both streams settle first
            let mut click = [0f32; FRAME_LEN];
            if warm && frame_index.is_multiple_of(period) && armed.is_none() {
                click[0] = 0.9;
                click[1] = 0.9;
                sent.push(frame_index * 48);
                armed = Some(frame_index * 48);
            }
            output_port.push(&click);
            input_port.pull(&mut frame);
            if let Some(at) = armed {
                if let Some(i) = frame.as_chunks::<2>().0.iter().position(|p| p[0].abs() > 0.3 || p[1].abs() > 0.3) {
                    let arrival = frame_index * 48 + i as u64;
                    found.push((arrival - at) as f64 / 48.0);
                    armed = None;
                } else if frame_index * 48 > at + 48 * 450 {
                    armed = None; // lost; try the next click
                }
            }
        }
        event.wait(Some(Instant::now() + Duration::from_micros(1000)));
    }
    let mut sorted = found.clone();
    sorted.sort_by(f64::total_cmp);
    let own = output.reading().fill_ms + input.reading().fill_ms;
    Ok(format!(
        "Round trip \"{}\" -> \"{}\" over {} clicks: min {:.1} ms, median {:.1} ms, max {:.1} ms.\n\
         This includes about {:.1} ms of this tool's own buffering (playback and recording rings), \
         so the path through Windows and the mixer is about {:.1} ms.",
        render_device.name,
        capture_device.name,
        sorted.len(),
        sorted[0],
        sorted[sorted.len() / 2],
        sorted[sorted.len() - 1],
        own,
        sorted[sorted.len() / 2] - own
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_round_trip_through_waveformatextensible() {
        for sample in [Sample::F32, Sample::I16, Sample::I24, Sample::I32] {
            let wave = make_format(sample, 2, 44_100);
            // SAFETY: a valid extensible format.
            let parsed = unsafe { parse_format(&wave as *const WAVEFORMATEXTENSIBLE as *const WAVEFORMATEX) }.unwrap();
            assert_eq!((parsed.sample, parsed.channels, parsed.rate), (sample, 2, 44_100));
        }
    }

    #[test]
    fn samples_convert_both_ways() {
        let stereo = [[0.5f32, -0.25], [1.5, -1.5]];
        for sample in [Sample::F32, Sample::I16, Sample::I24, Sample::I32] {
            for channels in [1, 2, 6] {
                let wave = make_format(sample, channels, 48_000);
                // SAFETY: a valid extensible format.
                let format = unsafe { parse_format(&wave as *const _ as *const WAVEFORMATEX) }.unwrap();
                let mut bytes = vec![0xAAu8; format.block * 2];
                write_samples(&format, &stereo, &mut bytes);
                let mut back = [[0f32; 2]; 2];
                read_samples(&format, &bytes, &mut back);
                let expect = if channels == 1 { [[0.125, 0.125], [0.0, 0.0]] } else { [[0.5, -0.25], [1.0, -1.0]] };
                for (got, want) in back.iter().flatten().zip(expect.iter().flatten()) {
                    assert!((got - want).abs() < 1e-4, "{sample:?} {channels} ch: {back:?}");
                }
            }
        }
    }

    #[test]
    fn enumeration_works_or_reports_why() {
        // Machines without audio devices (CI) list none rather than fail.
        for direction in [Direction::Render, Direction::Capture] {
            if let Ok(devices) = enumerate(direction) {
                assert!(devices.iter().filter(|d| d.is_default).count() <= 1);
            }
        }
    }
}
