use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::model::{Project, Studio};

pub(crate) const PROJECT_PATH_ENV: &str = "DAW_AI_PROJECT_PATH";
const DEFAULT_PROJECT_FILE: &str = "sound-graph.json";
const MAX_PROJECT_BYTES: u64 = 16 * 1024 * 1024;
static TEMP_FILE_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub(crate) struct ProjectStore {
    path: PathBuf,
}

impl ProjectStore {
    pub(crate) fn open_from_environment() -> io::Result<(Self, Studio)> {
        let path = match std::env::var_os(PROJECT_PATH_ENV) {
            Some(path) if !path.is_empty() => PathBuf::from(path),
            _ => std::env::current_dir()?.join(DEFAULT_PROJECT_FILE),
        };
        Self::open(path)
    }

    pub(crate) fn open(path: PathBuf) -> io::Result<(Self, Studio)> {
        let store = Self { path };
        if store.path.exists() {
            let project = store.read()?;
            Ok((store, Studio::from_project(project)))
        } else {
            let studio = Studio::new();
            store.save(studio.project())?;
            Ok((store, studio))
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn read(&self) -> io::Result<Project> {
        let file = OpenOptions::new().read(true).open(&self.path)?;
        let metadata = file.metadata()?;
        if metadata.len() > MAX_PROJECT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("sound graph exceeds the {MAX_PROJECT_BYTES}-byte limit"),
            ));
        }
        let mut source = String::with_capacity(metadata.len() as usize);
        file.take(MAX_PROJECT_BYTES + 1)
            .read_to_string(&mut source)?;
        if source.len() as u64 > MAX_PROJECT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("sound graph exceeds the {MAX_PROJECT_BYTES}-byte limit"),
            ));
        }
        Project::from_json(&source).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid sound graph {}: {error}", self.path.display()),
            )
        })
    }

    pub(crate) fn save(&self, project: &Project) -> io::Result<()> {
        let source = format!("{}\n", project.to_json());
        if source.len() as u64 > MAX_PROJECT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("sound graph exceeds the {MAX_PROJECT_BYTES}-byte limit"),
            ));
        }
        Project::from_json(&source).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("refusing to save an invalid sound graph: {error}"),
            )
        })?;
        replace_text_file(&self.path, &source)
    }
}

pub(crate) fn replace_text_file(path: &Path, source: &str) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    if !parent.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("file directory does not exist: {}", parent.display()),
        ));
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("daw-ai-file");
    let id = TEMP_FILE_ID.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(".{file_name}.{}.{id}.tmp", std::process::id()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(source.as_bytes())?;
        file.sync_all()?;
        drop(file);
        replace_destination(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn replace_destination(temporary: &Path, destination: &Path) -> io::Result<()> {
    match fs::rename(temporary, destination) {
        Ok(()) => Ok(()),
        Err(error) => {
            #[cfg(windows)]
            if destination.is_file()
                && matches!(
                    error.kind(),
                    io::ErrorKind::AlreadyExists | io::ErrorKind::PermissionDenied
                )
            {
                fs::remove_file(destination)?;
                return fs::rename(temporary, destination);
            }
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_project_path(label: &str) -> PathBuf {
        let id = TEMP_FILE_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("daw-ai-{label}-{}-{id}.json", std::process::id()))
    }

    #[test]
    fn creates_and_reloads_the_sound_graph() {
        let path = temporary_project_path("store");
        let (store, mut studio) = ProjectStore::open(path.clone()).expect("new store");
        studio
            .configure_sound_tool(2, "instrument", 201, None, "preset", "Surge Lead")
            .expect("valid graph edit");
        store.save(studio.project()).expect("saved graph");

        let (_, reloaded) = ProjectStore::open(path.clone()).expect("reloaded store");
        assert!(reloaded.to_json().contains("\"preset\":\"Surge Lead\""));
        fs::remove_file(path).expect("remove test graph");
    }

    #[test]
    fn reports_invalid_graph_files_without_overwriting_them() {
        let path = temporary_project_path("invalid");
        fs::write(&path, b"{not json}\n").expect("write invalid graph");
        let error = match ProjectStore::open(path.clone()) {
            Ok(_) => panic!("invalid graph must fail"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::read_to_string(&path).unwrap(), "{not json}\n");
        fs::remove_file(path).expect("remove test graph");
    }

    #[test]
    fn rejects_an_invalid_candidate_before_replacing_the_project() {
        let path = temporary_project_path("invalid-save");
        let (store, studio) = ProjectStore::open(path.clone()).expect("new store");
        let original = fs::read_to_string(&path).expect("stored graph");
        let mut project = studio.project().clone();
        project.tracks[0].routing.output = "effect:999".to_owned();

        let error = store.save(&project).expect_err("invalid graph must fail");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        fs::remove_file(path).expect("remove test graph");
    }

    #[test]
    fn rejects_an_oversized_candidate_before_replacing_the_project() {
        let path = temporary_project_path("oversized-save");
        let (store, studio) = ProjectStore::open(path.clone()).expect("new store");
        let original = fs::read_to_string(&path).expect("stored graph");
        let mut project = studio.project().clone();
        project.name = "x".repeat(MAX_PROJECT_BYTES as usize);

        let error = store.save(&project).expect_err("oversized graph must fail");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        fs::remove_file(path).expect("remove test graph");
    }
}
