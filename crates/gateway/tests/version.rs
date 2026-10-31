use std::process::Command;

#[test]
fn version_flag_prints_package_version() {
    let output = Command::new(env!("CARGO_BIN_EXE_agentsflow"))
        .arg("--version")
        .output()
        .expect("failed to run agentsflow");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("stdout is not UTF-8"),
        format!("agentsflow {}\n", env!("CARGO_PKG_VERSION"))
    );
}
