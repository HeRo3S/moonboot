use crate::config::Config;
use std::path::PathBuf;

/// A window-local draft, never stored in egui memory or sent to a logger.
pub struct Draft {
    pub config: Config,
    pub location: PathBuf,
    pub error: Option<String>,
    pub warning: Option<String>,
    pub save_blocked: bool,
    external: bool,
    credentials_path: String,
    ids: Vec<egui::Id>,
    focus_on_open: bool,
}

impl Draft {
    pub fn new(config: Config, location: PathBuf) -> Self {
        Self {
            external: config.tuya.credentials_file.is_some(),
            credentials_path: config
                .tuya
                .credentials_file
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            config,
            location,
            error: None,
            warning: None,
            save_blocked: false,
            ids: Vec::new(),
            focus_on_open: true,
        }
    }

    pub fn discard(&mut self, ctx: &egui::Context) {
        ctx.data_mut(|data| {
            for id in self.ids.drain(..) {
                data.remove::<egui::text_edit::TextEditState>(id);
            }
        });
    }

    fn field(
        ui: &mut egui::Ui,
        ids: &mut Vec<egui::Id>,
        label: &str,
        text: &mut String,
        password: bool,
    ) -> egui::Response {
        let label_response = ui.label(label);
        let mut output = egui::TextEdit::singleline(text)
            .id_salt(label)
            .password(password)
            .desired_width(f32::INFINITY)
            .show(ui);
        output.response = output.response.labelled_by(label_response.id);
        // Secrets must not survive in undo buffers, even within this editor.
        if password {
            output.state.clear_undoer();
        }
        output.state.store(ui.ctx(), output.response.id);
        if !ids.contains(&output.response.id) {
            ids.push(output.response.id);
        }
        output.response
    }

    /// The controller owns filesystem access; opening a draft is inert.
    pub fn draw(&mut self, ctx: &egui::Context, busy: bool) -> Option<super::Action> {
        let mut open = true;
        let mut action = None;
        egui::Window::new("Configuration Settings")
            .open(&mut open)
            .collapsible(false)
            .default_width(460.0)
            .show(ctx, |ui| {
                ui.label(format!("Configuration location: {}", self.location.display()));
                ui.small("Save preserves any symlink and writes its private target with mode 0600. New private config directories use mode 0700.");
                ui.small("Read-only agenix or Nix store targets cannot be saved here: edit the encrypted/declarative source and redeploy.");
                ui.small("Credentials are plaintext in the private config file. Keep it out of Git; encrypt the entire file with agenix for deployment.");
                if let Some(error) = &self.error {
                    ui.colored_label(egui::Color32::from_rgb(255, 143, 132), error);
                }
                if let Some(warning) = &self.warning {
                    ui.colored_label(egui::Color32::from_rgb(246, 194, 87), warning);
                }
                egui::ScrollArea::vertical().max_height((ctx.content_rect().height() - 270.0).clamp(80.0, 410.0)).show(ui, |ui| {
                    ui.add_enabled_ui(!busy, |ui| {
                        let endpoint = Self::field(ui, &mut self.ids, "Tuya endpoint", &mut self.config.tuya.endpoint, false);
                        if self.focus_on_open {
                            endpoint.request_focus();
                            self.focus_on_open = false;
                        }
                        Self::field(ui, &mut self.ids, "Device ID", &mut self.config.tuya.device_id, false);
                        Self::field(ui, &mut self.ids, "Switch code", &mut self.config.tuya.switch_code, false);
                        ui.add_enabled_ui(!self.external, |ui| {
                            Self::field(ui, &mut self.ids, "Cloud client ID", &mut self.config.tuya.client_id, true);
                            Self::field(ui, &mut self.ids, "Cloud client secret", &mut self.config.tuya.client_secret, true);
                        });
                        ui.separator();
                        Self::field(ui, &mut self.ids, "Moonlight host", &mut self.config.moonlight.host, false);
                        Self::field(ui, &mut self.ids, "Moonlight app", &mut self.config.moonlight.app, false);
                        Self::field(ui, &mut self.ids, "Moonlight executable", &mut self.config.moonlight.executable, false);
                        ui.label("Extra arguments: one literal argument per line, not shell syntax.");
                        let mut remove = None;
                        for (index, argument) in self.config.moonlight.stream_args.iter_mut().enumerate() {
                            Self::field(ui, &mut self.ids, &format!("Extra argument {}", index + 1), argument, false);
                            if ui.button(format!("Remove argument {}", index + 1)).clicked() { remove = Some(index); }
                        }
                        if let Some(index) = remove { self.config.moonlight.stream_args.remove(index); }
                        if ui.button("Add argument").clicked() { self.config.moonlight.stream_args.push(String::new()); }
                        egui::CollapsingHeader::new("Advanced").show(ui, |ui| {
                            for (label, value) in [
                                ("Startup timeout (seconds)", &mut self.config.startup.timeout_seconds),
                                ("Poll interval (seconds)", &mut self.config.startup.poll_interval_seconds),
                                ("Probe timeout (seconds)", &mut self.config.startup.probe_timeout_seconds),
                                ("HTTP timeout (seconds)", &mut self.config.startup.http_timeout_seconds),
                            ] {
                                ui.horizontal(|ui| {
                                    let response = ui.label(label);
                                    ui.add(egui::DragValue::new(value).range(1..=86400)).labelled_by(response.id);
                                });
                            }
                            ui.checkbox(&mut self.config.notifications.enabled, "Enable notifications");
                            if ui.checkbox(&mut self.external, "Use external credentials file").changed() {
                                self.config.tuya.client_id.clear();
                                self.config.tuya.client_secret.clear();
                            }
                            ui.add_enabled_ui(self.external, |ui| {
                                Self::field(ui, &mut self.ids, "External credentials file", &mut self.credentials_path, false);
                            });
                            ui.small("External credentials (including agenix runtime keys) are read only when an operation starts, never by Settings.");
                        });
                    });
                });
                ui.horizontal(|ui| {
                    if ui.add_enabled(!busy && !self.save_blocked, egui::Button::new("Save")).clicked() {
                        self.config.tuya.credentials_file = self.external.then(|| PathBuf::from(&self.credentials_path));
                        action = Some(super::Action::SaveSettings);
                    }
                    if ui.add_enabled(!busy, egui::Button::new("Reload from disk")).clicked() { action = Some(super::Action::ReloadSettings); }
                    if ui.button("Cancel").clicked() { action = Some(super::Action::CancelSettings); }
                });
                ui.small("Reload picks up redeployed configuration and discards unsaved edits only on success. External credentials are never read here.");
                ui.small("Save only validates and writes configuration. No power, network, authentication, or executable checks.");
            });
        if !open || ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
            action = Some(super::Action::CancelSettings);
        }
        action
    }
}
