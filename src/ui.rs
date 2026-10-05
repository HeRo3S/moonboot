use crate::{
    backend::{Event, Operation, Phase},
    demo::Scenario,
};
use std::{sync::mpsc::Sender, time::SystemTime};

#[path = "settings.rs"]
pub mod settings;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Run(Operation),
    Cancel,
    EndDemo,
    Hide,
    Quit,
    OpenSettings,
    SaveSettings,
    ReloadSettings,
    CancelSettings,
}

/// Presentation state; only a worker at the backend approval boundary can install a sender.
pub struct Model {
    pub host: String,
    pub app: String,
    pub device: String,
    pub demo: Option<Scenario>,
    pub phase: Phase,
    pub busy: bool,
    pub configured: bool,
    pub status: Option<(bool, SystemTime)>,
    pub message: String,
    pub error: Option<String>,
    pub settings: Option<settings::Draft>,
    confirmation: String,
    confirmation_generation: u64,
    approval: Option<Sender<bool>>,
    visible: bool,
}

impl Model {
    pub fn new(host: String, app: String, device: String, demo: Option<Scenario>) -> Self {
        Self {
            host,
            app,
            device,
            demo,
            phase: Phase::Idle,
            busy: false,
            configured: true,
            status: None,
            message: "Idle. Status is only refreshed on request.".into(),
            error: None,
            settings: None,
            confirmation: String::new(),
            confirmation_generation: 0,
            approval: None,
            visible: true,
        }
    }

    pub fn event(&mut self, event: Event) {
        match event {
            Event::Phase(phase) => {
                self.phase = phase;
                let message = match phase {
                    Phase::Idle => None,
                    Phase::SwitchingOn => Some("Requesting explicit plug-on; never a toggle or power cycle."),
                    Phase::Waiting => Some("Waiting for Sunshine and the configured app. Cancellation leaves power unchanged."),
                    Phase::Launching => Some("Launching the Moonlight client."),
                    Phase::Streaming => Some("Session active. Ending the stream does not shut down the host or cut power."),
                    Phase::ConfirmingOff => Some("Plug status validated. Awaiting fresh confirmation of completed host shutdown."),
                    Phase::SwitchingOff => Some("Sending explicitly approved off and verifying reported switch status."),
                };
                if let Some(message) = message {
                    self.message = message.into();
                }
            }
            Event::Status(on) => self.status = Some((on, SystemTime::now())),
            Event::Message(message) => self.message = message,
        }
    }

    pub fn begin_confirmation(&mut self, approval: Sender<bool>) {
        self.decline();
        if !self.visible {
            let _ = approval.send(false);
            return;
        }
        self.phase = Phase::ConfirmingOff;
        self.confirmation.clear();
        self.confirmation_generation += 1;
        self.approval = Some(approval);
    }

    pub fn set_visible(&mut self, visible: bool) {
        self.visible = visible;
        if !visible {
            self.decline();
        }
    }

    pub fn settings_allowed(&self) -> bool {
        self.demo.is_none() && !self.busy && self.phase == Phase::Idle && self.approval.is_none()
    }

    pub fn discard_settings(&mut self, ctx: &egui::Context) {
        if let Some(mut draft) = self.settings.take() {
            draft.discard(ctx);
        }
    }

    pub fn settings_saved(
        &mut self,
        config: crate::config::Config,
        outcome: crate::config::SaveOutcome,
        location: std::path::PathBuf,
        ctx: &egui::Context,
    ) {
        self.status = None;
        self.error = None;
        self.configured = !outcome.requires_reload;
        self.message = if outcome.requires_reload {
            "Configuration target written, but the config selection changed; Reload from disk before operations.".into()
        } else {
            self.host = config.moonlight.host.clone();
            self.app = config.moonlight.app.clone();
            self.device = config.tuya.device_id.clone();
            "Configuration saved. Status is Unknown until explicitly refreshed.".into()
        };
        if let Some(warning) = outcome.warning {
            self.message.push(' ');
            self.message.push_str(&warning);
        }
        let editor_open = self.settings.is_some();
        self.discard_settings(ctx);
        if editor_open && outcome.requires_reload {
            let mut draft = settings::Draft::new(config, location);
            draft.save_blocked = true;
            draft.warning = Some(self.message.clone());
            self.settings = Some(draft);
        }
    }

    pub fn decline(&mut self) {
        if let Some(approval) = self.approval.take() {
            let _ = approval.send(false);
        }
        self.confirmation.clear();
    }

    pub fn finish(&mut self, result: Result<(), crate::backend::Error>) {
        self.decline();
        if result.is_ok() {
            self.message =
                "Requested operation completed. No automatic host shutdown or power reversal."
                    .into();
        }
        self.busy = false;
        self.phase = Phase::Idle;
        self.error = result.err().map(|error| error.to_string());
    }

    pub fn draw(&mut self, ctx: &egui::Context) -> Option<Action> {
        let mut action = None;
        let editing = self.settings.is_some();
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.heading("Moonboot");
                if let Some(scenario) = self.demo {
                    ui.colored_label(egui::Color32::from_rgb(246, 194, 87), format!("DEMO / {} / simulated only", scenario.name()));
                }
                ui.label(format!("Host: {}", self.host));
                ui.label(format!("App: {}", self.app));
                ui.separator();
                match self.status {
                    Some((on, observed)) => {
                        let timestamp = observed.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs();
                        let age = observed.elapsed().unwrap_or_default().as_secs();
                        ui.label(format!("Plug: {} / observed at {timestamp} ({age}s ago, not live)", if on { "On" } else { "Off" }));
                    }
                    None => { ui.label("Plug: Unknown / not observed"); }
                }
                ui.small("Plug-on is not proof the PC is running. No idle cloud polling.");
                ui.add_space(12.0);
                ui.heading(if self.error.is_some() { "Error" } else { phase_label(self.phase) });
                ui.label(&self.message);
                if let Some(error) = &self.error { ui.colored_label(egui::Color32::from_rgb(255, 143, 132), error); }
                ui.add_space(12.0);
                ui.horizontal_wrapped(|ui| {
                    if ui.add_enabled(self.configured && !self.busy && !editing, egui::Button::new("Start Session")).clicked() { action = Some(Action::Run(Operation::Start)); }
                    if ui.add_enabled(self.configured && !self.busy && !editing, egui::Button::new("Refresh Status")).clicked() { action = Some(Action::Run(Operation::Refresh)); }
                    if ui.add_enabled(self.configured && !self.busy && !editing, egui::Button::new("Plug Off")).clicked() { action = Some(Action::Run(Operation::PlugOff)); }
                });
                if ui.add_enabled(self.settings_allowed() && !editing, egui::Button::new("Settings")).clicked() { action = Some(Action::OpenSettings); }
                if self.demo.is_some() { ui.small("Settings disabled in demo: real configuration and secrets are never accessed."); }
                let waiting = self.busy && !matches!(self.phase, Phase::Streaming | Phase::SwitchingOff | Phase::ConfirmingOff);
                if ui.add_enabled(waiting, egui::Button::new("Cancel Startup")).clicked() { action = Some(Action::Cancel); }
                if self.demo.is_some() && self.phase == Phase::Streaming && ui.button("End Demo Session").clicked() { action = Some(Action::EndDemo); }
                ui.separator();
                ui.small("Cancel and Quit stop local work only. They never shut down the host or cut power.");
                ui.small(if let Some(scenario) = self.demo {
                    format!("Hide or close keeps work alive. Without a tray, reopen with moonboot gui --demo {}.", scenario.name())
                } else {
                    "Hide or close keeps work alive. Without a tray, reopen with moonboot gui.".into()
                });
                ui.horizontal(|ui| {
                    if ui.add_enabled(!editing, egui::Button::new("Hide")).clicked() { self.set_visible(false); action = Some(Action::Hide); }
                    if ui.add_enabled(!editing, egui::Button::new("Quit")).clicked() { self.decline(); action = Some(Action::Quit); }
                });
            });
        });
        if self.approval.is_some() {
            let mut open = true;
            let mut decision = None;
            egui::Window::new("Confirm Plug Off").open(&mut open).collapsible(false).resizable(false).default_width(420.0).show(ctx, |ui| {
                ui.label(format!("Host: {}", self.host));
                ui.label(format!("Plug device: {}", self.device));
                ui.label("This cuts AC power, not Linux shutdown. Data loss may occur if Linux is still running.");
                ui.label("Independently verify shutdown has fully completed. Other automation can change power outside this lock.");
                ui.label("Type exactly POWER OFF, then click Confirm Power Off.");
                let label = ui.label("Shutdown confirmation phrase");
                let mut output = egui::TextEdit::singleline(&mut self.confirmation).hint_text("POWER OFF").id_source(("off-phrase", self.confirmation_generation)).show(ui);
                output.response = output.response.labelled_by(label.id);
                output.state.clear_undoer();
                output.state.store(ctx, output.response.id);
                if ui.add_enabled(self.confirmation == "POWER OFF", egui::Button::new("Confirm Power Off")).clicked() { decision = Some(true); }
                if ui.button("Cancel").clicked() { decision = Some(false); }
            });
            if !open || ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
                decision = Some(false);
            }
            if let Some(approved) = decision {
                if let Some(sender) = self.approval.take() {
                    let _ = sender.send(approved);
                }
                self.confirmation.clear();
            }
        }
        let settings_blocked = !self.settings_allowed();
        if let Some(draft) = &mut self.settings {
            if let Some(settings_action) = draft.draw(ctx, settings_blocked) {
                action = Some(settings_action);
            }
        }
        action
    }
}

pub fn phase_label(phase: Phase) -> &'static str {
    match phase {
        Phase::Idle => "Idle",
        Phase::SwitchingOn => "Switching On",
        Phase::Waiting => "Waiting for Sunshine",
        Phase::Launching => "Launching Moonlight",
        Phase::Streaming => "Streaming",
        Phase::ConfirmingOff => "Confirming Plug Off",
        Phase::SwitchingOff => "Switching Off",
    }
}
