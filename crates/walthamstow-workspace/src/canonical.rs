//! Copyright (c) Scott A Dixon
//!
//! The canonical record of collaborative documents: the store an engine publishes what its
//! working copy accepted to, and reads documents back from. A product renders each document
//! into named parts with its [`DocumentCodec`]. Every store is given the same
//! [`Publication`] and keeps what it can of it: [`FileCanonicalStore`] writes the parts as
//! workspace files, and [`MemoryCanonicalStore`] keeps the operations beside them.

mod files;
mod memory;

pub use files::{FileCanonicalStore, RENDERINGS};
pub use memory::MemoryCanonicalStore;

use async_trait::async_trait;
use clerkenwell_store::{CollaborationDocumentId, CollaborationVersion};
use serde_json::Value;
use std::fmt;
use walthamstow_kernel::journals::Journal;
use walthamstow_kernel::leases::Lease;
use walthamstow_kernel::local_store::JournalKey;
use walthamstow_kernel::{KernelError, KernelResult};

/// The name a codec gives one part of a rendered document: a relative path of one or more
/// plain names separated by `/`. A name is not empty, not hidden, holds no `\`, and is one
/// Windows allows. The last is no name a durable write stages a file under.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PartName(String);

impl PartName {
    pub fn new(name: impl Into<String>) -> KernelResult<Self> {
        let name = name.into();
        let valid = name.split('/').all(|component| {
            !component.starts_with('.') && walthamstow_kernel::files::windows_allows(component)
        }) && !name
            .rsplit('/')
            .next()
            .is_some_and(walthamstow_kernel::durability::staged_by_write);
        if !valid {
            return Err(KernelError::invalid_request(format!(
                "\"{name}\" cannot name a part of a document."
            )));
        }
        Ok(Self(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PartName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// One rendered part of a document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    pub name: PartName,
    pub bytes: Vec<u8>,
}

/// A document's accepted operations, as one Clerkenwell update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Operations {
    pub schema_version: u32,
    pub update_base64: String,
}

/// What an engine publishes of one document: the version its working copy accepted, every
/// operation that version holds, and the document as the product's codec renders it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Publication {
    pub version: CollaborationVersion,
    pub operations: Operations,
    pub parts: Vec<Part>,
}

/// What a store holds of one document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pulled {
    /// The version the store holds; `None` when its parts were written outside any engine, so
    /// no version names them.
    pub version: Option<CollaborationVersion>,
    /// The operations the store keeps; `None` when it keeps none.
    pub operations: Option<Operations>,
    pub parts: Vec<Part>,
}

/// Renders a collaborative entity's documents into parts and reads them back. A product
/// supplies one per entity, and every store keeps the parts it renders.
#[async_trait]
pub trait DocumentCodec: Send + Sync + fmt::Debug {
    /// The parts rendering `state`, `document`'s accepted state, given the parts the store
    /// holds of it now: none for a document the store does not hold. A document renders at
    /// least one part, and no two whose names fold alike.
    async fn render(
        &self,
        document: &CollaborationDocumentId,
        state: &Value,
        held: &[Part],
    ) -> KernelResult<Vec<Part>>;

    /// The state `parts` render.
    fn parse(&self, document: &CollaborationDocumentId, parts: &[Part]) -> KernelResult<Value>;

    /// The resource whose document `part` is a part of, for a store that finds a part by its
    /// name alone: a file someone added to the workspace, for example. `None` when the part is
    /// no document's.
    fn resource_of(&self, part: &Part) -> KernelResult<Option<String>>;
}

/// The canonical record of collaborative documents.
#[async_trait]
pub trait CanonicalStore: Send + Sync + fmt::Debug {
    /// Publishes `publication` of `document`, only while `lease` holds the document, and
    /// returns the version the store holds after. Publishing what the store holds changes
    /// nothing.
    async fn publish(
        &self,
        document: &CollaborationDocumentId,
        lease: &Lease,
        publication: &Publication,
    ) -> KernelResult<CollaborationVersion>;

    /// What the store holds of `document`; `None` when it holds none.
    async fn pull(&self, document: &CollaborationDocumentId) -> KernelResult<Option<Pulled>>;

    /// Every document of `entity` the store holds, in resource order.
    async fn list(&self, entity: &str) -> KernelResult<Vec<CollaborationDocumentId>>;

    /// Removes `document`, only while `lease` holds it. Removing a document the store does
    /// not hold changes nothing.
    async fn remove(&self, document: &CollaborationDocumentId, lease: &Lease) -> KernelResult<()>;

    /// Refuses `parts` the store cannot keep as `document`'s: parts whose names fold alike or
    /// hold another beneath them, and parts it would hold as another document's.
    fn admit(&self, document: &CollaborationDocumentId, parts: &[Part]) -> KernelResult<()>;

    /// Replaces `document`'s parts with `parts`, which no version names, only while `lease`
    /// holds the document and what the store holds lies [`between`] `expected`, the parts an
    /// upgrade read (none for a document it adds), and `parts`. A replacement that stopped part
    /// way is finished by repeating it. No parts removes the document. Returns whether it
    /// replaced them. For upgrades.
    async fn replace(
        &self,
        document: &CollaborationDocumentId,
        lease: &Lease,
        expected: &[Part],
        parts: &[Part],
    ) -> KernelResult<bool>;
}

/// Whether `held`, the parts a store holds of a document, lies between `expected`, the parts
/// an upgrade read, and `parts`, the parts it writes: each held part is one of either, its
/// name compared case-folded. A replacement that stopped part way leaves its document between
/// the two, whether it had removed what it drops, renamed a part in case or written some parts.
pub fn between(held: &[Part], expected: &[Part], parts: &[Part]) -> bool {
    let alike = |held: &Part, other: &Part| {
        held.bytes == other.bytes
            && held.name.as_str().to_lowercase() == other.name.as_str().to_lowercase()
    };
    held.iter()
        .all(|part| expected.iter().chain(parts).any(|other| alike(part, other)))
}

/// The refusal of a write whose lease no longer holds.
fn lapsed(document: &CollaborationDocumentId) -> KernelError {
    KernelError::conflict(format!(
        "The lease on {} \"{}\" no longer holds.",
        document.entity, document.resource_id
    ))
}

/// Refuses parts whose names a file system may fold to one, compared case-folded, and a part
/// whose name holds another part beneath it, which no file system can keep beside it.
fn distinct(parts: &[Part]) -> KernelResult<()> {
    let mut names = parts
        .iter()
        .map(|part| part.name.as_str().to_lowercase())
        .collect::<Vec<_>>();
    names.sort();
    if let Some(pair) = names.windows(2).find(|pair| pair[0] == pair[1]) {
        return Err(KernelError::invalid_request(format!(
            "A document names its part \"{}\" twice.",
            pair[0]
        )));
    }
    for name in &names {
        let beneath = format!("{name}/");
        if let Some(inner) = names.iter().find(|other| other.starts_with(&beneath)) {
            return Err(KernelError::invalid_request(format!(
                "A document's part \"{name}\" cannot hold its part \"{inner}\"."
            )));
        }
    }
    Ok(())
}

/// Refuses a publication that renders no part: a store would hold the document and list it
/// nowhere.
fn rendered(parts: &[Part]) -> KernelResult<()> {
    if parts.is_empty() {
        return Err(KernelError::invalid_request(
            "A published document renders at least one part.",
        ));
    }
    Ok(())
}

/// Refuses a write under a lease on another document.
fn names_document(document: &CollaborationDocumentId, lease: &Lease) -> KernelResult<()> {
    if lease.document != *document {
        return Err(lapsed(document));
    }
    Ok(())
}

/// The record of `document` in `journal`: one name, the document's entity and resource as a JSON
/// pair, which is never empty whatever the identity.
pub(crate) fn document_key(journal: Journal, document: &CollaborationDocumentId) -> JournalKey {
    let pair = serde_json::to_string(&(&document.entity, &document.resource_id))
        .expect("a pair of strings encodes");
    JournalKey::new(journal, [pair]).expect("a JSON pair is not empty")
}

/// The document a record [`document_key`] names belongs to.
pub(crate) fn key_document(key: &JournalKey) -> KernelResult<CollaborationDocumentId> {
    let [pair] = key.names() else {
        return Err(KernelError::internal(format!("{key} names no document.")));
    };
    let (entity, resource_id): (String, String) = serde_json::from_str(pair)
        .map_err(|error| KernelError::internal(format!("{key} names no document: {error}")))?;
    Ok(CollaborationDocumentId::new(entity, resource_id))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use walthamstow_kernel::leases::Holder;

    pub(crate) fn document(resource_id: &str) -> CollaborationDocumentId {
        CollaborationDocumentId::new("Note", resource_id)
    }

    pub(crate) fn version(incarnation: &str, counter: u64) -> CollaborationVersion {
        CollaborationVersion {
            incarnation: incarnation.into(),
            peers: BTreeMap::from([(1, counter)]),
        }
    }

    pub(crate) fn part(name: &str, bytes: &str) -> Part {
        Part {
            name: PartName::new(name).unwrap(),
            bytes: bytes.as_bytes().to_vec(),
        }
    }

    pub(crate) fn publication(version: CollaborationVersion, parts: Vec<Part>) -> Publication {
        Publication {
            version,
            operations: Operations {
                schema_version: 1,
                update_base64: "dXBkYXRl".into(),
            },
            parts,
        }
    }

    pub(crate) fn holder(name: &str) -> Holder {
        Holder::new(name).unwrap()
    }

    /// Notes rendered as one file each, named after the note, holding its resource id on its
    /// first line.
    #[derive(Debug)]
    pub(crate) struct NoteCodec;

    #[async_trait]
    impl DocumentCodec for NoteCodec {
        async fn render(
            &self,
            document: &CollaborationDocumentId,
            state: &Value,
            held: &[Part],
        ) -> KernelResult<Vec<Part>> {
            let name = held.first().map_or_else(
                || PartName::new(format!("{}.yaml", document.resource_id)),
                |held| Ok(held.name.clone()),
            )?;
            let body = state["body"].as_str().unwrap_or_default();
            Ok(vec![Part {
                name,
                bytes: format!("{}\n{body}", document.resource_id).into_bytes(),
            }])
        }

        fn parse(&self, document: &CollaborationDocumentId, parts: &[Part]) -> KernelResult<Value> {
            let text = String::from_utf8_lossy(&parts[0].bytes).into_owned();
            let body = text.split_once('\n').map_or("", |(_, body)| body);
            Ok(serde_json::json!({"note_id": document.resource_id, "body": body}))
        }

        fn resource_of(&self, part: &Part) -> KernelResult<Option<String>> {
            let text = String::from_utf8_lossy(&part.bytes).into_owned();
            Ok(text
                .lines()
                .next()
                .filter(|line| !line.is_empty())
                .map(str::to_owned))
        }
    }

    /// What every canonical store does, whatever keeps its documents, given the leases that
    /// fence its writes and the holder they grant every document to.
    pub(crate) async fn holds_the_canonical_store_contract(
        store: &dyn CanonicalStore,
        leases: &dyn walthamstow_kernel::leases::Leases,
        holder: &Holder,
    ) {
        let one = document("one");
        let two = document("two");
        let one_lease = leases.acquire(&one, holder).await.unwrap().unwrap();
        let two_lease = leases.acquire(&two, holder).await.unwrap().unwrap();
        let lease = |document: &CollaborationDocumentId| {
            if document == &one {
                one_lease.clone()
            } else {
                two_lease.clone()
            }
        };
        // A lease of an epoch the leases have not granted.
        let lapsed_lease = |document: &CollaborationDocumentId| Lease {
            epoch: lease(document).epoch.wrapping_add(1),
            ..lease(document)
        };
        assert_eq!(store.pull(&one).await.unwrap(), None);
        assert!(store.list("Note").await.unwrap().is_empty());

        let first = publication(version("a", 1), vec![part("one.yaml", "one\nfirst")]);
        assert_eq!(
            store.publish(&one, &lease(&one), &first).await.unwrap(),
            first.version
        );
        let pulled = store
            .pull(&one)
            .await
            .unwrap()
            .expect("a published document");
        assert_eq!(pulled.version.as_ref(), Some(&first.version));
        assert_eq!(pulled.parts, first.parts);
        assert_eq!(
            store.publish(&one, &lease(&one), &first).await.unwrap(),
            first.version,
            "publishing what the store holds changes nothing"
        );

        assert!(
            store
                .publish(
                    &one,
                    &lapsed_lease(&one),
                    &publication(version("a", 9), vec![part("one.yaml", "one\nlost")])
                )
                .await
                .is_err(),
            "a write under a lease that no longer holds is refused"
        );
        assert_eq!(store.pull(&one).await.unwrap().unwrap().parts, first.parts);

        let second = publication(version("a", 2), vec![part("one.yaml", "one\nsecond")]);
        store.publish(&one, &lease(&one), &second).await.unwrap();
        store
            .publish(
                &two,
                &lease(&two),
                &publication(version("b", 1), vec![part("two.yaml", "two\nbody")]),
            )
            .await
            .unwrap();
        assert_eq!(
            store.list("Note").await.unwrap(),
            vec![one.clone(), two.clone()]
        );
        let pulled = store.pull(&one).await.unwrap().unwrap();
        assert_eq!(pulled.version.as_ref(), Some(&second.version));
        assert_eq!(pulled.parts, second.parts);

        assert!(store.remove(&one, &lapsed_lease(&one)).await.is_err());
        store.remove(&one, &lease(&one)).await.unwrap();
        assert_eq!(store.pull(&one).await.unwrap(), None);
        store.remove(&one, &lease(&one)).await.unwrap();
        assert_eq!(store.list("Note").await.unwrap(), vec![two.clone()]);

        for refused in [
            vec![part("one.yaml", "one\na"), part("one.yaml", "one\nb")],
            vec![part("one.yaml", "one\na"), part("ONE.yaml", "one\nb")],
            vec![part("one", "one\na"), part("one/b.yaml", "one\nb")],
            vec![part("One", "one\na"), part("one/b.yaml", "one\nb")],
            Vec::new(),
        ] {
            assert!(
                store
                    .publish(&one, &lease(&one), &publication(version("c", 1), refused))
                    .await
                    .is_err(),
                "parts that fold to one name or nest, or none, are refused"
            );
        }
        assert!(
            store
                .publish(
                    &one,
                    &lease(&two),
                    &publication(version("c", 1), vec![part("one.yaml", "one\nwrong")])
                )
                .await
                .is_err(),
            "a lease on another document fences nothing"
        );
        assert!(store.remove(&two, &lease(&one)).await.is_err());
        assert_eq!(store.pull(&one).await.unwrap(), None);

        // Replacement, as an upgrade makes it.
        let held = store.pull(&two).await.unwrap().unwrap().parts;
        let upgraded = vec![part("two.yaml", "two\nupgraded")];
        assert!(
            !store
                .replace(
                    &two,
                    &lease(&two),
                    &[part("two.yaml", "two\nstale")],
                    &upgraded
                )
                .await
                .unwrap(),
            "a replacement of parts the store no longer holds replaces nothing"
        );
        assert!(store
            .replace(&two, &lapsed_lease(&two), &held, &upgraded)
            .await
            .is_err());
        assert!(store
            .replace(&two, &lease(&one), &held, &upgraded)
            .await
            .is_err());
        assert!(store
            .replace(&two, &lease(&two), &held, &upgraded)
            .await
            .unwrap());
        let replaced = store.pull(&two).await.unwrap().unwrap();
        assert_eq!(replaced.version, None, "no version names upgraded parts");
        assert_eq!(replaced.parts, upgraded);

        // A replacement that stopped part way is finished by repeating it.
        let read = vec![
            part("two-old.yaml", "two\nold"),
            part("two.yaml", "two\nupgraded"),
        ];
        let written = vec![
            part("two-new.yaml", "two\nnew"),
            part("two.yaml", "two\nagain"),
        ];
        assert!(store
            .replace(&two, &lease(&two), &upgraded, &read)
            .await
            .unwrap());
        let hybrid = vec![read[0].clone(), written[1].clone()];
        assert!(store
            .replace(&two, &lease(&two), &read, &hybrid)
            .await
            .unwrap());
        assert!(store
            .replace(&two, &lease(&two), &read, &written)
            .await
            .unwrap());
        assert_eq!(store.pull(&two).await.unwrap().unwrap().parts, written);
        assert!(
            store
                .replace(&two, &lease(&two), &read, &written)
                .await
                .unwrap(),
            "a replacement already made is made again"
        );
        assert!(!store
            .replace(&two, &lease(&two), &read, &[part("two.yaml", "two\nother")])
            .await
            .unwrap());

        let added = vec![part("one.yaml", "one\nadded")];
        assert!(store
            .replace(&one, &lease(&one), &[], &added)
            .await
            .unwrap());
        assert_eq!(store.pull(&one).await.unwrap().unwrap().parts, added);
        assert!(store
            .replace(&one, &lease(&one), &added, &[])
            .await
            .unwrap());
        assert_eq!(store.pull(&one).await.unwrap(), None, "no parts removes it");

        let unnamed = document("");
        let unnamed_lease = leases.acquire(&unnamed, holder).await.unwrap().unwrap();
        let held = publication(version("d", 1), vec![part("unnamed.yaml", "\nbody")]);
        store
            .publish(&unnamed, &unnamed_lease, &held)
            .await
            .unwrap();
        assert_eq!(
            store
                .pull(&unnamed)
                .await
                .unwrap()
                .map(|pulled| pulled.parts),
            Some(held.parts),
            "a document with an empty resource id is kept like any other"
        );
    }

    #[test]
    fn a_part_is_named_by_plain_relative_components() {
        for name in [
            "",
            "/absolute",
            "a//b",
            "a/../b",
            ".hidden",
            "a/.hidden",
            "a\\b",
            "con",
            "nested/aux",
            "con.yaml",
            "note.",
            "note ",
            "a:b.yaml",
            "what?.yaml",
            "note.0b1e2f3a-4c5d-4e6f-8a9b-0c1d2e3f4a5b.tmp",
            "nested/note.0b1e2f3a-4c5d-4e6f-8a9b-0c1d2e3f4a5b.tmp",
        ] {
            assert!(PartName::new(name).is_err(), "{name:?}");
        }
        for name in [
            "note.yaml",
            "nested/note.yaml",
            "Ünïcode name.yaml",
            "note.tmp",
            "note.0b1e2f3a-4c5d-4e6f-8a9b-0c1d2e3f4a5b.tmp/inside.yaml",
        ] {
            assert!(PartName::new(name).is_ok(), "{name:?}");
        }
    }
}
