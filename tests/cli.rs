use std::{
    io::Write,
    process::{Command, Stdio},
};

fn binary() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_moonboot"));
    command
        .env_clear()
        .env("PATH", "/nonexistent-test-path")
        .env("HOME", "/nonexistent-test-home");
    command
}

#[test]
fn help_version_and_scoped_demo_options() {
    let output = binary().arg("--help").output().unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    for text in [
        "start",
        "check",
        "plug-off",
        "tray",
        "gui",
        "--config",
        "--verbose",
        "--version",
    ] {
        assert!(help.contains(text));
    }
    assert!(binary().arg("--version").output().unwrap().status.success());
    for command in ["gui", "tray"] {
        let output = binary().args([command, "--help"]).output().unwrap();
        let help = String::from_utf8(output.stdout).unwrap();
        for scenario in [
            "success",
            "already-on",
            "cloud-error",
            "readiness-timeout",
            "off-error",
        ] {
            assert!(help.contains(scenario));
        }
    }
    assert!(!binary()
        .args(["start", "--demo", "success"])
        .output()
        .unwrap()
        .status
        .success());
    assert!(!binary()
        .args(["plug-off", "--yes"])
        .output()
        .unwrap()
        .status
        .success());
}

#[test]
fn piped_off_phrase_is_rejected_before_config_or_cloud() {
    let mut child = binary()
        .args([
            "plug-off",
            "--config",
            "/missing-secret-config",
            "--verbose",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let _ = child.stdin.take().unwrap().write_all(b"POWER OFF\n");
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(error.contains("interactive terminal"));
    assert!(!error.contains("Cannot read configuration"));
    assert!(!error.contains("Cloud"));
}

#[test]
fn config_global_on_either_side_and_errors_do_not_echo_contents() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.toml");
    std::fs::write(&path, "SECRET_SENTINEL = invalid").unwrap();
    for args in [
        vec!["--config", path.to_str().unwrap(), "check"],
        vec!["check", "--config", path.to_str().unwrap()],
    ] {
        let output = binary().args(args).output().unwrap();
        assert_eq!(output.status.code(), Some(2));
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(error.contains("Invalid configuration"));
        assert!(!error.contains("SECRET_SENTINEL"));
    }
}
