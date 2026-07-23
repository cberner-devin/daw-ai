use std::path::{Path, PathBuf};
use std::{env, fs};

pub(crate) const FACTORY_PREFIX: &str = "Factory/";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Preset {
    pub(crate) id: String,
    pub(crate) category: String,
    pub(crate) name: String,
    pub(crate) path: PathBuf,
}

pub(crate) fn catalog() -> Vec<Preset> {
    let Some(root) = factory_root() else {
        return Vec::new();
    };
    let mut presets = Vec::new();
    collect_presets(&root, &root, &mut presets);
    presets.sort_by(|left, right| {
        left.category
            .cmp(&right.category)
            .then_with(|| left.name.cmp(&right.name))
    });
    presets
}

pub(crate) fn find(id: &str) -> Option<Preset> {
    catalog().into_iter().find(|preset| preset.id == id)
}

pub(crate) fn is_factory_id(value: &str) -> bool {
    let Some(relative) = value.strip_prefix(FACTORY_PREFIX) else {
        return false;
    };
    !relative.is_empty()
        && !relative.starts_with('/')
        && !relative.ends_with('/')
        && relative
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

fn factory_root() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = env::var_os("DAW_AI_SURGE_PRESET_DIR") {
        candidates.push(PathBuf::from(path));
    }
    if let Some(path) = env::var_os("SURGE_DATA_HOME") {
        let path = PathBuf::from(path);
        candidates.push(path.join("patches_factory"));
        candidates.push(path.join("resources/data/patches_factory"));
    }
    candidates.push(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("vendor/surge-sys/sbmod/surge/resources/data/patches_factory"),
    );
    candidates.into_iter().find(|path| path.is_dir())
}

fn collect_presets(root: &Path, directory: &Path, presets: &mut Vec<Preset>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_presets(root, &path, presets);
            continue;
        }
        if path.extension().and_then(|extension| extension.to_str()) != Some("fxp") {
            continue;
        }
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let Some(name) = path.file_stem().and_then(|name| name.to_str()) else {
            continue;
        };
        let category = relative
            .parent()
            .map(|parent| parent.to_string_lossy().replace('\\', "/"))
            .filter(|category| !category.is_empty())
            .unwrap_or_else(|| "Uncategorized".to_owned());
        presets.push(Preset {
            id: format!("{FACTORY_PREFIX}{category}/{name}"),
            category,
            name: name.to_owned(),
            path,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn factory_ids_are_stable_and_safe() {
        assert!(is_factory_id("Factory/Pads/Flux Capacitor"));
        assert!(!is_factory_id("Factory/../secret"));
        assert!(!is_factory_id("Factory/Pads/"));
    }

    #[test]
    fn checked_out_factory_catalog_is_discoverable() {
        let presets = catalog();
        assert!(presets.len() > 100);
        assert!(
            presets
                .iter()
                .any(|preset| preset.id == "Factory/Pads/Flux Capacitor")
        );
    }
}
