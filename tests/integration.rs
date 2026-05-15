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

// --- TOML manifest input path (Fix 1) ---

#[test]
fn toml_manifest_parses_to_expected_structure() {
    // Note: in TOML, root-level fields (like `gotchas`) must come BEFORE any
    // subtable header — otherwise they get attached to the previous table.
    // This is a real footgun agents will hit; the parse error from
    // `librarian_manifest_write` should help them recover.
    let toml_str = r#"
gotchas = ["watch out"]

[meta]
category = "comms"
summary = "x"

[[tool_categories]]
name = "A"
tools = ["t1"]

[[workflows]]
title = "W"
body = "step"
"#;
    let parsed: Manifest = toml::from_str(toml_str).unwrap();
    assert_eq!(parsed.meta.category.as_deref(), Some("comms"));
    assert_eq!(parsed.meta.summary.as_deref(), Some("x"));
    assert_eq!(parsed.tool_categories.len(), 1);
    assert_eq!(parsed.tool_categories[0].name, "A");
    assert_eq!(parsed.tool_categories[0].tools, vec!["t1".to_string()]);
    assert_eq!(parsed.workflows.len(), 1);
    assert_eq!(parsed.workflows[0].title, "W");
    assert_eq!(parsed.workflows[0].body, "step");
    assert_eq!(parsed.gotchas, vec!["watch out".to_string()]);
    assert!(!playbook::manifest_is_empty(&parsed));
}

#[test]
fn toml_multiline_body_preserves_newlines() {
    // The whole point of TOML: triple-quoted blocks let agents emit multi-line
    // workflow bodies without JSON escape mania. Confirms the leading newline
    // after `"""` is trimmed and the content lands as written.
    let toml_str = r#"
[meta]
summary = "x"

[[workflows]]
title = "W"
body = """
1. step
2. step"""
"#;
    let parsed: Manifest = toml::from_str(toml_str).unwrap();
    assert_eq!(parsed.workflows[0].body, "1. step\n2. step");
}

#[test]
fn malformed_toml_returns_parse_error() {
    let bad = "[meta\ninvalid syntax bro";
    let result: Result<Manifest, _> = toml::from_str(bad);
    assert!(result.is_err(), "expected parse failure on malformed TOML");
    let msg = format!("{}", result.unwrap_err());
    // toml error messages include position info — verify roughly
    assert!(
        msg.contains("expected") || msg.contains("invalid") || msg.contains("line"),
        "expected line-numbered error, got: {msg}"
    );
}

// --- note enum strictness (Fix 4) ---
//
// Claude Desktop reported that passing `null` for `kind` and `basis` on
// librarian_note appeared to succeed with auto-defaulted (ErrorPattern,
// Observed). My code has no auto-inference. These tests verify that serde
// rejects bad enum inputs at the deserialization layer — meaning any silent
// defaulting Claude Desktop saw was happening UPSTREAM in its own MCP
// client, not in the librarian. If these tests fail, we have a serde
// permissiveness bug to fix.

#[test]
fn note_params_rejects_null_kind() {
    let json = r#"{"server":"x","kind":null,"basis":"observed","claim":"y"}"#;
    let result: Result<mcp_librarian::server::NoteParams, _> =
        serde_json::from_str(json);
    assert!(result.is_err(), "null kind should be rejected; got {:?}", result.ok().map(|p| p.kind));
}

#[test]
fn note_params_rejects_missing_kind() {
    let json = r#"{"server":"x","basis":"observed","claim":"y"}"#;
    let result: Result<mcp_librarian::server::NoteParams, _> =
        serde_json::from_str(json);
    assert!(result.is_err(), "missing kind should be rejected");
}

#[test]
fn note_params_rejects_invalid_kind_variant() {
    let json = r#"{"server":"x","kind":"not_a_real_kind","basis":"observed","claim":"y"}"#;
    let result: Result<mcp_librarian::server::NoteParams, _> =
        serde_json::from_str(json);
    assert!(result.is_err(), "unknown kind variant should be rejected");
}

#[test]
fn note_params_rejects_null_basis() {
    let json = r#"{"server":"x","kind":"tip","basis":null,"claim":"y"}"#;
    let result: Result<mcp_librarian::server::NoteParams, _> =
        serde_json::from_str(json);
    assert!(result.is_err(), "null basis should be rejected");
}

#[test]
fn note_params_accepts_valid_input() {
    let json = r#"{"server":"x","kind":"workflow","basis":"observed","claim":"y"}"#;
    let result: Result<mcp_librarian::server::NoteParams, _> =
        serde_json::from_str(json);
    assert!(result.is_ok(), "valid input should parse: {:?}", result.err());
}

// --- search surfaces manifest-only servers (Fix 3) ---

#[test]
fn list_manifest_servers_lets_search_find_them() {
    // Indirect test: list_manifest_servers + manifest content should be enough
    // for search to surface a server even when it has no indexed tools.
    let (_tmp, paths) = temp_paths();
    let manifest = Manifest {
        meta: ManifestMeta {
            category: Some("developer-tools".into()),
            summary: Some("Repository management — issues, PRs, files".into()),
            paired_cli: None,
        },
        workflows: vec![ManifestWorkflow {
            title: "Browse files in a repo".into(),
            body: "...".into(),
        }],
        ..Default::default()
    };
    playbook::write_manifest(&paths, "github", &manifest).unwrap();

    // The list scanner finds it
    let servers = playbook::list_manifest_servers(&paths).unwrap();
    assert!(servers.contains(&"github".to_string()));

    // And loading the manifest gives us the searchable content
    let loaded = playbook::load_manifest(&paths, "github").unwrap().unwrap();
    assert!(loaded.meta.summary.as_deref().is_some_and(|s| s.contains("Repository")));
    assert!(loaded.workflows.iter().any(|w| w.title.contains("repo")));
}

// --- mtime label helper (diff direction disambiguation) ---

#[test]
fn format_mtime_pair_labels_newer_and_older() {
    use chrono::TimeZone;
    let a = Utc.with_ymd_and_hms(2026, 5, 15, 17, 42, 0).unwrap();
    let b = Utc.with_ymd_and_hms(2026, 5, 15, 17, 45, 0).unwrap();

    // Current newer than backup (normal post-write state)
    let (curr, bak) = playbook::format_mtime_pair(Some(b), Some(a));
    assert!(curr.contains("(newer)"), "current is newer; got: {curr}");
    assert!(bak.contains("(older)"), "backup is older; got: {bak}");

    // Backup newer than current (post-restore state)
    let (curr, bak) = playbook::format_mtime_pair(Some(a), Some(b));
    assert!(curr.contains("(older)"), "current is older; got: {curr}");
    assert!(bak.contains("(newer)"), "backup is newer; got: {bak}");
}

#[test]
fn format_mtime_pair_handles_missing_files() {
    let (curr, bak) = playbook::format_mtime_pair(None, None);
    assert_eq!(curr, "(unknown)");
    assert_eq!(bak, "(unknown)");
}

#[test]
fn manifest_mtimes_reflects_disk_state() {
    let (_tmp, paths) = temp_paths();
    // Neither file exists yet
    let (c, b) = playbook::manifest_mtimes(&paths, "foo");
    assert!(c.is_none() && b.is_none());

    // Write the manifest; only current should have an mtime
    let m = Manifest {
        meta: ManifestMeta {
            summary: Some("x".into()),
            ..Default::default()
        },
        ..Default::default()
    };
    playbook::write_manifest(&paths, "foo", &m).unwrap();
    let (c, b) = playbook::manifest_mtimes(&paths, "foo");
    assert!(c.is_some() && b.is_none());

    // Write a second time; now both exist (backup gets created on overwrite)
    playbook::write_manifest(&paths, "foo", &m).unwrap();
    let (c, b) = playbook::manifest_mtimes(&paths, "foo");
    assert!(c.is_some() && b.is_some());
}

// --- librarian self-topics ---

#[test]
fn librarian_manifest_schema_topic_renders() {
    let out = playbook::render_librarian_topic("manifest_schema");
    // Headers and key references
    assert!(out.contains("manifest schema"), "should have title");
    assert!(out.contains("TOML grammar trap"), "should warn about root-before-section");
    // All four sections covered
    assert!(out.contains("[meta]"));
    assert!(out.contains("[[tool_categories]]"));
    assert!(out.contains("[[workflows]]"));
    assert!(out.contains("[[topics]]"));
    assert!(out.contains("gotchas"));
    // Working example present
    assert!(out.contains("Minimal working example"));
    assert!(out.contains("foo_get"), "example should include concrete tool names");
}

#[test]
fn librarian_topic_aliases_resolve() {
    let canonical = playbook::render_librarian_topic("manifest_schema");
    assert_eq!(playbook::render_librarian_topic("manifest"), canonical);
    assert_eq!(playbook::render_librarian_topic("schema"), canonical);
    assert_eq!(playbook::render_librarian_topic("Manifest-Schema"), canonical);
}

#[test]
fn librarian_unknown_topic_returns_friendly_message() {
    let out = playbook::render_librarian_topic("not-a-real-topic");
    assert!(out.contains("no topic by that name"));
    assert!(out.contains("manifest_schema"), "should list known topics");
}

#[test]
fn librarian_render_self_advertises_topics() {
    let s = playbook::render_self();
    assert!(s.contains("Available Topics"));
    assert!(s.contains("manifest_schema"));
}

// --- virtual topics (Fix 2) ---

#[test]
fn virtual_topic_gotchas_renders_manifest_gotchas() {
    let entry = fake_entry("foo", vec![]);
    let manifest = Manifest {
        gotchas: vec!["watch the rate limit".into(), "tokens expire fast".into()],
        ..Default::default()
    };
    let out = playbook::render_help(&entry, Some(&manifest), &[], Some("gotchas"));
    assert!(out.contains("foo — Gotchas"));
    assert!(out.contains("watch the rate limit"));
    assert!(out.contains("tokens expire fast"));
    assert!(
        !out.contains("No manifest topic by that name"),
        "virtual topic should not trip the no-topic warning"
    );
}

#[test]
fn virtual_topic_workflows_renders_manifest_workflows() {
    let entry = fake_entry("foo", vec![]);
    let manifest = Manifest {
        workflows: vec![ManifestWorkflow {
            title: "Bootstrap".into(),
            body: "1. init\n2. ack".into(),
        }],
        ..Default::default()
    };
    let out = playbook::render_help(&entry, Some(&manifest), &[], Some("workflows"));
    assert!(out.contains("foo — Workflows"));
    assert!(out.contains("Bootstrap"));
    assert!(out.contains("1. init"));
}

#[test]
fn virtual_topic_categories_renders_tool_groupings() {
    let entry = fake_entry(
        "foo",
        vec![
            ("query_a", "a", vec![]),
            ("query_b", "b", vec![]),
            ("write_x", "x", vec![]),
        ],
    );
    let out = playbook::render_help(&entry, None, &[], Some("categories"));
    assert!(out.contains("foo — Tool categories"));
    // Auto-prefix grouping should bucket query_a/query_b together
    assert!(out.contains("query"));
}

#[test]
fn virtual_topic_does_not_duplicate_notes_in_related_section() {
    // A note with kind=Tip + basis=Observed AND topic="gotchas" should appear
    // ONCE in the virtual gotchas section (under "Learned gotchas"), not also
    // in a separate "Related Notes" section.
    use chrono::Utc;
    let entry = fake_entry("foo", vec![]);
    let manifest = Manifest::default();
    let note = Note {
        timestamp: Utc::now(),
        session_id: None,
        server: "foo".into(),
        tool: Some("bar".into()),
        topic: Some("gotchas".into()),
        kind: NoteKind::Tip,
        basis: NoteBasis::Observed,
        claim: "X is rate-limited at 1/sec".into(),
        tags: vec![],
        possibly_stale: false,
    };
    let out = playbook::render_help(&entry, Some(&manifest), &[note], Some("gotchas"));
    // Should appear exactly once — count occurrences of the claim text
    let occurrences = out.matches("X is rate-limited at 1/sec").count();
    assert_eq!(
        occurrences, 1,
        "expected note to render exactly once in virtual gotchas topic, got {occurrences}:\n{out}"
    );
    // Specifically: the Related Notes section should not appear at all
    assert!(
        !out.contains("## Related Notes"),
        "virtual topic should suppress the separate Related Notes section"
    );
}

#[test]
fn user_defined_topic_wins_over_virtual_name() {
    // If the manifest defines a topic literally named "gotchas", we render
    // THAT, not the virtual section.
    let entry = fake_entry("foo", vec![]);
    let manifest = Manifest {
        gotchas: vec!["from-manifest-array".into()],
        topics: vec![ManifestTopic {
            name: "gotchas".into(),
            title: "Custom Gotchas Section".into(),
            body: "user-authored topic content".into(),
        }],
        ..Default::default()
    };
    let out = playbook::render_help(&entry, Some(&manifest), &[], Some("gotchas"));
    assert!(out.contains("Custom Gotchas Section"));
    assert!(out.contains("user-authored topic content"));
}

// --- manifest preview + fingerprint ---

#[test]
fn manifest_preview_summarizes_structure() {
    // Preview is intentionally compact (counts + names, not full content) —
    // the agent already has the proposed manifest in its own context, and
    // verbose previews trip up some MCP clients on the post-call render.
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
        gotchas: vec!["X is gated at 1/sec".into(), "Y too".into()],
    };
    let target = std::path::PathBuf::from("/tmp/foo.toml");
    let out = playbook::render_manifest_preview("foo", &manifest, &target, None);
    assert!(out.contains("PREVIEW"));
    assert!(out.contains("CREATE"));
    assert!(out.contains("comms"));
    assert!(out.contains("Chat server."));
    // Structural facts — names + counts, not full content
    assert!(out.contains("Read"));
    assert!(out.contains("3 tools"));
    assert!(out.contains("Threaded reply"));
    assert!(out.contains("rate_limits"));
    assert!(out.contains("Gotchas: 2 entries"));
    // Full gotcha text NOT in preview (full content is in the proposed manifest itself)
    assert!(!out.contains("X is gated at 1/sec"));
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
fn manifest_only_server_renders_manifest_categories() {
    // Regression: when entry.tools is empty (manifest-only / not installed)
    // but the manifest defines tool_categories, render those categories
    // directly so the user sees structure. Before this fix the overview
    // skipped the categories section entirely.
    let entry = mcp_librarian::index::entry_manifest_only("slack");
    let manifest = Manifest {
        meta: ManifestMeta {
            summary: Some("Slack workspace".into()),
            ..Default::default()
        },
        tool_categories: vec![
            ManifestCategory {
                name: "Channels".into(),
                tools: vec!["list_channels".into(), "get_history".into()],
            },
            ManifestCategory {
                name: "Messaging".into(),
                tools: vec!["post_message".into()],
            },
        ],
        ..Default::default()
    };
    let out = playbook::render_help(&entry, Some(&manifest), &[], None);
    assert!(out.contains("Tool Categories"));
    assert!(out.contains("Channels"));
    assert!(out.contains("list_channels"));
    assert!(out.contains("get_history"));
    assert!(out.contains("Messaging"));
    assert!(out.contains("post_message"));
    // Banner explains why these come from manifest not probe
    assert!(out.contains("from manifest"));
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
