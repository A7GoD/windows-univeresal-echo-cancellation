# Project Specification: RustDAC AEC System

## 1. Overview
This project implements a real-time Audio Echo Canceller (AEC) system using Rust. It captures audio from an input device, processes the signal through an AEC algorithm (`aec3`), applies digital filtering (High Pass Filter), and outputs the processed audio to a virtual speaker device. The application runs concurrently with a Terminal User Interface (TUI) that displays real-time metrics and system performance data.

## 2. Core Functionality
The primary function is to provide bidirectional, low-latency audio processing:
1.  **Capture:** Receive raw audio frames from the default microphone input device.
2.  **Processing Pipeline:** Feed captured audio into an AEC algorithm for echo cancellation. Simultaneously, receive synthesized/rendered audio (e.g., playback signal) and feed it to the AEC's analysis stage.
3.  **Filtering & Output:** Apply a High Pass Filter to the processed capture signal before outputting the final, cleaned audio stream to a specified virtual speaker device.
4.  **Monitoring:** Display system metrics (CPU usage of the process) and internal AEC performance statistics in a TUI.

## 3. Technical Components & Dependencies
| Component | Library/Module | Purpose |
| :--- | :--- | :--- |
| **Audio I/O** | `cpal` | Handles interaction with system audio hardware (Input/Output streams). |
| **AEC Engine** | `aec3` | Core Digital Signal Processing (DSP) for Echo Cancellation. |
| **Filtering** | Custom (`HighPassFilter`) | Applies frequency domain filtering to the captured signal. |
| **Concurrency** | `crossbeam_channel` | Manages safe, asynchronous data transfer between threads. |
| **User Interface** | `ratatui`, `crossterm` | Renders a responsive TUI in the terminal. |
| **System Monitoring** | `sysinfo` | Retrieves real-time CPU usage of the running process. |

## 4. Data Flow Diagram (Conceptual)

```mermaid
graph TD
    A[Microphone Input Device] -->|Raw Audio Frames| B(Input Stream Callback);
    B --> C{Channel Buffer};
    C --> D[Processing Thread];
    E[Real Speaker Output] -->|Rendered Audio Frames| F(Render Stream Callback);
    F --> G{Channel Buffer};

    D -- Capture Data --> H[AEC Analysis (Capture)];
    G -- Render Data --> I[AEC Analysis (Render)];

    H & I --> J[DSP Pipeline];
    J --> K[High Pass Filter];
    K --> L[AEC Processing (Output)];
    L --> M{Channel Buffer};
    M --> N(Virtual Speaker Output Stream);

    D -- Metrics/State --> O[Metrics Channel];
    O --> P[Main Thread TUI Loop];
    P --> Q[System Info Polling];
