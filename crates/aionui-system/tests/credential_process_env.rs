#![cfg(unix)]

use std::process::Command;

use aionui_common::host_env::HostEnv;
use aws_sdk_bedrock::config::ProvideCredentials;

#[tokio::test]
async fn aws_profile_command_filters_host_env() {
    const CHILD: &str = "HOST78_AWS_FIXTURE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("config");
        let names = HostEnv::ALL
            .iter()
            .map(|setting| setting.name())
            .collect::<Vec<_>>()
            .join(" ");
        let helper = dir.path().join("credentials.sh");
        std::fs::write(&helper, format!(
            "for name in {names}; do if printenv \"$name\" >/dev/null; then exit 23; fi; done\nprintf '%s\\n' '{{\"Version\":1,\"AccessKeyId\":\"fixture-access\",\"SecretAccessKey\":\"fixture-secret\"}}'\n"
        )).unwrap();
        std::fs::write(
            &profile,
            format!("[profile fixture]\ncredential_process = sh '{}'\n", helper.display()),
        )
        .unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "aws_profile_command_filters_host_env", "--nocapture"])
            .env(CHILD, "1")
            .env("AWS_CONFIG_FILE", &profile)
            .env("AWS_SHARED_CREDENTIALS_FILE", dir.path().join("empty"))
            .env("AWS_EC2_METADATA_DISABLED", "true")
            .env_remove("AWS_ACCESS_KEY_ID")
            .env_remove("AWS_SECRET_ACCESS_KEY")
            .env_remove("SECRET_ACCESS_KEY")
            .envs(HostEnv::ALL.iter().map(|setting| (setting.name(), "host sentinel")))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .profile_name("fixture")
        .region(aws_config::Region::new("us-east-1"))
        .load()
        .await;
    let credentials = config
        .credentials_provider()
        .unwrap()
        .provide_credentials()
        .await
        .unwrap();
    assert_eq!(credentials.access_key_id(), "fixture-access");
}
