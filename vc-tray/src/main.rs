use anyhow::Result;
use eframe::egui;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use vc_core::input::InputMode;
use vc_core::peer::PeerState;
use vc_core::{PeerDisplayInfo, Session, SessionConfig, SessionShared};

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([420.0, 500.0])
            .with_min_inner_size([360.0, 400.0]),
        ..Default::default()
    };

    eframe::run_native(
        "VoiceChat",
        options,
        Box::new(|_cc| Ok(Box::new(VoiceChatApp::new()))),
    )
    .map_err(|e| anyhow::anyhow!("eframe error: {e}"))?;

    Ok(())
}

enum AppState {
    /// Lobby screen: configure and connect.
    Lobby,
    /// Active session.
    Connected,
}

struct LobbyConfig {
    is_host: bool,
    bind_addr: String,
    host_addr: String,
    passphrase: String,
    denoise: bool,
    input_mode_idx: usize,
    input_devices: Vec<String>,
    output_devices: Vec<String>,
    /// 0 = Default, 1..N = specific device (index into input_devices + 1)
    selected_input: usize,
    /// 0 = Default, 1..N = specific device (index into output_devices + 1)
    selected_output: usize,
}

struct VoiceChatApp {
    state: AppState,
    lobby: LobbyConfig,
    session_shared: Option<Arc<SessionShared>>,
    running: Arc<AtomicBool>,
    error_msg: Option<String>,
}

impl VoiceChatApp {
    fn new() -> Self {
        Self {
            state: AppState::Lobby,
            lobby: LobbyConfig {
                is_host: true,
                bind_addr: "0.0.0.0:4567".to_string(),
                host_addr: "127.0.0.1:4567".to_string(),
                passphrase: String::new(),
                denoise: false,
                input_mode_idx: 0,
                input_devices: vc_core::audio::list_input_devices().unwrap_or_default(),
                output_devices: vc_core::audio::list_output_devices().unwrap_or_default(),
                selected_input: 0,
                selected_output: 0,
            },
            session_shared: None,
            running: Arc::new(AtomicBool::new(false)),
            error_msg: None,
        }
    }

    fn start_session(&mut self) -> Result<()> {
        let bind_addr: SocketAddr = self.lobby.bind_addr.parse()?;
        let host_addr: Option<SocketAddr> = if self.lobby.is_host {
            None
        } else {
            Some(self.lobby.host_addr.parse()?)
        };

        let input_mode = match self.lobby.input_mode_idx {
            1 => InputMode::PushToTalk,
            2 => InputMode::VoiceActivation,
            _ => InputMode::AlwaysOn,
        };

        let input_device = if self.lobby.selected_input > 0 {
            self.lobby
                .input_devices
                .get(self.lobby.selected_input - 1)
                .cloned()
        } else {
            None
        };
        let output_device = if self.lobby.selected_output > 0 {
            self.lobby
                .output_devices
                .get(self.lobby.selected_output - 1)
                .cloned()
        } else {
            None
        };

        let config = SessionConfig {
            bind_addr,
            passphrase: self.lobby.passphrase.clone(),
            is_host: self.lobby.is_host,
            host_addr,
            input_device,
            output_device,
            noise_suppression: self.lobby.denoise,
            input_mode,
            vad_config: vc_core::vad::VadConfig::default(),
        };

        let session = Session::new(config);
        self.session_shared = Some(session.shared());
        self.running = session.running_flag();

        // Run session in background thread
        let running = self.running.clone();
        std::thread::spawn(move || {
            if let Err(e) = session.run() {
                log::error!("Session error: {e}");
            }
            running.store(false, Ordering::SeqCst);
        });

        self.state = AppState::Connected;
        Ok(())
    }

    fn disconnect(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        self.session_shared = None;
        self.state = AppState::Lobby;
    }

    fn draw_lobby(&mut self, ui: &mut egui::Ui) {
        ui.heading("VoiceChat");
        ui.separator();

        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.lobby.is_host, true, "Host");
            ui.selectable_value(&mut self.lobby.is_host, false, "Join");
        });

        ui.add_space(8.0);

        egui::Grid::new("lobby_grid")
            .num_columns(2)
            .spacing([10.0, 6.0])
            .show(ui, |ui| {
                ui.label("Bind address:");
                ui.text_edit_singleline(&mut self.lobby.bind_addr);
                ui.end_row();

                if !self.lobby.is_host {
                    ui.label("Host address:");
                    ui.text_edit_singleline(&mut self.lobby.host_addr);
                    ui.end_row();
                }

                ui.label("Passphrase:");
                ui.add(egui::TextEdit::singleline(&mut self.lobby.passphrase).password(true));
                ui.end_row();

                ui.label("Noise suppression:");
                ui.checkbox(&mut self.lobby.denoise, "Enable RNNoise");
                ui.end_row();

                ui.label("Input mode:");
                egui::ComboBox::from_id_salt("input_mode")
                    .selected_text(match self.lobby.input_mode_idx {
                        1 => "Push-to-Talk",
                        2 => "Voice Activation",
                        _ => "Always On",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.lobby.input_mode_idx, 0, "Always On");
                        ui.selectable_value(&mut self.lobby.input_mode_idx, 1, "Push-to-Talk");
                        ui.selectable_value(&mut self.lobby.input_mode_idx, 2, "Voice Activation");
                    });
                ui.end_row();

                ui.label("Input device:");
                let input_label = if self.lobby.selected_input == 0 {
                    "Default".to_string()
                } else {
                    self.lobby
                        .input_devices
                        .get(self.lobby.selected_input - 1)
                        .cloned()
                        .unwrap_or_else(|| "Default".to_string())
                };
                egui::ComboBox::from_id_salt("input_device")
                    .selected_text(&input_label)
                    .width(200.0)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.lobby.selected_input, 0, "Default");
                        for (i, name) in self.lobby.input_devices.iter().enumerate() {
                            ui.selectable_value(&mut self.lobby.selected_input, i + 1, name);
                        }
                    });
                ui.end_row();

                ui.label("Output device:");
                let output_label = if self.lobby.selected_output == 0 {
                    "Default".to_string()
                } else {
                    self.lobby
                        .output_devices
                        .get(self.lobby.selected_output - 1)
                        .cloned()
                        .unwrap_or_else(|| "Default".to_string())
                };
                egui::ComboBox::from_id_salt("output_device")
                    .selected_text(&output_label)
                    .width(200.0)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.lobby.selected_output, 0, "Default");
                        for (i, name) in self.lobby.output_devices.iter().enumerate() {
                            ui.selectable_value(&mut self.lobby.selected_output, i + 1, name);
                        }
                    });
                ui.end_row();
            });

        if ui.small_button("Refresh devices").clicked() {
            self.lobby.input_devices = vc_core::audio::list_input_devices().unwrap_or_default();
            self.lobby.output_devices = vc_core::audio::list_output_devices().unwrap_or_default();
            self.lobby.selected_input = 0;
            self.lobby.selected_output = 0;
        }

        ui.add_space(12.0);

        let can_connect = !self.lobby.passphrase.is_empty();
        if ui
            .add_enabled(
                can_connect,
                egui::Button::new(if self.lobby.is_host {
                    "Start Hosting"
                } else {
                    "Connect"
                }),
            )
            .clicked()
        {
            match self.start_session() {
                Ok(()) => self.error_msg = None,
                Err(e) => self.error_msg = Some(format!("Failed to start: {e}")),
            }
        }

        if let Some(ref err) = self.error_msg {
            ui.colored_label(egui::Color32::RED, err);
        }
    }

    fn draw_connected(&mut self, ui: &mut egui::Ui) {
        let shared = match &self.session_shared {
            Some(s) => s.clone(),
            None => {
                self.state = AppState::Lobby;
                return;
            }
        };

        // Check if session is still running
        if !self.running.load(Ordering::Relaxed) {
            self.disconnect();
            return;
        }

        ui.heading("VoiceChat - Connected");
        ui.separator();

        // Controls
        ui.horizontal(|ui| {
            let mut ns = shared.noise_suppression.load(Ordering::Relaxed);
            if ui.checkbox(&mut ns, "Noise Suppression").changed() {
                shared.noise_suppression.store(ns, Ordering::Relaxed);
            }

            let is_tx = shared.is_transmitting.load(Ordering::Relaxed);
            let tx_label = if is_tx { "TX: ON" } else { "TX: off" };
            let tx_color = if is_tx {
                egui::Color32::GREEN
            } else {
                egui::Color32::GRAY
            };
            ui.colored_label(tx_color, tx_label);
        });

        // Input mode
        ui.horizontal(|ui| {
            ui.label("Input:");
            if let Ok(mut mode) = shared.input_mode.lock() {
                let mut idx = match *mode {
                    InputMode::AlwaysOn => 0,
                    InputMode::PushToTalk => 1,
                    InputMode::VoiceActivation => 2,
                };
                let changed = egui::ComboBox::from_id_salt("mode_active")
                    .selected_text(match idx {
                        1 => "PTT",
                        2 => "VOX",
                        _ => "Always On",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut idx, 0, "Always On");
                        ui.selectable_value(&mut idx, 1, "PTT");
                        ui.selectable_value(&mut idx, 2, "VOX");
                    })
                    .response
                    .changed();
                if changed {
                    *mode = match idx {
                        1 => InputMode::PushToTalk,
                        2 => InputMode::VoiceActivation,
                        _ => InputMode::AlwaysOn,
                    };
                }
            }

            // PTT button
            if let Ok(mode) = shared.input_mode.lock() {
                if *mode == InputMode::PushToTalk {
                    let ptt = shared.ptt_active.load(Ordering::Relaxed);
                    let btn = ui.button(if ptt { "Release" } else { "Push to Talk" });
                    if btn.clicked() {
                        shared.ptt_active.store(!ptt, Ordering::Relaxed);
                    }
                }
            }
        });

        ui.add_space(8.0);

        // Peer list
        ui.heading("Peers");
        ui.separator();

        if let Ok(peers) = shared.peer_info.lock() {
            if peers.is_empty() {
                ui.label("No peers connected");
            } else {
                egui::Grid::new("peer_grid")
                    .num_columns(5)
                    .spacing([10.0, 4.0])
                    .striped(true)
                    .show(ui, |ui| {
                        ui.strong("ID");
                        ui.strong("Address");
                        ui.strong("State");
                        ui.strong("Latency");
                        ui.strong("Security");
                        ui.end_row();

                        for p in peers.iter() {
                            ui.label(format!("{}", p.id));
                            ui.label(format!("{}", p.addr));

                            let (state_str, state_color) = format_peer_state(p.state);
                            ui.colored_label(state_color, state_str);

                            ui.label(format_latency(p));
                            ui.label(if p.key_exchange_done { "PFS" } else { "PSK" });
                            ui.end_row();
                        }
                    });
            }
        }

        ui.add_space(12.0);

        if ui.button("Disconnect").clicked() {
            self.disconnect();
        }
    }
}

fn format_peer_state(state: PeerState) -> (&'static str, egui::Color32) {
    match state {
        PeerState::Connected => ("Connected", egui::Color32::GREEN),
        PeerState::HelloSent => ("Connecting...", egui::Color32::YELLOW),
        PeerState::Discovered => ("Discovered", egui::Color32::LIGHT_BLUE),
        PeerState::Disconnected => ("Disconnected", egui::Color32::RED),
    }
}

fn format_latency(p: &PeerDisplayInfo) -> String {
    if p.latency.sample_count == 0 {
        "—".to_string()
    } else {
        format!(
            "{:.1}ms (j:{:.1})",
            p.latency.avg_rtt_us as f64 / 1000.0,
            p.latency.jitter_us as f64 / 1000.0,
        )
    }
}

impl eframe::App for VoiceChatApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| match self.state {
            AppState::Lobby => self.draw_lobby(ui),
            AppState::Connected => self.draw_connected(ui),
        });

        // Repaint periodically for stats updates
        if matches!(self.state, AppState::Connected) {
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }
    }
}
