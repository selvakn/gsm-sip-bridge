use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};
use clap::Parser;
use gsm_sip_bridge::modules::at_commander::{AtCommander, AtResponse};
use gsm_sip_bridge::modules::discovery;
use gsm_sip_bridge::observability::logging;
use libc;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

#[derive(Parser, Debug)]
#[command(
    name = "gsm-echo",
    version,
    about = "Debug tool: single-card GSM audio loopback (no SIP). \
             Answers incoming calls and loops captured audio back to the caller."
)]
struct Cli {
    #[arg(short = 's', long = "serial", requires = "audio")]
    serial: Option<PathBuf>,

    #[arg(short = 'a', long = "audio", requires = "serial")]
    audio: Option<String>,

    #[arg(short = 'v', long = "verbose")]
    verbose: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    logging::init(cli.verbose);

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "starting gsm-echo (GSM audio loopback)"
    );

    let (serial_port, audio_device) = match resolve_module(&cli) {
        Ok(pair) => pair,
        Err(e) => {
            tracing::error!(error = %e, "module resolution failed");
            return ExitCode::from(1);
        }
    };

    tracing::info!(
        serial = %serial_port.display(),
        audio = %audio_device,
        "using module"
    );

    let mut at = match AtCommander::open(&serial_port) {
        Ok(a) => a,
        Err(e) => {
            tracing::error!(error = %e, "failed to open serial port");
            return ExitCode::from(1);
        }
    };

    if let Err(e) = probe_module(&mut at) {
        tracing::error!(error = %e, "module probe failed");
        return ExitCode::from(1);
    }

    configure_module(&mut at);

    if let Ok((rssi, ber)) = at.check_signal() {
        tracing::info!(rssi, ber, "signal quality");
    }

    tracing::info!(
        "monitoring for incoming calls (Ctrl+C to quit). \
         Audio loopback via ALSA on: {audio_device}"
    );

    let exit = run_event_loop(&mut at, &audio_device);

    tracing::info!("gsm-echo stopped");
    exit
}

fn resolve_module(cli: &Cli) -> Result<(PathBuf, String), String> {
    if let (Some(serial), Some(audio)) = (&cli.serial, &cli.audio) {
        return Ok((serial.clone(), audio.clone()));
    }

    let modules = discovery::scan_modules().map_err(|e| e.to_string())?;
    let module = modules
        .into_iter()
        .find(|m| !m.audio_device.is_empty())
        .ok_or_else(|| {
            "no EC20 module with audio device found. Use --serial and --audio to specify manually."
                .to_string()
        })?;

    tracing::info!(
        module_id = %module.id,
        usb_serial = %module.usb_serial,
        "auto-discovered module"
    );
    Ok((module.serial_port, module.audio_device))
}

fn probe_module(at: &mut AtCommander) -> Result<(), String> {
    match at.send_command("AT") {
        Ok(AtResponse::Ok(_)) => {
            tracing::info!("AT probe OK");
            Ok(())
        }
        Ok(AtResponse::Error(e)) => Err(format!("AT probe returned ERROR: {e}")),
        Ok(AtResponse::CmeError(code, msg)) => {
            Err(format!("AT probe returned +CME ERROR {code}: {msg}"))
        }
        Err(e) => Err(format!("AT probe failed: {e}")),
    }
}

fn configure_module(at: &mut AtCommander) {
    at.send_command("ATE0").ok();
    at.send_command("AT+CLIP=1").ok();
    route_audio_to_usb(at);
}

fn route_audio_to_usb(at: &mut AtCommander) {
    match at.send_command("AT+QPCMV=1,2") {
        Ok(AtResponse::Ok(_)) => {
            tracing::info!("voice audio routed to USB (AT+QPCMV=1,2)");
        }
        _ => {
            tracing::warn!("AT+QPCMV=1,2 failed, trying AT+QPCMV=1,0");
            match at.send_command("AT+QPCMV=1,0") {
                Ok(AtResponse::Ok(_)) => {
                    tracing::info!("voice audio routed to USB (AT+QPCMV=1,0)");
                }
                _ => {
                    tracing::error!("failed to route voice audio to USB — audio will not work");
                }
            }
        }
    }
}

fn run_event_loop(at: &mut AtCommander, audio_device: &str) -> ExitCode {
    let mut in_call = false;
    let mut loopback: Option<AlsaLoopback> = None;

    loop {
        let line = match at.read_line_raw() {
            Ok(l) => l,
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("timeout") || msg.contains("TimedOut") {
                    continue;
                }
                tracing::error!(error = %msg, "serial read error");
                return ExitCode::from(1);
            }
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        tracing::debug!(urc = trimmed, "received");

        if trimmed == "RING" && !in_call {
            let volte = is_volte_call(at);
            tracing::info!(volte, "incoming call (RING), answering...");
            let caller_id = extract_clip(at);
            if !caller_id.is_empty() {
                tracing::info!(caller = %caller_id, "caller ID");
            }

            match at.answer_call() {
                Ok(()) => {
                    in_call = true;
                    match AlsaLoopback::start(audio_device) {
                        Ok(lb) => {
                            loopback = Some(lb);
                            tracing::info!(volte, "call answered — ALSA audio loopback active");
                        }
                        Err(e) => {
                            tracing::error!(error = %e, volte, "call answered but ALSA loopback failed");
                        }
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "failed to answer call");
                }
            }
        } else if (trimmed == "NO CARRIER" || trimmed == "BUSY" || trimmed == "NO ANSWER")
            && in_call
        {
            if let Some(lb) = loopback.take() {
                lb.stop();
            }
            in_call = false;
            tracing::info!("call ended ({trimmed}), returning to idle");
        }
    }
}

const SAMPLE_RATE: u32 = 8000;
const CHANNELS: u32 = 1;
const PERIOD_FRAMES: u32 = 160; // 20ms at 8kHz

struct AlsaLoopback {
    running: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl AlsaLoopback {
    fn start(device: &str) -> Result<Self, String> {
        let running = Arc::new(AtomicBool::new(true));
        let flag = running.clone();
        let dev = device.to_string();

        let handle = thread::Builder::new()
            .name("alsa-loopback".into())
            .spawn(move || {
                if let Err(e) = run_loopback(&dev, &flag) {
                    tracing::error!(error = %e, "ALSA loopback thread exited with error");
                }
            })
            .map_err(|e| format!("failed to spawn loopback thread: {e}"))?;

        Ok(Self {
            running,
            handle: Some(handle),
        })
    }

    fn stop(mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        tracing::info!("ALSA loopback stopped");
    }
}

impl Drop for AlsaLoopback {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn configure_pcm(pcm: &PCM, label: &str) -> Result<(), String> {
    let hwp = HwParams::any(pcm).map_err(|e| format!("{label}: HwParams::any: {e}"))?;
    hwp.set_access(Access::RWInterleaved)
        .map_err(|e| format!("{label}: set_access: {e}"))?;
    hwp.set_format(Format::s16())
        .map_err(|e| format!("{label}: set_format: {e}"))?;
    hwp.set_channels(CHANNELS)
        .map_err(|e| format!("{label}: set_channels: {e}"))?;
    hwp.set_rate(SAMPLE_RATE, ValueOr::Nearest)
        .map_err(|e| format!("{label}: set_rate: {e}"))?;
    hwp.set_period_size(PERIOD_FRAMES as alsa::pcm::Frames, ValueOr::Nearest)
        .map_err(|e| format!("{label}: set_period_size: {e}"))?;
    hwp.set_buffer_size((PERIOD_FRAMES * 4) as alsa::pcm::Frames)
        .map_err(|e| format!("{label}: set_buffer_size: {e}"))?;
    pcm.hw_params(&hwp)
        .map_err(|e| format!("{label}: hw_params apply: {e}"))?;
    Ok(())
}

fn run_loopback(device: &str, running: &AtomicBool) -> Result<(), String> {
    let capture = PCM::new(device, Direction::Capture, false)
        .map_err(|e| format!("ALSA capture open '{device}': {e}"))?;
    let playback = PCM::new(device, Direction::Playback, false)
        .map_err(|e| format!("ALSA playback open '{device}': {e}"))?;

    configure_pcm(&capture, "capture")?;
    configure_pcm(&playback, "playback")?;

    let actual_rate = capture
        .hw_params_current()
        .and_then(|h| h.get_rate())
        .unwrap_or(SAMPLE_RATE);
    let actual_period = capture
        .hw_params_current()
        .and_then(|h| h.get_period_size())
        .unwrap_or(PERIOD_FRAMES as alsa::pcm::Frames);

    tracing::info!(
        rate = actual_rate,
        period = actual_period,
        "ALSA loopback running on {device}"
    );

    let period = actual_period as usize;
    let mut buf = vec![0i16; period * CHANNELS as usize];

    // Pre-fill playback with silence to prevent initial underrun
    let silence = vec![0i16; period * CHANNELS as usize];
    let io_play = playback.io_i16().map_err(|e| format!("playback io: {e}"))?;
    for _ in 0..3 {
        let _ = io_play.writei(&silence);
    }

    let io_cap = capture.io_i16().map_err(|e| format!("capture io: {e}"))?;

    while running.load(Ordering::SeqCst) {
        match io_cap.readi(&mut buf) {
            Ok(_frames) => {}
            Err(e) if e.errno() == libc::EPIPE => {
                tracing::warn!("capture overrun, recovering");
                let _ = capture.prepare();
                continue;
            }
            Err(e) if e.errno() == libc::EINTR => continue,
            Err(e) => {
                tracing::error!(error = %e, "ALSA read error");
                break;
            }
        }

        match io_play.writei(&buf) {
            Ok(_) => {}
            Err(e) if e.errno() == libc::EPIPE => {
                tracing::warn!("playback underrun, recovering");
                let _ = playback.prepare();
                for _ in 0..2 {
                    let _ = io_play.writei(&silence);
                }
                let _ = io_play.writei(&buf);
            }
            Err(e) if e.errno() == libc::EINTR => {}
            Err(e) => {
                tracing::error!(error = %e, "ALSA write error");
                break;
            }
        }
    }

    Ok(())
}

fn is_volte_call(at: &mut AtCommander) -> bool {
    match at.send_command("AT+QNWINFO") {
        Ok(AtResponse::Ok(lines)) => {
            for line in &lines {
                if let Some(info) = line.strip_prefix("+QNWINFO:") {
                    let rat = info.split(',').next().unwrap_or("").trim().trim_matches('"');
                    let is_lte = rat.contains("LTE");
                    tracing::info!(rat, lte = is_lte, "network RAT");
                    return is_lte;
                }
            }
            false
        }
        _ => {
            tracing::warn!("failed to query network info for VoLTE detection");
            false
        }
    }
}

fn extract_clip(at: &mut AtCommander) -> String {
    for _ in 0..5 {
        match at.read_line_raw() {
            Ok(line) => {
                let trimmed = line.trim();
                if let Some(clip_data) = trimmed.strip_prefix("+CLIP:") {
                    if let Some(number) = clip_data.split(',').next() {
                        return number.trim().trim_matches('"').to_string();
                    }
                }
                if trimmed == "RING" || trimmed.is_empty() {
                    continue;
                }
            }
            Err(_) => break,
        }
    }
    String::new()
}
