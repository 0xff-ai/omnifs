use omnifs_host::runtime::operation_ids::OperationIds;
use std::collections::HashSet;
use std::sync::Arc;
use std::thread;

#[test]
fn test_allocate_unique_ids() {
    let tracker = OperationIds::new();
    let id1 = tracker.allocate();
    let id2 = tracker.allocate();
    assert_ne!(id1, id2);
}

#[test]
fn test_allocate_ids_without_pending_map() {
    let tracker = OperationIds::new();
    let id1 = tracker.allocate();
    let id2 = tracker.allocate();
    assert!(id2 > id1);
}

#[test]
fn test_allocate_ids_are_unique_across_threads() {
    let tracker = Arc::new(OperationIds::new());
    let mut handles = Vec::new();
    let thread_count = 8;
    let ids_per_thread = 256;

    for _ in 0..thread_count {
        let tracker = Arc::clone(&tracker);
        handles.push(thread::spawn(move || {
            (0..ids_per_thread)
                .map(|_| tracker.allocate())
                .collect::<Vec<_>>()
        }));
    }

    let mut ids = Vec::with_capacity(thread_count * ids_per_thread);
    for handle in handles {
        ids.extend(handle.join().expect("thread should allocate IDs"));
    }

    let unique: HashSet<_> = ids.iter().copied().collect();
    assert_eq!(unique.len(), ids.len());
}
