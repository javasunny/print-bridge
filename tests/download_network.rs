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
