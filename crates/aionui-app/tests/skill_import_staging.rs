#[path = "../../aionui-extension/tests/zip_import_security/owner_fixture.rs"]
mod owner_fixture;

use aionui_app::{AppConfig, AppServices};
use aionui_db::init_database_memory;

#[tokio::test]
async fn startup_removes_abandoned_zip_staging_before_skill_catalog_scan() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let staging = data_dir.join("skills/.import-tmp/leased-crashed/orphan");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::write(
        staging.join("SKILL.md"),
        "---\nname: orphan\ndescription: must stay private\n---\nBody",
    )
    .unwrap();
    let install_staging = data_dir.join("skills/users/fixture-user/.import-staging-leased-crashed.tmp");
    std::fs::create_dir_all(&install_staging).unwrap();
    std::fs::write(
        install_staging.join("SKILL.md"),
        "---\nname: orphan-install\ndescription: must stay private\n---\nBody",
    )
    .unwrap();
    let ended = owner_fixture::ended_pid().await;
    owner_fixture::write_owner(&data_dir.join("skills/.import-tmp"), "container", ended);
    owner_fixture::write_owner(&data_dir.join("skills/.import-tmp/leased-crashed"), "zip", ended);
    owner_fixture::write_owner(&install_staging, "install", ended);
    let config = AppConfig {
        data_dir: data_dir.clone(),
        work_dir: tmp.path().join("work"),
        local: true,
        ..Default::default()
    };
    let services = AppServices::from_config(init_database_memory().await.unwrap(), &config)
        .await
        .unwrap();

    assert!(!data_dir.join("skills/.import-tmp").exists());
    assert!(!install_staging.exists());
    assert!(
        services
            .skill_repo
            .find_by_name_for_user("system_default_user", "orphan")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        services
            .skill_repo
            .find_by_name_for_user("system_default_user", "orphan-install")
            .await
            .unwrap()
            .is_none()
    );
}
