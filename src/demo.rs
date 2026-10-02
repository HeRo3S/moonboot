//! Hardware-free scenarios. This module never loads configuration or calls production work.
use crate::backend::{self, Error, Event, Operation, Phase};
use clap::ValueEnum;
use std::{
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Scenario {
    Success,
    AlreadyOn,
    CloudError,
    ReadinessTimeout,
    OffError,
}

impl Scenario {
    pub fn name(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::AlreadyOn => "already-on",
            Self::CloudError => "cloud-error",
            Self::ReadinessTimeout => "readiness-timeout",
            Self::OffError => "off-error",
        }
    }
}

pub struct Demo {
    pub scenario: Scenario,
    pub on: bool,
    /// Recorded simulated commands, never real power commands.
    pub commands: Vec<bool>,
    pub streams: usize,
}

impl Demo {
    pub fn new(scenario: Scenario) -> Self {
        Self {
            scenario,
            on: matches!(scenario, Scenario::AlreadyOn | Scenario::OffError),
            commands: Vec::new(),
            streams: 0,
        }
    }

    pub fn run(
        &mut self,
        operation: Operation,
        cancel: &AtomicBool,
        end_stream: &AtomicBool,
        emit: impl Fn(Event),
        approve: impl FnOnce() -> bool,
    ) -> Result<(), Error> {
        let _lock = backend::operation_lock(&format!("demo-{}-operation", self.scenario.name()))?;
        self.simulate(
            operation,
            cancel,
            end_stream,
            emit,
            approve,
            Duration::from_secs(1),
        )
    }

    fn simulate(
        &mut self,
        operation: Operation,
        cancel: &AtomicBool,
        end_stream: &AtomicBool,
        emit: impl Fn(Event),
        approve: impl FnOnce() -> bool,
        step: Duration,
    ) -> Result<(), Error> {
        backend::cancelled(cancel)?;
        if self.scenario == Scenario::CloudError {
            return Err(Error::Cloud(
                "DEMO: simulated cloud failure; no real service contacted".into(),
            ));
        }
        emit(Event::Status(self.on));
        match operation {
            Operation::Check | Operation::Refresh => {
                emit(Event::Message(
                    "DEMO: simulated plug status; not proof the PC is running".into(),
                ));
            }
            Operation::PlugOff => {
                if !self.on {
                    emit(Event::Message(
                        "DEMO: plug already reports off; no simulated command sent".into(),
                    ));
                    return Ok(());
                }
                emit(Event::Phase(Phase::ConfirmingOff));
                if !approve() {
                    return Err(Error::ApprovalDeclined);
                }
                backend::cancelled(cancel)?;
                emit(Event::Phase(Phase::SwitchingOff));
                self.commands.push(false);
                wait(step, cancel)?;
                if self.scenario == Scenario::OffError {
                    return Err(Error::Cloud(
                        "DEMO: off result unconfirmed; no retry or toggle attempted".into(),
                    ));
                }
                self.on = false;
                emit(Event::Status(false));
            }
            Operation::Start => {
                if !self.on {
                    emit(Event::Phase(Phase::SwitchingOn));
                    self.commands.push(true);
                    wait(step, cancel)?;
                    self.on = true;
                    emit(Event::Status(true));
                }
                emit(Event::Phase(Phase::Waiting));
                wait(step * 2, cancel)?;
                if self.scenario == Scenario::ReadinessTimeout {
                    return Err(Error::ReadinessTimeout);
                }
                emit(Event::Phase(Phase::Launching));
                wait(step / 2, cancel)?;
                self.streams += 1;
                emit(Event::Phase(Phase::Streaming));
                while !end_stream.load(Ordering::Relaxed) {
                    wait(Duration::from_millis(20), cancel)?;
                }
                emit(Event::Message(
                    "DEMO: simulated session ended; plug power unchanged".into(),
                ));
            }
        }
        Ok(())
    }
}

fn wait(duration: Duration, cancel: &AtomicBool) -> Result<(), Error> {
    let deadline = Instant::now() + duration;
    loop {
        backend::cancelled(cancel)?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(());
        }
        thread::sleep(remaining.min(Duration::from_millis(20)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scenarios_record_only_simulated_authorized_commands() {
        for scenario in Scenario::value_variants() {
            let mut demo = Demo::new(*scenario);
            let cancel = AtomicBool::new(false);
            let end = AtomicBool::new(true);
            let result = demo.simulate(
                Operation::Start,
                &cancel,
                &end,
                |_| {},
                || panic!("start approval"),
                Duration::ZERO,
            );
            assert!(!demo.commands.contains(&false));
            assert_eq!(demo.streams, usize::from(result.is_ok()));
            let before = demo.commands.clone();
            let _ = demo.simulate(
                Operation::PlugOff,
                &cancel,
                &end,
                |_| {},
                || false,
                Duration::ZERO,
            );
            assert_eq!(demo.commands, before);
            let _ = demo.simulate(
                Operation::PlugOff,
                &cancel,
                &end,
                |_| {},
                || true,
                Duration::ZERO,
            );
            if scenario != &Scenario::CloudError {
                assert_eq!(demo.commands.last(), Some(&false));
            }
        }
    }

    #[test]
    fn cancellation_never_reverses_power() {
        let mut demo = Demo::new(Scenario::Success);
        let cancel = AtomicBool::new(false);
        let result = demo.simulate(
            Operation::Start,
            &cancel,
            &AtomicBool::new(false),
            |event| {
                if event == Event::Phase(Phase::Waiting) {
                    cancel.store(true, Ordering::Relaxed);
                }
            },
            || false,
            Duration::ZERO,
        );
        assert!(matches!(result, Err(Error::Cancelled)));
        assert_eq!(demo.commands, [true]);
        assert_eq!(demo.streams, 0);
    }
}
