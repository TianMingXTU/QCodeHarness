//! 事件轮转保留原始字节，空跑不产生归档，并与日志写入者互斥。

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use qcode_harness::application::service::EventJournal;
use qcode_harness::domain::service::ServiceEventPayload;
use qcode_harness::infrastructure::service_events::{JsonEventJournal, rotate_events};

struct Workspace(PathBuf);

impl Workspace {
    fn new(name: &str) -> Self {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir().join(format!(
            "qch-events-rotate-{name}-{}-{stamp}", std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn log(&self) -> PathBuf {
        self.0.join(".qcodeharness/service/events.jsonl")
    }

    fn write_log(&self, bytes: &[u8]) {
        fs::create_dir_all(self.log().parent().unwrap()).unwrap();
        fs::write(self.log(), bytes).unwrap();
    }

    fn archives(&self) -> Vec<PathBuf> {
        fs::read_dir(self.log().parent().unwrap()).unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "archive"))
            .collect()
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn a_nonempty_journal_is_archived_and_replaced_by_an_empty_journal() {
    let workspace = Workspace::new("archive");
    let journal = JsonEventJournal::open(workspace.log()).unwrap();
    journal.append(None, ServiceEventPayload::ExecutionStarted).unwrap();
    drop(journal);
    let original = fs::read(workspace.log()).unwrap();

    let report = rotate_events(&workspace.0).unwrap();

    assert!(report.archived);
    assert!(!report.empty_run);
    let archive = report.archive_path.unwrap();
    assert_eq!(archive.parent(), workspace.log().parent());
    let name = archive.file_name().unwrap().to_str().unwrap();
    let stamp = name.strip_prefix("events.jsonl.").unwrap().strip_suffix(".archive").unwrap();
    assert!(stamp.parse::<u128>().is_ok());
    assert_eq!(fs::read(&archive).unwrap(), original);
    assert_eq!(fs::metadata(workspace.log()).unwrap().len(), 0);
    assert!(!report.recommendation.is_empty());
    let fresh = JsonEventJournal::open(workspace.log()).unwrap();
    assert_eq!(fresh.latest().unwrap(), 0);
    assert_eq!(fresh.append(None, ServiceEventPayload::ExecutionStarted).unwrap().sequence, 1);
}

#[test]
fn a_missing_service_directory_is_an_idempotent_empty_run() {
    let workspace = Workspace::new("absent");
    for _ in 0..2 {
        let report = rotate_events(&workspace.0).unwrap();
        assert!(report.empty_run);
        assert!(!report.archived);
        assert!(report.archive_path.is_none());
    }
    assert!(!workspace.0.join(".qcodeharness").exists());
}

#[test]
fn a_missing_journal_in_an_existing_service_directory_stays_missing() {
    let workspace = Workspace::new("missing-log");
    fs::create_dir_all(workspace.log().parent().unwrap()).unwrap();
    for _ in 0..2 {
        let report = rotate_events(&workspace.0).unwrap();
        assert!(report.empty_run);
        assert!(!report.archived);
        assert!(report.archive_path.is_none());
    }
    assert!(!workspace.log().exists());
    assert!(workspace.archives().is_empty());
}

#[test]
fn repeat_rotation_without_new_events_preserves_the_single_archive() {
    let workspace = Workspace::new("repeat");
    workspace.write_log(b"original event\n");
    let first = rotate_events(&workspace.0).unwrap();
    let archive = first.archive_path.unwrap();
    for _ in 0..3 {
        let report = rotate_events(&workspace.0).unwrap();
        assert!(report.empty_run);
        assert!(!report.archived);
        assert!(report.archive_path.is_none());
    }
    assert_eq!(workspace.archives(), vec![archive.clone()]);
    assert_eq!(fs::read(&archive).unwrap(), b"original event\n");
    assert_eq!(fs::metadata(workspace.log()).unwrap().len(), 0);
}

#[test]
fn damaged_journals_are_archived_without_decoding_or_changing_their_bytes() {
    let workspace = Workspace::new("damaged");
    let original = b"{\"version\":1,\"sequence\":9}\r\n\xff\x00unfinished";
    workspace.write_log(original);

    let report = rotate_events(&workspace.0).unwrap();

    assert_eq!(fs::read(report.archive_path.unwrap()).unwrap(), original);
    assert!(fs::read(workspace.log()).unwrap().is_empty());
}

#[test]
fn an_active_writer_prevents_rotation_and_keeps_the_original_journal() {
    let workspace = Workspace::new("locked");
    let journal = JsonEventJournal::open(workspace.log()).unwrap();
    journal.append(None, ServiceEventPayload::ExecutionStarted).unwrap();
    let original = fs::read(workspace.log()).unwrap();

    let error = rotate_events(&workspace.0).unwrap_err();

    assert_eq!(error.code, "journal_locked");
    assert_eq!(fs::read(workspace.log()).unwrap(), original);
    assert!(workspace.archives().is_empty());
    journal.close().unwrap();
    assert!(rotate_events(&workspace.0).unwrap().archived);
}

#[test]
fn new_events_can_be_rotated_again_without_overwriting_prior_archives() {
    let workspace = Workspace::new("new-events");
    workspace.write_log(b"first\n");
    let first = rotate_events(&workspace.0).unwrap().archive_path.unwrap();
    workspace.write_log(b"second\n");
    let second = rotate_events(&workspace.0).unwrap().archive_path.unwrap();

    assert_ne!(first, second);
    assert_eq!(fs::read(first).unwrap(), b"first\n");
    assert_eq!(fs::read(second).unwrap(), b"second\n");
    assert_eq!(workspace.archives().len(), 2);
}
