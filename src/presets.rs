//! Named rule-stack presets: reusable, versioned JSON files on disk.
//!
//! A preset stores the ordered rule pipeline only. It never stores file paths,
//! so loading a preset can never act on a folder the user did not choose. The
//! format is explicitly versioned and refused when the version is unknown, and
//! writes go through a temporary file plus a rename so an interrupted save
//! cannot leave a half-written preset behind.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::rules::RulePipeline;

pub const PRESET_FORMAT: &str = "filenameweaver-preset";
pub const PRESET_FORMAT_VERSION: u32 = 1;

/// Longest slug written to disk, in bytes. The preset's display name is stored
/// inside the file, so truncating the file name loses nothing.
const SLUG_LIMIT: usize = 48;

#[derive(Debug, Error)]
pub enum PresetError {
    #[error("a preset needs a name")]
    EmptyName,
    #[error("the preset name has no characters that can be used in a file name")]
    UnusableName,
    #[error("this file is not a FilenameWeaver preset (format tag found: {0:?})")]
    WrongFormat(String),
    #[error("preset format version {0} is not supported by this version of FilenameWeaver")]
    UnsupportedVersion(u32),
    #[error("preset files must be addressed by file name only: {0}")]
    InvalidFileName(String),
    #[error("the preset file could not be read or written: {0}")]
    Io(#[from] io::Error),
    #[error("the preset file could not be parsed: {0}")]
    Json(#[from] serde_json::Error),
}

/// The portable preset file (`.json`). The envelope fields are the format
/// commitment; `pipeline` reuses the exact rule shape the planner validates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresetFile {
    pub format: String,
    pub format_version: u32,
    pub app_version: String,
    pub name: String,
    pub pipeline: RulePipeline,
}

impl PresetFile {
    pub fn new(name: impl Into<String>, pipeline: RulePipeline) -> Self {
        Self {
            format: PRESET_FORMAT.to_string(),
            format_version: PRESET_FORMAT_VERSION,
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            name: name.into(),
            pipeline,
        }
    }

    pub fn to_json(&self) -> Result<String, PresetError> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// Parses a preset file, refusing anything that is not a supported
    /// FilenameWeaver preset with a specific reason.
    pub fn from_json(text: &str) -> Result<Self, PresetError> {
        let file: PresetFile = serde_json::from_str(text)?;
        if file.format != PRESET_FORMAT {
            return Err(PresetError::WrongFormat(file.format));
        }
        if file.format_version != PRESET_FORMAT_VERSION {
            return Err(PresetError::UnsupportedVersion(file.format_version));
        }
        Ok(file)
    }
}

/// A listed preset: enough to display it and to load or delete it by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresetSummary {
    pub name: String,
    pub file_name: String,
    pub rule_count: usize,
}

/// Result of scanning the preset folder. Unreadable or foreign files are
/// reported as warnings instead of failing the whole listing, because one bad
/// file must not hide every other preset.
#[derive(Debug, Clone, Default)]
pub struct PresetListing {
    pub presets: Vec<PresetSummary>,
    pub warnings: Vec<String>,
}

/// A folder of preset files. The directory is created on the first save.
#[derive(Debug, Clone)]
pub struct PresetStore {
    directory: PathBuf,
}

impl PresetStore {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
        }
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Writes `pipeline` under `name`, replacing any preset that maps to the
    /// same file name. Distinct names can share a file name (for example
    /// `Photo clean` and `photo clean!`); the most recent save wins, and the
    /// stored display name is the one from that save.
    pub fn save(&self, name: &str, pipeline: &RulePipeline) -> Result<PresetSummary, PresetError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(PresetError::EmptyName);
        }
        let slug = slug_for(name)?;
        fs::create_dir_all(&self.directory)?;

        let preset = PresetFile::new(name, pipeline.clone());
        let text = preset.to_json()?;
        let path = self.directory.join(format!("{slug}.json"));
        let temp = self.unique_temp_path(&slug);
        fs::write(&temp, text.as_bytes())?;
        // Rename is the commit point: the preset file is either the old
        // complete version or the new complete version, never a partial one.
        if let Err(error) = fs::rename(&temp, &path) {
            let _ = fs::remove_file(&temp);
            return Err(error.into());
        }
        Ok(PresetSummary {
            name: preset.name,
            file_name: file_name_text(&path),
            rule_count: preset.pipeline.rules.len(),
        })
    }

    pub fn list(&self) -> Result<PresetListing, PresetError> {
        let mut listing = PresetListing::default();
        let reader = match fs::read_dir(&self.directory) {
            Ok(reader) => reader,
            // No presets folder yet is an empty listing, not an error.
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(listing),
            Err(error) => return Err(error.into()),
        };
        for entry in reader {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    listing
                        .warnings
                        .push(format!("Could not inspect a preset entry: {error}"));
                    continue;
                }
            };
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            match fs::read_to_string(&path)
                .map_err(PresetError::from)
                .and_then(|text| PresetFile::from_json(&text))
            {
                Ok(file) => listing.presets.push(PresetSummary {
                    name: file.name,
                    file_name: file_name_text(&path),
                    rule_count: file.pipeline.rules.len(),
                }),
                Err(error) => listing
                    .warnings
                    .push(format!("Skipped {}: {error}", path.display())),
            }
        }
        listing.presets.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.file_name.cmp(&b.file_name))
        });
        Ok(listing)
    }

    pub fn load(&self, file_name: &str) -> Result<PresetFile, PresetError> {
        let path = self.path_for(file_name)?;
        let text = fs::read_to_string(path)?;
        PresetFile::from_json(&text)
    }

    pub fn delete(&self, file_name: &str) -> Result<(), PresetError> {
        let path = self.path_for(file_name)?;
        fs::remove_file(path)?;
        Ok(())
    }

    /// Resolves a stored file name to a path inside the preset folder. Only a
    /// bare file name is accepted, so a preset file can never address a path
    /// outside the folder.
    fn path_for(&self, file_name: &str) -> Result<PathBuf, PresetError> {
        let trimmed = file_name.trim();
        let is_bare = !trimmed.is_empty()
            && !trimmed.contains('/')
            && !trimmed.contains('\\')
            && trimmed != "."
            && trimmed != ".."
            && Path::new(trimmed).components().count() == 1;
        if !is_bare {
            return Err(PresetError::InvalidFileName(file_name.to_owned()));
        }
        Ok(self.directory.join(trimmed))
    }

    fn unique_temp_path(&self, slug: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        self.directory
            .join(format!(".{slug}.{}.{nanos}.partial", std::process::id()))
    }
}

fn file_name_text(path: &Path) -> String {
    path.file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Builds a conservative, lowercase file-name slug. Unicode alphanumerics are
/// kept; spaces and dots become single dashes; everything else is dropped.
fn slug_for(name: &str) -> Result<String, PresetError> {
    let mut slug = String::with_capacity(name.len());
    for character in name.chars() {
        if character.is_alphanumeric() || character == '_' {
            slug.push(character.to_ascii_lowercase());
        } else if (character.is_whitespace() || character == '-' || character == '.')
            && !slug.ends_with('-')
        {
            slug.push('-');
        }
    }
    let trimmed = slug.trim_matches('-');
    if trimmed.is_empty() {
        return Err(PresetError::UnusableName);
    }
    let limited: String = trimmed.chars().take(SLUG_LIMIT).collect();
    Ok(limited.trim_end_matches('-').to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::{CaseStyle, Rule};
    use tempfile::tempdir;

    fn pipeline() -> RulePipeline {
        RulePipeline {
            rules: vec![
                Rule::AddPrefix {
                    text: "2026-".into(),
                },
                Rule::ChangeCase {
                    style: CaseStyle::Title,
                },
            ],
        }
    }

    #[test]
    fn roundtrip_preserves_the_rule_order() {
        let file = PresetFile::new("Photo cleanup", pipeline());
        let parsed = PresetFile::from_json(&file.to_json().unwrap()).unwrap();
        assert_eq!(parsed.format, PRESET_FORMAT);
        assert_eq!(parsed.name, "Photo cleanup");
        assert_eq!(parsed.pipeline.rules.len(), 2);
        assert!(matches!(parsed.pipeline.rules[0], Rule::AddPrefix { .. }));
        assert!(matches!(
            parsed.pipeline.rules[1],
            Rule::ChangeCase {
                style: CaseStyle::Title
            }
        ));
    }

    #[test]
    fn foreign_and_future_formats_are_refused() {
        let file = PresetFile::new("x", pipeline());
        let text = file.to_json().unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();

        value["format"] = serde_json::json!("someone-elses-format");
        assert!(matches!(
            PresetFile::from_json(&value.to_string()),
            Err(PresetError::WrongFormat(_))
        ));

        value["format"] = serde_json::json!(PRESET_FORMAT);
        value["format_version"] = serde_json::json!(99);
        assert!(matches!(
            PresetFile::from_json(&value.to_string()),
            Err(PresetError::UnsupportedVersion(99))
        ));
    }

    #[test]
    fn save_list_load_delete_roundtrip() {
        let directory = tempdir().unwrap();
        let store = PresetStore::new(directory.path().join("presets"));

        let saved = store.save("Photo cleanup", &pipeline()).unwrap();
        assert_eq!(saved.name, "Photo cleanup");
        assert_eq!(saved.rule_count, 2);
        assert_eq!(saved.file_name, "photo-cleanup.json");

        // Saving again replaces the same file rather than duplicating it.
        let changed = RulePipeline {
            rules: vec![Rule::TrimAndCollapseWhitespace],
        };
        store.save("photo clean-up", &changed).unwrap();
        let listing = store.list().unwrap();
        assert_eq!(listing.presets.len(), 2, "{:?}", listing.presets);
        assert!(listing.warnings.is_empty());

        let loaded = store.load(&saved.file_name).unwrap();
        assert_eq!(loaded.name, "Photo cleanup");
        assert_eq!(loaded.pipeline.rules.len(), 2);

        store.delete(&saved.file_name).unwrap();
        let listing = store.list().unwrap();
        assert_eq!(listing.presets.len(), 1);
        assert_eq!(listing.presets[0].name, "photo clean-up");
    }

    #[test]
    fn overwriting_one_preset_keeps_a_single_file() {
        let directory = tempdir().unwrap();
        let store = PresetStore::new(directory.path());
        store.save("Clean names", &pipeline()).unwrap();
        store.save("clean names", &RulePipeline::default()).unwrap();
        let listing = store.list().unwrap();
        assert_eq!(listing.presets.len(), 1);
        assert_eq!(listing.presets[0].rule_count, 0);
    }

    #[test]
    fn empty_and_unusable_names_are_rejected() {
        let directory = tempdir().unwrap();
        let store = PresetStore::new(directory.path());
        assert!(matches!(
            store.save("   ", &pipeline()),
            Err(PresetError::EmptyName)
        ));
        assert!(matches!(
            store.save("///", &pipeline()),
            Err(PresetError::UnusableName)
        ));
    }

    #[test]
    fn preset_files_cannot_address_paths_outside_the_folder() {
        let directory = tempdir().unwrap();
        let store = PresetStore::new(directory.path());
        for name in ["../escape.json", "sub/dir.json", "sub\\dir.json", "..", ""] {
            assert!(matches!(
                store.load(name),
                Err(PresetError::InvalidFileName(_))
            ));
            assert!(matches!(
                store.delete(name),
                Err(PresetError::InvalidFileName(_))
            ));
        }
    }

    #[test]
    fn broken_files_are_skipped_with_a_warning() {
        let directory = tempdir().unwrap();
        let store = PresetStore::new(directory.path());
        store.save("Good", &pipeline()).unwrap();
        fs::write(directory.path().join("broken.json"), "{ not json").unwrap();
        fs::write(directory.path().join("notes.txt"), "ignored").unwrap();

        let listing = store.list().unwrap();
        assert_eq!(listing.presets.len(), 1);
        assert_eq!(listing.presets[0].name, "Good");
        assert_eq!(listing.warnings.len(), 1);
        assert!(listing.warnings[0].contains("broken.json"));
    }

    #[test]
    fn missing_folder_lists_as_empty() {
        let directory = tempdir().unwrap();
        let store = PresetStore::new(directory.path().join("never-created"));
        let listing = store.list().unwrap();
        assert!(listing.presets.is_empty());
        assert!(listing.warnings.is_empty());
    }

    #[test]
    fn a_successful_save_leaves_no_temporary_file() {
        let directory = tempdir().unwrap();
        let store = PresetStore::new(directory.path());
        store.save("Keep", &pipeline()).unwrap();
        let leftovers: Vec<String> = fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| file_name_text(&entry.unwrap().path()))
            .filter(|name| name.ends_with(".partial"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }
}
