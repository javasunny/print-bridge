use print_bridge::download::SumatraManager;

#[test]
#[ignore = "Downloads the fixed upstream release; run explicitly when network access is available"]
fn official_component_download_is_verified_and_reusable_offline() {
    let directory = tempfile::tempdir().unwrap();
    let manager = SumatraManager::new(directory.path().to_path_buf());
    assert!(!manager.status().ready);
    let installed = manager.download().unwrap();
    assert!(installed.ready);
    assert!(!installed.downloading);
    assert!(manager.exe_path().is_file());
    let reopened = SumatraManager::new(directory.path().to_path_buf());
    assert!(reopened.status().ready);
}

#[test]
#[ignore = "Reads the official SumatraPDF stable update metadata; requires network access"]
fn official_stable_update_check_reports_a_version_without_downloading() {
    let directory = tempfile::tempdir().unwrap();
    let manager = SumatraManager::new(directory.path().to_path_buf());
    let checked = manager.check_update().unwrap();

    assert!(!checked.ready);
    assert!(!checked.downloading);
    assert!(checked.current_version.is_empty());
    assert!(checked.update_available);
    assert!(checked
        .latest_version
        .as_deref()
        .is_some_and(|version| version.split('.').count() >= 2));
    assert!(!manager.exe_path().exists());
}
