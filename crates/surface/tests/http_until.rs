//! The serve loop's pulse (ADR-0013): `tick` runs between requests, and
//! `Ok(false)` stops the server cleanly — the seam the hosted face
//! replicates and shuts down through.

use std::fs;
use std::path::Path;

use atelier_sdk::Workspace;
use atelier_surface::serve_http_until;

#[expect(unsafe_code, reason = "set_var wires the workspace to the test config")]
fn set_actor(config_home: &Path) {
    fs::create_dir_all(config_home).expect("create config home");
    fs::write(
        config_home.join("config.toml"),
        "[actor]\nname = \"test-actor\"\nkind = \"human\"\n",
    )
    .expect("write actor config");
    // SAFETY: this integration test binary runs the one test below; no
    // other thread reads or writes the environment.
    unsafe {
        std::env::set_var("ATELIER_CONFIG_HOME", config_home);
    }
}

#[test]
fn a_false_tick_stops_the_server_cleanly() {
    let config = tempfile::tempdir().expect("create config tempdir");
    set_actor(config.path());
    let root = tempfile::tempdir().expect("create workspace tempdir");
    Workspace::init(root.path()).expect("init the workspace");

    let mut ticks = 0;
    serve_http_until(root.path(), "127.0.0.1:0", false, None, || {
        ticks += 1;
        Ok(ticks < 3)
    })
    .expect("the loop returns cleanly");
    assert_eq!(ticks, 3, "the third tick stopped the server");
}
