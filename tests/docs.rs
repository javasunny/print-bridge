use std::{fs, path::PathBuf};

fn documents() -> Vec<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut paths = vec![
        root.join("agents.md"),
        root.join("README.md"),
        root.join("desktop/README.md"),
        root.join("CONTRIBUTING.md"),
        root.join("CHANGELOG.md"),
    ];
    paths.extend(
        fs::read_dir(root.join("docs"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "md")),
    );
    paths
}

#[test]
fn markdown_has_one_title_and_closed_code_blocks() {
    for path in documents() {
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            text.ends_with('\n'),
            "Missing final newline: {}",
            path.display()
        );
        assert_eq!(
            text.lines().filter(|line| line.starts_with("# ")).count(),
            1,
            "{}",
            path.display()
        );
        let mut fence: Option<(char, usize)> = None;
        for line in text.lines() {
            let Some(marker @ ('`' | '~')) = line.chars().next() else {
                continue;
            };
            let count = line.chars().take_while(|c| *c == marker).count();
            if count < 3 {
                continue;
            }
            match fence {
                None => fence = Some((marker, count)),
                Some((open, length)) if open == marker && count >= length => fence = None,
                _ => (),
            }
        }
        assert!(fence.is_none(), "Unclosed code block: {}", path.display());
    }
}

#[test]
fn prd_sections_are_contiguous() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/PRD.md");
    let text = fs::read_to_string(path).unwrap();
    let numbers: Vec<usize> = text
        .lines()
        .filter_map(|line| line.strip_prefix("## ")?.split_once(". ")?.0.parse().ok())
        .collect();
    assert!(numbers.len() >= 13);
    assert_eq!(numbers, (1..=numbers.len()).collect::<Vec<_>>());
}

#[test]
fn local_document_links_exist() {
    for path in documents() {
        let text = fs::read_to_string(&path).unwrap();
        for fragment in text.split("](").skip(1) {
            let target = fragment.split(')').next().unwrap();
            if target.starts_with("https://") || target.starts_with("http://") {
                continue;
            }
            let file = target.split('#').next().unwrap();
            if !file.is_empty() {
                assert!(
                    path.parent().unwrap().join(file).exists(),
                    "Broken link {target} in {}",
                    path.display()
                );
            }
        }
    }
}

#[test]
fn release_version_matches_manifests_readme_and_changelog() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let config: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(root.join("desktop/tauri.conf.json")).unwrap())
            .unwrap();
    let version = config["version"].as_str().unwrap();
    let manifest = fs::read_to_string(root.join("desktop/Cargo.toml")).unwrap();
    let package = manifest
        .split_once("[package]\n")
        .unwrap()
        .1
        .split("\n[")
        .next()
        .unwrap();
    assert!(package
        .lines()
        .any(|line| line == format!("version = \"{version}\"")));
    let lock = fs::read_to_string(root.join("desktop/Cargo.lock")).unwrap();
    assert!(lock.contains(&format!(
        "name = \"print-bridge-desktop\"\nversion = \"{version}\""
    )));
    let readme = fs::read_to_string(root.join("README.md")).unwrap();
    assert!(readme.contains(&format!("Print-Bridge_{version}_x64-setup.exe")));
    for download in readme.split("Print-Bridge_").skip(1) {
        assert!(download.starts_with(&format!("{version}_x64-setup.exe")));
    }
    let changelog = fs::read_to_string(root.join("CHANGELOG.md")).unwrap();
    let latest = changelog
        .lines()
        .find(|line| {
            line.strip_prefix("## ")
                .is_some_and(|heading| heading.starts_with(|c: char| c.is_ascii_digit()))
        })
        .unwrap();
    assert!(latest.starts_with(&format!("## {version} —")));
}

#[test]
fn release_guidance_does_not_default_to_pre_release_for_early_versions() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for path in ["agents.md", "docs/RELEASING.md"] {
        let text = fs::read_to_string(root.join(path)).unwrap();
        assert!(text.contains("默认按正式版本发布"), "{path}");
        assert!(text.contains("用户明确要求试用版、测试版"), "{path}");
        assert!(!text.contains("早期版本标记为 pre-release"), "{path}");
    }
}

#[test]
fn license_material_is_present_in_the_installer_resources() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let config: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(root.join("desktop/tauri.conf.json")).unwrap())
            .unwrap();
    let resources = config["bundle"]["resources"].as_object().unwrap();
    for name in [
        "LICENSE",
        "THIRD_PARTY_LICENSES.txt",
        "SumatraPDF-NOTICE.txt",
    ] {
        let (source, _) = resources
            .iter()
            .find(|(_, target)| target.as_str() == Some(name))
            .unwrap_or_else(|| panic!("Missing installer license: {name}"));
        assert!(root.join("desktop").join(source).is_file());
    }
    let license = fs::read_to_string(root.join("LICENSE")).unwrap();
    assert!(license.starts_with("MIT License\n"));
    for path in ["Cargo.toml", "desktop/Cargo.toml"] {
        assert!(fs::read_to_string(root.join(path))
            .unwrap()
            .contains("license = \"MIT\""));
    }
    let notices = fs::read_to_string(root.join("THIRD_PARTY_LICENSES.txt")).unwrap();
    assert!(notices.contains("MPL-2.0"));
    assert!(notices.contains("https://crates.io/api/v1/crates/"));
}
