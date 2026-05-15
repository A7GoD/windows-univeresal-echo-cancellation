/// check_mic_usage.rs
///
/// A test binary that detects whether "CABLE Output (VB-Audio Virtual Cable)"
/// is currently being used as a microphone (capture device) by any external process.
///
/// This does NOT open or stream from the device — it only reads WASAPI session metadata.
///
/// Run with: cargo run --bin check_mic_usage
use anyhow::{Context, Result};
use sysinfo::{Pid, System};
use wasapi::{DeviceEnumerator, Direction, SessionState, initialize_mta};

const TARGET_MIC: &str = "CABLE Output (VB-Audio Virtual Cable)";
const POLL_INTERVAL_MS: u64 = 2000;

fn check_mic_in_use() -> Result<bool> {
    // Create device enumerator — this lets us browse all audio endpoints
    let enumerator = DeviceEnumerator::new().context("Failed to create DeviceEnumerator")?;

    // Get collection of all active capture (microphone) devices
    let collection = enumerator
        .get_device_collection(&Direction::Capture)
        .context("Failed to get capture DeviceCollection")?;

    // Find our target device by friendly name — does NOT open any audio stream
    let device = collection
        .get_device_with_name(TARGET_MIC)
        .with_context(|| format!("Device '{}' not found. Is VB-Cable installed?", TARGET_MIC))?;

    // Get the session manager — still does NOT open an audio stream
    let session_manager = device
        .get_iaudiosessionmanager()
        .context("Failed to get AudioSessionManager")?;

    // Enumerate all audio sessions currently registered on this device
    let session_enum = session_manager
        .get_audiosessionenumerator()
        .context("Failed to get AudioSessionEnumerator")?;

    let count = session_enum
        .get_count()
        .context("Failed to get session count")?;

    println!("  Found {} session(s) total on '{}'", count, TARGET_MIC);

    // Load sysinfo to resolve PID → process name
    let mut sys = System::new();
    let mut active_sessions = 0u32;

    for i in 0..count {
        let session = match session_enum.get_session(i) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("  [warn] Could not read session {}: {}", i, e);
                continue;
            }
        };

        let state = match session.get_state() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("  [warn] Could not read state for session {}: {}", i, e);
                continue;
            }
        };

        let pid = session.get_process_id().unwrap_or(0);

        // Resolve process name from PID
        let proc_name = if pid > 0 {
            sys.refresh_processes_specifics(
                sysinfo::ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
                true,
                sysinfo::ProcessRefreshKind::nothing(),
            );
            sys.process(Pid::from_u32(pid))
                .map(|p| p.name().to_string_lossy().to_string())
                .unwrap_or_else(|| format!("<unknown PID {}>", pid))
        } else {
            "<system/PID=0>".to_string()
        };

        let state_label = match state {
            SessionState::Active => {
                active_sessions += 1;
                "ACTIVE ✓"
            }
            SessionState::Inactive => "inactive",
            SessionState::Expired => "expired",
        };

        println!(
            "  Session {:2}: [{:<8}] PID={:6}  ({})",
            i, state_label, pid, proc_name
        );
    }

    Ok(active_sessions > 0)
}

fn main() -> Result<()> {
    // Initialise COM in multi-threaded apartment mode (same as the main app uses)
    // initialize_mta() returns HRESULT — S_OK (0) and S_FALSE (1) are both acceptable
    // (S_FALSE means COM was already initialized on this thread)
    let hr = initialize_mta();
    if hr.is_err() {
        anyhow::bail!("Failed to initialize COM: {:?}", hr);
    }

    println!("Monitoring '{}' for active users...", TARGET_MIC);
    println!("Press Ctrl+C to stop.\n");

    loop {
        println!("--- Checking sessions ---");

        match check_mic_in_use() {
            Ok(true) => println!("→ Microphone IS in use (at least one ACTIVE session).\n"),
            Ok(false) => println!("→ Microphone is NOT in use (no active sessions).\n"),
            Err(e) => eprintln!("→ Error: {:#}\n", e),
        }

        std::thread::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS));
    }
}
