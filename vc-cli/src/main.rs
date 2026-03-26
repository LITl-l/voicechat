use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "voicechat", about = "Ultra-low-latency voice chat")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, ValueEnum)]
enum InputModeArg {
    /// Always transmitting
    AlwaysOn,
    /// Push-to-talk (hold space in terminal)
    Ptt,
    /// Voice activation (transmit when speech detected)
    Vox,
}

#[derive(Subcommand)]
enum Command {
    /// Host a voice chat session (rendezvous peer)
    Host {
        #[arg(short, long, default_value = "0.0.0.0:4567")]
        bind: SocketAddr,

        /// Session passphrase for encryption
        #[arg(short, long)]
        passphrase: String,

        /// Input audio device name
        #[arg(long)]
        input_device: Option<String>,

        /// Output audio device name
        #[arg(long)]
        output_device: Option<String>,

        /// Enable RNNoise noise suppression
        #[arg(long, default_value_t = false)]
        denoise: bool,

        /// Input mode: always-on, ptt, or vox
        #[arg(long, value_enum, default_value_t = InputModeArg::AlwaysOn)]
        input_mode: InputModeArg,
    },

    /// Join an existing voice chat session
    Join {
        /// Host address (ip:port)
        host: SocketAddr,

        #[arg(short, long, default_value = "0.0.0.0:4568")]
        bind: SocketAddr,

        /// Session passphrase for encryption
        #[arg(short, long)]
        passphrase: String,

        /// Input audio device name
        #[arg(long)]
        input_device: Option<String>,

        /// Output audio device name
        #[arg(long)]
        output_device: Option<String>,

        /// Enable RNNoise noise suppression
        #[arg(long, default_value_t = false)]
        denoise: bool,

        /// Input mode: always-on, ptt, or vox
        #[arg(long, value_enum, default_value_t = InputModeArg::AlwaysOn)]
        input_mode: InputModeArg,
    },

    /// Run a local loopback test (capture -> encode -> decode -> playout)
    Loopback {
        /// Input audio device name
        #[arg(long)]
        input_device: Option<String>,

        /// Output audio device name
        #[arg(long)]
        output_device: Option<String>,

        /// Enable RNNoise noise suppression
        #[arg(long, default_value_t = false)]
        denoise: bool,
    },

    /// List available audio devices
    Devices,
}

fn to_input_mode(arg: &InputModeArg) -> vc_core::input::InputMode {
    match arg {
        InputModeArg::AlwaysOn => vc_core::input::InputMode::AlwaysOn,
        InputModeArg::Ptt => vc_core::input::InputMode::PushToTalk,
        InputModeArg::Vox => vc_core::input::InputMode::VoiceActivation,
    }
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let cli = Cli::parse();
    let running = Arc::new(AtomicBool::new(true));

    // Proper Ctrl+C handling
    let r = running.clone();
    ctrlc::set_handler(move || {
        log::info!("Ctrl+C received, shutting down...");
        r.store(false, Ordering::SeqCst);
    })?;

    match cli.command {
        Command::Host {
            bind,
            passphrase,
            input_device,
            output_device,
            denoise,
            input_mode,
        } => {
            log::info!("Hosting voice chat on {bind}");
            let mode = to_input_mode(&input_mode);
            let session = vc_core::Session::new(vc_core::SessionConfig {
                bind_addr: bind,
                passphrase,
                is_host: true,
                host_addr: None,
                input_device,
                output_device,
                noise_suppression: denoise,
                input_mode: mode.clone(),
                vad_config: vc_core::vad::VadConfig::default(),
            });

            // For PTT mode, spawn a thread to watch for space key in terminal
            if mode == vc_core::input::InputMode::PushToTalk {
                let shared = session.shared();
                spawn_ptt_listener(shared, running.clone());
            }

            // Print session info
            let shared = session.shared();
            let run_flag = session.running_flag();
            spawn_stats_printer(shared, run_flag);

            session.run()?;
        }

        Command::Join {
            host,
            bind,
            passphrase,
            input_device,
            output_device,
            denoise,
            input_mode,
        } => {
            log::info!("Joining voice chat at {host}");
            let mode = to_input_mode(&input_mode);
            let session = vc_core::Session::new(vc_core::SessionConfig {
                bind_addr: bind,
                passphrase,
                is_host: false,
                host_addr: Some(host),
                input_device,
                output_device,
                noise_suppression: denoise,
                input_mode: mode.clone(),
                vad_config: vc_core::vad::VadConfig::default(),
            });

            if mode == vc_core::input::InputMode::PushToTalk {
                let shared = session.shared();
                spawn_ptt_listener(shared, running.clone());
            }

            let shared = session.shared();
            let run_flag = session.running_flag();
            spawn_stats_printer(shared, run_flag);

            session.run()?;
        }

        Command::Loopback {
            input_device,
            output_device,
            denoise,
        } => {
            log::info!("Running loopback test");
            vc_core::run_loopback(
                input_device.as_deref(),
                output_device.as_deref(),
                denoise,
                running,
            )?;
        }

        Command::Devices => {
            println!("Input devices:");
            for name in vc_core::audio::list_input_devices()? {
                println!("  {name}");
            }
            println!("\nOutput devices:");
            for name in vc_core::audio::list_output_devices()? {
                println!("  {name}");
            }
        }
    }

    Ok(())
}

/// Spawn a background thread that prints peer latency stats every 5 seconds.
fn spawn_stats_printer(shared: Arc<vc_core::SessionShared>, running: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        // Wait for session to actually start
        std::thread::sleep(std::time::Duration::from_secs(3));

        while running.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_secs(5));
            if let Ok(peers) = shared.peer_info.lock() {
                if peers.is_empty() {
                    continue;
                }
                for p in peers.iter() {
                    let state = format!("{:?}", p.state);
                    let lat = &p.latency;
                    let pfs = if p.key_exchange_done { "PFS" } else { "PSK" };
                    if lat.sample_count > 0 {
                        log::info!(
                            "Peer {} ({}): {} | RTT {:.1}/{:.1}/{:.1}ms | jitter {:.1}ms | {}",
                            p.id,
                            p.addr,
                            state,
                            lat.min_rtt_us as f64 / 1000.0,
                            lat.avg_rtt_us as f64 / 1000.0,
                            lat.max_rtt_us as f64 / 1000.0,
                            lat.jitter_us as f64 / 1000.0,
                            pfs,
                        );
                    } else {
                        log::info!(
                            "Peer {} ({}): {} | no latency data | {}",
                            p.id,
                            p.addr,
                            state,
                            pfs
                        );
                    }
                }
            }
        }
    });
}

/// Spawn a thread that reads stdin for PTT (space key toggles).
fn spawn_ptt_listener(shared: Arc<vc_core::SessionShared>, running: Arc<AtomicBool>) {
    log::info!("PTT mode: press Enter to toggle transmit");
    std::thread::spawn(move || {
        let mut transmitting = false;
        let stdin = std::io::stdin();
        while running.load(Ordering::Relaxed) {
            let mut line = String::new();
            if stdin.read_line(&mut line).is_ok() {
                transmitting = !transmitting;
                shared.ptt_active.store(transmitting, Ordering::Relaxed);
                if transmitting {
                    log::info!("PTT: transmitting (press Enter to stop)");
                } else {
                    log::info!("PTT: muted (press Enter to transmit)");
                }
            }
        }
    });
}
