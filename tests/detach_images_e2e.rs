//! End-to-end: a detachable session mirrors inline images.
//!
//! The unit tests in `session.rs` drive the core loop directly; this one runs a
//! real command in a real terminal-less PTY and asserts the sixel it prints
//! reaches the published frames as a placement with a decoded payload — the whole
//! path (PTY read → `ImagePipe` → deferred decode worker → `Core::ImageReady` →
//! frame) that used to be absent from the detached owner entirely.
#![cfg(all(feature = "push", unix))]

use shellglass::model::Frame;
use std::time::{Duration, Instant};

#[test]
fn detached_pty_mirrors_a_real_sixel() {
    let sock = std::env::temp_dir().join(format!("sg-detach-img-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock);

    // A 4x2 sixel, then a brief hold so the session outlives the assertion.
    let cmd: Vec<String> = vec![
        "sh".into(),
        "-c".into(),
        r#"printf '\033Pq"1;1;4;2#0;2;100;0;0#0~~~~$-\033\\'; sleep 2"#.into(),
    ];

    let session = shellglass::session::start_detached(&cmd, &sock, (80, 24), false)
        .expect("starting the detached session");
    let rx = session.frames.clone();

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut found = None;
    while Instant::now() < deadline {
        {
            let Frame::Screen(grid) = &**rx.borrow();
            // A placement is only published once its payload has landed, so
            // seeing one proves the decode round trip completed.
            if let Some(p) = grid.images.first() {
                found = Some((p.clone(), grid.image_data.len()));
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let _ = std::fs::remove_file(&sock);

    let (placement, payloads) = found.expect("no image placement appeared in the detached mirror");
    assert!(
        !placement.hash.is_empty(),
        "placement must address a payload"
    );
    assert!(payloads > 0, "frame must carry the decoded payload");
}
