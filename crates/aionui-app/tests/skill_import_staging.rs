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
    assert!(
        services
            .skill_repo
            .find_by_name_for_user("system_default_user", "orphan")
            .await
            .unwrap()
            .is_none()
    );
}
