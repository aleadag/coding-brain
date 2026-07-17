use codexctl_core::runtime::{BrainRuntime, MockBrainRuntime};

#[test]
fn final_runtime_exposes_only_brain_source_actions_and_navigation() {
    let runtime: BrainRuntime = MockBrainRuntime::default().into_runtime();

    assert!(runtime.source.snapshot(Default::default()).is_ok());
    assert_eq!(runtime.source.gate_mode().as_str(), "on");
}

#[test]
fn removed_dashboard_and_management_flags_are_rejected() {
    let binary = env!("CARGO_BIN_EXE_coding-brain");
    for flag in [
        "--list",
        "--watch",
        "--new",
        "--resume",
        "--budget",
        "--record",
        "--clean",
        "--history",
        "--terminal-auto-approve-fallback",
    ] {
        let output = std::process::Command::new(binary)
            .arg(flag)
            .output()
            .unwrap();
        assert!(!output.status.success(), "{flag} unexpectedly succeeded");
    }
}
