pub fn get_repo_root() -> std::path::PathBuf {
    std::path::PathBuf::from(file!())
        .canonicalize()
        .unwrap()
        .parent()
        .and_then(|f| f.parent())
        .and_then(|f| f.parent())
        .unwrap()
        .to_path_buf()
}

pub fn get_test_data() -> std::path::PathBuf {
    get_repo_root().join("src").join("test").join("data")
}

#[test]
fn test_dev_dir() {
    assert!(get_test_data().exists());
    assert!(get_test_data().join("config").exists());
    assert!(get_test_data().join("packages").exists());
}
