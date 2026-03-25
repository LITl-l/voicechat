use anyhow::Result;
use clap::{Parser, Subcommand};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "voicechat", about = "Ultra-low-latency voice chat")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Host a voice chat session (rendezvous peer)
    Host {
        /// Bind address (ip:port)
        #[arg(short, long, default_value = "0.0.0.0:4567")]
        bind: SocketAddr,

        /// Session passphrase for encryption
        #[arg(short, long)]
        passphrase: String,

        /// Input audio device name (default: system default)
        #[arg(long)]
        input_device: Option<String>,

        /// Output audio device name (default: system default)
        #[arg(long)]
        output_device: Option<String>,
    },

    /// Join an existing voice chat session
    Join {
        /// Host address to connect to (ip:port)
        host: SocketAddr,

        /// Bind address (ip:port)
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
    },

    /// Run a local loopback test (capture → encode → decode → playout)
    Loopback {
        /// Input audio device name
        #[arg(long)]
        input_device: Option<String>,

        /// Output audio device name
        #[arg(long)]
        output_device: Option<String>,
    },

    /// List available audio devices
    Devices,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let cli = Cli::parse();
    let running = Arc::new(AtomicBool::new(true));

    // Ctrl+C handler
    let r = running.clone();
    ctrlc_handler(r);

    match cli.command {
        Command::Host {
            bind,
            passphrase,
            input_device,
            output_device,
        } => {
            log::info!("Hosting voice chat on {bind}");
            let session = vc_core::Session::new(vc_core::SessionConfig {
                bind_addr: bind,
                passphrase,
                is_host: true,
                host_addr: None,
                input_device,
                output_device,
            });
            session.run()?;
        }

        Command::Join {
            host,
            bind,
            passphrase,
            input_device,
            output_device,
        } => {
            log::info!("Joining voice chat at {host}");
            let session = vc_core::Session::new(vc_core::SessionConfig {
                bind_addr: bind,
                passphrase,
                is_host: false,
                host_addr: Some(host),
                input_device,
                output_device,
            });
            session.run()?;
        }

        Command::Loopback {
            input_device,
            output_device,
        } => {
            log::info!("Running loopback test");
            vc_core::run_loopback(
                input_device.as_deref(),
                output_device.as_deref(),
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

fn ctrlc_handler(running: Arc<AtomicBool>) {
    let _ = std::thread::spawn(move || {
        // Simple signal handling: read from stdin or use platform-specific
        // For portability, we just set a flag. The main loop checks it.
        // A proper implementation would use the `ctrlc` crate.
        loop {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if !running.load(Ordering::Relaxed) {
                break;
            }
        }
    });
}
