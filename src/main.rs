use aec3::api::config::{EchoCanceller3Config, MaskingThresholds, Tuning};
use anyhow::Result;
use crossbeam_channel::{Receiver, Sender, bounded};
use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{prelude::*, widgets::*};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread;
use sysinfo::System;

use aec3::audio_processing::audio_buffer::AudioBuffer;
use aec3::audio_processing::high_pass_filter::HighPassFilter;
use aec3::audio_processing::stream_config::StreamConfig;
use aec3::{api::EchoControl, audio_processing::aec3::echo_canceller3::EchoCanceller3};

// ---------------------------------------------------------------------------
// AEC Suppression Presets
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
enum AecPreset {
    Transparent,
    Aggressive,
}

impl AecPreset {
    const ALL: [AecPreset; 2] = [AecPreset::Transparent, AecPreset::Aggressive];

    fn name(self) -> &'static str {
        match self {
            AecPreset::Transparent => "Transparent",
            AecPreset::Aggressive => "Aggressive",
        }
    }

    fn next(self) -> AecPreset {
        let idx = AecPreset::ALL.iter().position(|&p| p == self).unwrap_or(0);
        AecPreset::ALL[(idx + 1) % AecPreset::ALL.len()]
    }

    /// Build an EchoCanceller3Config tuned for this preset.
    fn apply_to(self, cfg: &mut EchoCanceller3Config) {
        match self {
            // Suppressor almost fully disabled — passes voice through unaltered.
            AecPreset::Transparent => {
                cfg.suppressor.normal_tuning = Tuning::new(
                    MaskingThresholds::new(10.0, 15.0, 5.0),
                    MaskingThresholds::new(10.0, 15.0, 5.0),
                    4.0,
                    0.5,
                );
                cfg.suppressor.nearend_tuning = Tuning::new(
                    MaskingThresholds::new(10.0, 15.0, 5.0),
                    MaskingThresholds::new(10.0, 15.0, 5.0),
                    4.0,
                    0.5,
                );
                cfg.ep_strength.default_gain = 0.0;
                cfg.ep_strength.bounded_erl = true;
            }

            // Strong echo cancellation with reduced NLP to preserve voice clarity.
            // Masking thresholds at moderate values (vs. extreme 0.1 for max suppression)
            // so the post-filter doesn't over-suppress near-end speech.
            AecPreset::Aggressive => {
                cfg.suppressor.nearend_tuning = Tuning::new(
                    MaskingThresholds::new(10.0, 15.0, 5.0),
                    MaskingThresholds::new(10.0, 15.0, 5.0),
                    4.0,
                    0.5,
                );
            }
        }
    }

    fn build_config(self, channels: usize) -> EchoCanceller3Config {
        let mut cfg = EchoCanceller3::create_default_config(channels, channels);
        self.apply_to(&mut cfg);
        cfg
    }
}
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

// ---------------------------------------------------------------------------
// Simple first-order IIR high-pass filter
// Removes frequencies below `cutoff_hz` (e.g. 150 Hz) to eliminate rumble.
// y[n] = alpha * (y[n-1] + x[n] - x[n-1])
// ---------------------------------------------------------------------------
struct HighPass150 {
    alpha: f32,
    prev_x: Vec<f32>, // previous input sample per channel
    prev_y: Vec<f32>, // previous output sample per channel
}

impl HighPass150 {
    fn new(sample_rate: usize, channels: usize, cutoff_hz: f32) -> Self {
        let rc = 1.0 / (2.0 * std::f32::consts::PI * cutoff_hz);
        let dt = 1.0 / sample_rate as f32;
        let alpha = rc / (rc + dt);
        Self {
            alpha,
            prev_x: vec![0.0; channels],
            prev_y: vec![0.0; channels],
        }
    }

    fn process(&mut self, channels_data: &mut [Vec<f32>]) {
        for ch in 0..channels_data.len() {
            for s in 0..channels_data[ch].len() {
                let x = channels_data[ch][s];
                let y = self.alpha * (self.prev_y[ch] + x - self.prev_x[ch]);
                self.prev_x[ch] = x;
                self.prev_y[ch] = y;
                channels_data[ch][s] = y;
            }
        }
    }
}

#[cfg(windows)]
fn enable_efficiency_mode() {
    use std::ffi::c_void;
    use windows::Win32::System::Threading::{
        GetCurrentProcess, IDLE_PRIORITY_CLASS, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_STATE,
        ProcessPowerThrottling, SetPriorityClass, SetProcessInformation,
    };

    unsafe {
        // Set idle priority class to prefer E-cores
        let _ = SetPriorityClass(GetCurrentProcess(), IDLE_PRIORITY_CLASS);

        // Explicitly enable EcoQoS (Efficiency mode in Task Manager)
        let mut state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        };
        let _ = SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            &mut state as *mut _ as *mut c_void,
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        );
    }
}

// ---------------------------------------------------------------------------
// WASAPI consumer detection
// Returns true if at least one audio session on the "CABLE Output" capture
// device is currently Active (i.e. an app has the virtual mic open).
// ---------------------------------------------------------------------------
#[cfg(windows)]
fn has_cable_output_consumers() -> bool {
    use windows::Win32::Media::Audio::{
        AudioSessionStateActive, IAudioSessionEnumerator, IAudioSessionManager2,
        IMMDeviceEnumerator, MMDeviceEnumerator, eCapture,
    };
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
    };

    unsafe {
        // COM may already be initialised on this thread; ignore the error.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

        let enumerator: IMMDeviceEnumerator =
            match CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) {
                Ok(e) => e,
                Err(_) => return false,
            };

        // Enumerate all capture (microphone) endpoints.
        use windows::Win32::Media::Audio::DEVICE_STATE;
        let collection = match enumerator.EnumAudioEndpoints(
            eCapture,
            DEVICE_STATE(0x0000_0001), /* DEVICE_STATE_ACTIVE */
        ) {
            Ok(c) => c,
            Err(_) => return false,
        };

        let count = collection.GetCount().unwrap_or(0);
        for i in 0..count {
            let device = match collection.Item(i) {
                Ok(d) => d,
                Err(_) => continue,
            };

            // Check if this device name contains "CABLE Output".
            use windows::Win32::System::Com::STGM;
            let props = match device.OpenPropertyStore(STGM(0x00000000) /* STGM_READ */) {
                Ok(p) => p,
                Err(_) => continue,
            };
            // PKEY_Device_FriendlyName = {a45c254e-df1c-4efd-8020-67d146a850e0}, 14
            use windows::Win32::Devices::Properties::DEVPKEY_Device_FriendlyName;
            use windows::Win32::Foundation::PROPERTYKEY;
            let name_prop = match props
                .GetValue(&DEVPKEY_Device_FriendlyName as *const _ as *const PROPERTYKEY)
            {
                Ok(v) => v,
                Err(_) => continue,
            };
            let name = name_prop.Anonymous.Anonymous.Anonymous.pwszVal;
            // pwszVal is a PWSTR; call to_string() on it directly.
            let name_str = name.to_string().unwrap_or_default();
            if !name_str.contains("CABLE Output") {
                continue;
            }

            // Found the CABLE Output device. Get the session manager via IMMDevice::Activate.
            let session_mgr: IAudioSessionManager2 = match device.Activate(CLSCTX_ALL, None) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let enumerator2: IAudioSessionEnumerator = match session_mgr.GetSessionEnumerator() {
                Ok(e) => e,
                Err(_) => continue,
            };
            let session_count = enumerator2.GetCount().unwrap_or(0);
            for j in 0..session_count {
                if let Ok(ctrl) = enumerator2.GetSession(j) {
                    if let Ok(state) = ctrl.GetState() {
                        if state == AudioSessionStateActive {
                            return true;
                        }
                    }
                }
            }
        }
        false
    }
}

fn interleaved_to_channels(
    interleaved: &[f32],
    channels: usize,
    frames: usize,
    out: &mut [Vec<f32>],
) {
    let avail_frames = interleaved.len() / channels;
    let copy_frames = std::cmp::min(avail_frames, frames);
    for frame in 0..copy_frames {
        for ch in 0..channels {
            out[ch][frame] = interleaved[frame * channels + ch];
        }
    }
}

fn channels_to_interleaved(channels_data: &mut [&[f32]], out: &mut [f32]) {
    let channels = channels_data.len();
    let frames = channels_data[0].len();
    for frame in 0..frames {
        for ch in 0..channels {
            out[frame * channels + ch] = channels_data[ch][frame];
        }
    }
}

fn processing_thread(
    rx_in: Receiver<Vec<f32>>,
    rx_render: Receiver<Vec<f32>>,
    tx_out: Sender<Vec<f32>>,
    tx_metrics: Sender<String>,
    rx_preset: Receiver<AecPreset>,
    sample_rate: usize,
    channels: usize,
    sleeping: Arc<AtomicBool>,
    no_consumers: Arc<AtomicBool>,
    // Calibration / threshold atomics
    current_rms_atomic: Arc<AtomicU32>,
    silence_threshold_atomic: Arc<AtomicU32>,
    calibrate_trigger: Arc<AtomicBool>,
    calibrating: Arc<AtomicBool>,
) {
    // ── Frame size ────────────────────────────────────────────────────────────
    let frames = sample_rate / 100; // 10 ms at 48 kHz = 480 frames

    // ── AEC3 at native 48 kHz ─────────────────────────────────────────────────
    let mut current_preset = AecPreset::Aggressive;
    let cfg = current_preset.build_config(channels);
    let mut aec3 = EchoCanceller3::new(cfg, sample_rate as i32, channels, channels);

    let mut audio_buf =
        AudioBuffer::from_sample_rates(sample_rate, channels, sample_rate, channels, sample_rate);
    let stream_config = StreamConfig::new(sample_rate as i32, channels, false);
    let mut render_buf =
        AudioBuffer::from_sample_rates(sample_rate, channels, sample_rate, channels, sample_rate);

    // ── Pre-allocated working buffers ─────────────────────────────────────────
    let mut cap_buf = vec![vec![0f32; frames]; channels];
    let mut rnd_buf = vec![vec![0f32; frames]; channels];
    let mut out_buf = vec![vec![0f32; frames]; channels];
    let silence = vec![0.0f32; frames * channels];
    let mut output = vec![0f32; frames * channels];

    // ── 150 Hz high-pass on capture ───────────────────────────────────────────
    let mut hp_150 = HighPass150::new(sample_rate, channels, 150.0);

    // ── aec3 HighPassFilter (applied on split band-0) ─────────────────────────
    let mut hp_filter = HighPassFilter::new(sample_rate as i32, channels);

    let mut last_metrics = std::time::Instant::now();
    let metrics_interval = std::time::Duration::from_millis(100);

    // ── Sleep-mode parameters ─────────────────────────────────────────────────
    // Park the thread after this many consecutive silent frames.
    // At 480 frames / 10 ms each → 20 frames = 200 ms of silence.
    // The threshold is dynamic — loaded each frame from the shared atomic.
    const SLEEP_FRAMES: u32 = 200; // 200 ms at 100 fps
    let mut silent_frame_count: u32 = 0;

    // ── Calibration state ─────────────────────────────────────────────────────
    const CAL_FRAMES: u32 = 200; // 2 s of ambient sampling
    let mut cal_count: u32 = 0;
    let mut cal_sum: f32 = 0.0;

    while let Ok(frame) = rx_in.recv() {
        // ── Wake-up: if we were sleeping, clear the flag and reset counter ────
        if sleeping.load(Ordering::Relaxed) {
            sleeping.store(false, Ordering::Relaxed);
            silent_frame_count = 0;
        }
        // ── Preset change ─────────────────────────────────────────────────────
        if let Ok(new_preset) = rx_preset.try_recv() {
            if new_preset != current_preset {
                current_preset = new_preset;
                let new_cfg = current_preset.build_config(channels);
                aec3 = EchoCanceller3::new(new_cfg, sample_rate as i32, channels, channels);
            }
        }

        // ── Capture: deinterleave ─────────────────────────────────────────────
        interleaved_to_channels(&frame, channels, frames, &mut cap_buf);

        // ── Capture: 150 Hz high-pass ─────────────────────────────────────────
        hp_150.process(&mut cap_buf);

        // ── Capture: copy into AEC AudioBuffer (full-band, before split) ──────
        let refs: Vec<&[f32]> = cap_buf.iter().map(|v| v.as_slice()).collect();
        audio_buf.copy_from(&refs, &stream_config);
        aec3.analyze_capture(&mut audio_buf);
        audio_buf.split_into_frequency_bands();

        // ── Capture: AEC HPF on band-0 sub-bands ─────────────────────────────
        let mut hp_filter_channels: Vec<Vec<f32>> = (0..channels)
            .map(|ch| audio_buf.split_band(ch, 0).to_vec())
            .collect();
        hp_filter.process(&mut hp_filter_channels);
        for ch in 0..channels {
            audio_buf
                .split_band_mut(ch, 0)
                .copy_from_slice(&hp_filter_channels[ch]);
        }

        // ── Render: deinterleave ──────────────────────────────────────────────
        let render_data = rx_render.try_recv().unwrap_or_else(|_| silence.clone());
        interleaved_to_channels(&render_data, channels, frames, &mut rnd_buf);
        let refs_render: Vec<&[f32]> = rnd_buf.iter().map(|v| v.as_slice()).collect();
        render_buf.copy_from(&refs_render, &stream_config);
        render_buf.split_into_frequency_bands();
        aec3.analyze_render(&mut render_buf);

        // ── AEC process ───────────────────────────────────────────────────────
        aec3.process_capture(&mut audio_buf, false);
        audio_buf.merge_frequency_bands();

        // ── Output: copy AEC result into channel buffers ──────────────────────
        let mut out_refs: Vec<&mut [f32]> = out_buf.iter_mut().map(|v| v.as_mut_slice()).collect();
        audio_buf.copy_to_stream(&stream_config, &mut out_refs);

        // ── Output: interleave + clamp ────────────────────────────────────────
        let out_refs_immut: Vec<&[f32]> = out_buf.iter().map(|v| v.as_slice()).collect();
        channels_to_interleaved(&mut out_refs_immut.clone(), &mut output);
        for sample in output.iter_mut() {
            *sample = sample.clamp(-1.0, 1.0);
        }

        // ── Metrics ───────────────────────────────────────────────────────────
        if last_metrics.elapsed() >= metrics_interval {
            let _ = tx_metrics.try_send(format!("{:#?}", aec3.metrics()));
            last_metrics = std::time::Instant::now();
        }

        let _ = tx_out.try_send(output.clone());

        // ── Compute frame RMS ─────────────────────────────────────────────────
        let rms: f32 = {
            let sum_sq: f32 = frame.iter().map(|&s| s * s).sum();
            (sum_sq / frame.len().max(1) as f32).sqrt()
        };
        // Publish for TUI display.
        current_rms_atomic.store(rms.to_bits(), Ordering::Relaxed);

        // ── Calibration ───────────────────────────────────────────────────────
        if calibrate_trigger.load(Ordering::Relaxed) {
            calibrating.store(true, Ordering::Relaxed);
            cal_sum += rms;
            cal_count += 1;
            if cal_count >= CAL_FRAMES {
                let avg = cal_sum / CAL_FRAMES as f32;
                // New threshold = 2× ambient average (headroom), minimum 0.001.
                let new_threshold = (avg * 2.0).max(0.001_f32);
                silence_threshold_atomic.store(new_threshold.to_bits(), Ordering::Relaxed);
                calibrate_trigger.store(false, Ordering::Relaxed);
                calibrating.store(false, Ordering::Relaxed);
                cal_count = 0;
                cal_sum = 0.0;
            }
        }

        // ── Sleep gate: no consumers → park immediately ────────────────────────
        if no_consumers.load(Ordering::Relaxed) {
            sleeping.store(true, Ordering::Relaxed);
            while no_consumers.load(Ordering::Relaxed) || sleeping.load(Ordering::Relaxed) {
                thread::park();
            }
            silent_frame_count = 0;
            continue;
        }

        // Read the current (possibly just-calibrated) threshold.
        let silence_threshold = f32::from_bits(silence_threshold_atomic.load(Ordering::Relaxed));

        // Don't sleep while calibration is running — it needs frames to complete.
        if !calibrate_trigger.load(Ordering::Relaxed) {
            if rms < silence_threshold {
                silent_frame_count = silent_frame_count.saturating_add(1);
                if silent_frame_count >= SLEEP_FRAMES {
                    sleeping.store(true, Ordering::Relaxed);
                    // Park until the input callback or watcher wakes us.
                    while sleeping.load(Ordering::Relaxed) {
                        thread::park();
                    }
                    // Must reset here: the input callback clears `sleeping` before
                    // unparking, so the top-of-loop check sees false and won't reset.
                    silent_frame_count = 0;
                }
            } else {
                silent_frame_count = 0;
            }
        } else {
            // Calibrating — keep the counter zeroed so sleep kicks in fresh after.
            silent_frame_count = 0;
        }
    }
}
fn find_device_by_name(host: &cpal::Host, name: &str) -> Option<cpal::Device> {
    host.devices()
        .ok()?
        .find(|d| d.name().ok().as_deref() == Some(name))
}

fn run_logic() -> Result<bool> {
    let host = cpal::default_host();

    let input_device = match host.default_input_device() {
        Some(d) => d,
        None => return Ok(true), // returning true means we'll retry
    };
    let real_mic_name = input_device
        .name()
        .unwrap_or_else(|_| "Unknown".to_string());

    // Real speaker device (used for ANC only)
    // let real_speaker_device = host
    //     .default_output_device()
    //     .expect("No output device available");

    // // Virtual speaker device (processed output)
    // let virtual_speaker_device = host
    //     .default_output_device()
    //     .expect("No virtual speaker device available"); // Replace with virtual device selection
    const VIRTUAL_MIC: &str = "CABLE In 16 Ch (VB-Audio Virtual Cable)";

    let virtual_speaker_device = match find_device_by_name(&host, VIRTUAL_MIC) {
        Some(d) => d,
        None => return Ok(true),
    };
    let virtual_mic_name = virtual_speaker_device
        .name()
        .unwrap_or_else(|_| "Unknown".to_string());

    let sample_rate_hz = 48_000usize;
    let channels = 2usize;
    let frames_per_buffer = (sample_rate_hz / 100) as usize; // 10 ms

    let (tx_in, rx_in) = bounded::<Vec<f32>>(16);
    let (tx_out, rx_out) = bounded::<Vec<f32>>(16);
    let (tx_render, rx_render) = bounded::<Vec<f32>>(16);
    let (tx_metrics, rx_metrics) = bounded::<String>(2);
    let (tx_err, rx_err) = bounded::<()>(1);

    let (tx_preset, rx_preset) = bounded::<AecPreset>(4);

    // ── Sleep-mode shared state ───────────────────────────────────────────────
    let sleeping = Arc::new(AtomicBool::new(false));
    let sleeping_proc = Arc::clone(&sleeping);
    let sleeping_input = Arc::clone(&sleeping);

    // ── Consumer-watcher: poll WASAPI every 2 s ───────────────────────────────
    // Starts true (pessimistic) so we don't spin until the first poll completes.
    let no_consumers = Arc::new(AtomicBool::new(true));
    let no_consumers_proc = Arc::clone(&no_consumers);
    let no_consumers_tui = Arc::clone(&no_consumers);

    // ── Ambient calibration state ─────────────────────────────────────────────
    // Default threshold = 0.02 (~-34 dBFS). Will be overwritten after first cal.
    let default_threshold: f32 = 0.02;
    let silence_threshold_atomic = Arc::new(AtomicU32::new(default_threshold.to_bits()));
    let silence_threshold_proc = Arc::clone(&silence_threshold_atomic);
    let silence_threshold_tui = Arc::clone(&silence_threshold_atomic);

    let current_rms_atomic = Arc::new(AtomicU32::new(0u32));
    let current_rms_proc = Arc::clone(&current_rms_atomic);
    let current_rms_tui = Arc::clone(&current_rms_atomic);

    // Start true so calibration runs immediately on startup.
    let calibrate_trigger = Arc::new(AtomicBool::new(true));
    let calibrate_trigger_proc = Arc::clone(&calibrate_trigger);
    let calibrate_trigger_tui = Arc::clone(&calibrate_trigger);

    let calibrating = Arc::new(AtomicBool::new(false));
    let calibrating_proc = Arc::clone(&calibrating);
    let calibrating_tui = Arc::clone(&calibrating);

    let proc_handle = thread::spawn(move || {
        processing_thread(
            rx_in,
            rx_render,
            tx_out,
            tx_metrics,
            rx_preset,
            sample_rate_hz,
            channels,
            sleeping_proc,
            no_consumers_proc,
            current_rms_proc,
            silence_threshold_proc,
            calibrate_trigger_proc,
            calibrating_proc,
        )
    });

    // Keep the JoinHandle so the watcher can unpark the processing thread.
    // We pass only Thread (cheaply cloneable) to the watcher closure.
    let proc_thread = proc_handle.thread().clone();

    thread::spawn(move || {
        loop {
            #[cfg(windows)]
            let consumers = has_cable_output_consumers();
            #[cfg(not(windows))]
            let consumers = true; // non-Windows: assume always active

            let was_empty = no_consumers.load(Ordering::Relaxed);
            no_consumers.store(!consumers, Ordering::Relaxed);

            // If a consumer just appeared, unpark the processing thread.
            if was_empty && consumers {
                proc_thread.unpark();
            }

            thread::sleep(std::time::Duration::from_secs(2));
        }
    });

    // --- Input stream ---
    let in_config = cpal::StreamConfig {
        channels: channels as u16,
        sample_rate: cpal::SampleRate(sample_rate_hz as u32),
        buffer_size: cpal::BufferSize::Fixed(frames_per_buffer as u32),
    };
    let tx_in_clone = tx_in.clone();
    // Share threshold with the input callback so silent frames don't wake the thread.
    let silence_threshold_input = Arc::clone(&silence_threshold_atomic);
    let input_stream = input_device.build_input_stream(
        &in_config,
        move |data: &[f32], _| {
            // Only wake the sleeping processing thread when the incoming audio is
            // actually above the silence threshold. Silent frames keep being queued
            // (and dropped if the channel is full) without disturbing the sleeper.
            if sleeping_input.load(Ordering::Relaxed) {
                let threshold = f32::from_bits(silence_threshold_input.load(Ordering::Relaxed));
                let sum_sq: f32 = data.iter().map(|&s| s * s).sum();
                let rms = (sum_sq / data.len().max(1) as f32).sqrt();
                if rms > threshold {
                    sleeping_input.store(false, Ordering::Relaxed);
                    proc_handle.thread().unpark();
                }
            }
            let _ = tx_in_clone.try_send(data.to_vec());
        },
        {
            let tx_err = tx_err.clone();
            move |err| {
                eprintln!("input stream error: {:?}", err);
                let _ = tx_err.try_send(());
            }
        },
        None,
    )?;

    let render_device = match host.default_output_device() {
        Some(d) => d,
        None => return Ok(true),
    };
    let speaker_filter_name = render_device
        .name()
        .unwrap_or_else(|_| "Unknown".to_string());
    // --- Real speaker stream (ANC render) ---
    let stream_config: cpal::StreamConfig = cpal::StreamConfig {
        channels: channels as u16,
        sample_rate: cpal::SampleRate(sample_rate_hz as u32),
        buffer_size: cpal::BufferSize::Fixed(frames_per_buffer as u32),
    };
    let tx_render_clone = tx_render.clone();
    let real_output_stream = render_device.build_input_stream(
        &stream_config,
        move |out: &[f32], _: &cpal::InputCallbackInfo| {
            let _ = tx_render_clone.try_send(out.to_vec());
        },
        {
            let tx_err = tx_err.clone();
            move |err| {
                eprintln!("real output stream error: {:?}", err);
                let _ = tx_err.try_send(());
            }
        },
        None,
    )?;

    // --- Virtual speaker stream (processed mic output) ---
    let virtual_output_stream = virtual_speaker_device.build_output_stream(
        &stream_config,
        move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
            if let Ok(frame) = rx_out.try_recv() {
                let len = out.len().min(frame.len());
                out[..len].copy_from_slice(&frame[..len]);
                for s in out[len..].iter_mut() {
                    *s = 0.0;
                }
            } else {
                for s in out.iter_mut() {
                    *s = 0.0;
                }
            }
        },
        {
            let tx_err = tx_err.clone();
            move |err| {
                eprintln!("virtual output stream error: {:?}", err);
                let _ = tx_err.try_send(());
            }
        },
        None,
    )?;

    input_stream.play()?;
    real_output_stream.play()?;
    virtual_output_stream.play()?;

    // Setup TUI
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;

    let mut sys = System::new_all();
    let pid = sysinfo::get_current_pid().unwrap();
    let mut current_metrics = String::from("Waiting for AEC metrics...");
    let mut current_preset = AecPreset::Aggressive;

    let mut last_sys_refresh = std::time::Instant::now();
    let mut current_cpu_usage = 0.0;

    let mut should_restart = false;
    let app_result = (|| -> Result<()> {
        loop {
            if rx_err.try_recv().is_ok() {
                should_restart = true;
                break;
            }

            // Check for new metrics without blocking
            while let Ok(m) = rx_metrics.try_recv() {
                current_metrics = m;
            }

            if last_sys_refresh.elapsed() >= std::time::Duration::from_millis(500) {
                sys.refresh_processes_specifics(
                    sysinfo::ProcessesToUpdate::Some(&[pid]),
                    true,
                    sysinfo::ProcessRefreshKind::nothing().with_cpu(),
                );
                if let Some(process) = sys.process(pid) {
                    current_cpu_usage = process.cpu_usage();
                }
                last_sys_refresh = std::time::Instant::now();
            }

            terminal.draw(|f| {
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .margin(1)
                    .constraints(
                        [
                            Constraint::Length(3), // header
                            Constraint::Length(3), // ambient calibration bar
                            Constraint::Min(0),    // AEC metrics
                            Constraint::Length(5), // device status footer
                        ]
                        .as_ref(),
                    )
                    .split(f.area());

                // ── Ambient calibration panel ─────────────────────────────────
                let rms_now = f32::from_bits(current_rms_tui.load(Ordering::Relaxed));
                let threshold_now = f32::from_bits(silence_threshold_tui.load(Ordering::Relaxed));
                let is_calibrating = calibrating_tui.load(Ordering::Relaxed);

                // Convert to dBFS (floor at -80 dB).
                let to_db = |r: f32| if r > 0.0 { 20.0 * r.log10() } else { -80.0_f32 };
                let rms_db = to_db(rms_now);
                let thr_db = to_db(threshold_now);

                // Build a 20-character bar from -60 dB to 0 dB.
                const BAR_LEN: usize = 20;
                const DB_MIN: f32 = -60.0;
                const DB_MAX: f32 = 0.0;
                let filled = (((rms_db - DB_MIN) / (DB_MAX - DB_MIN)).clamp(0.0, 1.0)
                    * BAR_LEN as f32) as usize;
                let bar: String = "█".repeat(filled) + &"░".repeat(BAR_LEN - filled);

                let cal_status = if is_calibrating {
                    " ⏳ CALIBRATING...".to_string()
                } else {
                    format!(" Threshold: {:.1} dB  ('c' to recalibrate)", thr_db)
                };
                let ambient_text = format!(" Level: {:>6.1} dB  [{}]{}", rms_db, bar, cal_status);
                let ambient_color = if is_calibrating {
                    Color::Yellow
                } else if rms_now < threshold_now {
                    Color::DarkGray
                } else {
                    Color::Green
                };
                let ambient_panel = Paragraph::new(ambient_text)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title("Ambient Level"),
                    )
                    .style(Style::default().fg(ambient_color));
                f.render_widget(ambient_panel, chunks[1]);

                let is_sleeping = sleeping.load(Ordering::Relaxed);
                let no_consumers_now = no_consumers_tui.load(Ordering::Relaxed);
                let sleep_label = match (is_sleeping, no_consumers_now) {
                    (_, true) => "💤 NO CONSUMERS",
                    (true, _) => "💤 SILENCE SLEEP",
                    _ => "▶  ACTIVE       ",
                };
                let header_text = format!(
                    "RustDAC TUI | PID: {} | CPU: {:.2}% | AEC: {} | {} | 's' cycle 'w' wake",
                    pid,
                    current_cpu_usage,
                    current_preset.name(),
                    sleep_label,
                );
                let header_color = if is_sleeping || no_consumers_now {
                    Color::DarkGray
                } else {
                    Color::Cyan
                };
                let header = Paragraph::new(header_text)
                    .block(Block::default().borders(Borders::ALL).title("Status"))
                    .style(Style::default().fg(header_color));
                f.render_widget(header, chunks[0]);

                let metrics_paragraph = Paragraph::new(current_metrics.as_str())
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title("Live AEC Metrics"),
                    )
                    .wrap(Wrap { trim: false });
                f.render_widget(metrics_paragraph, chunks[2]);

                // Device status footer
                let footer_text = vec![
                    Line::from(vec![
                        Span::styled("  Real Mic      : ", Style::default().fg(Color::DarkGray)),
                        Span::styled(real_mic_name.as_str(), Style::default().fg(Color::Green)),
                    ]),
                    Line::from(vec![
                        Span::styled("  Speaker Filter: ", Style::default().fg(Color::DarkGray)),
                        Span::styled(
                            speaker_filter_name.as_str(),
                            Style::default().fg(Color::Yellow),
                        ),
                    ]),
                    Line::from(vec![
                        Span::styled("  Virtual Mic   : ", Style::default().fg(Color::DarkGray)),
                        Span::styled(
                            virtual_mic_name.as_str(),
                            Style::default().fg(Color::Magenta),
                        ),
                    ]),
                ];
                let footer = Paragraph::new(footer_text)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title("Active Devices"),
                    )
                    .style(Style::default());
                f.render_widget(footer, chunks[3]);
            })?;

            if event::poll(std::time::Duration::from_millis(100))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == crossterm::event::KeyEventKind::Press {
                        match key.code {
                            KeyCode::Char('q') => break,
                            KeyCode::Char('s') => {
                                current_preset = current_preset.next();
                                let _ = tx_preset.try_send(current_preset);
                            }
                            KeyCode::Char('w') => {
                                sleeping.store(false, Ordering::Relaxed);
                            }
                            // Trigger ambient calibration (2 s sample).
                            KeyCode::Char('c') => {
                                if !calibrating_tui.load(Ordering::Relaxed) {
                                    calibrate_trigger_tui.store(true, Ordering::Relaxed);
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        Ok(())
    })();

    // Restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    app_result?;

    Ok(should_restart)
}

fn main() -> Result<()> {
    #[cfg(windows)]
    enable_efficiency_mode();

    loop {
        match run_logic() {
            Ok(true) => {
                println!("Audio device changed or error occurred. Restarting in 1s...");
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            Ok(false) => {
                break; // Normal exit via 'q'
            }
            Err(e) => {
                eprintln!("Application error: {:?}. Restarting in 1s...", e);
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
    }
    Ok(())
}
