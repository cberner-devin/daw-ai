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

pub(crate) fn search<'a>(
    presets: &'a [Preset],
    query: Option<&str>,
    category: Option<&str>,
) -> Vec<&'a Preset> {
    let mut matches = presets
        .iter()
        .filter(|preset| category.is_none_or(|category| preset.category == category))
        .filter_map(|preset| {
            query
                .map(|query| preset_score(preset, query))
                .unwrap_or(Some(0))
                .map(|score| (score, preset))
        })
        .collect::<Vec<_>>();
    matches.sort_by(|(left_score, left), (right_score, right)| {
        right_score
            .cmp(left_score)
            .then_with(|| left.category.cmp(&right.category))
            .then_with(|| left.name.cmp(&right.name))
    });
    matches.into_iter().map(|(_, preset)| preset).collect()
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

fn preset_score(preset: &Preset, query: &str) -> Option<u32> {
    let query = query.trim().to_ascii_lowercase();
    if query.is_empty() {
        return Some(0);
    }
    let id = preset.id.to_ascii_lowercase();
    let category = preset.category.to_ascii_lowercase();
    let name = preset.name.to_ascii_lowercase();
    if name == query || id == query {
        return Some(1_000);
    }
    if name.contains(&query) || id.contains(&query) {
        return Some(900);
    }
    let query_words = words(&query);
    let candidate_words = words(&format!("{category} {name}"));
    if query_words
        .iter()
        .all(|query| candidate_words.iter().any(|candidate| candidate == query))
    {
        return Some(800);
    }
    let mut score = 0;
    let mut matched = false;
    for query_word in query_words {
        if let Some(distance) = candidate_words
            .iter()
            .map(|candidate| edit_distance(&query_word, candidate))
            .min()
            .filter(|distance| *distance <= 2)
        {
            score += 600 - distance as u32 * 80;
            matched = true;
        }
        if let Some(profile) = descriptor_profile(&query_word) {
            if profile.categories.iter().any(|value| *value == category) {
                score += 400;
                matched = true;
            }
            for keyword in profile.keywords {
                if name.contains(keyword) {
                    score += 80;
                }
            }
        }
    }
    matched.then_some(score)
}

struct DescriptorProfile {
    categories: &'static [&'static str],
    keywords: &'static [&'static str],
}

fn descriptor_profile(word: &str) -> Option<DescriptorProfile> {
    let bass = DescriptorProfile {
        categories: &["basses"],
        keywords: &[
            "dist", "fm", "crush", "evil", "behemoth", "doomsday", "monster", "grit", "dirty",
        ],
    };
    match word {
        "wobble" | "wub" | "growl" | "dub" | "dubstep" | "reese" | "neuro" => Some(bass),
        "warm" | "lush" | "soft" | "dreamy" => Some(DescriptorProfile {
            categories: &["pads", "polysynths"],
            keywords: &["warm", "analog", "soft", "silk", "mellow", "lush"],
        }),
        "ambient" | "atmospheric" | "spacious" | "evolving" => Some(DescriptorProfile {
            categories: &["pads", "fx"],
            keywords: &["space", "drone", "evol", "atmos", "air", "cloud"],
        }),
        "pluck" | "plucky" | "mallet" => Some(DescriptorProfile {
            categories: &["plucks"],
            keywords: &["pluck", "bell", "mallet"],
        }),
        "acid" | "squelch" | "303" => Some(DescriptorProfile {
            categories: &["basses", "sequences"],
            keywords: &["acid", "squelch", "303"],
        }),
        "cinematic" | "impact" | "riser" | "transition" => Some(DescriptorProfile {
            categories: &["fx", "pads"],
            keywords: &["impact", "rise", "cinema", "sweep", "noise"],
        }),
        _ => None,
    }
}

fn words(value: &str) -> Vec<String> {
    value
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_owned)
        .collect()
}

fn edit_distance(left: &str, right: &str) -> usize {
    let mut previous = (0..=right.len()).collect::<Vec<_>>();
    let mut current = vec![0; right.len() + 1];
    for (left_index, left_byte) in left.bytes().enumerate() {
        current[0] = left_index + 1;
        for (right_index, right_byte) in right.bytes().enumerate() {
            current[right_index + 1] = (current[right_index] + 1)
                .min(previous[right_index + 1] + 1)
                .min(previous[right_index] + usize::from(left_byte != right_byte));
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.len()]
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

    #[test]
    fn musical_descriptors_and_typos_rank_useful_presets() {
        let presets = catalog();
        for query in ["wobble", "growl", "dubstep"] {
            let matches = search(&presets, Some(query), None);
            assert!(matches.len() > 10, "{query} returned too few presets");
            assert!(
                matches
                    .iter()
                    .take(10)
                    .all(|preset| preset.category == "Basses"),
                "{query} did not prioritize basses"
            );
            assert!(
                matches.iter().take(10).any(|preset| {
                    ["dist", "fm", "crush", "behemoth", "doomsday", "evil"]
                        .iter()
                        .any(|keyword| preset.name.to_ascii_lowercase().contains(keyword))
                }),
                "{query} did not prioritize designed bass names"
            );
        }
        assert_eq!(
            search(&presets, Some("Flux Capacitro"), Some("Pads"))[0].name,
            "Flux Capacitor"
        );
    }
}
