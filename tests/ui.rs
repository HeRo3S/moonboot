use egui_kittest::{
    kittest::{NodeT, Queryable},
    Harness,
};
use moonboot::{
    backend::{Error, Event, Operation, Phase},
    demo::Scenario,
    ui::{Action, Model},
};
use std::{sync::mpsc, time::Duration};

struct State {
    model: Model,
    actions: Vec<Action>,
}

fn harness() -> Harness<'static, State> {
    Harness::builder()
        .with_size(egui::vec2(700.0, 620.0))
        .build_state(
            |ctx, state: &mut State| {
                if let Some(action) = state.model.draw(ctx) {
                    state.actions.push(action);
                }
            },
            State {
                model: Model::new(
                    "demo-host.invalid".into(),
                    "Demo Desktop".into(),
                    "demo-plug".into(),
                    Some(Scenario::Success),
                ),
                actions: Vec::new(),
            },
        )
}

#[test]
fn start_progress_conflicts_cancel_and_stream_controls() {
    let mut h = harness();
    h.get_by_label_contains("DEMO / success");
    h.get_by_label("Plug: Unknown / not observed");
    h.get_by_label("Start Session").click();
    h.run();
    assert_eq!(h.state().actions, [Action::Run(Operation::Start)]);
    h.state_mut().model.busy = true;
    for phase in [
        Phase::SwitchingOn,
        Phase::Waiting,
        Phase::Launching,
        Phase::Streaming,
    ] {
        h.state_mut().model.event(Event::Phase(phase));
        h.state_mut().model.event(Event::Status(true));
        h.run();
        h.get_by_label(moonboot::ui::phase_label(phase));
        for label in ["Start Session", "Plug Off", "Refresh Status"] {
            assert!(h.get_by_label(label).accesskit_node().is_disabled());
        }
        h.get_by_label_contains("observed at");
        h.get_by_label_contains("Plug-on is not proof");
        if phase == Phase::Waiting {
            h.get_by_label("Cancel Startup").click();
            h.run();
            assert_eq!(h.state().actions.last(), Some(&Action::Cancel));
        }
    }
    assert!(h
        .get_by_label("Cancel Startup")
        .accesskit_node()
        .is_disabled());
    h.get_by_label("End Demo Session").click();
    h.run();
    assert_eq!(h.state().actions.last(), Some(&Action::EndDemo));
    h.get_by_label("Hide").click();
    h.run();
    assert!(h.state().model.busy);
    assert_eq!(h.state().model.phase, Phase::Streaming);
    h.state_mut().model.finish(Err(Error::ReadinessTimeout));
    h.run();
    h.get_by_label("Error");
    h.get_by_label_contains("Readiness timed out");
    assert!(!h
        .get_by_label("Start Session")
        .accesskit_node()
        .is_disabled());
    assert!(!h.state().actions.contains(&Action::Run(Operation::PlugOff)));
}

#[test]
fn off_requires_fresh_exact_phrase_and_distinct_click() {
    let mut h = harness();
    h.get_by_label("Plug Off").click();
    h.run();
    assert_eq!(h.state().actions, [Action::Run(Operation::PlugOff)]);
    assert!(h.query_by_label("Confirm Power Off").is_none());
    // The worker supplies this only after status validation at ConfirmingOff.
    let (tx, rx) = mpsc::channel();
    h.state_mut().model.busy = true;
    h.state_mut().model.begin_confirmation(tx);
    h.run();
    h.get_by_label("Plug device: demo-plug");
    h.get_by_label_contains("Data loss");
    assert!(h
        .get_by_label("Confirm Power Off")
        .accesskit_node()
        .is_disabled());
    h.get_by_role_and_label(
        egui::accesskit::Role::TextInput,
        "Shutdown confirmation phrase",
    )
    .click();
    h.run();
    h.get_by_role_and_label(
        egui::accesskit::Role::TextInput,
        "Shutdown confirmation phrase",
    )
    .type_text("POWER off");
    h.run();
    assert_eq!(
        h.get_by_role_and_label(
            egui::accesskit::Role::TextInput,
            "Shutdown confirmation phrase"
        )
        .value()
        .as_deref(),
        Some("POWER off")
    );
    assert!(h
        .get_by_label("Confirm Power Off")
        .accesskit_node()
        .is_disabled());
    assert!(rx.try_recv().is_err());
    h.get_by_label("Cancel").click();
    h.run();
    assert!(!rx.recv_timeout(Duration::from_secs(1)).unwrap());
    let (tx, rx) = mpsc::channel();
    h.state_mut().model.begin_confirmation(tx);
    h.run();
    assert!(h
        .get_by_label("Confirm Power Off")
        .accesskit_node()
        .is_disabled());
    h.get_by_role_and_label(
        egui::accesskit::Role::TextInput,
        "Shutdown confirmation phrase",
    )
    .click();
    h.run();
    h.get_by_role_and_label(
        egui::accesskit::Role::TextInput,
        "Shutdown confirmation phrase",
    )
    .type_text("POWER OFF");
    h.run();
    assert!(!h
        .get_by_label("Confirm Power Off")
        .accesskit_node()
        .is_disabled());
    h.key_press(egui::Key::Enter);
    h.run();
    assert!(rx.try_recv().is_err());
    h.get_by_label("Confirm Power Off").click();
    h.run();
    assert!(rx.recv_timeout(Duration::from_secs(1)).unwrap());
    h.state_mut()
        .model
        .finish(Err(Error::Cloud("DEMO: off unconfirmed".into())));
    let (tx, rx) = mpsc::channel();
    h.state_mut().model.begin_confirmation(tx);
    h.run();
    assert!(h
        .get_by_label("Confirm Power Off")
        .accesskit_node()
        .is_disabled());
    h.key_press(egui::Key::Escape);
    h.run();
    assert!(!rx.recv_timeout(Duration::from_secs(1)).unwrap());
    let (tx, rx) = mpsc::channel();
    h.state_mut().model.begin_confirmation(tx);
    h.run();
    h.get_by_role_and_label(
        egui::accesskit::Role::TextInput,
        "Shutdown confirmation phrase",
    )
    .click();
    h.run();
    h.key_press_modifiers(egui::Modifiers::CTRL, egui::Key::Z);
    h.run();
    assert!(h
        .get_by_label("Confirm Power Off")
        .accesskit_node()
        .is_disabled());
    assert_eq!(
        h.get_by_role_and_label(
            egui::accesskit::Role::TextInput,
            "Shutdown confirmation phrase"
        )
        .value()
        .unwrap_or_default(),
        ""
    );
    h.get_by_label("Close window").click();
    h.run();
    assert!(!rx.recv_timeout(Duration::from_secs(1)).unwrap());
}

#[test]
fn hide_rejects_pending_approval_and_preserves_busy_work() {
    let mut h = harness();
    let (tx, rx) = mpsc::channel();
    h.state_mut().model.busy = true;
    h.state_mut().model.begin_confirmation(tx);
    h.run();
    h.get_by_label("Hide").click();
    h.run();
    assert!(!rx.recv_timeout(Duration::from_secs(1)).unwrap());
    assert!(h.state().model.busy);
}

#[test]
fn hide_before_status_validation_declines_late_approval() {
    let mut h = harness();
    h.get_by_label_contains("Without a tray, reopen with moonboot gui --demo success.");
    h.state_mut().model.busy = true;
    h.get_by_label("Hide").click();
    h.run();
    let (tx, rx) = mpsc::channel();
    h.state_mut()
        .model
        .event(Event::Phase(Phase::ConfirmingOff));
    h.state_mut().model.begin_confirmation(tx);
    // No redraw is needed to reject an approval request for a hidden panel.
    assert!(!rx.recv_timeout(Duration::from_secs(1)).unwrap());
    h.run();
    assert!(h.query_by_label("Confirm Power Off").is_none());
    assert!(h.state().model.busy);
    h.state_mut().model.finish(Err(Error::ApprovalDeclined));
    h.state_mut().model.set_visible(true);
    let (tx, rx) = mpsc::channel();
    h.state_mut().model.begin_confirmation(tx);
    h.run();
    assert!(h
        .get_by_label("Confirm Power Off")
        .accesskit_node()
        .is_disabled());
    assert!(rx.try_recv().is_err());
}
