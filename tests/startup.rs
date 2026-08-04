use std::process::Command;

#[test]
fn invalid_configuration_reports_variable_without_leaking_credentials() {
    const INVALID_MASTER_KEY: &str = "invalid-master-key-secret";
    const ADMIN_TOKEN: &str = "invalid-admin-token-secret";
    let output = Command::new(env!("CARGO_BIN_EXE_github_webhook_exporter"))
        .env_clear()
        .env("GHE_DATABASE_PATH", "/tmp/exporter-invalid.db")
        .env("GHE_MASTER_KEY", INVALID_MASTER_KEY)
        .env("GHE_ADMIN_TOKEN", ADMIN_TOKEN)
        .output()
        .expect("exporter process starts");

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(stderr.contains("GHE_MASTER_KEY"));
    assert!(!stderr.contains(INVALID_MASTER_KEY));
    assert!(!stderr.contains(ADMIN_TOKEN));
}
