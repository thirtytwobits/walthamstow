//! Copyright (c) Scott A Dixon
//!
//! The two files a log document is kept in, and the revision they give. The head file is one JSON
//! object. The log file holds one record per line, each a `StoredRecord` as JSON. A line counts
//! once the head's next position is past it.
//!
//! The chain is the SHA-256, over the committed lines in order, of the previous chain value and
//! the line, without its newline; the chain of no lines is the SHA-256 of nothing. A revision is
//! the head's incarnation, tagged with the SHA-256 of the head file's digest and the chain.

use crate::fault::LogFault;
use crate::types::{Position, Revision, StoredRecord};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;
use walthamstow_kernel::hashing::sha256_hex;

/// What the head file holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct HeadFile {
    /// The product's format for the document.
    pub format: u32,
    /// The document's incarnation, which `create` chose.
    pub incarnation: Uuid,
    /// How many writes the store has made to the document. Every write advances it, so no two
    /// writes leave the same head file.
    pub writes: u64,
    /// The position the next record appended is given. A line counts once this is past it.
    pub next_position: Position,
    /// The product's head.
    pub head: Value,
}

impl HeadFile {
    /// The head file's bytes: the object, indented, and a newline.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = serde_json::to_vec_pretty(self).expect("a head file serialises");
        bytes.push(b'\n');
        bytes
    }

    /// The head `bytes` hold, or why they are not one.
    pub fn parse(bytes: &[u8]) -> Result<Self, LogFault> {
        serde_json::from_slice(bytes).map_err(|error| LogFault::Invalid {
            detail: format!("the head file is not a head: {error}"),
        })
    }
}

/// `record` as a line of the log file, with its newline.
pub(super) fn line(record: &StoredRecord) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(record).expect("a record serialises");
    bytes.push(b'\n');
    bytes
}

/// The chain of a log of no lines.
pub(super) fn empty_chain() -> String {
    sha256_hex(b"")
}

/// The chain after `line`, without its newline, follows `chain`.
pub(super) fn extend_chain(chain: &str, line: &[u8]) -> String {
    let mut bytes = Vec::with_capacity(chain.len() + line.len());
    bytes.extend_from_slice(chain.as_bytes());
    bytes.extend_from_slice(line);
    sha256_hex(&bytes)
}

/// The revision of the document whose head file holds `head` and whose log has `chain`.
pub(super) fn revision(incarnation: Uuid, head: &[u8], chain: &str) -> Revision {
    let mut bytes = sha256_hex(head).into_bytes();
    bytes.extend_from_slice(chain.as_bytes());
    Revision::new(incarnation, sha256_hex(&bytes)).expect("a digest is a tag")
}

/// What the committed lines of a log file give.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Committed {
    pub chain: String,
    /// The length in bytes of the committed lines, with the newline of each that has one.
    pub length: u64,
    /// Whether the last committed line ends with a newline. Only the final line can lack one.
    pub terminated: bool,
}

/// A log file as read: its committed records and what their lines give.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Log {
    pub records: Vec<StoredRecord>,
    pub committed: Committed,
}

/// The log file `bytes` hold under a head whose next position is `next`. Every line must be a
/// record, and positions must increase along the log. A line at or past `next` is uncommitted
/// and passed over; so is a final line without a newline that is not a record.
pub(super) fn parse_log(bytes: &[u8], next: Position) -> Result<Log, LogFault> {
    let mut records = Vec::new();
    let mut committed = Committed {
        chain: empty_chain(),
        length: 0,
        terminated: true,
    };
    let mut last: Option<Position> = None;
    let mut offset = 0;
    let mut number = 0;
    while offset < bytes.len() {
        number += 1;
        let (line, end, terminated) = match bytes[offset..].iter().position(|&byte| byte == b'\n') {
            Some(index) => (&bytes[offset..offset + index], offset + index + 1, true),
            None => (&bytes[offset..], bytes.len(), false),
        };
        offset = end;
        let record = match serde_json::from_slice::<StoredRecord>(line) {
            Ok(record) => record,
            // A final line a write did not finish.
            Err(_) if !terminated => break,
            Err(error) => {
                return Err(LogFault::Invalid {
                    detail: format!("line {number} of the log is not a record: {error}"),
                })
            }
        };
        if !terminated && record.position >= next {
            break;
        }
        if let Some(last) = last.filter(|last| record.position <= *last) {
            return Err(LogFault::Invalid {
                detail: format!(
                    "line {number} of the log is at position {}, which does not follow {last}",
                    record.position
                ),
            });
        }
        last = Some(record.position);
        if record.position < next {
            committed = Committed {
                chain: extend_chain(&committed.chain, line),
                length: end as u64,
                terminated,
            };
            records.push(record);
        }
    }
    Ok(Log { records, committed })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use clerkenwell_events::{ActorKind, Principal};

    fn record(position: u64) -> StoredRecord {
        StoredRecord {
            position: Position::new(position),
            author: Principal::new("writer", ActorKind::Agent),
            recorded: DateTime::<Utc>::UNIX_EPOCH,
            blobs: Vec::new(),
            payload: Value::from(position),
        }
    }

    fn log(positions: &[u64]) -> Vec<u8> {
        positions
            .iter()
            .flat_map(|position| line(&record(*position)))
            .collect()
    }

    #[test]
    fn a_head_file_reads_back_and_refuses_what_is_not_a_head() {
        let head = HeadFile {
            format: 3,
            incarnation: Uuid::new_v4(),
            writes: 2,
            next_position: Position::new(7),
            head: serde_json::json!({ "title": "one" }),
        };
        assert_eq!(HeadFile::parse(&head.to_bytes()).unwrap(), head);
        for bytes in [&b"{}"[..], b"not json", br#"{"format":1,"extra":true}"#] {
            assert!(matches!(
                HeadFile::parse(bytes),
                Err(LogFault::Invalid { .. })
            ));
        }
    }

    #[test]
    fn lines_at_or_past_the_next_position_are_passed_over() {
        let bytes = log(&[0, 2, 5, 6]);
        let parsed = parse_log(&bytes, Position::new(5)).unwrap();
        assert_eq!(parsed.records, [record(0), record(2)]);
        assert_eq!(
            parsed.committed,
            parse_log(&log(&[0, 2]), Position::new(5))
                .unwrap()
                .committed
        );
        assert_eq!(parsed.committed.length, log(&[0, 2]).len() as u64);
    }

    #[test]
    fn a_final_line_a_write_did_not_finish_is_passed_over() {
        let mut bytes = log(&[0, 1]);
        let whole = parse_log(&bytes, Position::new(9)).unwrap();
        bytes.extend_from_slice(&line(&record(2))[..10]);
        assert_eq!(parse_log(&bytes, Position::new(9)).unwrap(), whole);
    }

    #[test]
    fn a_final_committed_line_without_a_newline_counts_unterminated() {
        let mut bytes = log(&[0, 1]);
        bytes.pop();
        let parsed = parse_log(&bytes, Position::new(2)).unwrap();
        assert_eq!(parsed.records, [record(0), record(1)]);
        assert!(!parsed.committed.terminated);
        assert_eq!(
            parsed.committed.chain,
            parse_log(&log(&[0, 1]), Position::new(2))
                .unwrap()
                .committed
                .chain
        );
    }

    #[test]
    fn a_line_that_is_not_a_record_or_positions_out_of_order_are_invalid() {
        let mut garbage = log(&[0]);
        garbage.extend_from_slice(b"not a record\n");
        garbage.extend_from_slice(&log(&[1]));
        let mut blank = log(&[0]);
        blank.push(b'\n');
        for bytes in [garbage, blank, log(&[1, 0]), log(&[1, 1]), log(&[0, 5, 3])] {
            assert!(
                matches!(
                    parse_log(&bytes, Position::new(9)),
                    Err(LogFault::Invalid { .. })
                ),
                "{}",
                String::from_utf8_lossy(&bytes)
            );
        }
    }

    #[test]
    fn the_chain_extends_line_by_line_and_two_copies_agree() {
        let bytes = log(&[0, 1, 2]);
        let parsed = parse_log(&bytes, Position::new(3)).unwrap();
        let mut chain = empty_chain();
        for position in 0..3 {
            let line = line(&record(position));
            chain = extend_chain(&chain, &line[..line.len() - 1]);
        }
        assert_eq!(parsed.committed.chain, chain);
        assert_eq!(parse_log(&bytes.clone(), Position::new(3)).unwrap(), parsed);
        assert_ne!(
            parse_log(&log(&[0, 1]), Position::new(3))
                .unwrap()
                .committed
                .chain,
            chain
        );
    }

    #[test]
    fn a_revision_depends_on_the_head_file_and_the_chain() {
        let incarnation = Uuid::new_v4();
        let chain = empty_chain();
        let one = revision(incarnation, b"one", &chain);
        assert_eq!(one, revision(incarnation, b"one", &chain));
        assert_ne!(one, revision(incarnation, b"two", &chain));
        assert_ne!(
            one,
            revision(incarnation, b"one", &extend_chain(&chain, b"x"))
        );
        assert_ne!(one, revision(Uuid::new_v4(), b"one", &chain));
    }
}
