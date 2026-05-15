use chrono::Utc;
use mcp_librarian::config::Paths;
use mcp_librarian::discovery;
use mcp_librarian::index::{
    self, ArgSummary, Index, IndexedTool, Note, NoteBasis, NoteKind, ProbeStatus, ServerEntry,
};
use mcp_librarian::playbook::{self, Manifest, ManifestCategory, ManifestMeta, ManifestTopic, ManifestWorkflow};
use serde_json::json;
use std::collections::BTreeMap;
use tempfile::TempDir;

// --- helpers ---

fn temp_paths() -> (TempDir, Paths) {
    let dir = TempDir::new().unwrap();
    let base = dir.path();
    let paths = Paths {
        cache_dir: base.join("cache"),
        config_dir: base.join("config"),
        manifest_dir: base.join("config/manifests"),
        learned_dir: base.join("data/learned"),
        cache_file: base.join("cache/index.json"),
        docs_cache_dir: base.join("cache/docs"),
    };
    paths.ensure_dirs().unwrap();
    (dir, paths)
}

fn fake_entry(name: &str, tools: Vec<(&str, &str, Vec<&str>)>) -> ServerEntry {
    let tools = tools
        .into_iter()
        .map(|(n, d, required)| IndexedTool {
            name: n.to_string(),
            description: d.to_string(),
            arg_summary: if required.is_empty() {
                None
            } else {
                let mut props = BTreeMap::new();
                for r in &required {
                    props.insert((*r).to_string(), "string".to_string());
                }
                Some(ArgSummary {
                    required: required.into_iter().map(str::to_string).collect(),
                    properties: props,
                })
            },
        })
        .collect();
    ServerEntry {
        name: name.to_string(),
        transport_descriptor: "fake".to_string(),
        probeable: true,
        probe_status: ProbeStatus::Ok,
        indexed_at: Utc::now(),
        tools,
        summary: None,
        category: None,
    }
}

// --- discovery ---

#[test]
fn discovery_reads_override_json() {
    let dir = TempDir::new().unwrap();
    let cfg_path = dir.path().join("custom.json");
    let payload = json!({
        "mcpServers": {
            "local-foo": {
                "command": "foo-cli",
                "args": ["--stdio"],
                "env": { "FOO_TOKEN": "bar" }
            },
            "cloud-bar": {
                "url": "https://bar.example.com/mcp",
                "type": "http"
            }
        }
    });
    std::fs::write(&cfg_path, serde_json::to_vec_pretty(&payload).unwrap()).unwrap();

    // Use the override env var to inject our config and skip the global ones.
    // Safety: tests don't run in parallel against the same env, so this is fine in
    // a serial single-thread run, but cargo runs tests in parallel by default.
    // To stay deterministic, call the loader directly.
    unsafe {
        std::env::set_var("MCP_LIBRARIAN_CONFIG", &cfg_path);
    }
    let servers = discovery::discover().unwrap();
    unsafe {
        std::env::remove_var("MCP_LIBRARIAN_CONFIG");
    }

    let names: Vec<&str> = servers.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"local-foo"), "saw {names:?}");
    assert!(names.contains(&"cloud-bar"), "saw {names:?}");

    let foo = servers.iter().find(|s| s.name == "local-foo").unwrap();
    assert!(foo.probeable, "stdio entry should be probeable");
    let bar = servers.iter().find(|s| s.name == "cloud-bar").unwrap();
    assert!(!bar.probeable, "url entry should not be probeable");
}

// --- notes round-trip ---

#[test]
fn note_round_trip() {
    let (_tmp, paths) = temp_paths();
    let note = Note {
        timestamp: Utc::now(),
        session_id: Some("s1".into()),
        server: "demo-kb".into(),
        tool: Some("ingest_run".into()),
        topic: Some("ingestion".into()),
        kind: NoteKind::Workflow,
        basis: NoteBasis::Observed,
        claim: "preview → manifest_write → run → rescan".into(),
        tags: vec!["project:librarian".into()],
        possibly_stale: false,
    };
    index::append_note(&paths, &note).unwrap();

    let read = index::read_notes(&paths, "demo-kb").unwrap();
    assert_eq!(read.len(), 1);
    assert_eq!(read[0].claim, note.claim);
    assert_eq!(read[0].kind, NoteKind::Workflow);
    assert_eq!(read[0].basis, NoteBasis::Observed);
}

#[test]
fn notes_write_back_preserves_order() {
    let (_tmp, paths) = temp_paths();
    for i in 0..3 {
        let note = Note {
            timestamp: Utc::now(),
            session_id: None,
            server: "x".into(),
            tool: Some(format!("tool_{i}")),
            topic: None,
            kind: NoteKind::Tip,
            basis: NoteBasis::Observed,
            claim: format!("tip {i}"),
            tags: vec![],
            possibly_stale: false,
        };
        index::append_note(&paths, &note).unwrap();
    }
    let mut notes = index::read_notes(&paths, "x").unwrap();
    notes[0].possibly_stale = true;
    index::write_notes(&paths, "x", &notes).unwrap();
    let again = index::read_notes(&paths, "x").unwrap();
    assert_eq!(again.len(), 3);
    assert!(again[0].possibly_stale);
    assert!(!again[1].possibly_stale);
}

// --- render: list ---

#[test]
fn render_list_groups_by_category() {
    let mut a_manifest = Manifest::default();
    a_manifest.meta.category = Some("knowledge".into());
    a_manifest.meta.summary = Some("Knowledge graph server.".into());

    let pairs = vec![
        (fake_entry("demo-kb", vec![("query_nodes", "find nodes", vec![])]), Some(a_manifest)),
        (fake_entry("playwright", vec![("browser_click", "click", vec![])]), None),
    ];
    let out = playbook::render_list(&pairs, None);
    assert!(out.contains("## knowledge"));
    assert!(out.contains("## uncategorized"));
    assert!(out.contains("**demo-kb**"));
    assert!(out.contains("Knowledge graph server."));
    assert!(out.contains("**playwright**"));
    // Self-bootstrap footer must be present.
    assert!(out.contains("librarian_help"));
    assert!(out.contains("librarian_search"));
}

#[test]
fn render_list_filters_by_category() {
    let mut a = Manifest::default();
    a.meta.category = Some("knowledge".into());
    let pairs = vec![
        (fake_entry("demo-kb", vec![]), Some(a)),
        (fake_entry("playwright", vec![]), None),
    ];
    let only_kn = playbook::render_list(&pairs, Some("knowledge"));
    assert!(only_kn.contains("demo-kb"));
    assert!(!only_kn.contains("playwright"));
}

// --- render: help overview & topic ---

#[test]
fn render_overview_includes_workflows_and_gotchas() {
    let entry = fake_entry(
        "demo-kb",
        vec![
            ("query_nodes", "find nodes", vec![]),
            ("graph_add_node", "add a node", vec!["type", "content"]),
        ],
    );
    let manifest = Manifest {
        meta: ManifestMeta {
            category: Some("knowledge".into()),
            summary: Some("Graph KB.".into()),
            paired_cli: None,
        },
        topics: vec![ManifestTopic {
            name: "ingestion".into(),
            title: "Ingestion Pipeline".into(),
            body: "Run ingest_preview, then ingest_run.".into(),
        }],
        workflows: vec![ManifestWorkflow {
            title: "Bootstrap".into(),
            body: "1. session_init\n2. policy_ack".into(),
        }],
        tool_categories: vec![],
        gotchas: vec!["Writes are budget-capped.".into()],
    };
    let notes = vec![Note {
        timestamp: Utc::now(),
        session_id: None,
        server: "demo-kb".into(),
        tool: None,
        topic: None,
        kind: NoteKind::Workflow,
        basis: NoteBasis::Observed,
        claim: "remember to journal between tier2 calls".into(),
        tags: vec![],
        possibly_stale: false,
    }];
    let out = playbook::render_help(&entry, Some(&manifest), &notes, None);
    assert!(out.contains("Graph KB."));
    assert!(out.contains("## Key Workflows"));
    assert!(out.contains("### Bootstrap"));
    assert!(out.contains("session_init"));
    assert!(out.contains("Tool Categories"));
    assert!(out.contains("Writes are budget-capped."));
    assert!(out.contains("remember to journal"));
    assert!(out.contains("## Available Topics"));
    assert!(out.contains("**ingestion**"));
}

#[test]
fn render_topic_drill_down() {
    let entry = fake_entry("demo-kb", vec![]);
    let manifest = Manifest {
        meta: ManifestMeta::default(),
        topics: vec![ManifestTopic {
            name: "ingestion".into(),
            title: "Ingestion Pipeline".into(),
            body: "Run ingest_preview, then ingest_run.".into(),
        }],
        workflows: vec![],
        tool_categories: vec![],
        gotchas: vec![],
    };
    let out = playbook::render_help(&entry, Some(&manifest), &[], Some("ingestion"));
    assert!(out.contains("Ingestion Pipeline"));
    assert!(out.contains("ingest_preview"));
}

#[test]
fn topic_renders_tools_in_named_category() {
    let entry = fake_entry(
        "demo-kb",
        vec![
            ("query_nodes", "find nodes", vec!["query"]),
            ("query_edges", "find edges", vec![]),
            ("graph_add_node", "add a node", vec!["type", "content"]),
        ],
    );
    let manifest = Manifest {
        meta: ManifestMeta::default(),
        topics: vec![],
        workflows: vec![],
        tool_categories: vec![ManifestCategory {
            name: "Read".into(),
            tools: vec!["query_nodes".into(), "query_edges".into()],
        }],
        gotchas: vec![],
    };
    let out = playbook::render_help(&entry, Some(&manifest), &[], Some("Read"));
    assert!(out.contains("query_nodes"));
    assert!(out.contains("query_edges"));
    assert!(!out.contains("graph_add_node"), "should not include non-Read tools");
}

#[test]
fn librarian_self_help_is_nonempty() {
    let out = playbook::render_self();
    assert!(out.len() > 200);
    assert!(out.contains("librarian"));
    assert!(out.contains("librarian_note"));
    assert!(out.contains("librarian_seed_playbook"));
    assert!(out.contains("workflow"));
}

#[test]
fn inferred_notes_render_in_weaker_section() {
    let entry = fake_entry("foo", vec![]);
    let notes = vec![
        Note {
            timestamp: Utc::now(),
            session_id: None,
            server: "foo".into(),
            tool: None,
            topic: None,
            kind: NoteKind::Behavior,
            basis: NoteBasis::Inferred,
            claim: "probably batches requests".into(),
            tags: vec![],
            possibly_stale: false,
        },
    ];
    let out = playbook::render_help(&entry, None, &notes, None);
    assert!(out.contains("Inferred (unverified)"));
    assert!(out.contains("probably batches requests"));
}

#[test]
fn stale_notes_render_with_marker() {
    let entry = fake_entry("foo", vec![]);
    let notes = vec![Note {
        timestamp: Utc::now(),
        session_id: None,
        server: "foo".into(),
        tool: Some("bar".into()),
        topic: None,
        kind: NoteKind::Behavior,
        basis: NoteBasis::Observed,
        claim: "behaves like X".into(),
        tags: vec![],
        possibly_stale: true,
    }];
    let out = playbook::render_help(&entry, None, &notes, None);
    assert!(out.contains("⚠"), "stale marker should appear");
}

// --- drift ---

#[test]
fn arg_shape_drift_detection() {
    let old = Some(ArgSummary {
        required: vec!["query".into()],
        properties: {
            let mut m = BTreeMap::new();
            m.insert("query".into(), "string".into());
            m
        },
    });
    let new = Some(ArgSummary {
        required: vec!["query".into(), "limit".into()],
        properties: {
            let mut m = BTreeMap::new();
            m.insert("query".into(), "string".into());
            m.insert("limit".into(), "number".into());
            m
        },
    });
    assert!(Index::arg_shape_drifted(&old, &new));
    assert!(!Index::arg_shape_drifted(&old, &old));
    assert!(!Index::arg_shape_drifted(&None, &None));
    assert!(Index::arg_shape_drifted(&None, &old));
}

// --- cache I/O ---

#[test]
fn index_save_load_round_trip() {
    let (_tmp, paths) = temp_paths();
    let mut idx = Index::default();
    idx.servers.insert(
        "demo-kb".to_string(),
        fake_entry("demo-kb", vec![("query_nodes", "find nodes", vec!["query"])]),
    );
    idx.save(&paths.cache_file).unwrap();
    let loaded = Index::load(&paths.cache_file).unwrap();
    assert!(loaded.servers.contains_key("demo-kb"));
    assert_eq!(loaded.servers["demo-kb"].tools.len(), 1);
}

// --- seed playbook → render ---

// --- manifest preview + fingerprint ---

#[test]
fn manifest_preview_enumerates_concrete_additions() {
    let manifest = Manifest {
        meta: ManifestMeta {
            category: Some("comms".into()),
            summary: Some("Chat server.".into()),
            paired_cli: None,
        },
        tool_categories: vec![ManifestCategory {
            name: "Read".into(),
            tools: vec!["a".into(), "b".into(), "c".into()],
        }],
        workflows: vec![ManifestWorkflow {
            title: "Threaded reply".into(),
            body: "1. find parent\n2. reply".into(),
        }],
        topics: vec![ManifestTopic {
            name: "rate_limits".into(),
            title: "Rate limits".into(),
            body: "stuff".into(),
        }],
        gotchas: vec!["X is gated at 1/sec".into()],
    };
    let target = std::path::PathBuf::from("/tmp/foo.toml");
    let out = playbook::render_manifest_preview("foo", &manifest, &target, None);
    assert!(out.contains("PREVIEW"));
    assert!(out.contains("CREATE"));
    assert!(out.contains("comms"));
    assert!(out.contains("Chat server."));
    assert!(out.contains("Read"));
    // Every tool name is enumerated — no vague "3 tools" summary
    assert!(out.contains("a, b, c"));
    assert!(out.contains("Threaded reply"));
    assert!(out.contains("rate_limits"));
    assert!(out.contains("X is gated at 1/sec"));
}

#[test]
fn manifest_preview_marks_overwrite_when_existing() {
    let existing = Manifest::default();
    let new = Manifest {
        meta: ManifestMeta {
            category: Some("x".into()),
            ..Default::default()
        },
        ..Default::default()
    };
    let target = std::path::PathBuf::from("/tmp/foo.toml");
    let out = playbook::render_manifest_preview("foo", &new, &target, Some(&existing));
    assert!(out.contains("OVERWRITE"));
}

#[test]
fn manifest_fingerprint_stable_for_identical_content() {
    let m1 = Manifest {
        meta: ManifestMeta {
            category: Some("comms".into()),
            summary: Some("a".into()),
            paired_cli: None,
        },
        ..Default::default()
    };
    let m2 = Manifest {
        meta: ManifestMeta {
            category: Some("comms".into()),
            summary: Some("a".into()),
            paired_cli: None,
        },
        ..Default::default()
    };
    let m3 = Manifest {
        meta: ManifestMeta {
            category: Some("comms".into()),
            summary: Some("b".into()), // different
            paired_cli: None,
        },
        ..Default::default()
    };
    assert_eq!(playbook::manifest_fingerprint(&m1), playbook::manifest_fingerprint(&m2));
    assert_ne!(playbook::manifest_fingerprint(&m1), playbook::manifest_fingerprint(&m3));
}

#[test]
fn manifest_is_empty_detects_default() {
    assert!(playbook::manifest_is_empty(&Manifest::default()));

    let mut m = Manifest::default();
    m.meta.summary = Some("x".into());
    assert!(!playbook::manifest_is_empty(&m));

    let mut m = Manifest::default();
    m.gotchas.push("y".into());
    assert!(!playbook::manifest_is_empty(&m));
}

// --- manifest write backup / diff / restore ---

fn sample_manifest(category: &str, summary: &str) -> Manifest {
    Manifest {
        meta: ManifestMeta {
            category: Some(category.into()),
            summary: Some(summary.into()),
            paired_cli: None,
        },
        tool_categories: vec![ManifestCategory {
            name: "Cat".into(),
            tools: vec!["t1".into(), "t2".into()],
        }],
        workflows: vec![],
        topics: vec![],
        gotchas: vec!["watch out".into()],
    }
}

#[test]
fn write_manifest_creates_backup_on_overwrite() {
    let (_tmp, paths) = temp_paths();
    let v1 = sample_manifest("comms", "first");
    let v2 = sample_manifest("comms", "second");

    playbook::write_manifest(&paths, "foo", &v1).unwrap();
    // First write: no backup yet
    assert!(!paths.manifest_backup_path("foo").exists());

    playbook::write_manifest(&paths, "foo", &v2).unwrap();
    // Second write: backup is the first version
    assert!(paths.manifest_backup_path("foo").exists());
    let bak = playbook::load_manifest_backup(&paths, "foo").unwrap().unwrap();
    assert_eq!(bak.meta.summary.as_deref(), Some("first"));
    let curr = playbook::load_manifest(&paths, "foo").unwrap().unwrap();
    assert_eq!(curr.meta.summary.as_deref(), Some("second"));
}

#[test]
fn diff_shows_added_changed_removed() {
    let prev = Manifest {
        meta: ManifestMeta {
            category: Some("comms".into()),
            summary: Some("old".into()),
            paired_cli: None,
        },
        tool_categories: vec![
            ManifestCategory { name: "A".into(), tools: vec!["a".into()] },
            ManifestCategory { name: "B".into(), tools: vec!["b".into()] },
        ],
        workflows: vec![],
        topics: vec![],
        gotchas: vec!["g1".into(), "g2".into()],
    };
    let curr = Manifest {
        meta: ManifestMeta {
            category: Some("comms".into()),
            summary: Some("new".into()),
            paired_cli: None,
        },
        tool_categories: vec![
            ManifestCategory { name: "A".into(), tools: vec!["a".into(), "a2".into()] }, // changed
            ManifestCategory { name: "C".into(), tools: vec!["c".into()] },              // added
            // B removed
        ],
        workflows: vec![],
        topics: vec![],
        gotchas: vec!["g1".into(), "g3".into()], // g2 removed, g3 added
    };
    let out = playbook::diff_manifests(&prev, &curr);
    assert!(out.contains("summary"));
    assert!(out.contains("old"));
    assert!(out.contains("new"));
    assert!(out.contains("ADDED: `C`"));
    assert!(out.contains("REMOVED: `B`"));
    assert!(out.contains("CHANGED: `A`"));
    assert!(out.contains("ADDED: g3"));
    assert!(out.contains("REMOVED: g2"));
}

#[test]
fn diff_empty_when_identical() {
    let m = sample_manifest("comms", "x");
    let out = playbook::diff_manifests(&m, &m);
    assert!(out.contains("no differences"));
}

#[test]
fn restore_is_reversible() {
    let (_tmp, paths) = temp_paths();
    let v1 = sample_manifest("comms", "first");
    let v2 = sample_manifest("comms", "second");

    playbook::write_manifest(&paths, "foo", &v1).unwrap();
    playbook::write_manifest(&paths, "foo", &v2).unwrap();
    // current = second, backup = first
    assert_eq!(
        playbook::load_manifest(&paths, "foo").unwrap().unwrap().meta.summary.as_deref(),
        Some("second")
    );

    // First restore: current ↔ backup
    playbook::restore_manifest(&paths, "foo").unwrap();
    assert_eq!(
        playbook::load_manifest(&paths, "foo").unwrap().unwrap().meta.summary.as_deref(),
        Some("first")
    );
    assert_eq!(
        playbook::load_manifest_backup(&paths, "foo").unwrap().unwrap().meta.summary.as_deref(),
        Some("second")
    );

    // Second restore: should swap back
    playbook::restore_manifest(&paths, "foo").unwrap();
    assert_eq!(
        playbook::load_manifest(&paths, "foo").unwrap().unwrap().meta.summary.as_deref(),
        Some("second")
    );
    assert_eq!(
        playbook::load_manifest_backup(&paths, "foo").unwrap().unwrap().meta.summary.as_deref(),
        Some("first")
    );
}

#[test]
fn restore_errors_without_backup() {
    let (_tmp, paths) = temp_paths();
    let v1 = sample_manifest("comms", "first");
    playbook::write_manifest(&paths, "foo", &v1).unwrap();
    // No backup yet
    let result = playbook::restore_manifest(&paths, "foo");
    assert!(result.is_err());
    let msg = format!("{:#}", result.unwrap_err());
    assert!(msg.contains("no backup"));
}

// --- manifest-only / orphan manifest flows ---

#[test]
fn list_manifest_servers_finds_only_toml_files() {
    let (_tmp, paths) = temp_paths();
    // Write a fake manifest and a fake backup; only the .toml should be listed.
    std::fs::write(paths.manifest_path("foo"), "[meta]\n").unwrap();
    std::fs::write(paths.manifest_backup_path("foo"), "[meta]\n").unwrap();
    std::fs::write(paths.manifest_dir.join("not-a-manifest.txt"), "x").unwrap();
    let mut servers = playbook::list_manifest_servers(&paths).unwrap();
    servers.sort();
    assert_eq!(servers, vec!["foo".to_string()]);
}

#[test]
fn synthetic_librarian_manifest_has_category_and_summary() {
    let m = playbook::synthetic_librarian_manifest();
    assert_eq!(m.meta.category.as_deref(), Some("meta"));
    assert!(m.meta.summary.as_ref().is_some_and(|s| s.contains("MCP")));
}

#[test]
fn entry_manifest_only_marks_status() {
    let e = mcp_librarian::index::entry_manifest_only("github");
    assert_eq!(e.name, "github");
    assert!(!e.probeable);
    assert_eq!(e.probe_status, mcp_librarian::index::ProbeStatus::ManifestOnly);
}

#[test]
fn render_help_shows_manifest_only_banner() {
    let entry = mcp_librarian::index::entry_manifest_only("github");
    let mut manifest = Manifest::default();
    manifest.meta.summary = Some("Future GitHub MCP".into());
    let out = playbook::render_help(&entry, Some(&manifest), &[], None);
    assert!(out.contains("Manifest only"));
    assert!(out.contains("NOT currently installed"));
    assert!(out.contains("Future GitHub MCP"));
}

#[test]
fn render_list_includes_manifest_only_marker() {
    let entry = mcp_librarian::index::entry_manifest_only("github");
    let mut manifest = Manifest::default();
    manifest.meta.category = Some("developer-tools".into());
    manifest.meta.summary = Some("Pre-authored playbook".into());
    let pairs = vec![(entry, Some(manifest))];
    let out = playbook::render_list(&pairs, None);
    assert!(out.contains("**github**"));
    assert!(out.contains("manifest only"));
    assert!(out.contains("Pre-authored playbook"));
}

#[test]
fn seeded_server_renders_overview() {
    let mut entry = fake_entry("claude.ai_Notion", vec![]);
    entry.probe_status = ProbeStatus::Seeded;
    entry.probeable = false;
    entry.tools.push(IndexedTool {
        name: "notion-search".into(),
        description: "Search Notion.".into(),
        arg_summary: None,
    });
    entry.summary = Some("Remote Notion MCP — seeded by agent".into());

    let out = playbook::render_help(&entry, None, &[], None);
    assert!(out.contains("claude.ai_Notion"));
    assert!(out.contains("Tool Categories"));
    assert!(out.contains("notion-search"));
}
