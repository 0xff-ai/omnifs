mod support;

use omnifs_cache::{Record as CacheRecord, RecordKind};
use omnifs_core::path::Path;
use omnifs_core::view::{
    self as view_types, DirentRecord, DirentsPayload, EntryMeta, FileAttrsCache,
};
use omnifs_nfs::{Export, NodeKind, ReadOnlyExport, Status};
use support::{root_mounted_test_export, test_export, test_export_with_mount, test_provider_spec};
use tokio::runtime::Builder;

const OLD_OPEN_MATERIALIZE_LIMIT_BYTES: u64 = 64 * 1024 * 1024;

fn p(value: &str) -> Path {
    Path::parse(value).unwrap()
}

#[test]
#[should_panic(expected = "NFS adapter requires a multi-thread Tokio runtime")]
fn omnifs_export_rejects_current_thread_runtime() {
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime");

    let harness = test_export();
    let _ = Export::new(runtime.handle().clone(), harness.registry);
}

fn hello_dir(export: &Export) -> u64 {
    let test_root = export
        .lookup(export.root(), "test")
        .expect("top-level mount lookup");
    export.lookup(test_root, "hello").expect("hello lookup")
}

#[test]
fn omnifs_export_lists_and_reads_through_runtime() {
    let harness = test_export();
    let export = &harness.export;
    let root_listing = export.readdir(export.root()).expect("root listing");
    assert!(root_listing.exhaustive);
    let root_names = root_listing
        .entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(root_names, vec!["test"]);
    let test_root = export
        .lookup(export.root(), "test")
        .expect("top-level mount lookup");
    assert_eq!(
        export.parent(test_root).expect("provider root parent"),
        export.root()
    );
    let hello = export.lookup(test_root, "hello").expect("hello lookup");
    let hello_attr = export.attr(hello).expect("hello attr");
    assert_eq!(hello_attr.kind, NodeKind::Directory);

    let listing = export.readdir(hello).expect("hello listing");
    let names = listing
        .entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect::<Vec<_>>();
    assert!(names.contains(&"message"));
    assert!(names.contains(&"bundle"));
    let ranged_entry = listing
        .entries
        .iter()
        .find(|entry| entry.name == "ranged")
        .expect("ranged listing entry");
    assert_eq!(ranged_entry.attr.size, 26);
    let large_ranged_entry = listing
        .entries
        .iter()
        .find(|entry| entry.name == "large-ranged")
        .expect("large ranged listing entry");
    assert_eq!(
        large_ranged_entry.attr.size,
        OLD_OPEN_MATERIALIZE_LIMIT_BYTES + 1
    );

    let message = export.lookup(hello, "message").expect("message lookup");
    let message_attr = export.attr(message).expect("message attr");
    assert_eq!(message_attr.kind, NodeKind::File);
    assert_eq!(
        export.read(message).expect("message read"),
        b"Hello, world!".to_vec()
    );
    assert_eq!(
        export.attr(message).expect("message attr after read").size,
        13
    );

    let _listing_after_read = export.readdir(hello).expect("hello relisting");
    assert_eq!(
        export
            .attr(message)
            .expect("message attr after relisting")
            .size,
        13
    );

    let ranged = export
        .lookup(hello, "ranged")
        .expect("cached ranged lookup after readdir");
    assert_eq!(
        export
            .attr(ranged)
            .expect("cached ranged attr after readdir")
            .size,
        26
    );

    let bundle = export.lookup(hello, "bundle").expect("bundle lookup");
    for entry in export.readdir(bundle).expect("bundle listing").entries {
        if entry.attr.kind == NodeKind::File {
            assert!(matches!(export.readdir(entry.id), Err(Status::NotDir)));
        }
    }

    let items = export.lookup(test_root, "items").expect("items lookup");
    let all = export.lookup(items, "all").expect("all items lookup");
    let item = export.lookup(all, "7").expect("item lookup");
    let item_json = export.lookup(item, "item.json").expect("item.json lookup");
    let item_json = export.read(item_json).expect("canonical item read");
    assert!(
        String::from_utf8_lossy(&item_json).contains(r#""number":7"#),
        "canonical item JSON should be readable through a mount-relative NFS path"
    );
}

#[test]
fn mount_enumeration_root_change_tracks_loaded_mounts() {
    let harness = test_export();
    let export = &harness.export;
    let root = export.root();
    let before = export
        .attr(root)
        .expect("root attr before mount add")
        .change;

    harness
        .registry
        .add_mount(test_provider_spec("other"), harness.runtime.handle())
        .expect("load second test mount");

    let after = export.attr(root).expect("root attr after mount add").change;
    assert_ne!(
        before, after,
        "root directory change must move when the mount set changes so NFS clients drop stale empty listings"
    );

    let root_listing = export.readdir(root).expect("root listing after mount add");
    let root_names = root_listing
        .entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(root_names, vec!["other", "test"]);
}

#[test]
fn omnifs_export_treats_omnifs_as_a_normal_provider_mount_name() {
    let harness = test_export_with_mount("omnifs");
    let export = &harness.export;

    let omnifs_dir = export
        .lookup(export.root(), "omnifs")
        .expect("provider named omnifs must resolve from root");
    let root_listing = export.readdir(export.root()).expect("root listing");
    let omnifs_entries = root_listing
        .entries
        .iter()
        .filter(|entry| entry.name == "omnifs")
        .collect::<Vec<_>>();
    assert_eq!(omnifs_entries.len(), 1);
    assert_eq!(omnifs_entries[0].id, omnifs_dir);
    assert_eq!(
        export.parent(omnifs_dir).expect("provider parent"),
        export.root()
    );
    // The hello directory comes from the test provider, so resolving it
    // proves `omnifs` is a normal configured provider root, not a reserved
    // export alias.
    let hello = export
        .lookup(omnifs_dir, "hello")
        .expect("hello under provider-named-omnifs");
    assert_eq!(
        export.attr(hello).expect("hello attr").kind,
        NodeKind::Directory
    );
}

#[test]
fn omnifs_export_does_not_synthesize_top_level_omnifs_alias() {
    let harness = test_export();
    let export = &harness.export;

    let root_listing = export.readdir(export.root()).expect("root listing");
    let root_names = root_listing
        .entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(root_names, vec!["test"]);

    let export_root = export
        .lookup(export.root(), "omnifs")
        .expect("mount export lookup");
    let export_listing = export.readdir(export_root).expect("export listing");
    let export_names = export_listing
        .entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(export_names, vec!["test"]);

    let test_from_export_root = export
        .lookup(export_root, "test")
        .expect("provider lookup under hidden export root");
    assert_eq!(
        export
            .parent(test_from_export_root)
            .expect("export-root provider parent"),
        export_root
    );
    let test_from_protocol_root = export
        .lookup(export.root(), "test")
        .expect("provider lookup under protocol root");
    assert_eq!(
        export
            .parent(test_from_protocol_root)
            .expect("protocol-root provider parent"),
        export.root()
    );
    assert_ne!(test_from_export_root, test_from_protocol_root);
}

#[test]
fn omnifs_export_root_mount_projects_provider_at_volume_root() {
    let harness = root_mounted_test_export();
    let export = &harness.export;

    let root_listing = export.readdir(export.root()).expect("root listing");
    assert!(
        root_listing
            .entries
            .iter()
            .any(|entry| entry.name == "hello")
    );

    let export_root = export
        .lookup(export.root(), "omnifs")
        .expect("hidden export lookup");
    let export_listing = export.readdir(export_root).expect("export listing");
    assert!(
        export_listing
            .entries
            .iter()
            .any(|entry| entry.name == "hello")
    );
    assert!(
        !export_listing
            .entries
            .iter()
            .any(|entry| entry.name == "test")
    );
}

#[test]
fn omnifs_export_ranged_read_contract() {
    let harness = test_export();
    let export = &harness.export;
    let hello = hello_dir(export);

    let ranged = export.lookup(hello, "ranged").expect("ranged lookup");
    assert_eq!(export.attr(ranged).expect("ranged attr").size, 26);
    let opened = export.open_state(7, ranged, 1, 1).expect("ranged open");
    let chunk = export
        .read_state(opened.stateid, 2, 4)
        .expect("ranged chunk");
    assert_eq!(chunk.data, b"cdef".to_vec());
    assert!(!chunk.eof);
    let eof = export
        .read_state(opened.stateid, 26, 8)
        .expect("ranged eof");
    assert_eq!(eof.data, Vec::<u8>::new());
    assert!(eof.eof);
    export.close_state(opened.stateid).expect("ranged close");

    let large = export
        .lookup(hello, "large-ranged")
        .expect("large ranged lookup");
    assert_eq!(
        export.attr(large).expect("large ranged attr").size,
        OLD_OPEN_MATERIALIZE_LIMIT_BYTES + 1
    );
    let opened = export
        .open_state(7, large, 1, 1)
        .expect("large ranged open");
    assert_eq!(opened.attr.size, OLD_OPEN_MATERIALIZE_LIMIT_BYTES + 1);
    let head = export
        .read_state(opened.stateid, 0, 4)
        .expect("large ranged head");
    assert_eq!(head.data, b"LLLL".to_vec());
    assert!(!head.eof);
    let tail = export
        .read_state(opened.stateid, OLD_OPEN_MATERIALIZE_LIMIT_BYTES, 8)
        .expect("large ranged tail");
    assert_eq!(tail.data, b"L".to_vec());
    assert!(tail.eof);
    export
        .close_state(opened.stateid)
        .expect("large ranged close");

    let unknown = export
        .lookup(hello, "unknown-ranged")
        .expect("unknown ranged lookup");
    assert_eq!(export.attr(unknown).expect("pre-open attr").size, 1);
    let opened = export
        .open_state(7, unknown, 1, 1)
        .expect("unknown ranged open");
    let tail = export
        .read_state(opened.stateid, 8, 32)
        .expect("unknown ranged tail");
    assert_eq!(tail.data, b"size\n".to_vec());
    assert!(tail.eof);
    assert_eq!(export.attr(unknown).expect("learned attr").size, 13);
    export
        .close_state(opened.stateid)
        .expect("unknown ranged close");
}

#[test]
fn omnifs_export_positive_cache_evidence_beats_expected_negative_probe() {
    let harness = test_export();
    let export = &harness.export;
    let hello = hello_dir(export);

    for name in [".DS_Store", "._message"] {
        assert!(matches!(export.lookup(hello, name), Err(Status::NoEnt)));

        let runtime = harness.registry.get("test").expect("test runtime");
        let attrs =
            FileAttrsCache::inline(b"live\n".to_vec(), view_types::Stability::Dynamic, None)
                .expect("valid inline attrs");
        let dirents = DirentsPayload {
            entries: vec![DirentRecord {
                name: name.to_string(),
                meta: EntryMeta::file(attrs),
            }],
            exhaustive: false,
            validator: None,
            next_cursor: None,
            paginated: false,
        };
        let record = CacheRecord::new(
            RecordKind::Dirents,
            dirents.serialize().expect("dirents serialize"),
        );
        runtime.cache_put(&p("/hello"), RecordKind::Dirents, None, &record);

        let id = export
            .lookup(hello, name)
            .expect("positive cached Finder-probe lookup");
        assert_eq!(
            export.read(id).expect("cached probe read"),
            b"live\n".to_vec()
        );
    }
}

#[test]
fn omnifs_export_reads_inline_cached_projection_without_provider_file_route() {
    let harness = test_export();
    let export = &harness.export;
    let runtime = harness.registry.get("test").expect("test runtime");
    let test_root = export
        .lookup(export.root(), "test")
        .expect("top-level mount lookup");

    let attrs = FileAttrsCache::inline(b"live\n".to_vec(), view_types::Stability::Dynamic, None)
        .expect("valid inline attrs");
    let dirents = DirentsPayload {
        entries: vec![DirentRecord {
            name: "inline-only".to_string(),
            meta: EntryMeta::file(attrs),
        }],
        exhaustive: false,
        validator: None,
        next_cursor: None,
        paginated: false,
    };
    let record = CacheRecord::new(
        RecordKind::Dirents,
        dirents.serialize().expect("dirents serialize"),
    );
    runtime.cache_put(&p("/"), RecordKind::Dirents, None, &record);

    let inline = export
        .lookup(test_root, "inline-only")
        .expect("inline cached lookup");
    assert_eq!(export.attr(inline).expect("inline attr").size, 5);
    assert_eq!(
        export.read(inline).expect("inline cached read"),
        b"live\n".to_vec()
    );
    assert_eq!(export.attr(inline).expect("learned attr").size, 5);
}

#[test]
fn omnifs_export_invalidates_path_state_and_open_cache() {
    let harness = test_export();
    let export = &harness.export;
    let test_root = export
        .lookup(export.root(), "test")
        .expect("top-level mount lookup");
    let scoped = export.lookup(test_root, "scoped").expect("scoped lookup");
    let item = export.lookup(scoped, "item").expect("item lookup");
    let opened = export.open_state(7, item, 1, 1).expect("item open");
    let read = export
        .read_state(opened.stateid, 0, 32)
        .expect("open-state read before invalidation");
    assert_eq!(read.data, b"scoped\n".to_vec());

    let runtime = harness.registry.get("test").expect("test runtime");
    harness
        .runtime
        .block_on(runtime.call_timer_tick())
        .expect("timer tick");

    assert!(matches!(export.attr(item), Err(Status::Stale)));
    assert!(matches!(
        export.read_state(opened.stateid, 0, 32),
        Err(Status::BadStateId)
    ));
    let refreshed_item = export
        .lookup(scoped, "item")
        .expect("item lookup after invalidation");
    assert_ne!(refreshed_item, item);
}

#[test]
fn omnifs_export_handles_concurrent_lookup_and_readdir_allocation() {
    let harness = test_export();
    let export = &harness.export;
    let test_root = export
        .lookup(export.root(), "test")
        .expect("top-level mount lookup");

    std::thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(|| {
                for _ in 0..50 {
                    let hello = export.lookup(test_root, "hello").expect("hello lookup");
                    assert_eq!(
                        export.attr(hello).expect("hello attr").kind,
                        NodeKind::Directory
                    );
                }
            });
        }

        for _ in 0..4 {
            scope.spawn(|| {
                for _ in 0..50 {
                    let listing = export.readdir(test_root).expect("test root listing");
                    assert!(listing.entries.iter().any(|entry| entry.name == "hello"));
                }
            });
        }
    });

    let lookup_id = export.lookup(test_root, "hello").expect("hello lookup");
    let listing_id = export
        .readdir(test_root)
        .expect("test root listing")
        .entries
        .into_iter()
        .find(|entry| entry.name == "hello")
        .expect("hello entry")
        .id;
    assert_eq!(lookup_id, listing_id);
}

#[test]
fn omnifs_export_preserves_dynamic_prefix_lookup_after_implicit_dir_lookup() {
    let harness = test_export();
    let export = &harness.export;
    let test_root = export
        .lookup(export.root(), "test")
        .expect("top-level mount lookup");
    let dynamic = export
        .lookup(test_root, "dynamic")
        .expect("implicit dynamic prefix lookup");
    let captured = export
        .lookup(dynamic, "alpha")
        .expect("captured dynamic child lookup");
    assert_eq!(
        export.attr(captured).expect("captured attr").kind,
        NodeKind::Directory
    );
    let value = export
        .lookup(captured, "value")
        .expect("captured value lookup");
    assert_eq!(
        export.read(value).expect("captured value read"),
        b"alpha\n".to_vec()
    );
}
