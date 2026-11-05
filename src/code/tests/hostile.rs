//! A repository is untrusted input: hostile files, a stuck or dead neighbour and a slow refresh must end in a
//! bounded answer, not in an out-of-memory kill, an endless wait or a wedged database.

use std::time::{Duration, Instant};

use super::*;

/// `count` lines of `line` (one definition that calls itself, many times).
fn lines(line: &str, count: usize) -> String {
    let mut text = String::with_capacity(line.len() * count + count);

    for _ in 0..count {
        text.push_str(line);
        text.push('\n');
    }

    text
}

/// Indexes a repository of one file and returns how long it took, the counts and the edges.
fn index_one(file: &str, text: &str) -> (Duration, Counts, usize) {
    let mut fixture = Fixture::new();

    fixture.write(file, text);

    let root = fixture.root().to_path_buf();
    let started = Instant::now();
    let (graph, _) = fixture.index.graph(&root, &|| false).expect("graph");

    (started.elapsed(), graph.counts(), graph.edges())
}

#[test]
fn eighty_thousand_self_calling_functions_of_one_name_make_no_edges_and_finish_fast() {
    let cases = [
        ("a.rs", "fn x(){x()}"),
        ("a.rs", "impl A { fn x(&self){self.x()} }"),
        ("a.ts", "function x(){x()}"),
        ("a.ts", "class A { x(){this.x()} }"),
        ("a.py", "def x(): x()"),
        ("a.go", "func x(){x()}"),
    ];

    for (file, line) in cases {
        let (took, counts, edges) = index_one(file, &lines(line, 80_000));

        // the graph used to hold every definition-reference pair: 80 000 x 80 000 edges
        assert!(took < Duration::from_secs(60), "{file} {line}: {took:?}");
        assert!(edges <= 64 * counts.refs, "{file} {line}: {edges} edges");
        assert!(counts.symbols <= 20_000, "{file} {line}: {} symbols", counts.symbols);
        assert!(counts.refs <= 100_000, "{file} {line}: {} refs", counts.refs);
        assert_eq!(
            counts.resolved, 0,
            "{file} {line}: an ambiguous name resolves to nothing"
        );
    }
}

#[test]
fn thousands_of_subclasses_of_one_base_and_a_chain_of_bases_keep_the_search_bounded() {
    let mut fixture = Fixture::new();
    let mut classes = String::from("class Base:\n    def run(self):\n        pass\n");

    // a flat fan-out, and a chain of classes each deriving from the one before it
    for n in 0..5_000 {
        classes.push_str(&format!("class F{n}(Base):\n    def run(self):\n        pass\n"));
    }

    for n in 1..2_000 {
        classes.push_str(&format!("class C{n}(C{}):\n    def run(self):\n        pass\n", n - 1));
    }

    classes.push_str("class C0(Base):\n    def run(self):\n        pass\n");
    fixture.write("a/classes.py", &classes);
    fixture.write(
        "a/main.py",
        "from classes import Base\n\ndef go(base: Base):\n    base.run()\n\ndef nowhere():\n    pass\n",
    );

    let root = fixture.root().to_path_buf();
    let started = Instant::now();
    let (graph, _) = fixture.index.graph(&root, &|| false).expect("graph");

    // the first overrides are reached, the search for something that nothing leads to ends, and both are quick
    let missed = graph.path("go", "nowhere", 6);
    let first = graph.path("go", "F0.run", 3);

    assert!(missed.starts_with("no chain within 6 hops"), "{missed}");
    assert!(first.contains("via subclass/impl of Base::run"), "{first}");
    assert!(started.elapsed() < Duration::from_secs(60), "{:?}", started.elapsed());
}

#[test]
fn one_class_redefined_many_times_and_a_hub_calling_its_base_keep_a_search_without_chain_quick() {
    let mut fixture = Fixture::new();

    fixture.write("a/base.py", "class Base:\n    def run(self):\n        pass\n");

    // `F0` with an override of `run` in each of many files: all of them are overrides of `Base.run`
    for file in 0..10 {
        let mut classes = String::from("from base import Base\n\n");

        for _ in 0..5_000 {
            classes.push_str("class F0(Base):\n    def run(self):\n        pass\n");
        }

        fixture.write(&format!("a/f{file}.py"), &classes);
    }

    // a hub that calls `Base.run` thousands of times, and a function that nothing leads to
    let mut hub = String::from("from base import Base\n\ndef hub(b: Base):\n");

    for _ in 0..20_000 {
        hub.push_str("    b.run()\n");
    }

    hub.push_str("\ndef nowhere():\n    pass\n");
    fixture.write("a/hub.py", &hub);

    let root = fixture.root().to_path_buf();
    let (graph, _) = fixture.index.graph(&root, &|| false).expect("graph");
    let started = Instant::now();
    let missed = graph.path("hub", "nowhere", 8);
    let took = started.elapsed();

    assert!(missed.starts_with("no chain within 8 hops"), "{missed}");
    assert!(
        took < Duration::from_secs(if cfg!(debug_assertions) { 10 } else { 1 }),
        "{took:?}"
    );
}

#[test]
fn a_name_defined_in_thousands_of_files_is_too_ambiguous_to_link() {
    let mut fixture = Fixture::new();

    for n in 0..1_500 {
        fixture.write(&format!("d{n}/mod.rs"), "pub fn shared() {}\n");
    }

    fixture.write("user.rs", &lines("fn caller() { shared(); }", 5_000));

    let root = fixture.root().to_path_buf();
    let (graph, _) = fixture.index.graph(&root, &|| false).expect("graph");

    assert_eq!(graph.counts().resolved, 0);
    assert_eq!(graph.edges(), 0);
}

#[test]
fn the_definitions_a_file_may_hold_are_capped_and_the_cut_is_counted() {
    let mut fixture = Fixture::new();

    fixture.write(
        "big.rs",
        &(0..30_000).map(|n| format!("fn f{n}(){{}}\n")).collect::<String>(),
    );

    let refresh = fixture.refresh();

    assert_eq!(refresh.skipped.truncated, 1);
    assert_eq!(fixture.counts().symbols, 20_000);
}

#[test]
fn a_call_between_two_definitions_still_resolves() {
    let mut fixture = Fixture::new();

    fixture.write("a.rs", "pub fn one() { two(); }\npub fn two() {}\n");

    let counts = fixture.counts();

    assert_eq!(counts.resolved, 1);
}

/// An observer that spends the time budget of the build it watches.
struct Sleeper(Duration);

impl BuildObserver for Sleeper {
    fn on_start(&mut self, _id: i64, _kind: BuildKind, _total: usize) {
        std::thread::sleep(self.0);
    }

    fn on_progress(&mut self, _done: usize) {}

    fn on_finish(&mut self, _outcome: Result<(), &str>) {}
}

#[test]
fn a_refresh_past_its_time_budget_fails_and_the_next_call_goes_on() {
    let mut fixture = Fixture::new();

    for n in 0..1_200 {
        fixture.write(&format!("src/f{n}.rs"), &format!("pub fn g{n}_fn() {{}}\n"));
    }

    let root = fixture.root().to_path_buf();

    fixture.index.set_budget(Duration::from_secs(1));

    // the build starts within the budget and then runs past it
    let outcome = fixture
        .index
        .graph_observed(&root, &|| false, &mut Sleeper(Duration::from_millis(1_300)));

    assert_eq!(outcome.err().as_deref(), Some(TOO_SLOW));

    let status: String = fixture
        .index
        .store
        .conn()
        .query_row("SELECT status || ':' || error FROM builds", [], |row| row.get(0))
        .expect("the build row");

    assert_eq!(status, "error:indexing took too long");

    fixture.index.set_budget(pace::BUDGET);

    assert_eq!(fixture.counts().files, 1_200);
}

#[test]
fn the_walk_alone_is_held_to_the_time_budget() {
    let mut fixture = Fixture::new();

    // nothing to index and nothing to read: only the walk and its ignore files take time
    for n in 0..30 {
        fixture.write(&format!("d{n}/notes.md"), "text");
        fixture.write(&format!("d{n}/.gitignore"), &"# comment\n".repeat(20_000));
    }

    let root = fixture.root().to_path_buf();

    fixture.index.set_budget(Duration::ZERO);

    let outcome = fixture.index.graph(&root, &|| false);

    assert_eq!(outcome.err().as_deref(), Some(TOO_SLOW));

    fixture.index.set_budget(pace::BUDGET);

    assert_eq!(fixture.counts().files, 0);
}

#[test]
fn the_second_session_serves_the_last_committed_graph_while_the_first_holds_the_lock() {
    let mut fixture = Fixture::new();

    fixture.write("src/lib.rs", "pub fn old_name() {}\n");
    fixture.refresh();
    fixture.write("src/lib.rs", "pub fn new_name() {}\n");

    let path = fixture._data.path().join("code.sqlite3");
    let held = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let release = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (held_there, release_there) = (held.clone(), release.clone());

    // another session in the middle of a long build: it has the write lock and does not give it back
    let other = std::thread::spawn(move || {
        let conn = rusqlite::Connection::open(path).expect("open");

        conn.execute_batch("BEGIN IMMEDIATE").expect("lock");
        held_there.store(true, std::sync::atomic::Ordering::SeqCst);

        while !release_there.load(std::sync::atomic::Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(10));
        }

        conn.execute_batch("ROLLBACK").expect("release");
    });

    while !held.load(std::sync::atomic::Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(5));
    }

    fixture.index.set_patience(Duration::from_millis(300));

    let root = fixture.root().to_path_buf();
    let started = Instant::now();
    let (graph, refresh) = fixture.index.graph(&root, &|| false).expect("served, not failed");
    let served = graph.symbol("old_name", 5);

    // it used to wait for the whole busy timeout (a minute) and then fail with "database is locked"
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
    assert!(refresh.stale);
    assert!(served.contains("old_name"), "{served}");

    release.store(true, std::sync::atomic::Ordering::SeqCst);
    other.join().expect("the other session");

    let refresh = fixture.refresh();

    assert!(!refresh.stale);
    assert_eq!(refresh.reparsed, 1);
    assert!(fixture.symbol("new_name").contains("src/lib.rs:1"));
}

#[test]
fn a_build_that_died_is_ended_as_an_error_and_the_call_that_goes_on_leaves_its_status_file_ready() {
    let mut fixture = Fixture::new();

    fixture.write("src/lib.rs", "pub fn alpha() {}\n");
    fixture.refresh();

    // the corpse of an earlier process: a `running` row and a `building` status file, both long untouched
    let key = fixture.root().to_str().expect("utf-8").to_string();
    let repo = fixture.index.store.known(&key).expect("known").expect("repo").repo;

    fixture
        .index
        .store
        .conn()
        .execute(
            "INSERT INTO builds(repo_id, kind, status, started_at, files_total, files_done) VALUES (?1, 'full', 'running', ?2, 10, 3)",
            rusqlite::params![repo, store::now() - 10_000],
        )
        .expect("dead build");

    let data = fixture._data.path().to_path_buf();
    let mut status = status::StatusFile::new(&data, fixture.root());
    let path = status.path();

    std::fs::create_dir_all(path.parent().expect("folder")).expect("folder");
    std::fs::write(
        &path,
        r#"{"version":1,"buildId":1,"root":"x","repoName":"x","kind":"full","phase":"building","filesTotal":10,"filesDone":3,"startedAt":1,"finishedAt":null,"error":null}"#,
    )
    .expect("status");

    let old = std::time::SystemTime::now() - Duration::from_secs(3_600);

    std::fs::File::options()
        .write(true)
        .open(&path)
        .expect("status file")
        .set_modified(old)
        .expect("age it");

    // any change makes the next refresh take the write lock, and that is when the corpse is found
    fixture.write("src/lib.rs", "pub fn beta() {}\n");

    let root = fixture.root().to_path_buf();

    fixture
        .index
        .graph_observed(&root, &|| false, &mut status)
        .expect("graph");

    let rows: String = fixture
        .index
        .store
        .conn()
        .query_row(
            "SELECT group_concat(status || ':' || ifnull(error, ''), ',') FROM builds",
            [],
            |row| row.get(0),
        )
        .expect("rows");
    let document: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).expect("file")).expect("json");

    // the corpse ends as an error; the call that found it goes on with the rest, a build of its own, so the
    // file ends `ready` over a complete index instead of keeping the error
    assert_eq!(rows, "ok:,error:the build was interrupted,ok:");
    assert_eq!(document["phase"], "ready");
    assert!(document["error"].is_null());
}

#[test]
fn a_dead_build_of_another_repository_is_ended_and_its_status_file_stops_saying_building() {
    let mut fixture = Fixture::new();
    let other = tempfile::tempdir().expect("other repo");
    let other_root = other.path().to_path_buf();

    fixture.write("src/lib.rs", "pub fn alpha() {}\n");
    fixture.refresh();
    fs::write(other_root.join("lib.rs"), "pub fn gamma() {}\n").expect("write");
    fixture.index.graph(&other_root, &|| false).expect("other graph");

    // the corpse of a build of the other repository: a `running` row and an untouched `building` file
    let other_key = other_root.to_str().expect("utf-8").to_string();
    let other_repo = fixture
        .index
        .store
        .known(&other_key)
        .expect("known")
        .expect("repo")
        .repo;

    fixture
        .index
        .store
        .conn()
        .execute(
            "INSERT INTO builds(repo_id, kind, status, started_at, files_total, files_done) VALUES (?1, 'full', 'running', ?2, 10, 3)",
            rusqlite::params![other_repo, store::now() - 10_000],
        )
        .expect("dead build");

    let data = fixture._data.path().to_path_buf();
    let path = status::path_of(&data, &other_root);

    fs::create_dir_all(path.parent().expect("folder")).expect("folder");
    fs::write(
        &path,
        r#"{"version":1,"buildId":1,"root":"x","repoName":"x","kind":"full","phase":"building","filesTotal":10,"filesDone":3,"startedAt":1,"finishedAt":null,"error":null}"#,
    )
    .expect("status");
    std::fs::File::options()
        .write(true)
        .open(&path)
        .expect("status file")
        .set_modified(std::time::SystemTime::now() - Duration::from_secs(3_600))
        .expect("age it");

    // a refresh of this repository takes the write lock and finds both corpses
    fixture.write("src/lib.rs", "pub fn beta() {}\n");

    let root = fixture.root().to_path_buf();
    let mut status = status::StatusFile::new(&data, &root);

    fixture
        .index
        .graph_observed(&root, &|| false, &mut status)
        .expect("graph");

    let running: i64 = fixture
        .index
        .store
        .conn()
        .query_row("SELECT count(*) FROM builds WHERE status = 'running'", [], |row| {
            row.get(0)
        })
        .expect("rows");
    let document: serde_json::Value = serde_json::from_slice(&fs::read(&path).expect("file")).expect("json");

    assert_eq!(running, 0);
    assert_eq!(document["phase"], "error");
    assert_eq!(document["error"], "the build was interrupted");
}

#[test]
fn a_repository_nobody_used_for_a_month_is_dropped_when_another_one_is_built() {
    let mut fixture = Fixture::new();

    fixture.write("src/lib.rs", "pub fn alpha() {}\n");
    fixture.refresh();

    let key = fixture.root().to_str().expect("utf-8").to_string();

    fixture
        .index
        .store
        .conn()
        .execute(
            "UPDATE repos SET used_at = ?1 WHERE root = ?2",
            rusqlite::params![store::now() - store::UNUSED_SECONDS - 10, key],
        )
        .expect("age it");

    let other = tempfile::tempdir().expect("other repo");

    fs::write(other.path().join("lib.rs"), "pub fn beta() {}\n").expect("write");
    fixture
        .index
        .graph(other.path(), &|| false)
        .expect("the other repository");

    assert!(fixture.index.store.known(&key).expect("known").is_none());
    assert!(
        fixture
            .index
            .store
            .known(other.path().to_str().expect("utf-8"))
            .expect("known")
            .is_some()
    );

    let files: i64 = fixture
        .index
        .store
        .conn()
        .query_row("SELECT count(*) FROM files WHERE path = 'src/lib.rs'", [], |row| {
            row.get(0)
        })
        .expect("count");

    assert_eq!(files, 0, "the rows of the dropped repository are gone too");
}

#[test]
fn a_repository_in_use_is_kept() {
    let mut fixture = Fixture::new();

    fixture.write("src/lib.rs", "pub fn alpha() {}\n");
    fixture.refresh();

    let other = tempfile::tempdir().expect("other repo");

    fs::write(other.path().join("lib.rs"), "pub fn beta() {}\n").expect("write");
    fixture
        .index
        .graph(other.path(), &|| false)
        .expect("the other repository");

    let key = fixture.root().to_str().expect("utf-8");

    assert!(fixture.index.store.known(key).expect("known").is_some());
}

#[test]
fn the_path_from_a_symbol_to_itself_is_zero_hops() {
    let mut fixture = Fixture::new();

    fixture.write("src/lib.rs", "pub fn alpha() { beta(); }\npub fn beta() { alpha(); }\n");

    let same = fixture.chain("alpha", "alpha", 4);

    assert!(same.starts_with("`alpha` → `alpha`: 0 hops"), "{same}");
    assert!(same.contains("src/lib.rs:1"), "{same}");
    assert!(!same.contains("no chain"), "{same}");
}

#[test]
fn impact_does_not_count_the_file_that_defines_the_symbol_as_affected() {
    let mut fixture = Fixture::new();

    fixture.write("src/def.rs", "pub fn target() {}\n");
    fixture.write("src/user.rs", "pub fn user() { target(); }\n");

    let impact = fixture.impact("target", 2);

    assert!(impact.contains("1 production file(s)"), "{impact}");
    assert!(impact.contains("d0 src/def.rs  [defined here: target]"), "{impact}");
    assert!(impact.contains("d1 src/user.rs"), "{impact}");
}

#[test]
fn a_file_that_defines_and_calls_the_symbol_is_affected_by_its_callers() {
    let mut fixture = Fixture::new();

    fixture.write("src/both.rs", "pub fn target() {}\npub fn caller() { target(); }\n");

    let impact = fixture.impact("target", 2);

    assert!(impact.contains("1 production file(s)"), "{impact}");
    assert!(impact.contains("d0 src/both.rs  [defined here: target]"), "{impact}");
    assert!(impact.contains("d1 src/both.rs  [caller → target]"), "{impact}");
}

#[test]
fn a_method_defined_on_one_type_name_in_over_a_thousand_places_is_ambiguous_not_cut_to_the_first_ones() {
    let mut fixture = Fixture::new();

    // 1 016 Rust definitions come first, then 100 TypeScript ones: the call below could only mean the latter
    fixture.write("a.rs", &"impl A { fn x(&self) {} }\n".repeat(1_016));
    fixture.write("b.ts", &"class A { x() {} }\n".repeat(100));
    fixture.write("c.ts", "import { A } from './b';\nfunction f(a: A) { a.x(); }\n");

    let root = fixture.root().to_path_buf();
    let (graph, _) = fixture.index.graph(&root, &|| false).expect("graph");

    // the 8 definitions inside the first 1 024 used to be taken for all of them
    assert_eq!(graph.edges(), 0);
}

#[test]
fn a_method_defined_in_a_few_places_still_resolves() {
    let mut fixture = Fixture::new();

    fixture.write("a.rs", &"impl A { fn x(&self) {} }\n".repeat(3));
    fixture.write("b.ts", &"class A { x() {} }\n".repeat(3));
    fixture.write("c.ts", "import { A } from './b';\nfunction f(a: A) { a.x(); }\n");

    let root = fixture.root().to_path_buf();
    let (graph, _) = fixture.index.graph(&root, &|| false).expect("graph");

    assert!(graph.edges() > 0);
}

/// How many rows `table` holds.
fn rows(fixture: &Fixture, table: &str) -> i64 {
    fixture
        .index
        .store
        .conn()
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row.get(0))
        .expect("count")
}

#[test]
fn a_repository_past_the_caps_stops_writing_at_the_cap_fails_fast_and_retries_once_it_shrank() {
    let mut fixture = Fixture::new();

    // 600 files of one definition each: the first batch of 500 fits, the second would pass the cap
    for n in 0..600 {
        fixture.write(&format!("src/f{n:03}.rs"), &format!("pub fn g{n:03}() {{}}\n"));
    }

    fixture.index.caps = store::Caps {
        syms: 550,
        refs: 10_000,
        imports: 10_000,
        ..store::Caps::default()
    };

    let root = fixture.root().to_path_buf();

    assert_eq!(fixture.index.graph(&root, &|| false).err().as_deref(), Some(TOO_LARGE));

    // the database never held more than the cap (it used to take every file: 600 rows)
    let held = rows(&fixture, "symbols");
    let files = rows(&fixture, "files");
    let builds = rows(&fixture, "builds");

    assert!(held <= 550, "{held}");
    assert_eq!(held, 500);

    // the next calls do nothing: no build, no row
    for _ in 0..2 {
        assert_eq!(fixture.index.graph(&root, &|| false).err().as_deref(), Some(TOO_LARGE));
        assert_eq!(rows(&fixture, "symbols"), held);
        assert_eq!(rows(&fixture, "files"), files);
        assert_eq!(rows(&fixture, "builds"), builds);
    }

    // the repository shrinks to what fits: the index goes on from where it stopped
    for n in 550..600 {
        fs::remove_file(fixture.path(&format!("src/f{n:03}.rs"))).expect("remove");
    }

    let counts = fixture.counts();

    assert_eq!(counts.files, 550);
    assert_eq!(counts.symbols, 550);

    // and it is a normal index again
    assert!(fixture.symbol("g010").contains("src/f010.rs:1"));
}

#[test]
fn the_caps_hold_for_references_and_imports_too() {
    let mut fixture = Fixture::new();

    fixture.write(
        "a.rs",
        &format!("use crate::b::one;\nfn f() {{ {} }}\n", "g();".repeat(30)),
    );
    fixture.index.caps = store::Caps {
        syms: 100,
        refs: 20,
        imports: 100,
        ..store::Caps::default()
    };

    let root = fixture.root().to_path_buf();

    assert_eq!(fixture.index.graph(&root, &|| false).err().as_deref(), Some(TOO_LARGE));
    assert_eq!(rows(&fixture, "refs"), 0);

    fixture.write("a.rs", "use crate::b::one;\nuse crate::b::two;\nfn f() {}\n");
    fixture.index.caps = store::Caps {
        syms: 100,
        refs: 100,
        imports: 1,
        ..store::Caps::default()
    };

    assert_eq!(fixture.index.graph(&root, &|| false).err().as_deref(), Some(TOO_LARGE));
    assert_eq!(rows(&fixture, "imports"), 0);
}

#[test]
fn a_repository_past_the_binding_cap_is_marked_too_large_and_writes_no_more() {
    let mut fixture = Fixture::new();

    // many files, each with a few assignments that bind a name: the bindings alone pass the cap
    for n in 0..30 {
        fixture.write(&format!("m{n}.py"), &format!("def f():\n{}", " a=b\n".repeat(10)));
    }

    fixture.index.caps = store::Caps {
        binds: 100,
        ..store::Caps::default()
    };

    let root = fixture.root().to_path_buf();

    assert_eq!(fixture.index.graph(&root, &|| false).err().as_deref(), Some(TOO_LARGE));

    let held = rows(&fixture, "binds");
    let builds = rows(&fixture, "builds");

    assert!(held <= 100, "{held}");

    // the repository is marked, so the next calls read nothing and write nothing
    for _ in 0..2 {
        assert_eq!(fixture.index.graph(&root, &|| false).err().as_deref(), Some(TOO_LARGE));
        assert_eq!(rows(&fixture, "binds"), held);
        assert_eq!(rows(&fixture, "builds"), builds);
    }
}

#[test]
fn the_bindings_of_a_file_that_goes_are_taken_off_the_repository_total() {
    let mut fixture = Fixture::new();

    fixture.write("a.py", &format!("def f():\n{}", " a=b\n".repeat(10)));
    fixture.write("b.py", &format!("def g():\n{}", " a=b\n".repeat(10)));

    let root = fixture.root().to_path_buf();
    fixture.index.graph(&root, &|| false).expect("graph");
    fs::remove_file(fixture.path("a.py")).expect("remove");
    fixture.index.graph(&root, &|| false).expect("graph");

    let counted: i64 = fixture
        .index
        .store
        .conn()
        .query_row("SELECT binds FROM repos", [], |row| row.get(0))
        .expect("total");

    assert_eq!(counted, rows(&fixture, "binds"));
    assert!(counted > 0);
}

#[test]
fn a_string_that_names_nothing_defined_is_stored_but_not_loaded_into_the_graph() {
    let key = ["sk_", "live_", "4eC39HqLyjWDarjtT1zdp7dc"].concat();
    let mut named = Fixture::new();

    named.write(
        "a.rs",
        "pub fn run() { send(\"never_defined_name\"); send(\"other_unknown_name\"); }\n",
    );
    named.write(
        "b.rs",
        &format!("pub fn keyed() {{ send(\"{key}\"); }}\npub fn defined_handler() {{}}\n"),
    );
    named.write("c.rs", "pub fn caller() { send(\"defined_handler\"); }\n");

    // the strings that name nothing are in the index file, the one that names a definition links to it
    let _ = named.counts();
    let stored: i64 = named
        .index
        .store
        .conn()
        .query_row("SELECT COUNT(*) FROM refs WHERE kind = 'str'", [], |row| row.get(0))
        .expect("count");
    let texts: Vec<String> = named
        .index
        .store
        .conn()
        .prepare("SELECT name FROM refs WHERE kind = 'str'")
        .expect("prepare")
        .query_map([], |row| row.get(0))
        .expect("query")
        .map(|row| row.expect("row"))
        .collect();

    assert!(texts.contains(&"never_defined_name".to_owned()), "{texts:?}");
    assert!(
        !texts.contains(&key),
        "a credential written like a name is not stored: {texts:?}"
    );
    assert_eq!(stored, 3, "{texts:?}");

    // in the graph only what can resolve is held: the four calls of `send` and the one string that names a
    // definition (the two other strings are left out)
    assert_eq!(named.counts().refs, 5);
}

/// A TSX component that writes the class tokens `t0 .. t{count}`, one to a line (a line of thousands of bytes
/// marks a file as minified).
fn component_with_classes(count: usize) -> String {
    let tokens: Vec<String> = (0..count).map(|n| format!("t{n}")).collect();

    format!(
        "export function Page() {{ return <p className=\"{}\">x</p>; }}\n",
        tokens.join("\n")
    )
}

#[test]
fn class_tokens_past_their_cap_are_cut_and_told_but_never_make_the_repository_too_large() {
    let mut fixture = Fixture::new();

    fixture.write("web/Page.tsx", &component_with_classes(1_000));
    fixture.write("web/page.css", ".t0 { color: red; }\n.t999 { color: red; }\n");
    fixture.index.caps = store::Caps {
        refs: 20,
        classes: 50,
        ..store::Caps::default()
    };

    let root = fixture.root().to_path_buf();

    assert!(fixture.index.graph(&root, &|| false).is_ok());
    assert_eq!(rows(&fixture, "refs WHERE kind = 'cls'"), 50);
    assert_eq!(rows(&fixture, "files WHERE class_cut = 1"), 1);

    let first = fixture.symbol("web/page.css::.t0");
    let last = fixture.symbol("web/page.css::.t999");

    assert!(first.contains("web/Page.tsx:1 Page (class)"), "{first}");
    assert!(
        first.contains("class uses past the index caps were not read (1 files"),
        "{first}"
    );
    assert!(
        last.contains("used by 0 site(s)") && last.contains("class uses past the index caps were not read"),
        "a use that was cut is told, not reported as run-time: {last}"
    );

    // a reference of the code still counts toward the cap
    fixture.write(
        "web/Page.tsx",
        &format!("export function Page() {{ {} }}\n", "g();".repeat(30)),
    );

    assert_eq!(fixture.index.graph(&root, &|| false).err().as_deref(), Some(TOO_LARGE));
}

#[test]
fn pages_left_out_by_the_page_cap_are_told_in_the_count_of_a_selector() {
    let mut fixture = Fixture::new();

    fixture.write("web/page.css", ".lead { color: red; }\n");

    for n in 0..crate::code::walk::MAX_HTML_FILES {
        fixture.write(&format!("docs/p{n:04}.html"), "<p class=\"lead\">x</p>\n");
    }

    let whole = fixture.symbol("web/page.css::.lead");

    assert!(!whole.contains("class uses past the index caps"), "{whole}");

    fixture.write("docs/p9999.html", "<p class=\"lead\">x</p>\n");
    fixture.write("docs/p9998.html", "<p class=\"lead\">x</p>\n");

    let cut = fixture.symbol("web/page.css::.lead");

    assert!(
        cut.contains("class uses past the index caps were not read (2 pages), so this count may be low"),
        "{cut}"
    );
}

#[test]
fn a_token_on_two_hundred_selectors_keeps_its_own_rule_and_says_the_list_is_cut() {
    let mut fixture = Fixture::new();
    let mut sheet: String = (0..200).map(|n| format!(".p{n} .btn {{ color: red; }}\n")).collect();

    sheet.push_str(".btn { color: blue; }\n");
    fixture.write("web/btn.css", &sheet);
    fixture.write(
        "web/Button.tsx",
        "export function Button() { return <button className=\"btn\">x</button>; }\n",
    );

    let exact = fixture.symbol("web/btn.css::.btn");
    let past = fixture.symbol("web/btn.css::.p199 .btn");
    let early = fixture.symbol("web/btn.css::.p0 .btn");

    assert!(exact.contains("web/Button.tsx:1 Button (class)"), "{exact}");
    assert!(
        exact.contains("class `btn` is on 201 selectors; uses are linked to the first 64, so this count may be low"),
        "{exact}"
    );
    assert!(early.contains("web/Button.tsx:1 Button (class)"), "{early}");
    assert!(
        past.contains("used by 0 site(s)") && past.contains("class `btn` is on 201 selectors"),
        "{past}"
    );
}

#[test]
fn thousands_of_class_uses_leave_the_edges_of_the_code_as_they_were() {
    let sites = |classes: bool| {
        let mut fixture = Fixture::new();

        fixture.write(
            "web/lib.ts",
            "export function helper() { return 1; }\nexport function again() { return helper(); }\n",
        );
        fixture.write(
            "web/use.ts",
            "import { helper } from './lib';\nexport function run() { return helper(); }\n",
        );

        if classes {
            let sheet: String = (0..3_000).map(|n| format!(".t{n} .u{n} {{ color: red; }}\n")).collect();

            fixture.write("web/many.css", &sheet);

            for part in 0..3 {
                let tokens: Vec<String> = (0..1_000).map(|n| format!("t{}", part * 1_000 + n)).collect();

                fixture.write(
                    &format!("web/Page{part}.tsx"),
                    &format!(
                        "export function Page{part}() {{ return <p className=\"{}\">x</p>; }}\n",
                        tokens.join("\n")
                    ),
                );
            }
        }

        if classes {
            assert!(
                fixture.symbol("web/many.css::.t2005 .u2005").contains("Page2 (class)"),
                "the class links are made"
            );
        }

        let answer = fixture.symbol("web/lib.ts::helper");

        answer
            .lines()
            .filter(|line| line.contains("web/"))
            .map(str::to_string)
            .collect::<Vec<_>>()
    };

    assert_eq!(sites(true), sites(false));
}

/// Components that each import one stylesheet hub of `imports` imports (none of them exists), and write a
/// class the stylesheet next to it styles: the lookups of the hub are the work of the class pass.
fn hub_fixture(files: usize, imports: usize) -> Fixture {
    let fixture = Fixture::new();
    let hub: String = (0..imports).map(|n| format!("@import \"./m{n}\";\n")).collect();

    fixture.write("src/hub.scss", &hub);
    fixture.write("src/x.css", ".x { top: 0; }\n");

    for n in 0..files {
        fixture.write(
            &format!("src/p{n}.tsx"),
            &format!("import './hub.scss';\nexport const c{n} = () => <div className=\"x\" />;\n"),
        );
    }

    fixture
}

/// A cancellation check that is slow, so that the time a stage takes follows the number of times it asks and
/// not how fast the machine is.
fn slow_stop() -> bool {
    std::thread::sleep(Duration::from_millis(2));

    false
}

/// A budget that leaves the stages before the class pass room on this machine and that the class pass cannot finish
/// in. What those stages cost differs by far between machines: what they do without the slow check, and how long a
/// slow check really sleeps. The stylesheet lookups alone ask the pace about thirteen hundred times; the stages before
/// them ask far fewer, so six hundred and fifty checks and twice the plain work fit them and not the lookups.
fn budget_past_the_stages_before_the_class_pass() -> Duration {
    let mut twin = hub_fixture(60, 20_000);
    let root = twin.root().to_path_buf();

    twin.index.set_budget(Duration::from_secs(120));

    let plain = Instant::now();

    twin.index
        .validate(&root, &|| false, &mut crate::code::builds::NoObserver, &|| {})
        .expect("the same index without the slow check");

    let plain = plain.elapsed();
    let checks = Instant::now();

    for _ in 0..50 {
        slow_stop();
    }

    plain * 2 + checks.elapsed() * 13
}

#[test]
fn a_class_pass_that_runs_out_of_its_time_does_not_fail_the_index_that_the_worker_validates() {
    let mut fixture = hub_fixture(60, 20_000);
    let root = fixture.root().to_path_buf();
    let budget = budget_past_the_stages_before_the_class_pass();

    fixture.index.set_budget(budget);

    let (_, _, counts) = fixture
        .index
        .validate(&root, &slow_stop, &mut crate::code::builds::NoObserver, &|| {})
        .expect("a class pass out of time is no reason to fail the index");

    assert_eq!(counts.files, 61 + 1);

    // the same load and trial on one pace, as the worker runs them: the uses are counted as unread
    let key = root.to_str().expect("utf-8").to_owned();
    let repo = fixture.index.store.known(&key).expect("known").expect("repo").repo;
    let pace = Pace::new(&slow_stop, budget);
    let answer = fixture
        .index
        .store
        .snapshot(|conn| {
            let graph = Graph::load(conn, repo, &pace)?;

            graph.trial(&pace)?;

            Ok(graph.symbol(".x", 20))
        })
        .expect("load and trial");

    assert!(
        answer.contains("class uses past the index caps were not read"),
        "{answer}"
    );
}

/// One stylesheet that `hubs` hubs import by way of `dups` spellings of one folder that holds sixteen sheets.
fn fan_fixture(files: usize, hubs: usize, dups: usize) -> Fixture {
    let fixture = Fixture::new();

    for ext in ["scss", "sass", "css", "less"] {
        for p in ["y", "_y", "y/_index", "y/index"] {
            fixture.write(&format!("src/{p}.{ext}"), "");
        }
    }

    for h in 0..hubs {
        let text: String = (0..dups).map(|i| format!("@import \"./y?{i}\";\n")).collect();

        fixture.write(&format!("src/h{h}.scss"), &text);
    }

    let hub_imports: String = (0..hubs).map(|h| format!("@import \"./h{h}.scss\";\n")).collect();

    fixture.write("src/m.scss", &format!(".x {{ top: 0; }}\n{hub_imports}"));

    for i in 0..files {
        fixture.write(
            &format!("src/p{i}.tsx"),
            &format!("import './m.scss';\nexport const c{i} = () => <div className=\"x\"/>;\n"),
        );
    }

    fixture
}

#[test]
fn imports_that_resolve_to_the_same_sixteen_sheets_are_walked_once_not_once_per_spelling() {
    let fixture = fan_fixture(2_000, 40, 400);
    let root = fixture.root().to_path_buf();
    let started = Instant::now();

    fixture
        .index
        .validate(&root, &|| false, &mut crate::code::builds::NoObserver, &|| {})
        .expect("validate");

    // without the duplicates removed, every use walked sixteen sheets for each of sixteen thousand spellings
    assert!(started.elapsed() < Duration::from_secs(8), "{:?}", started.elapsed());
}
