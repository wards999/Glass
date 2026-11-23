//! The host's sound (M4b, ADR 0025): WASAPI loopback of the default output
//! device, 48 kHz stereo float (Windows converts from the device's own
//! format), in 10 ms frames to the session's `AudioOut`.
//!
//! - Frames follow the device's clock while sound plays. When nothing
//!   plays, Windows delivers nothing, so silence frames go out on a 10 ms
//!   clock instead and the viewer's buffer never runs dry.
//! - While being viewed, the PC's own speakers are muted, and restored at
//!   the end (the user's rule: sound goes to the viewer only). Loopback
//!   still hears it on most devices, since Windows mutes after the mix it
//!   captures; `MUTE_OK` reports whether it did here.
//!
//! LATER(M4b step 4): the service's sessions (capture in the user's
//! session helper).

use glass_core::audio::FRAME_LEN;
use glass_core::clock;
use glass_core::net::AudioOut;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
};

/// What the capture did, for the host's results.
#[derive(Default)]
pub struct SoundCounters {
    pub frames: AtomicU64,
    pub silent_frames: AtomicU64,
    /// The speakers were muted for the session (and restored).
    pub muted: AtomicBool,
    pub error: std::sync::Mutex<Option<String>>,
}

/// Captures until dropped.
pub struct Capture {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    pub counters: Arc<SoundCounters>,
}

impl Capture {
    /// `mute`: silence this PC's speakers while capturing.
    pub fn start(out: AudioOut, mute: bool) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let counters = Arc::new(SoundCounters::default());
        let (s, c) = (stop.clone(), counters.clone());
        let thread = std::thread::Builder::new()
            .name("glass-sound".into())
            .spawn(move || {
                // SAFETY: COM for this thread only.
                let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
                if let Err(e) = run(&out, mute, &s, &c) {
                    *c.error.lock().unwrap_or_else(|e| e.into_inner()) = Some(e);
                }
                unsafe { CoUninitialize() };
            })
            .ok();
        Self {
            stop,
            thread,
            counters,
        }
    }

    /// The results line.
    pub fn line(&self) -> String {
        let c = &self.counters;
        let err = c.error.lock().unwrap_or_else(|e| e.into_inner()).clone();
        match err {
            Some(e) => format!("sound (M4b): not captured: {e}"),
            None => format!(
                "sound (M4b): {} frames sent ({} silent) | this PC's speakers {}",
                c.frames.load(Ordering::Relaxed),
                c.silent_frames.load(Ordering::Relaxed),
                if c.muted.load(Ordering::Relaxed) {
                    "muted during the session"
                } else {
                    "left as they were"
                }
            ),
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Mutes the device's output and restores what it was when dropped.
struct Muted(IAudioEndpointVolume, windows::core::BOOL);

impl Drop for Muted {
    fn drop(&mut self) {
        // SAFETY: the endpoint this struct muted.
        unsafe {
            let _ = self.0.SetMute(self.1.as_bool(), std::ptr::null());
        }
    }
}

fn e(what: &'static str) -> impl Fn(windows::core::Error) -> String {
    move |err| format!("{what}: {err}")
}

fn run(out: &AudioOut, mute: bool, stop: &AtomicBool, c: &SoundCounters) -> Result<(), String> {
    // SAFETY: plain WASAPI calls on this thread's COM objects; buffers are
    // read only between GetBuffer and ReleaseBuffer, for the frames given.
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
                AUDCLNT_STREAMFLAGS_LOOPBACK
                    | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                    | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                200_000, // 20 ms
                0,
                &fmt,
                None,
            )
            .map_err(e("loopback"))?;
        let cap: IAudioCaptureClient = client.GetService().map_err(e("capture"))?;
        let _muted = if mute {
            dev.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None)
                .ok()
                .and_then(|v| {
                    let was = v.GetMute().ok()?;
                    v.SetMute(true, std::ptr::null()).ok()?;
                    c.muted.store(true, Ordering::Relaxed);
                    Some(Muted(v, was))
                })
        } else {
            None
        };
        client.Start().map_err(e("start"))?;

        // Interleaved stereo waiting to fill a frame.
        let mut fifo: Vec<f32> = Vec::with_capacity(FRAME_LEN * 8);
        let silence = [0f32; FRAME_LEN];
        let mut last_data = clock::now_us();
        let mut next_silence = last_data;
        while !stop.load(Ordering::Relaxed) && out.connected() {
            let mut got = false;
            loop {
                let n = cap.GetNextPacketSize().map_err(e("read"))?;
                if n == 0 {
                    break;
                }
                let mut data = std::ptr::null_mut();
                let (mut frames, mut flags) = (0u32, 0u32);
                cap.GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                    .map_err(e("read"))?;
                let len = frames as usize * 2;
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                    fifo.resize(fifo.len() + len, 0.0);
                } else {
                    fifo.extend_from_slice(std::slice::from_raw_parts(data.cast::<f32>(), len));
                }
                cap.ReleaseBuffer(frames).map_err(e("read"))?;
                got = true;
            }
            let now = clock::now_us();
            if got {
                last_data = now;
            }
            // While sound flows: a frame whenever 10 ms are here, stamped
            // with when its first sample was captured.
            let mut sent = 0;
            while fifo.len() >= FRAME_LEN {
                let behind = (fifo.len() / 2) as u64 * 1_000_000 / 48_000;
                out.send(&fifo[..FRAME_LEN], now.saturating_sub(behind));
                fifo.drain(..FRAME_LEN);
                c.frames.fetch_add(1, Ordering::Relaxed);
                sent += 1;
            }
            if sent > 0 {
                next_silence = now + 10_000;
            } else if now.saturating_sub(last_data) >= 20_000 && now >= next_silence {
                // Nothing playing: silence on a 10 ms clock (with what was
                // left of the last sound first).
                let mut frame = silence;
                let k = fifo.len().min(FRAME_LEN);
                frame[..k].copy_from_slice(&fifo[..k]);
                fifo.clear();
                out.send(&frame, now);
                c.frames.fetch_add(1, Ordering::Relaxed);
                c.silent_frames.fetch_add(1, Ordering::Relaxed);
                next_silence += 10_000;
                if now > next_silence + 50_000 {
                    next_silence = now + 10_000; // after a stall: no burst
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let _ = client.Stop();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Media::Audio::IAudioRenderClient;

    /// Mutes the default output for ~2 s, plays a quiet tone into it (so
    /// nothing is heard) and checks whether loopback still captures it:
    /// whether "speakers silent while viewed" works on this PC's device.
    /// Run by hand: `cargo test -p glass-win loopback_hears -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn loopback_hears_through_mute() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let en: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).unwrap();
            let dev = en.GetDefaultAudioEndpoint(eRender, eConsole).unwrap();
            let vol: IAudioEndpointVolume = dev.Activate(CLSCTX_ALL, None).unwrap();
            let was = vol.GetMute().unwrap();
            vol.SetMute(true, std::ptr::null()).unwrap();
            let _restore = Muted(vol, was);
            let fmt = WAVEFORMATEX {
                wFormatTag: 3,
                nChannels: 2,
                nSamplesPerSec: 48_000,
                nAvgBytesPerSec: 48_000 * 8,
                nBlockAlign: 8,
                wBitsPerSample: 32,
                cbSize: 0,
            };
            let flags =
                AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
            let play: IAudioClient = dev.Activate(CLSCTX_ALL, None).unwrap();
            play.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 1_000_000, 0, &fmt, None)
                .unwrap();
            let render: IAudioRenderClient = play.GetService().unwrap();
            let size = play.GetBufferSize().unwrap();
            let rec: IAudioClient = dev.Activate(CLSCTX_ALL, None).unwrap();
            rec.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                flags | AUDCLNT_STREAMFLAGS_LOOPBACK,
                200_000,
                0,
                &fmt,
                None,
            )
            .unwrap();
            let cap: IAudioCaptureClient = rec.GetService().unwrap();
            play.Start().unwrap();
            rec.Start().unwrap();
            let (mut t, mut sum, mut n) = (0u64, 0f64, 0u64);
            let start = std::time::Instant::now();
            while start.elapsed().as_millis() < 1_500 {
                let pad = play.GetCurrentPadding().unwrap();
                let free = size - pad;
                if free > 0 {
                    let p = render.GetBuffer(free).unwrap().cast::<f32>();
                    for i in 0..free as usize {
                        let v =
                            ((t as f32) * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.05;
                        *p.add(i * 2) = v;
                        *p.add(i * 2 + 1) = v;
                        t += 1;
                    }
                    render.ReleaseBuffer(free, 0).unwrap();
                }
                while cap.GetNextPacketSize().unwrap() > 0 {
                    let mut data = std::ptr::null_mut();
                    let (mut frames, mut fl) = (0u32, 0u32);
                    cap.GetBuffer(&mut data, &mut frames, &mut fl, None, None)
                        .unwrap();
                    if fl & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 == 0 && !data.is_null() {
                        for v in std::slice::from_raw_parts(data.cast::<f32>(), frames as usize * 2)
                        {
                            sum += (*v as f64) * (*v as f64);
                            n += 1;
                        }
                    }
                    cap.ReleaseBuffer(frames).unwrap();
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            let _ = play.Stop();
            let _ = rec.Stop();
            let rms = (sum / n.max(1) as f64).sqrt();
            // The tone's RMS is 0.05 / sqrt(2) = 0.035.
            println!("loopback while muted: {n} samples, RMS {rms:.4} (the tone is 0.0354)");
        }
    }
}
