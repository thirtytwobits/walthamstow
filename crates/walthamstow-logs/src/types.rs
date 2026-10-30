//! Copyright (c) Scott A Dixon
//!
//! What a log document holds, and what a write and a read carry.

use crate::fault::LogFault;
use chrono::{DateTime, Utc};
use clerkenwell_events::Principal;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use std::str::FromStr;
use uuid::Uuid;
use walthamstow_kernel::{DocumentId, KernelError, KernelResult};

/// Where a record stands in its log. The store gives positions; they increase along the log and
/// need not be contiguous.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Position(u64);

impl Position {
    /// The position of the first record of a log that has never held one.
    pub const FIRST: Position = Position(0);

    pub const fn new(position: u64) -> Self {
        Self(position)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    /// The position after this one.
    pub fn next(self) -> KernelResult<Position> {
        self.0
            .checked_add(1)
            .map(Position)
            .ok_or_else(|| KernelError::invalid_request("A log has no position after the last."))
    }
}

impl fmt::Display for Position {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// A log document as one write left it. A revision carries the document's incarnation, which
/// `create` chooses, and a tag the adapter chooses so that it never gives one revision twice: a
/// count of the document's writes, or a digest of what the document holds. It is written as
/// `<incarnation>.<tag>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Revision {
    incarnation: Uuid,
    tag: String,
}

impl Revision {
    /// The revision of `incarnation` tagged `tag`, which is one or more ASCII lowercase letters
    /// or digits.
    pub fn new(incarnation: Uuid, tag: impl Into<String>) -> KernelResult<Self> {
        let tag = tag.into();
        if tag.is_empty()
            || !tag
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'z'))
        {
            return Err(KernelError::invalid_request(format!(
                "{tag:?} is not a revision tag: one or more ASCII lowercase letters or digits."
            )));
        }
        Ok(Self { incarnation, tag })
    }

    /// The document's incarnation, which `create` chose.
    pub fn incarnation(&self) -> Uuid {
        self.incarnation
    }

    /// The tag the adapter gave this revision.
    pub fn tag(&self) -> &str {
        &self.tag
    }
}

impl fmt::Display for Revision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}", self.incarnation, self.tag)
    }
}

impl FromStr for Revision {
    type Err = KernelError;

    fn from_str(text: &str) -> KernelResult<Self> {
        let invalid = || KernelError::invalid_request(format!("{text:?} is not a log revision."));
        let (incarnation, tag) = text.split_once('.').ok_or_else(invalid)?;
        let revision = Self::new(Uuid::try_parse(incarnation).map_err(|_| invalid())?, tag)
            .map_err(|_| invalid())?;
        // A revision has one spelling, the one it is written with.
        if revision.to_string() != text {
            return Err(invalid());
        }
        Ok(revision)
    }
}

impl Serialize for Revision {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Revision {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// Names one write. The writer chooses it, and repeats it, over the same base, to retry a write
/// that may have taken effect.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WriteId(String);

impl WriteId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for WriteId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Whether `name` can name a kind of blob, and so a directory: one or more lowercase ASCII
/// letters, digits, `-` or `_`, and no device name Windows reserves.
fn is_kind_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_'))
        && !walthamstow_kernel::files::is_windows_device_name(name)
}

/// A kind of blob the product registers: its name, which names the directory its blobs are
/// kept in, and the extension their files take.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlobKind {
    name: String,
    extension: String,
}

impl BlobKind {
    /// The kind named `name`, whose files take `extension`. A name is one or more lowercase ASCII
    /// letters, digits, `-` or `_`, and no device name Windows reserves; an extension is one or
    /// more lowercase ASCII letters or digits.
    pub fn new(name: impl Into<String>, extension: impl Into<String>) -> KernelResult<Self> {
        let name = name.into();
        let extension = extension.into();
        if !is_kind_name(&name) {
            return Err(KernelError::invalid_request(format!(
                "{name:?} is not a blob kind: one or more lowercase ASCII letters, digits, '-' \
                 or '_', and no device name."
            )));
        }
        if extension.is_empty()
            || !extension
                .bytes()
                .all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9'))
        {
            return Err(KernelError::invalid_request(format!(
                "{extension:?} is not a blob extension: one or more lowercase ASCII letters or \
                 digits."
            )));
        }
        Ok(Self { name, extension })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn extension(&self) -> &str {
        &self.extension
    }
}

/// A blob a record names: its kind, and the lowercase hex SHA-256 of its bytes. It is written
/// as `<kind>/<sha256>`, as its file is laid out. The log store never follows one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct BlobRef {
    kind: String,
    sha256: String,
}

impl BlobRef {
    /// The blob of `kind` named by `sha256`, which must be 64 lowercase hex digits.
    pub fn new(kind: impl Into<String>, sha256: impl Into<String>) -> KernelResult<Self> {
        let kind = kind.into();
        let sha256 = sha256.into();
        if !is_kind_name(&kind) {
            return Err(KernelError::invalid_request(format!(
                "{kind:?} is not a blob kind."
            )));
        }
        if sha256.len() != 64
            || !sha256
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        {
            return Err(KernelError::invalid_request(format!(
                "{sha256:?} is not a lowercase hex SHA-256 digest."
            )));
        }
        Ok(Self { kind, sha256 })
    }

    /// The name of the blob's kind.
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// The lowercase hex SHA-256 of the blob's bytes.
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
}

impl FromStr for BlobRef {
    type Err = KernelError;

    fn from_str(text: &str) -> KernelResult<Self> {
        let (kind, sha256) = text.split_once('/').ok_or_else(|| {
            KernelError::invalid_request(format!("{text:?} is not a blob reference."))
        })?;
        Self::new(kind, sha256)
    }
}

impl TryFrom<String> for BlobRef {
    type Error = KernelError;

    fn try_from(text: String) -> KernelResult<Self> {
        text.parse()
    }
}

impl From<BlobRef> for String {
    fn from(blob: BlobRef) -> Self {
        blob.to_string()
    }
}

impl fmt::Display for BlobRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.kind, self.sha256)
    }
}

/// A record a commit appends. The store gives its position and the time it was recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewRecord {
    /// Who wrote the record, as the writer names it.
    pub author: Principal,
    pub blobs: Vec<BlobRef>,
    pub payload: Value,
}

/// A record as the store keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredRecord {
    pub position: Position,
    /// Who wrote the record, as the writer named it.
    pub author: Principal,
    /// When the store recorded it, on the clock the store is given.
    pub recorded: DateTime<Utc>,
    pub blobs: Vec<BlobRef>,
    pub payload: Value,
}

/// A whole log document, for `create` and `replace`. Its records carry their positions, which
/// increase along the log. The document's next position is one past the highest of them, or
/// [`Position::FIRST`] when there are none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogImage {
    pub id: WriteId,
    pub format: u32,
    pub head: Value,
    pub records: Vec<StoredRecord>,
}

/// Records to append and, when the head changes, the head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogCommit {
    pub id: WriteId,
    pub head: Option<Value>,
    pub records: Vec<NewRecord>,
}

/// A log document's head, as `head` reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredHead {
    pub format: u32,
    pub head: Value,
    /// The position the next record appended is given.
    pub next: Position,
    pub revision: Revision,
}

/// One document of an entity, as `list` finds it: its revision, or why it cannot be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub document: DocumentId,
    pub state: Result<Revision, LogFault>,
}

impl LogImage {
    /// The next position the image leaves, refused as an invalid request when its positions do
    /// not increase along the log.
    pub(crate) fn next_position(&self) -> KernelResult<Position> {
        let mut next = Position::FIRST;
        for record in &self.records {
            if record.position < next {
                return Err(KernelError::invalid_request(format!(
                    "The image's record at position {} does not follow the record before it.",
                    record.position
                )));
            }
            next = record.position.next()?;
        }
        Ok(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_revision_reads_back_from_its_text() {
        let revision = Revision::new(Uuid::new_v4(), "a".repeat(64)).unwrap();
        let text = revision.to_string();
        assert_eq!(text.parse::<Revision>().unwrap(), revision);
        assert_eq!(
            serde_json::from_value::<Revision>(serde_json::to_value(&revision).unwrap()).unwrap(),
            revision
        );
    }

    #[test]
    fn a_revision_has_one_spelling() {
        let incarnation = Uuid::new_v4();
        for tag in ["", "+1", "A", "1.2", "a b"] {
            assert!(Revision::new(incarnation, tag).is_err(), "{tag:?}");
        }
        let incarnation = incarnation.to_string();
        for text in [
            format!("{incarnation}."),
            format!("{incarnation}.+1"),
            format!("{incarnation}.A"),
            format!("{}.1", incarnation.to_uppercase()),
            format!("{}.1", incarnation.replace('-', "")),
            incarnation.to_string(),
            String::new(),
        ] {
            assert!(text.parse::<Revision>().is_err(), "{text}");
        }
    }

    #[test]
    fn a_blob_is_named_by_its_kind_and_a_lowercase_sha_256_digest() {
        let blob = BlobRef::new("capture", "a".repeat(64)).unwrap();
        assert_eq!(
            (blob.kind(), blob.sha256()),
            ("capture", "a".repeat(64).as_str())
        );
        assert_eq!(blob.to_string().parse::<BlobRef>().unwrap(), blob);
        assert_eq!(
            serde_json::from_value::<BlobRef>(serde_json::to_value(&blob).unwrap()).unwrap(),
            blob
        );
        for digest in ["A".repeat(64), "a".repeat(63), "g".repeat(64)] {
            assert!(BlobRef::new("capture", digest).is_err());
        }
        for kind in ["", "Capture", "con", "a/b", ".hidden", "a b"] {
            assert!(BlobRef::new(kind, "a".repeat(64)).is_err(), "{kind:?}");
        }
        for text in ["x", "a".repeat(64).as_str(), "capture/"] {
            assert!(text.parse::<BlobRef>().is_err(), "{text:?}");
        }
    }

    #[test]
    fn a_kind_is_named_as_a_directory_with_an_extension_for_its_files() {
        let kind = BlobKind::new("capture", "png").unwrap();
        assert_eq!((kind.name(), kind.extension()), ("capture", "png"));
        for (name, extension) in [
            ("", "png"),
            ("Capture", "png"),
            ("com1", "png"),
            ("a.b", "png"),
            ("capture", ""),
            ("capture", "PNG"),
            ("capture", ".png"),
            ("capture", "tar.gz"),
        ] {
            assert!(
                BlobKind::new(name, extension).is_err(),
                "{name:?} {extension:?}"
            );
        }
    }
}
