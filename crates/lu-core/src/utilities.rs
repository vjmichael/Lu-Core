//! Utilities disclosed on demand.
//!
//! The model always has the five basic tools (commands, filesystem,
//! universal read, universal write, web search). A utility is a shortcut
//! that replaces a page of generated code with one checked call, and too
//! many tools confuse a model, so Lu shows a utility only when:
//!
//! - the task calls for it: the user's turn or the files in play match the
//!   utility's trigger (a `.pdf` mentioned, "make a Word document"), and
//! - for a *helper* (something the command line can also do), the model's
//!   profile says it needs the help: its record on that kind of task is
//!   unproven or weak. A model with a proven record never sees helpers; it
//!   does the work with the basic tools.
//!
//! A *required* utility (something the basic tools cannot do well, such as
//! reading one page of a PDF) is shown to any model whose task calls for it.
//!
//! Disclosure decides only what the model is shown. Every utility call
//! still goes through the origin gate, snapshots and checks like any action.

use crate::profile::{ModelRef, ProfileError, ProfileStore, TaskKind, MIN_SAMPLES};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    /// The basic tools cannot do this well; shown whenever the task calls for it.
    Required,
    /// A shortcut for work the command line can do; shown only to a model
    /// that needs the help.
    Helper,
}

/// When a task calls for a utility.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trigger {
    /// File extensions (lowercase, no dot) mentioned in the turn or in play.
    pub extensions: &'static [&'static str],
    /// Phrases in the user's turn (lowercase; matched on word boundaries).
    pub phrases: &'static [&'static str],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UtilitySpec {
    /// Tool name the model calls, e.g. `lu_read_document`.
    pub id: &'static str,
    /// What it does, in the model's tool list.
    pub description: &'static str,
    /// Parameters as JSON Schema (JSON is allowed at the model boundary).
    pub parameters: &'static str,
    pub need: Need,
    /// The profile task kind that decides whether a helper is needed.
    pub kind: TaskKind,
    pub trigger: Trigger,
}

/// Document formats the reader extracts.
pub const DOCUMENT_EXTENSIONS: &[&str] =
    &["pdf", "docx", "docm", "xlsx", "xlsm", "xls", "xlsb", "pptx", "pptm", "odt", "ods", "odp", "epub", "rtf"];

/// The starter set built so far. Each entry is implemented and tested; a
/// utility is added here only when it is.
pub const STARTER: &[UtilitySpec] = &[
UtilitySpec {
    id: "lu_read_document",
    description: "Read one page, sheet, slide (or its speaker notes) or chapter of a document \
                  (PDF, Word, Excel, PowerPoint, OpenDocument, EPUB, RTF). The plain file read \
                  already returns a document's whole text; use this for one part of a long one.",
    parameters: r#"{"type":"object","properties":{"path":{"type":"string"},"selector":{"type":"string","description":"page:N, sheet:NAME, slide:N, notes:N or chapter:N"},"max_chars":{"type":"integer","minimum":1,"maximum":200000}},"required":["path","selector"],"additionalProperties":false}"#,
    need: Need::Required,
    kind: TaskKind::FileWork,
    trigger: Trigger {
        extensions: DOCUMENT_EXTENSIONS,
        // Creating a file does not call for reading one: only words about
        // reading parts of a document, and document files, trigger this.
        phrases: &["pdf", "epub", "page", "chapter", "speaker notes"],
    },
},
UtilitySpec {
    id: "lu_create_word_document",
    description: "Create a Word document (.docx) from headings, paragraphs, bullets, tables and page breaks.                   Lu checks the file opens and holds every block before and after writing it.",
    parameters: r#"{"type":"object","properties":{"path":{"type":"string","description":"ends in .docx"},"replace":{"type":"boolean"},"title":{"type":"string"},"blocks":{"type":"array","minItems":1,"items":{"oneOf":[{"type":"object","properties":{"type":{"const":"heading"},"level":{"type":"integer","minimum":1,"maximum":3},"text":{"type":"string"}},"required":["type","level","text"],"additionalProperties":false},{"type":"object","properties":{"type":{"enum":["paragraph","bullet"]},"text":{"type":"string"}},"required":["type","text"],"additionalProperties":false},{"type":"object","properties":{"type":{"const":"table"},"rows":{"type":"array","items":{"type":"array","items":{"type":"string"}}}},"required":["type","rows"],"additionalProperties":false},{"type":"object","properties":{"type":{"const":"page_break"}},"required":["type"],"additionalProperties":false}]}}},"required":["path","blocks"],"additionalProperties":false}"#,
    need: Need::Helper,
    kind: TaskKind::FileWork,
    trigger: Trigger { extensions: &["docx"], phrases: &["word document", "word doc", "word file", ".docx", "docx"] },
},
UtilitySpec {
    id: "lu_create_presentation",
    description: "Create a PowerPoint presentation (.pptx): slides with a title, bullet points and speaker notes.                   Lu checks the file opens and holds every slide before and after writing it.",
    parameters: r#"{"type":"object","properties":{"path":{"type":"string","description":"ends in .pptx"},"replace":{"type":"boolean"},"slides":{"type":"array","minItems":1,"maxItems":500,"items":{"type":"object","properties":{"title":{"type":"string"},"bullets":{"type":"array","items":{"type":"string"}},"notes":{"type":"string"}},"required":["title"],"additionalProperties":false}}},"required":["path","slides"],"additionalProperties":false}"#,
    need: Need::Helper,
    kind: TaskKind::FileWork,
    trigger: Trigger { extensions: &["pptx"], phrases: &["powerpoint", "presentation", "slide deck", "slides", "deck", "pptx"] },
},
UtilitySpec {
    id: "lu_create_spreadsheet",
    description: "Create an Excel workbook (.xlsx): named sheets of rows; a cell is a number, true/false, text,                   null (empty) or {\"formula\": \"SUM(B2:B9)\"}. Excel calculates formulas when it opens the file.                   Lu checks the file opens and holds every sheet before and after writing it.",
    parameters: r#"{"type":"object","properties":{"path":{"type":"string","description":"ends in .xlsx"},"replace":{"type":"boolean"},"sheets":{"type":"array","minItems":1,"items":{"type":"object","properties":{"name":{"type":"string","minLength":1,"maxLength":31},"rows":{"type":"array","items":{"type":"array","items":{"oneOf":[{"type":"number"},{"type":"boolean"},{"type":"string"},{"type":"null"},{"type":"object","properties":{"formula":{"type":"string"}},"required":["formula"],"additionalProperties":false}]}}}},"required":["name","rows"],"additionalProperties":false}}},"required":["path","sheets"],"additionalProperties":false}"#,
    need: Need::Helper,
    kind: TaskKind::FileWork,
    trigger: Trigger { extensions: &["xlsx"], phrases: &["spreadsheet", "excel", "workbook", "xlsx"] },
},
UtilitySpec {
    id: "lu_list_archive",
    description: "List what is inside an archive (zip, tar, tar.gz, tar.xz, tar.bz2, gz, 7z, iso) without extracting it,                   including entries that extraction would skip as unsafe.",
    parameters: r#"{"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}"#,
    need: Need::Required,
    kind: TaskKind::FileWork,
    trigger: Trigger { extensions: ARCHIVE_EXTENSIONS, phrases: &["archive", "unzip", "extract", "zip file", "tarball", "disc image", "iso"] },
},
UtilitySpec {
    id: "lu_extract_archive",
    description: "Extract an archive (zip, tar, tar.gz, tar.xz, tar.bz2, gz, 7z, iso) into a new folder. Works the same on                   Windows and Linux. Unsafe entries (paths escaping the folder, links, device names) are skipped and listed;                   if anything fails, nothing is left half-extracted.",
    parameters: r#"{"type":"object","properties":{"path":{"type":"string"},"destination":{"type":"string","description":"a folder that does not exist yet"}},"required":["path","destination"],"additionalProperties":false}"#,
    need: Need::Required,
    kind: TaskKind::FileWork,
    trigger: Trigger { extensions: ARCHIVE_EXTENSIONS, phrases: &["unzip", "extract", "unpack", "decompress"] },
},
];

/// Archive formats Lu lists and extracts.
pub const ARCHIVE_EXTENSIONS: &[&str] = &["zip", "tar", "gz", "tgz", "xz", "txz", "bz2", "tbz2", "tbz", "7z", "iso"];

/// What Lu knows about the task when it builds the model's tool list.
#[derive(Debug, Clone, Default)]
pub struct TaskSignals {
    /// The user's own words this turn.
    pub user_text: String,
    /// Files in play: attached, named earlier in this task, or just read.
    pub files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Disclosed {
    pub spec: UtilitySpec,
    /// Why it is shown, for the record.
    pub reason: String,
}

fn extension(word: &str) -> Option<String> {
    let w = word.trim_matches(|c: char| !c.is_alphanumeric() && c != '.' && c != '_' && c != '-' && c != '/' && c != '\\');
    let (stem, ext) = w.rsplit_once('.')?;
    (!stem.is_empty() && !ext.is_empty() && ext.len() <= 5 && ext.chars().all(|c| c.is_ascii_alphanumeric()))
        .then(|| ext.to_ascii_lowercase())
}

fn has_phrase(text: &str, phrase: &str) -> bool {
    let mut from = 0;
    while let Some(at) = text[from..].find(phrase) {
        let start = from + at;
        let end = start + phrase.len();
        let before = text[..start].chars().next_back().is_none_or(|c| !c.is_alphanumeric());
        let after = text[end..].chars().next().is_none_or(|c| !c.is_alphanumeric());
        if before && after {
            return true;
        }
        from = end;
    }
    false
}

/// Why the task calls for this trigger, or `None`.
pub fn called_for(trigger: &Trigger, signals: &TaskSignals) -> Option<String> {
    let text = signals.user_text.to_lowercase();
    for word in text.split_whitespace() {
        if let Some(ext) = extension(word) {
            if trigger.extensions.contains(&ext.as_str()) {
                return Some(format!("the turn names a .{ext} file"));
            }
        }
    }
    for f in &signals.files {
        if let Some(ext) = extension(f) {
            if trigger.extensions.contains(&ext.as_str()) {
                return Some(format!("a .{ext} file is in play"));
            }
        }
    }
    trigger.phrases.iter().find(|p| has_phrase(&text, p)).map(|p| format!("the turn mentions \"{p}\""))
}

/// The utilities to show this model for this task, from `catalog`
/// (normally [`STARTER`]).
pub fn disclose(
    catalog: &[UtilitySpec],
    signals: &TaskSignals,
    profile: &ProfileStore,
    model: &ModelRef,
) -> Result<Vec<Disclosed>, ProfileError> {
    let mut shown = Vec::new();
    for spec in catalog {
        let Some(why) = called_for(&spec.trigger, signals) else { continue };
        let reason = match spec.need {
            Need::Required => why,
            Need::Helper => {
                let record = profile.stats(model, spec.kind)?.plain;
                if record.samples() < MIN_SAMPLES {
                    format!("{why}; this model's {} record is unproven ({} checked)", spec.kind.as_str(), record.samples())
                } else if record.struggling() {
                    format!("{why}; this model passed {} of {} {} tasks", record.passed, record.samples(), spec.kind.as_str())
                } else {
                    continue; // proven: the basic tools are enough
                }
            }
        };
        shown.push(Disclosed { spec: *spec, reason });
    }
    Ok(shown)
}
