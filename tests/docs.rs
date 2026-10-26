use std::{fs, path::PathBuf};

fn documents() -> Vec<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut paths = vec![root.join("README.md"), root.join("desktop/README.md")];
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
