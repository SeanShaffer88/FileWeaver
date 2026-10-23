use std::fmt;

use regex::Regex;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::OffsetDateTime;

use crate::domain::{FileSnapshot, StageValue};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RulePipeline {
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Rule {
    ReplaceText {
        find: String,
        replacement: String,
        replace_all: bool,
    },
    ReplaceRegex {
        pattern: String,
        replacement: String,
    },
    AddPrefix {
        text: String,
    },
    AddSuffixBeforeExtension {
        text: String,
    },
    TrimAndCollapseWhitespace,
    ChangeCase {
        style: CaseStyle,
    },
    /// Replaces the final extension. An empty (or dot-only) extension removes
    /// it. A leading dot in the value is ignored so both `jpg` and `.jpg`
    /// mean the same thing.
    ChangeExtension {
        extension: String,
    },
    AppendCounter {
        separator: String,
        start_at: u64,
        width: u8,
    },
    PrefixDate {
        source: DateSource,
        /// A `time` format description, for example `[year]-[month]-[day]`.
        format: String,
        separator: String,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum CaseStyle {
    Lower,
    Upper,
    Title,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum DateSource {
    ModifiedTime,
    JpegCaptureTime,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RuleProblem {
    #[error("regular expression is invalid: {0}")]
    InvalidRegex(String),
    #[error("date format is invalid: {0}")]
    InvalidDateFormat(String),
    #[error("this rule needs JPEG Exif capture time, but the file has none")]
    MissingCaptureTime,
    #[error("this rule needs filesystem modified time, but it is unavailable")]
    MissingModifiedTime,
    #[error("formatted date could not be represented")]
    DateFormattingFailed,
}

impl RulePipeline {
    pub fn validate(&self) -> Result<(), RuleProblem> {
        for rule in &self.rules {
            match rule {
                Rule::ReplaceRegex { pattern, .. } => {
                    Regex::new(pattern)
                        .map_err(|error| RuleProblem::InvalidRegex(error.to_string()))?;
                }
                Rule::PrefixDate { format, .. } => {
                    time::format_description::parse_owned::<3>(format)
                        .map_err(|error| RuleProblem::InvalidDateFormat(error.to_string()))?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub fn apply(
        &self,
        snapshot: &FileSnapshot,
        sorted_index: usize,
    ) -> Result<(String, Vec<StageValue>), RuleProblem> {
        let mut value = snapshot.original_name.clone();
        let mut stages = Vec::with_capacity(self.rules.len());

        for rule in &self.rules {
            value = apply_rule(rule, value, snapshot, sorted_index)?;
            stages.push(StageValue {
                rule_label: rule.label(),
                value: value.clone(),
            });
        }

        Ok((value, stages))
    }
}

fn apply_rule(
    rule: &Rule,
    current: String,
    snapshot: &FileSnapshot,
    sorted_index: usize,
) -> Result<String, RuleProblem> {
    match rule {
        Rule::ReplaceText {
            find,
            replacement,
            replace_all,
        } => {
            if *replace_all {
                Ok(current.replace(find, replacement))
            } else {
                Ok(current.replacen(find, replacement, 1))
            }
        }
        Rule::ReplaceRegex {
            pattern,
            replacement,
        } => {
            let regex = Regex::new(pattern)
                .map_err(|error| RuleProblem::InvalidRegex(error.to_string()))?;
            Ok(regex.replace_all(&current, replacement).into_owned())
        }
        Rule::AddPrefix { text } => Ok(format!("{text}{current}")),
        Rule::AddSuffixBeforeExtension { text } => Ok(insert_before_extension(&current, text)),
        Rule::TrimAndCollapseWhitespace => {
            Ok(current.split_whitespace().collect::<Vec<_>>().join(" "))
        }
        Rule::ChangeCase { style } => Ok(change_case(&current, *style)),
        Rule::ChangeExtension { extension } => Ok(change_extension(&current, extension)),
        Rule::AppendCounter {
            separator,
            start_at,
            width,
        } => {
            let counter = start_at.saturating_add(u64::try_from(sorted_index).unwrap_or(u64::MAX));
            let formatted = format!("{counter:0width$}", width = usize::from(*width));
            Ok(insert_before_extension(
                &current,
                &format!("{separator}{formatted}"),
            ))
        }
        Rule::PrefixDate {
            source,
            format,
            separator,
        } => {
            let date = match source {
                DateSource::ModifiedTime => offset_time(
                    snapshot
                        .modified_unix_nanos
                        .ok_or(RuleProblem::MissingModifiedTime)?,
                )?,
                DateSource::JpegCaptureTime => parse_exif_time(
                    snapshot
                        .jpeg_exif_datetime
                        .as_deref()
                        .ok_or(RuleProblem::MissingCaptureTime)?,
                )?,
            };
            let description = time::format_description::parse_owned::<3>(format)
                .map_err(|error| RuleProblem::InvalidDateFormat(error.to_string()))?;
            let prefix = date
                .format(&description)
                .map_err(|_| RuleProblem::DateFormattingFailed)?;
            Ok(format!("{prefix}{separator}{current}"))
        }
    }
}

fn offset_time(unix_nanos: i128) -> Result<OffsetDateTime, RuleProblem> {
    OffsetDateTime::from_unix_timestamp_nanos(unix_nanos)
        .map_err(|_| RuleProblem::DateFormattingFailed)
}

fn parse_exif_time(value: &str) -> Result<OffsetDateTime, RuleProblem> {
    // Exif has no zone. We intentionally interpret it as UTC for a stable
    // filename-only representation, and document the policy instead of
    // silently applying the current computer's local time zone.
    let format = time::macros::format_description!("[year]:[month]:[day] [hour]:[minute]:[second]");
    time::PrimitiveDateTime::parse(value.trim(), &format)
        .map(|time| time.assume_utc())
        .map_err(|_| RuleProblem::DateFormattingFailed)
}

fn insert_before_extension(name: &str, suffix: &str) -> String {
    let dot = name.rfind('.');
    match dot {
        Some(position) if position > 0 => {
            let (stem, extension) = name.split_at(position);
            format!("{stem}{suffix}{extension}")
        }
        _ => format!("{name}{suffix}"),
    }
}

/// Replaces (or removes) the final extension. Files without an extension, and
/// dot-files such as `.gitignore`, are treated as having only a stem, so
/// changing their extension appends one instead of eating the leading dot.
fn change_extension(name: &str, extension: &str) -> String {
    let cleaned = extension.trim().trim_start_matches('.');
    let stem = match name.rfind('.') {
        Some(position) if position > 0 => &name[..position],
        _ => name,
    };
    if cleaned.is_empty() {
        stem.to_owned()
    } else {
        format!("{stem}.{cleaned}")
    }
}

fn change_case(value: &str, style: CaseStyle) -> String {
    match style {
        CaseStyle::Lower => value.to_lowercase(),
        CaseStyle::Upper => value.to_uppercase(),
        CaseStyle::Title => {
            let mut result = String::with_capacity(value.len());
            let mut at_word_start = true;
            for character in value.chars() {
                if character.is_alphabetic() {
                    if at_word_start {
                        result.extend(character.to_uppercase());
                    } else {
                        result.extend(character.to_lowercase());
                    }
                    at_word_start = false;
                } else {
                    result.push(character);
                    at_word_start = !character.is_numeric();
                }
            }
            result
        }
    }
}

impl Rule {
    pub fn label(&self) -> String {
        match self {
            Self::ReplaceText { .. } => "Replace text".to_owned(),
            Self::ReplaceRegex { .. } => "Replace regular expression".to_owned(),
            Self::AddPrefix { .. } => "Add prefix".to_owned(),
            Self::AddSuffixBeforeExtension { .. } => "Add suffix".to_owned(),
            Self::TrimAndCollapseWhitespace => "Trim whitespace".to_owned(),
            Self::ChangeCase { .. } => "Change case".to_owned(),
            Self::ChangeExtension { .. } => "Change extension".to_owned(),
            Self::AppendCounter { .. } => "Append counter".to_owned(),
            Self::PrefixDate { .. } => "Prefix date".to_owned(),
        }
    }
}

impl fmt::Display for CaseStyle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lower => formatter.write_str("lowercase"),
            Self::Upper => formatter.write_str("UPPERCASE"),
            Self::Title => formatter.write_str("Title Case"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{FileFingerprint, FileSnapshot};
    use std::path::PathBuf;

    fn snapshot(name: &str) -> FileSnapshot {
        FileSnapshot {
            source: PathBuf::from(name),
            original_name: name.to_owned(),
            is_directory: false,
            fingerprint: FileFingerprint {
                size_bytes: 1,
                modified_unix_nanos: Some(0),
            },
            modified_unix_nanos: Some(0),
            jpeg_exif_datetime: None,
        }
    }

    #[test]
    fn counter_is_inserted_before_extension() {
        let pipeline = RulePipeline {
            rules: vec![Rule::AppendCounter {
                separator: "_".into(),
                start_at: 7,
                width: 3,
            }],
        };
        assert_eq!(
            pipeline.apply(&snapshot("Photo.JPG"), 2).unwrap().0,
            "Photo_009.JPG"
        );
    }

    #[test]
    fn regex_is_validated_before_execution() {
        let pipeline = RulePipeline {
            rules: vec![Rule::ReplaceRegex {
                pattern: "(".into(),
                replacement: String::new(),
            }],
        };
        assert!(matches!(
            pipeline.validate(),
            Err(RuleProblem::InvalidRegex(_))
        ));
    }

    #[test]
    fn extension_change_replaces_only_the_final_extension() {
        let pipeline = RulePipeline {
            rules: vec![Rule::ChangeExtension {
                extension: ".jpeg".into(),
            }],
        };
        assert_eq!(
            pipeline.apply(&snapshot("archive.tar.gz"), 0).unwrap().0,
            "archive.tar.jpeg"
        );
    }

    #[test]
    fn empty_extension_removes_it_and_dot_files_keep_their_stem() {
        let remove = RulePipeline {
            rules: vec![Rule::ChangeExtension {
                extension: String::new(),
            }],
        };
        assert_eq!(remove.apply(&snapshot("photo.jpg"), 0).unwrap().0, "photo");
        assert_eq!(
            remove.apply(&snapshot(".gitignore"), 0).unwrap().0,
            ".gitignore"
        );

        let add = RulePipeline {
            rules: vec![Rule::ChangeExtension {
                extension: "txt".into(),
            }],
        };
        assert_eq!(
            add.apply(&snapshot(".gitignore"), 0).unwrap().0,
            ".gitignore.txt"
        );
    }
}
