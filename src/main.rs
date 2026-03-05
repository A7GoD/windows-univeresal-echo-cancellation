use anyhow::Result;
use crossbeam_channel::{Receiver, Sender, bounded};
use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{prelude::*, widgets::*};
use std::thread;
use sysinfo::System;

use aec3::audio_processing::audio_buffer::AudioBuffer;
use aec3::audio_processing::high_pass_filter::HighPassFilter;
use aec3::audio_processing::stream_config::StreamConfig;
use aec3::{api::EchoControl, audio_processing::aec3::echo_canceller3::EchoCanceller3};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

fn interleaved_to_channels(interleaved: &[f32], channels: usize, frames: usize) -> Vec<Vec<f32>> {
    let avail_frames = interleaved.len() / channels;
    let mut out = vec![vec![0f32; frames]; channels];
    let copy_frames = std::cmp::min(avail_frames, frames);
    for frame in 0..copy_frames {
        for ch in 0..channels {
            out[ch][frame] = interleaved[frame * channels + ch];
        }
    }
    out
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
    sample_rate: usize,
    channels: usize,
) {
    let cfg = EchoCanceller3::create_default_config(channels, channels);
    let mut aec3 = EchoCanceller3::new(cfg, sample_rate as i32, channels, channels);

    let mut audio_buf =
        AudioBuffer::from_sample_rates(sample_rate, channels, sample_rate, channels, sample_rate);
    let stream_config = StreamConfig::new(sample_rate as i32, channels, false);

    let mut last_metrics = std::time::Instant::now();
    let metrics_interval = std::time::Duration::from_millis(100);

    let mut render_buf =
        AudioBuffer::from_sample_rates(sample_rate, channels, sample_rate, channels, sample_rate);

    while let Ok(frame) = rx_in.recv() {
        // Render path (real speaker)
        // Render path (real speaker)
        let mut render_received = false;
        while let Ok(render_frame) = rx_render.try_recv() {
            render_received = true;
            let per_channel_render =
                interleaved_to_channels(&render_frame, channels, stream_config.num_frames());
            let refs_render: Vec<&[f32]> =
                per_channel_render.iter().map(|v| v.as_slice()).collect();
            render_buf.copy_from(&refs_render, &stream_config);
            render_buf.split_into_frequency_bands();
            aec3.analyze_render(&mut render_buf);
            render_buf.merge_frequency_bands();
        }

        if !render_received {
            // Feed silence to keep AEC state valid
            let silence = vec![0.0f32; stream_config.num_frames() * channels];
            let per_channel_render =
                interleaved_to_channels(&silence, channels, stream_config.num_frames());
            let refs_render: Vec<&[f32]> =
                per_channel_render.iter().map(|v| v.as_slice()).collect();
            render_buf.copy_from(&refs_render, &stream_config);
            render_buf.split_into_frequency_bands();
            aec3.analyze_render(&mut render_buf);
            render_buf.merge_frequency_bands();
        }

        // Capture path (mic)
        let per_channel = interleaved_to_channels(&frame, channels, stream_config.num_frames());
        let refs: Vec<&[f32]> = per_channel.iter().map(|v| v.as_slice()).collect();
        audio_buf.copy_from(&refs, &stream_config);

        aec3.analyze_capture(&mut audio_buf);
        audio_buf.split_into_frequency_bands();

        let mut hp_filter_channels: Vec<Vec<f32>> = (0..channels)
            .map(|ch| audio_buf.split_band(ch, 0).to_vec())
            .collect();
        let mut hp_filter = HighPassFilter::new(sample_rate as i32, channels);
        hp_filter.process(&mut hp_filter_channels);
        for ch in 0..channels {
            let dst = audio_buf.split_band_mut(ch, 0);
            dst.copy_from_slice(&hp_filter_channels[ch]);
        }

        // aec3.set_audio_buffer_delay(404); // Adjust as needed
        aec3.process_capture(&mut audio_buf, false);
        audio_buf.merge_frequency_bands();

        if last_metrics.elapsed() >= metrics_interval {
            let metrics = aec3.metrics();
            let _ = tx_metrics.try_send(format!("{:#?}", metrics));
            last_metrics = std::time::Instant::now();
        }

        // Copy processed audio to interleaved
        let mut output = vec![0f32; frame.len()];
        let mut out_mut: Vec<Vec<f32>> = vec![vec![0f32; audio_buf.num_frames()]; channels];
        let mut out_refs: Vec<&mut [f32]> = out_mut.iter_mut().map(|v| v.as_mut_slice()).collect();
        audio_buf.copy_to_stream(&stream_config, &mut out_refs);
        let mut out_refs_immut: Vec<&[f32]> = out_refs.iter().map(|r| &**r).collect();
        channels_to_interleaved(&mut out_refs_immut, &mut output);

        // Apply output gain
        const GAIN: f32 = 5.0;
        let mut max_amp = 0.0f32;
        for sample in output.iter_mut() {
            *sample *= GAIN;
            if sample.abs() > max_amp {
                max_amp = sample.abs();
            }
        }

        let _ = tx_out.try_send(output.clone());
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

    let sample_rate_hz = 48_000;
    let channels = 2usize;
    let frames_per_buffer = (sample_rate_hz / 100) as usize; // 10 ms

    let (tx_in, rx_in) = bounded::<Vec<f32>>(16);
    let (tx_out, rx_out) = bounded::<Vec<f32>>(16);
    let (tx_render, rx_render) = bounded::<Vec<f32>>(16);
    let (tx_metrics, rx_metrics) = bounded::<String>(2);
    let (tx_err, rx_err) = bounded::<()>(1);

    thread::spawn(move || {
        processing_thread(
            rx_in,
            rx_render,
            tx_out,
            tx_metrics,
            sample_rate_hz,
            channels,
        )
    });

    // --- Input stream ---
    let in_config = cpal::StreamConfig {
        channels: channels as u16,
        sample_rate: cpal::SampleRate(sample_rate_hz as u32),
        buffer_size: cpal::BufferSize::Fixed(frames_per_buffer as u32),
    };
    let tx_in_clone = tx_in.clone();
    let input_stream = input_device.build_input_stream(
        &in_config,
        move |data: &[f32], _| {
            let vec = data.to_vec();
            let _ = tx_in_clone.try_send(vec);
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
                    .constraints([Constraint::Length(3), Constraint::Min(0)].as_ref())
                    .split(f.area());

                let header_text = format!(
                    "RustDAC TUI | PID: {} | Process CPU Usage: {:.2}%",
                    pid, current_cpu_usage
                );
                let header = Paragraph::new(header_text)
                    .block(Block::default().borders(Borders::ALL).title("Status"))
                    .style(Style::default().fg(Color::Cyan));
                f.render_widget(header, chunks[0]);

                let metrics_paragraph = Paragraph::new(current_metrics.as_str())
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title("Live AEC Metrics"),
                    )
                    .wrap(Wrap { trim: false });
                f.render_widget(metrics_paragraph, chunks[1]);
            })?;

            if event::poll(std::time::Duration::from_millis(100))? {
                if let Event::Key(key) = event::read()? {
                    if key.code == KeyCode::Char('q') {
                        break;
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
