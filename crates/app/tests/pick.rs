//! A real `fm --pick` window, started and answered through fm_dialog_shim's own transport (the
//! code that runs inside the host app): the marker check, the launch, the client check, the
//! request on the pipe, the filtered folder, Ready and the answer. `--dev-keys` feeds keys to fm's
//! own key handler (no input is synthesized). Shows a window on the desktop for a moment, hence
//! ignored: `cargo test -p fm-app --test pick -- --ignored`.
#![cfg(windows)]
use fm_dialog_shim::{
    pick::{Hosts, run},
    proto::{Msg, Request},
};
use std::{path::PathBuf, time::Duration};
use windows::Win32::Foundation::HWND;

fn hosts(keys: &str) -> Hosts {
    Hosts {
        // No door: always the started fm, never the user's resident one.
        door: format!("FmDoorNone-{}", std::process::id()),
        exe: Some(PathBuf::from(env!("CARGO_BIN_EXE_fm"))),
        installed: false,
        args: format!("--dev-keys {keys}"),
        connect: Duration::from_secs(30),
    }
}

#[test]
#[ignore]
fn started_picker_answers_on_the_pipe() {
    // After Ready the shim waits as long as the user does: a stuck picker must fail the test.
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(120));
        eprintln!("watchdog: no answer from the picker");
        std::process::exit(101);
    });
    let dir = std::env::temp_dir().join(format!("fm-pick-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for f in ["a.png", "b.txt", "c.jpg"] {
        std::fs::write(dir.join(f), "x").unwrap();
    }
    let path = |f: &str| dir.join(f).to_string_lossy().into_owned();
    let req = |multi: bool| Request {
        folder: dir.to_string_lossy().into_owned(),
        filters: vec![("Obrázky".into(), "*.png;*.jpg".into()), ("Vše".into(), "*.*".into())],
        index: 1,
        multi,
        ..Default::default()
    };
    // The file type hides b.txt: the list is a.png, c.jpg.
    let (via, m) = run(&mut req(false), HWND::default(), &hosts("sel:1,enter")).unwrap();
    assert_eq!((via, m), ("child", Msg::Picked(vec![path("c.jpg")])));
    let (_, m) = run(&mut req(true), HWND::default(), &hosts("ctrl+a,enter")).unwrap();
    assert_eq!(m, Msg::Picked(vec![path("a.png"), path("c.jpg")]));
    // Single select: Ctrl+A keeps one item.
    let (_, m) = run(&mut req(false), HWND::default(), &hosts("sel:0,ctrl+a,enter")).unwrap();
    assert_eq!(m, Msg::Picked(vec![path("a.png")]));
    let (_, m) = run(&mut req(false), HWND::default(), &hosts("esc")).unwrap();
    assert_eq!(m, Msg::Cancelled);
    let _ = std::fs::remove_dir_all(&dir);
}
