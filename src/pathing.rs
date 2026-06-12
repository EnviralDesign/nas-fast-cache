use std::path::{Component, Path, PathBuf};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum PathError {
    #[error("path must stay under the source root: {0}")]
    EscapesRoot(String),
    #[error("absolute path is not under source root: {0}")]
    OutsideSourceRoot(String),
}

pub fn normalize_relative_path(path: impl AsRef<Path>) -> Result<PathBuf, PathError> {
    let path = path.as_ref();
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(PathError::EscapesRoot(path.display().to_string()));
            }
        }
    }
    Ok(out)
}

pub fn relative_input(source_root: &Path, path: impl AsRef<Path>) -> Result<PathBuf, PathError> {
    let path = path.as_ref();
    if path.has_root() {
        let rel = path
            .strip_prefix(source_root)
            .map_err(|_| PathError::OutsideSourceRoot(path.display().to_string()))?;
        normalize_relative_path(rel)
    } else {
        normalize_relative_path(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_paths_that_escape_root() {
        assert!(normalize_relative_path("a/../b").is_err());
        assert!(normalize_relative_path("C:/x").is_err());
    }

    #[test]
    fn accepts_clean_relative_path() {
        assert_eq!(
            normalize_relative_path("movies/Casino Royale.mkv").unwrap(),
            PathBuf::from("movies").join("Casino Royale.mkv")
        );
    }
}
