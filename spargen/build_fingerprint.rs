use std::path::{Component, Path, PathBuf};

pub(crate) struct Input {
    pub(crate) path: PathBuf,
    pub(crate) label: String,
}

pub(crate) fn inputs(manifest_dir: &Path) -> Vec<Input> {
    let mut paths = vec![
        manifest_dir.join("Cargo.toml"),
        manifest_dir.join("build.rs"),
        manifest_dir.join("build_fingerprint.rs"),
    ];
    collect_files(&manifest_dir.join("src"), &mut paths);

    let workspace_lock = manifest_dir
        .parent()
        .map(|parent| parent.join("Cargo.lock"))
        .filter(|path| path.is_file());
    if let Some(lock) = &workspace_lock {
        paths.push(lock.clone());
    }

    paths.sort();
    paths.dedup();
    paths
        .into_iter()
        .map(|path| {
            let label = match path.strip_prefix(manifest_dir) {
                Ok(relative) => portable_label(relative),
                Err(_) if workspace_lock.as_deref() == Some(path.as_path()) => {
                    "../Cargo.lock".to_owned()
                }
                Err(_) => panic!(
                    "build fingerprint input `{}` is outside `{}`",
                    path.display(),
                    manifest_dir.display()
                ),
            };
            Input { path, label }
        })
        .collect()
}

fn collect_files(dir: &Path, files: &mut Vec<PathBuf>) {
    let mut entries = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            collect_files(&path, files);
        } else if path.is_file() {
            files.push(path);
        }
    }
}

fn portable_label(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::CurDir => None,
            Component::ParentDir => Some(".."),
            Component::Normal(segment) => Some(
                segment
                    .to_str()
                    .expect("build fingerprint paths must be valid UTF-8"),
            ),
            Component::RootDir | Component::Prefix(_) => {
                panic!("build fingerprint labels must be relative")
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}
