use std::collections::HashSet;

use fm_fs::attrs::{ARCHIVE, DIRECTORY, HIDDEN, NORMAL, SYSTEM};
use fm_index::r#gen::{XorShift64, generate};
use fm_index::{CancelToken, EntryId, Index, Meta, SearchOptions, parse_query};

fn m_file(mtime: i64, hidden: bool) -> Meta {
    Meta {
        attrs: ARCHIVE | if hidden { HIDDEN } else { 0 },
        size: 1024,
        mtime,
    }
}

fn m_dir(mtime: i64, hidden: bool) -> Meta {
    Meta {
        attrs: DIRECTORY | if hidden { HIDDEN } else { 0 },
        size: 0,
        mtime,
    }
}

fn build_fixture() -> Index {
    let mut idx = Index::new();
    let c = idx.add_root("C:\\");

    let proj = idx.insert(c, "Projekty", m_dir(100, false));
    let docs = idx.insert(c, "Dokumenty", m_dir(100, false));
    let hidden_dir = idx.insert(c, "TajnáSložka", m_dir(100, true));
    let sys_dir = idx.insert(
        c,
        "SystemData",
        Meta {
            attrs: DIRECTORY | SYSTEM,
            size: 0,
            mtime: 100,
        },
    );
    let sub_in_hidden = idx.insert(hidden_dir, "Podsložka", m_dir(100, false));
    let sub_proj = idx.insert(proj, "Alpha_Client", m_dir(200, false));

    idx.insert(docs, "Žluťoučký.txt", m_file(500, false));
    idx.insert(docs, "Příliš žluťoučký kůň.docx", m_file(400, false));
    idx.insert(docs, "Smlouva 2025.pdf", m_file(600, false));
    idx.insert(docs, "Faktura-leden.PDF", m_file(700, false));
    idx.insert(docs, "Straße_Plan.xlsx", m_file(300, false));
    idx.insert(docs, "Łódź_Notes.md", m_file(300, false));
    idx.insert(docs, "Søren_Øresund.txt", m_file(300, false));
    idx.insert(docs, "Đakovo_Report.txt", m_file(300, false));
    idx.insert(docs, "two words phrase.txt", m_file(250, false));
    idx.insert(docs, "words two reversed.txt", m_file(250, false));
    idx.insert(docs, "myCamelCaseHandler.rs", m_file(800, false));
    idx.insert(docs, "XMLParser.rs", m_file(750, false));
    idx.insert(docs, "foo:bar_weird", m_file(100, false));
    idx.insert(docs, ".gitignore", m_file(100, false));
    idx.insert(docs, "secret_file.txt", m_file(900, true));

    idx.insert(proj, "design_spec.pdf", m_file(500, false));
    idx.insert(proj, "alpha", m_file(500, false));
    idx.insert(sub_proj, "budget_final.xlsx", m_file(550, false));
    idx.insert(sub_proj, "client_meeting_notes.txt", m_file(550, false));

    idx.insert(hidden_dir, "under_hidden.txt", m_file(999, false));
    idx.insert(sub_in_hidden, "deep_under_hidden.pdf", m_file(999, false));
    idx.insert(
        sys_dir,
        "pagefile.sys",
        Meta {
            attrs: ARCHIVE,
            size: 4096,
            mtime: 999,
        },
    );

    idx
}

#[test]
fn semantics_table_48_cases() {
    let idx = build_fixture();
    let cancel = CancelToken::new();

    let cases: &[(&str, bool, &[&str])] = &[
        ("", false, &[]),
        ("   \t\n ", false, &[]),
        ("\"\"", false, &[]),
        ("\"   \"", false, &[]),
        (
            "zlu",
            false,
            &["Žluťoučký.txt", "Příliš žluťoučký kůň.docx"],
        ),
        ("ZLUTOUCKY.TXT", false, &["Žluťoučký.txt"]),
        ("kun", false, &["Příliš žluťoučký kůň.docx"]),
        ("strasse", false, &["Straße_Plan.xlsx"]),
        ("lodz", false, &["Łódź_Notes.md"]),
        ("soren oresund", false, &["Søren_Øresund.txt"]),
        ("dakovo", false, &["Đakovo_Report.txt"]),
        ("handler", false, &["myCamelCaseHandler.rs"]),
        ("smlouva 2025", false, &["Smlouva 2025.pdf"]),
        ("2025 smlouva", false, &["Smlouva 2025.pdf"]),
        (
            "two words",
            false,
            &["two words phrase.txt", "words two reversed.txt"],
        ),
        ("zlutoucky smlouva", false, &[]),
        ("\"two words\"", false, &["two words phrase.txt"]),
        ("\"words two\"", false, &["words two reversed.txt"]),
        ("\"žluťoučký kůň\"", false, &["Příliš žluťoučký kůň.docx"]),
        (
            "ext:pdf",
            false,
            &["Smlouva 2025.pdf", "Faktura-leden.PDF", "design_spec.pdf"],
        ),
        (
            "ext:.PDF",
            false,
            &["Smlouva 2025.pdf", "Faktura-leden.PDF", "design_spec.pdf"],
        ),
        (
            "ext:pdf;xlsx",
            false,
            &[
                "Smlouva 2025.pdf",
                "Faktura-leden.PDF",
                "design_spec.pdf",
                "Straße_Plan.xlsx",
                "budget_final.xlsx",
            ],
        ),
        (
            "ext:rs;md",
            false,
            &["myCamelCaseHandler.rs", "XMLParser.rs", "Łódź_Notes.md"],
        ),
        ("faktura ext:pdf;docx", false, &["Faktura-leden.PDF"]),
        ("ext:nonexistent", false, &[]),
        (
            "path:projekty",
            false,
            &[
                "Projekty",
                "Alpha_Client",
                "design_spec.pdf",
                "alpha",
                "budget_final.xlsx",
                "client_meeting_notes.txt",
            ],
        ),
        (
            "path:projekty/alpha_client",
            false,
            &[
                "Alpha_Client",
                "budget_final.xlsx",
                "client_meeting_notes.txt",
            ],
        ),
        (
            "path:Projekty\\Alpha_Client",
            false,
            &[
                "Alpha_Client",
                "budget_final.xlsx",
                "client_meeting_notes.txt",
            ],
        ),
        (
            "notes path:alpha_client",
            false,
            &["client_meeting_notes.txt"],
        ),
        (
            "path:\"Alpha_Client\"",
            false,
            &[
                "Alpha_Client",
                "budget_final.xlsx",
                "client_meeting_notes.txt",
            ],
        ),
        ("path:nonexistent", false, &[]),
        (
            "folder:",
            false,
            &["C:\\", "Projekty", "Dokumenty", "Alpha_Client"],
        ),
        ("folder:alpha", false, &["Alpha_Client"]),
        ("file:alpha", false, &["alpha"]),
        ("folder:proj", false, &["Projekty"]),
        ("file:budget", false, &["budget_final.xlsx"]),
        ("folder:a file:b", false, &[]),
        ("secret", false, &[]),
        ("secret", true, &["secret_file.txt"]),
        ("under_hidden", false, &[]),
        (
            "under_hidden",
            true,
            &["under_hidden.txt", "deep_under_hidden.pdf"],
        ),
        ("deep_under_hidden", false, &[]),
        ("deep_under_hidden", true, &["deep_under_hidden.pdf"]),
        ("pagefile", false, &[]),
        ("pagefile", true, &["pagefile.sys"]),
        ("foo:bar", false, &["foo:bar_weird"]),
        ("gitignore", false, &[".gitignore"]),
        ("\"two words", false, &["two words phrase.txt"]),
    ];

    assert!(cases.len() >= 40);
    for (i, &(q_str, include_hidden, expected)) in cases.iter().enumerate() {
        let q = parse_query(q_str);
        let opts = SearchOptions {
            include_hidden,
            limit: 1000,
        };
        let res = idx
            .search(&q, &opts, &cancel)
            .expect("search should not cancel");
        let actual: HashSet<&str> = res.ids.iter().map(|&id| idx.name(id)).collect();
        let exp: HashSet<&str> = expected.iter().copied().collect();
        assert_eq!(
            actual, exp,
            "case #{i}: query={q_str:?}, include_hidden={include_hidden}"
        );
        assert_eq!(res.total_matches, exp.len(), "case #{i} total_matches");
    }
}
#[test]
fn ranking_tiers_and_tie_breakers() {
    let mut idx = Index::new();
    let root = idx.add_root("C:\\");
    let d_a = idx.insert(root, "A", m_dir(100, false));
    let d_b = idx.insert(root, "B", m_dir(100, false));

    // Exact > Prefix > WordBoundary (space, _, -, ., camelCase) > Substring
    idx.insert(root, "xplanx", m_file(999, false));
    idx.insert(root, "my plan", m_file(100, false));
    idx.insert(root, "my_plan", m_file(100, false));
    idx.insert(root, "my-plan", m_file(100, false));
    idx.insert(root, "my.plan", m_file(100, false));
    idx.insert(root, "myPlan", m_file(100, false));
    idx.insert(root, "plan_2", m_file(100, false));
    idx.insert(root, "plan", m_dir(100, false));

    // Ties on same tier: shorter name, then newer mtime, then path
    idx.insert(d_b, "plan_xy", m_file(200, false));
    idx.insert(d_a, "plan_xz", m_file(200, false));
    idx.insert(d_a, "plan_xw", m_file(500, false));

    let cancel = CancelToken::new();
    let res = idx
        .search(
            &parse_query("plan"),
            &SearchOptions {
                include_hidden: false,
                limit: 100,
            },
            &cancel,
        )
        .unwrap();
    let paths: Vec<String> = res.ids.iter().map(|&id| idx.path(id)).collect();

    assert_eq!(paths[0], "C:\\plan");
    assert_eq!(paths[1], "C:\\plan_2");
    assert_eq!(paths[2], "C:\\A\\plan_xw");
    assert_eq!(paths[3], "C:\\A\\plan_xz");
    assert_eq!(paths[4], "C:\\B\\plan_xy");
    assert_eq!(paths[5], "C:\\myPlan");
    assert_eq!(*paths.last().unwrap(), "C:\\xplanx");

    let wb_set: HashSet<&str> = paths[5..10].iter().map(String::as_str).collect();
    assert_eq!(
        wb_set,
        HashSet::from([
            "C:\\myPlan",
            "C:\\my plan",
            "C:\\my_plan",
            "C:\\my-plan",
            "C:\\my.plan",
        ])
    );
}

#[test]
fn rename_move_remove_compact_and_upsert() {
    let mut idx = Index::new();
    let c = idx.add_root("C:\\");
    assert_eq!(idx.add_root("c:/"), c);

    let dir_a = idx.insert(c, "Alpha", m_dir(10, false));
    let dir_b = idx.insert(c, "Beta", m_dir(10, false));
    let sub = idx.insert(dir_a, "Sub", m_dir(10, false));
    let file = idx.insert(sub, "Document.txt", m_file(10, false));
    assert_eq!(idx.len(), 5);

    // Upsert by (parent, folded name) updates case and meta in place
    let file_again = idx.insert(sub, "document.txt", m_file(99, false));
    assert_eq!(file_again, file);
    assert_eq!(idx.len(), 5);
    assert_eq!(idx.name(file), "document.txt");
    assert_eq!(idx.meta(file).mtime, 99);

    // Rename
    idx.rename(file, "Žluťoučký_Nový.pdf");
    assert_eq!(idx.path(file), "C:\\Alpha\\Sub\\Žluťoučký_Nový.pdf");
    assert_eq!(idx.lookup("c:/alpha/sub/zlutoucky_novy.pdf/"), None);
    assert_eq!(idx.lookup("c:/alpha/sub/žluťoučký_nový.pdf/"), Some(file));
    assert_eq!(idx.lookup("C:/ALPHA/SUB/ŽLUŤOUČKÝ_NOVÝ.PDF/"), Some(file));

    let cancel = CancelToken::new();
    let opts = SearchOptions::default();
    let r = idx.search(&parse_query("ext:pdf"), &opts, &cancel).unwrap();
    assert_eq!(r.ids, vec![file]);

    // Move subtree under hidden dir_b -> propagates effective hidden
    idx.update_meta(dir_b, m_dir(20, true));
    idx.move_to(sub, dir_b, "MovedSub");
    assert_eq!(idx.path(file), "C:\\Beta\\MovedSub\\Žluťoučký_Nový.pdf");
    assert_eq!(idx.lookup("C:\\Alpha\\Sub"), None);
    assert_eq!(idx.lookup("C:\\Beta\\MovedSub\\zlutoucky_novy.pdf"), None);
    assert_eq!(
        idx.lookup("C:\\Beta\\MovedSub\\ŽLUŤOUČKÝ_NOVÝ.PDF"),
        Some(file)
    );
    assert_eq!(
        idx.search(&parse_query("zlutoucky"), &opts, &cancel)
            .unwrap()
            .total_matches,
        0
    );
    assert_eq!(
        idx.search(
            &parse_query("zlutoucky"),
            &SearchOptions {
                include_hidden: true,
                limit: 100
            },
            &cancel
        )
        .unwrap()
        .ids,
        vec![file]
    );

    // Un-hide dir_b -> visible again
    idx.update_meta(dir_b, m_dir(25, false));
    let before_compact = idx
        .search(&parse_query("zlutoucky"), &opts, &cancel)
        .unwrap();
    assert_eq!(before_compact.ids, vec![file]);

    // Remove subtree removes dir_a? Move another item under dir_a then remove dir_a
    let doomed = idx.insert(dir_a, "Doomed", m_dir(10, false));
    let _doomed_file = idx.insert(doomed, "gone.txt", m_file(10, false));
    assert_eq!(idx.len(), 7);
    idx.remove(dir_a);
    assert_eq!(idx.len(), 4);
    assert_eq!(idx.lookup("C:\\Alpha"), None);
    assert_eq!(idx.lookup("C:\\Alpha\\Doomed\\gone.txt"), None);
    assert_eq!(
        idx.search(&parse_query("gone"), &opts, &cancel)
            .unwrap()
            .total_matches,
        0
    );

    // Compact remaps IDs densely and invalidates old Results for refine()
    let prev = idx.search(&parse_query("zlut"), &opts, &cancel).unwrap();
    idx.compact();
    assert_eq!(idx.len(), 4);
    let refined = idx
        .refine(&prev, &parse_query("zlutoucky"), &opts, &cancel)
        .unwrap();
    let new_file_id = idx
        .lookup("C:\\Beta\\MovedSub\\Žluťoučký_Nový.pdf")
        .unwrap();
    assert_eq!(refined.ids, vec![new_file_id]);
    assert!(new_file_id < 4);
}

#[test]
fn lookup_roundtrip_10k_generated_tree() {
    let idx = generate(10_000, 0x51a7_cafe);
    assert_eq!(idx.len(), 10_000);
    for raw_id in 0..idx.len() as EntryId {
        let p = idx.path(raw_id);
        assert_eq!(idx.lookup(&p), Some(raw_id), "round-trip failed for {p}");
        let slashed = p.replace('\\', "/") + "/";
        assert_eq!(
            idx.lookup(&slashed),
            Some(raw_id),
            "slash lookup failed for {slashed}"
        );
    }
    let _normal = NORMAL;
}

#[test]
fn refine_matches_fresh_search_on_200k_tree() {
    let idx = generate(200_000, 0x200_0000);
    assert_eq!(idx.len(), 200_000);

    let cancel = CancelToken::new();
    let mut rng = XorShift64::new(0xdead_beef_1234);
    let prefixes = [
        ("sm", "sml"),
        ("fa", "fak"),
        ("po", "poz"),
        ("pr", "pri"),
        ("re", "rep"),
        ("in", "inv"),
        ("bu", "bud"),
        ("no", "not"),
        ("se", "sea"),
        ("ca", "cac"),
        ("žl", "žlu"),
        ("ků", "kůň"),
        ("vý", "výk"),
        ("do", "dov"),
        ("en", "eng"),
        ("cl", "cli"),
        ("ar", "arc"),
        ("de", "des"),
        ("sp", "spe"),
        ("ma", "man"),
    ];
    let exts = ["", " ext:pdf", " ext:pdf;docx", " ext:rs", " ext:txt"];
    let kinds = ["", " folder:", " file:"];

    for i in 0..200 {
        let (p1, p2) = prefixes[rng.gen_range(prefixes.len())];
        let ext = exts[rng.gen_range(exts.len())];
        let kind = kinds[rng.gen_range(kinds.len())];

        let q1_str = format!("{p1}{ext}{kind}");
        let q2_str = if i % 4 == 0 {
            format!("{p2} 2{ext}{kind}")
        } else {
            format!("{p2}{ext}{kind}")
        };

        let opts = SearchOptions {
            include_hidden: i % 5 == 0,
            limit: if i % 3 == 0 { 50 } else { 1000 },
        };

        let q1 = parse_query(&q1_str);
        let q2 = parse_query(&q2_str);
        let prev = idx.search(&q1, &opts, &cancel).unwrap();
        let refined = idx.refine(&prev, &q2, &opts, &cancel).unwrap();
        let fresh = idx.search(&q2, &opts, &cancel).unwrap();

        assert_eq!(
            refined.total_matches, fresh.total_matches,
            "query #{i}: {q1_str:?} -> {q2_str:?}"
        );
        assert_eq!(
            refined.ids, fresh.ids,
            "query #{i} ids mismatch: {q1_str:?} -> {q2_str:?}"
        );
    }
}

#[test]
fn cancellation_generation_and_explicit_cancel() {
    let idx = generate(5_000, 99);
    let source = CancelToken::new();
    let t1 = source.next();
    let t2 = source.next();

    assert!(t1.is_cancelled());
    assert!(!t2.is_cancelled());

    let q = parse_query("smlouva");
    let opts = SearchOptions::default();
    assert!(idx.search(&q, &opts, &t1).is_none());
    let r2 = idx.search(&q, &opts, &t2).expect("t2 is active");

    t2.cancel();
    assert!(t2.is_cancelled());
    assert!(
        idx.refine(&r2, &parse_query("smlouva 1"), &opts, &t2)
            .is_none()
    );
}

#[test]
fn path_queries_under_drive_root_and_refine_consistency() {
    let mut idx = Index::new();
    let c = idx.add_root("C:\\");
    let proj = idx.insert(c, "Projekty", m_dir(100, false));
    let sub = idx.insert(proj, "App", m_dir(100, false));
    let top_file = idx.insert(c, "pagefile.sys", m_file(100, false));
    let nested_file = idx.insert(sub, "main.rs", m_file(100, false));

    let cancel = CancelToken::new();
    let opts = SearchOptions::default();

    let r_proj = idx
        .search(&parse_query("path:c:\\projekty"), &opts, &cancel)
        .unwrap();
    let expected_proj: HashSet<EntryId> = [proj, sub, nested_file].into_iter().collect();
    assert_eq!(
        r_proj.ids.iter().copied().collect::<HashSet<_>>(),
        expected_proj
    );

    let prev = idx
        .search(&parse_query("path:c:\\"), &opts, &cancel)
        .unwrap();
    let refined = idx
        .refine(&prev, &parse_query("path:c:\\projekty"), &opts, &cancel)
        .unwrap();
    assert_eq!(refined.ids, r_proj.ids);
    assert_eq!(refined.total_matches, r_proj.total_matches);

    let r_top = idx
        .search(&parse_query("path:c:\\pagefile"), &opts, &cancel)
        .unwrap();
    assert_eq!(r_top.ids, vec![top_file]);

    assert_eq!(
        idx.search(&parse_query("path:\\\\"), &opts, &cancel)
            .unwrap()
            .total_matches,
        0
    );
    assert_eq!(
        idx.search(&parse_query("path:c:\\\\"), &opts, &cancel)
            .unwrap()
            .total_matches,
        0
    );

    let r_children = idx
        .search(&parse_query("path:c:\\projekty\\"), &opts, &cancel)
        .unwrap();
    let expected_children: HashSet<EntryId> = [sub, nested_file].into_iter().collect();
    assert_eq!(
        r_children.ids.iter().copied().collect::<HashSet<_>>(),
        expected_children
    );
}

#[test]
fn move_to_colliding_with_ancestor_preserves_subtree() {
    let mut idx = Index::new();
    let c = idx.add_root("C:\\");
    let p = idx.insert(c, "P", m_dir(10, false));
    let a = idx.insert(p, "A", m_dir(10, false));
    let b = idx.insert(a, "B", m_dir(10, false));
    let foo = idx.insert(b, "foo", m_dir(10, false));
    let child = idx.insert(foo, "child.txt", m_file(10, false));

    idx.move_to(foo, p, "A");

    assert!(idx.is_alive(foo));
    assert!(idx.is_alive(child));
    assert!(!idx.is_alive(a));
    assert!(!idx.is_alive(b));
    assert_eq!(idx.len(), 4);
    assert_eq!(idx.path(foo), "C:\\P\\A");
    assert_eq!(idx.path(child), "C:\\P\\A\\child.txt");
    assert_eq!(idx.lookup("C:\\P\\A"), Some(foo));
    assert_eq!(idx.lookup("C:\\P\\A\\child.txt"), Some(child));
    assert_eq!(idx.lookup("C:\\P\\A\\B"), None);
}

#[test]
fn camelcase_boundary_after_combining_mark() {
    let mut idx = Index::new();
    let c = idx.add_root("C:\\");
    let nfd_camel = idx.insert(c, "e\u{0301}Plan.txt", m_file(100, false));
    let substr = idx.insert(c, "xeplan.txt", m_file(100, false));

    let cancel = CancelToken::new();
    let res = idx
        .search(&parse_query("plan"), &SearchOptions::default(), &cancel)
        .unwrap();
    assert_eq!(res.ids, vec![nfd_camel, substr]);
}

#[test]
fn lookup_non_drive_multi_segment_and_unc_roots() {
    let mut idx = Index::new();
    let proj_root = idx.add_root("C:\\Projects");
    let src = idx.insert(proj_root, "src", m_dir(10, false));
    let main_rs = idx.insert(src, "main.rs", m_file(10, false));

    let unc_root = idx.add_root("\\\\Server\\Share");
    let sub = idx.insert(unc_root, "Docs", m_dir(10, false));

    assert_eq!(idx.lookup("c:\\projects"), Some(proj_root));
    assert_eq!(idx.lookup("c:/projects/src/main.rs/"), Some(main_rs));
    assert_eq!(idx.lookup("C:\\ProjectsOther"), None);
    assert_eq!(idx.lookup("\\\\server\\share"), Some(unc_root));
    assert_eq!(idx.lookup("//server/share/docs/"), Some(sub));
}

#[test]
fn case_insensitive_not_diacritic_insensitive_siblings() {
    use std::io::Cursor;

    let mut idx = Index::new();
    let c = idx.add_root("C:\\");
    let dir = idx.insert(c, "Docs", m_dir(10, false));

    // Both 'kun.txt' and 'kůň.txt' coexist under one parent
    let id_ascii = idx.insert(dir, "kun.txt", m_file(100, false));
    let id_czech = idx.insert(dir, "kůň.txt", m_file(200, false));
    assert_ne!(id_ascii, id_czech);
    assert_eq!(idx.len(), 4); // C:\, Docs, kun.txt, kůň.txt
    assert_eq!(idx.lookup("C:\\Docs\\kun.txt"), Some(id_ascii));
    assert_eq!(idx.lookup("C:\\Docs\\KUN.TXT"), Some(id_ascii));
    assert_eq!(idx.lookup("C:\\Docs\\kůň.txt"), Some(id_czech));
    assert_eq!(idx.lookup("C:\\Docs\\KŮŇ.TXT"), Some(id_czech));

    // Search for 'kun' returns both
    let cancel = CancelToken::new();
    let opts = SearchOptions::default();
    let res = idx.search(&parse_query("kun"), &opts, &cancel).unwrap();
    assert_eq!(res.total_matches, 2);
    assert_eq!(
        res.ids.iter().copied().collect::<HashSet<_>>(),
        HashSet::from([id_ascii, id_czech])
    );

    // Upsert 'KŮŇ.TXT' updates 'kůň.txt' without creating a third child or touching 'kun.txt'
    let updated = idx.insert(dir, "KŮŇ.TXT", m_file(300, false));
    assert_eq!(updated, id_czech);
    assert_eq!(idx.len(), 4);
    assert_eq!(idx.name(id_czech), "KŮŇ.TXT");
    assert_eq!(idx.name(id_ascii), "kun.txt");
    assert_eq!(idx.meta(id_czech).mtime, 300);

    // Snapshot round-trip keeps both entries
    let mut snap = Vec::new();
    idx.save(&mut snap).expect("save with diacritic siblings");
    let loaded = Index::load(Cursor::new(&snap), None).expect("load with diacritic siblings");
    assert_eq!(loaded.len(), 4);
    assert_eq!(loaded.lookup("C:\\Docs\\kun.txt"), Some(id_ascii));
    assert_eq!(loaded.lookup("C:\\Docs\\kůň.txt"), Some(id_czech));
    assert_eq!(loaded.name(id_czech), "KŮŇ.TXT");

    // Compact keeps both entries
    let mut compacted = loaded.clone();
    compacted.compact();
    assert_eq!(compacted.len(), 4);
    assert!(compacted.lookup("C:\\Docs\\kun.txt").is_some());
    assert!(compacted.lookup("C:\\Docs\\kůň.txt").is_some());

    // Remove 'kůň.txt' leaves 'kun.txt' intact and lookup-able
    idx.remove(id_czech);
    assert_eq!(idx.len(), 3);
    assert_eq!(idx.lookup("C:\\Docs\\kůň.txt"), None);
    assert_eq!(idx.lookup("C:\\Docs\\kun.txt"), Some(id_ascii));
    assert_eq!(idx.name(id_ascii), "kun.txt");
}

#[test]
fn rename_and_move_collision_is_case_only() {
    let mut idx = Index::new();
    let c = idx.add_root("C:\\");
    let d1 = idx.insert(c, "D1", m_dir(10, false));
    let d2 = idx.insert(c, "D2", m_dir(10, false));

    let a = idx.insert(d1, "a.txt", m_file(10, false));
    let b = idx.insert(d1, "b.txt", m_file(10, false));

    // Renaming 'b.txt' to 'á.txt' next to 'a.txt' must NOT remove 'a.txt'
    idx.rename(b, "á.txt");
    assert!(idx.is_alive(a));
    assert!(idx.is_alive(b));
    assert_eq!(idx.lookup("C:\\D1\\a.txt"), Some(a));
    assert_eq!(idx.lookup("C:\\D1\\á.txt"), Some(b));

    // Renaming 'á.txt' onto 'A.TXT' collides with 'a.txt' and removes 'a.txt'
    idx.rename(b, "A.TXT");
    assert!(!idx.is_alive(a));
    assert!(idx.is_alive(b));
    assert_eq!(idx.name(b), "A.TXT");
    assert_eq!(idx.lookup("C:\\D1\\a.txt"), Some(b));
    assert_eq!(idx.lookup("C:\\D1\\á.txt"), None);

    // Move collision is also case-only: moving 'd2/x.txt' to 'd1/á.txt' keeps 'd1/A.TXT',
    // while moving 'd2/y.txt' to 'd1/a.txt' replaces 'd1/A.TXT'.
    let x = idx.insert(d2, "x.txt", m_file(10, false));
    let y = idx.insert(d2, "y.txt", m_file(10, false));
    idx.move_to(x, d1, "á.txt");
    assert!(idx.is_alive(b));
    assert!(idx.is_alive(x));
    assert_eq!(idx.lookup("C:\\D1\\A.TXT"), Some(b));
    assert_eq!(idx.lookup("C:\\D1\\á.txt"), Some(x));

    idx.move_to(y, d1, "a.txt");
    assert!(!idx.is_alive(b));
    assert!(idx.is_alive(x));
    assert!(idx.is_alive(y));
    assert_eq!(idx.lookup("C:\\D1\\a.txt"), Some(y));
    assert_eq!(idx.lookup("C:\\D1\\á.txt"), Some(x));
}

#[test]
fn cross_index_refine_and_root_edge_cases() {
    use std::io::Cursor;

    let mut idx1 = Index::new();
    let c1 = idx1.add_root("C:\\");
    idx1.insert(c1, "alpha.txt", m_file(10, false));
    idx1.insert(c1, "alpha_beta.txt", m_file(10, false));

    let mut idx2 = Index::new();
    let c2 = idx2.add_root("C:\\");
    idx2.insert(c2, "alpha_gamma.txt", m_file(10, false));

    let cancel = CancelToken::new();
    let opts = SearchOptions::default();
    let prev1 = idx1.search(&parse_query("alpha"), &opts, &cancel).unwrap();
    // Refining Results from idx1 against idx2 must fall back to full search on idx2
    let r2 = idx2
        .refine(&prev1, &parse_query("alpha_g"), &opts, &cancel)
        .unwrap();
    assert_eq!(r2.total_matches, 1);
    assert_eq!(idx2.name(r2.ids[0]), "alpha_gamma.txt");

    // Leading slash on non-UNC path must not falsely match C:\
    assert_eq!(idx1.lookup("\\C:\\alpha.txt"), None);
    assert_eq!(idx1.lookup("/C:/alpha.txt"), None);

    // Renaming one root onto another deduplicates roots cleanly for snapshot round-trip
    let d1 = idx1.add_root("D:\\");
    idx1.insert(d1, "dfile.txt", m_file(10, false));
    idx1.rename(d1, "C:\\");
    let mut snap = Vec::new();
    idx1.save(&mut snap).expect("save after root rename");
    let reloaded = Index::load(Cursor::new(&snap), None).expect("load after root rename");
    assert_eq!(reloaded.lookup("C:\\dfile.txt"), Some(d1 + 1));

    // Non-compact snapshot with in-place display update still compacts when compact() is called
    let mut dirty = Index::new();
    let dc = dirty.add_root("C:\\");
    let f = dirty.insert(dc, "short.txt", m_file(10, false));
    dirty.rename(f, "short_longer_name.txt");
    let mut dirty_snap = Vec::new();
    dirty.save(&mut dirty_snap).expect("save dirty");
    let mut loaded_dirty = Index::load(Cursor::new(&dirty_snap), None).expect("load dirty");
    let prev_d = loaded_dirty
        .search(&parse_query("short"), &opts, &cancel)
        .unwrap();
    loaded_dirty.compact();
    let after_d = loaded_dirty
        .refine(&prev_d, &parse_query("short_longer"), &opts, &cancel)
        .unwrap();
    assert_eq!(after_d.total_matches, 1);
}
