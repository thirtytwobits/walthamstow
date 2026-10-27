//! Copyright (c) Scott A Dixon
//!
//! The daemon's endpoint record, published and retired in a local store in memory and in files.

use std::sync::Arc;
use walthamstow_kernel::durability::WriteDurability;
use walthamstow_kernel::local_store::{FileLocalStore, JournalKey, LocalStore, MemoryLocalStore};
use walthamstow_ops::endpoint::JOURNAL;
use walthamstow_ops::EndpointRecord;

fn record(port: u16) -> EndpointRecord {
    EndpointRecord {
        endpoint: format!("http://127.0.0.1:{port}/mcp"),
        token: format!("token-{port}"),
    }
}

/// Each store, the file store's data directory kept for its life.
fn stores() -> Vec<(Arc<dyn LocalStore>, Option<tempfile::TempDir>)> {
    let data = tempfile::tempdir().unwrap();
    let file: Arc<dyn LocalStore> =
        FileLocalStore::open(data.path(), WriteDurability::System).unwrap();
    vec![
        (Arc::new(MemoryLocalStore::new()), None),
        (file, Some(data)),
    ]
}

#[test]
fn a_cli_finds_the_daemon_through_its_record_until_the_daemon_retires_it() {
    for (store, _data) in stores() {
        assert_eq!(EndpointRecord::resolve(None, store.as_ref()).unwrap(), None);
        let published = record(4000).publish(store.as_ref()).unwrap();
        assert_eq!(
            EndpointRecord::resolve(None, store.as_ref()).unwrap(),
            Some(record(4000))
        );
        assert_eq!(
            EndpointRecord::resolve(Some(record(5000)), store.as_ref()).unwrap(),
            Some(record(5000))
        );
        assert!(published.retire(store.as_ref()).unwrap());
        assert_eq!(EndpointRecord::discover(store.as_ref()).unwrap(), None);
    }
}

#[test]
fn a_daemon_publishes_over_a_record_a_stopped_daemon_left_and_retires_only_its_own() {
    for (store, _data) in stores() {
        let left = record(4000).publish(store.as_ref()).unwrap();
        let published = record(4001).publish(store.as_ref()).unwrap();
        assert_eq!(
            EndpointRecord::discover(store.as_ref()).unwrap(),
            Some(record(4001))
        );
        assert!(!left.retire(store.as_ref()).unwrap());
        assert_eq!(
            EndpointRecord::discover(store.as_ref()).unwrap(),
            Some(record(4001))
        );
        assert!(published.retire(store.as_ref()).unwrap());
    }
}

#[test]
fn retiring_a_publication_leaves_a_later_one_of_the_same_record() {
    for (store, _data) in stores() {
        let left = record(4000).publish(store.as_ref()).unwrap();
        let published = record(4000).publish(store.as_ref()).unwrap();
        assert!(!left.retire(store.as_ref()).unwrap());
        assert_eq!(
            EndpointRecord::discover(store.as_ref()).unwrap(),
            Some(record(4000))
        );
        assert!(published.retire(store.as_ref()).unwrap());
        assert_eq!(EndpointRecord::discover(store.as_ref()).unwrap(), None);
    }
}

#[test]
fn a_record_that_cannot_be_read_is_refused() {
    for (store, _data) in stores() {
        let key = JournalKey::new(JOURNAL, ["endpoint"]).unwrap();
        store
            .compare_and_swap(&key, None, br#"{"endpoint":"http://127.0.0.1:1/mcp"}"#)
            .unwrap()
            .unwrap();
        assert!(EndpointRecord::discover(store.as_ref()).is_err());
    }
}

#[test]
fn a_record_s_debug_form_omits_its_credential() {
    assert!(!format!("{:?}", record(4000)).contains("token-4000"));
}
