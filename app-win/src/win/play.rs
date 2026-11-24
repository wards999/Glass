//! The host's sound on the Windows viewer (M4b step 3): core decodes it
//! into a `PcmRing` (48 kHz stereo float, its playout buffer and catch-up
//! logic shared with the Mac viewer); this plays the ring through WASAPI,
//! shared mode, event-driven with a 10 ms period, Windows converting to the
//! device's own format.
//!
//! The render thread only reads the ring (lock-free) and copies into the
//! device's buffer: no allocation, no lock, no logging.

use glass_core::audio::PcmRing;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioClient, IAudioRenderClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

/// Plays until dropped.
pub struct Player {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// The device's own delay (ms: its latency plus our buffer), for A/V.
    pub output_ms: Arc<AtomicU32>,
    pub error: Arc<std::sync::Mutex<Option<String>>>,
    pub playing: Arc<AtomicBool>,
}

impl Player {
    /// Plays `ring` (from `Client::audio_ring`, once the sound arrived).
    pub fn start(ring: Arc<PcmRing>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let output_ms = Arc::new(AtomicU32::new(0));
        let error = Arc::new(std::sync::Mutex::new(None));
        let playing = Arc::new(AtomicBool::new(false));
        let (s, o, er, pl) = (
            stop.clone(),
            output_ms.clone(),
            error.clone(),
            playing.clone(),
        );
        let thread = std::thread::Builder::new()
            .name("glass-play".into())
            .spawn(move || {
                // SAFETY: COM for this thread only.
                let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
                if let Err(e) = run(&ring, &s, &o, &pl) {
                    *er.lock().unwrap_or_else(|e| e.into_inner()) = Some(e);
                }
                pl.store(false, Ordering::Relaxed);
                unsafe { CoUninitialize() };
            })
            .ok();
        Self {
            stop,
            thread,
            output_ms,
            error,
            playing,
        }
    }

    /// For the results: empty while it plays fine.
    pub fn problem(&self) -> Option<String> {
        self.error.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn e(what: &'static str) -> impl Fn(windows::core::Error) -> String {
    move |err| format!("{what}: {err}")
}

fn run(
    ring: &PcmRing,
    stop: &AtomicBool,
    output_ms: &AtomicU32,
    playing: &AtomicBool,
) -> Result<(), String> {
    // SAFETY: plain WASAPI calls on this thread's COM objects; the device
    // buffer is written only between GetBuffer and ReleaseBuffer, for the
    // frames it gave.
    unsafe {
        let en: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(e("devices"))?;
        let dev = en
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .map_err(e("no output device"))?;
        let client: IAudioClient = dev.Activate(CLSCTX_ALL, None).map_err(e("audio client"))?;
        let fmt = WAVEFORMATEX {
            wFormatTag: 3, // WAVE_FORMAT_IEEE_FLOAT
            nChannels: 2,
            nSamplesPerSec: 48_000,
            nAvgBytesPerSec: 48_000 * 8,
            nBlockAlign: 8,
            wBitsPerSample: 32,
            cbSize: 0,
        };
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                    | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                    | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                100_000, // 10 ms
                0,
                &fmt,
                None,
            )
            .map_err(e("playback"))?;
        let event = CreateEventW(None, false, false, None).map_err(e("event"))?;
        let result = (|| {
            client.SetEventHandle(event).map_err(e("event"))?;
            let render: IAudioRenderClient = client.GetService().map_err(e("render"))?;
            let size = client.GetBufferSize().map_err(e("buffer"))?;
            let latency = client.GetStreamLatency().map_err(e("latency"))?;
            // Hundreds of ns: the device's latency plus our buffer.
            let ms = (latency as u64 + size as u64 * 10_000_000 / 48_000) / 10_000;
            output_ms.store(ms as u32, Ordering::Relaxed);
            // Start with the buffer full of silence.
            let p = render.GetBuffer(size).map_err(e("write"))?;
            std::ptr::write_bytes(p, 0, size as usize * 8);
            render.ReleaseBuffer(size, 0).map_err(e("write"))?;
            client.Start().map_err(e("start"))?;
            playing.store(true, Ordering::Relaxed);
            while !stop.load(Ordering::Relaxed) {
                if WaitForSingleObject(event, 100) != WAIT_OBJECT_0 {
                    continue;
                }
                let padding = client.GetCurrentPadding().map_err(e("write"))?;
                let free = size.saturating_sub(padding);
                if free == 0 {
                    continue;
                }
                let p = render.GetBuffer(free).map_err(e("write"))?;
                let out = std::slice::from_raw_parts_mut(p.cast::<f32>(), free as usize * 2);
                let n = ring.read(out, 1.0);
                // What didn't come plays as silence (the ring counts it).
                out[n..].fill(0.0);
                render.ReleaseBuffer(free, 0).map_err(e("write"))?;
            }
            let _ = client.Stop();
            Ok(())
        })();
        let _ = CloseHandle(event);
        result
    }
}
