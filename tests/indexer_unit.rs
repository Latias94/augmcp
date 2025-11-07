use augmcp::indexer::{ProjectsIndex, collect_blobs, incremental_plan};
use std::{collections::HashSet, fs, path::Path};

fn set_to(list: &[&str]) -> HashSet<String> {
    list.iter().map(|s| s.to_string()).collect()
}

#[test]
fn collect_respects_ext_exclude_gitignore_and_splitting() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path();

    // files
    let src_dir = root.join("src");
    let dist_dir = root.join("dist");
    let ignored_dir = root.join("ignored_dir");
    fs::create_dir_all(&src_dir).unwrap();
    fs::create_dir_all(&dist_dir).unwrap();
    fs::create_dir_all(&ignored_dir).unwrap();

    // .gitignore to ignore ignored_dir
    fs::write(root.join(".gitignore"), "ignored_dir\n").unwrap();

    // Create files
    fs::write(src_dir.join("main.rs"), "line1\nline2\n").unwrap();
    fs::write(src_dir.join("notes.txt"), "hello\n").unwrap();
    fs::write(dist_dir.join("bundle.js"), "alert(1)\n").unwrap();
    fs::write(ignored_dir.join("will_skip.txt"), "nope\n").unwrap();

    let text_exts = set_to(&[".rs", ".txt"]);
    let exclude = vec!["dist".to_string(), "ignored_dir".to_string()];

    // max_lines = 1 -> each line becomes a blob
    let blobs = collect_blobs(root, &text_exts, 1, &exclude).unwrap();

    // Expect: src/main.rs split into 2 chunks, src/notes.txt single.
    // Excluded: dist/bundle.js, ignored_dir/* via .gitignore
    let names: Vec<String> = blobs.iter().map(|b| b.path.clone()).collect();
    assert!(names.contains(&"src/main.rs#chunk1of2".to_string()));
    assert!(names.contains(&"src/main.rs#chunk2of2".to_string()));
    assert!(names.contains(&"src/notes.txt".to_string()));
    assert!(!names.iter().any(|p| p.contains("bundle.js")));
    assert!(!names.iter().any(|p| p.contains("will_skip.txt")));

    // incremental_plan should mark all as new if projects index is empty
    let projects = ProjectsIndex::default();
    let (new_blobs, all) = incremental_plan("proj", &blobs, &projects);
    assert_eq!(new_blobs.len(), blobs.len());
    assert_eq!(all.len(), blobs.len());
}
