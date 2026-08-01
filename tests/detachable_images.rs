#![cfg(unix)]

use base64::Engine as _;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const PNG_B64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAACklEQVR4nGMAAQAABQABDQottAAAAABJRU5ErkJggg==";

struct Process(Child);

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn http_get(addr: SocketAddr, path: &str) -> Option<(u16, Vec<u8>)> {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(200)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .ok()?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).ok()?;
    let split = response.windows(4).position(|w| w == b"\r\n\r\n")?;
    let headers = std::str::from_utf8(&response[..split]).ok()?;
    let status = headers.split_whitespace().nth(1)?.parse().ok()?;
    Some((status, response[split + 4..].to_vec()))
}

fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        thread::sleep(Duration::from_millis(25));
    }
    false
}

/// Full empirical path: real hub + detachable push + attach running in a PTY.
/// The command emits an image before attach (the hub must not get it), then asks
/// kitty for support after detach and emits the image only after receiving the
/// cached reply. Success is the exact PNG arriving at the hub's image endpoint.
#[test]
fn hub_receives_image_emitted_after_terminal_detaches() {
    let binary = env!("CARGO_BIN_EXE_shellglass");
    let key = "detachable-image-e2e-key";
    let id = shellglass::proto::session_id(key);
    let reservation = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = reservation.local_addr().unwrap();
    drop(reservation);

    let hub = Command::new(binary)
        .args([
            "hub",
            "--bind",
            &addr.to_string(),
            "--allow",
            &format!("{id}:image-e2e"),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut hub = Process(hub);
    assert!(wait_until(Duration::from_secs(5), || {
        TcpStream::connect_timeout(&addr, Duration::from_millis(100)).is_ok()
    }));

    let socket = std::env::temp_dir().join(format!(
        "shellglass-full-image-e2e-{}-{:?}.sock",
        std::process::id(),
        thread::current().id()
    ));
    let image = format!("\\033_Ga=T,f=100,t=d,c=1,r=1;{PNG_B64}\\033\\\\");
    let script = format!(
        "printf '{image}'; printf 'READY\\r\\n'; IFS= read -r line; \
         sleep 0.1; stty raw -echo; \
         printf '\\033_Gi=9,a=q,t=d,s=1,v=1;AAAA\\033\\\\'; \
         reply=$(dd bs=1 count=11 2>/dev/null); \
         case \"$reply\" in *OK*) printf '{image}';; esac; sleep 0.2"
    );
    let push = Command::new(binary)
        .args([
            "push",
            &format!("http://{addr}"),
            "--key",
            key,
            "--no-record",
            "--detachable",
            "--socket",
            socket.to_str().unwrap(),
            "--",
            "/bin/sh",
            "-c",
            &script,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut push = Process(push);

    let png = base64::engine::general_purpose::STANDARD
        .decode(PNG_B64)
        .unwrap();
    let hash = shellglass::proto::content_key("image/png", &png);
    let image_path = format!("/s/image-e2e/images/{hash}");
    assert!(wait_until(Duration::from_secs(8), || {
        socket.exists() && matches!(http_get(addr, "/s/image-e2e/"), Some((200, _)))
    }));
    assert_eq!(
        http_get(addr, &image_path).map(|r| r.0),
        Some(404),
        "image interception must be disabled before the first profile"
    );

    // Run the actual attach client inside a synthetic terminal. The emulator
    // answers its capability probe as a kitty-capable terminal, waits for the
    // READY repaint, then sends one input line followed by Ctrl-\\ detach.
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 640,
            pixel_height: 384,
        })
        .unwrap();
    let mut attach = CommandBuilder::new(binary);
    attach.args(["attach", socket.to_str().unwrap()]);
    let child = pair.slave.spawn_command(attach).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let mut writer = pair.master.take_writer().unwrap();
    let (output_tx, output_rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 || output_tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    let (exit_tx, exit_rx) = mpsc::channel();
    thread::spawn(move || {
        let mut child = child;
        let _ = child.wait();
        let _ = exit_tx.send(());
    });

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut output = Vec::new();
    let mut answered = false;
    let mut detached = false;
    while Instant::now() < deadline && !detached {
        if let Ok(chunk) = output_rx.recv_timeout(Duration::from_millis(200)) {
            output.extend_from_slice(&chunk);
        }
        if !answered && output.windows(3).any(|w| w == b"\x1b[c") {
            writer
                .write_all(
                    b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\\
                      \x1b]11;rgb:0000/0000/0000\x1b\\\
                      \x1b_Gi=1;OK\x1b\\\
                      \x1b[?1;2c",
                )
                .unwrap();
            writer.flush().unwrap();
            answered = true;
        }
        if answered && output.windows(5).any(|w| w == b"READY") {
            writer.write_all(b"go\n\x1c").unwrap();
            writer.flush().unwrap();
            detached = true;
        }
    }
    assert!(
        answered,
        "attach did not issue its terminal capability probe"
    );
    assert!(detached, "attach did not receive the owner's repaint");
    assert!(exit_rx.recv_timeout(Duration::from_secs(3)).is_ok());

    assert!(wait_until(Duration::from_secs(5), || {
        matches!(http_get(addr, &image_path), Some((200, ref body)) if body == &png)
    }));

    // Explicit early cleanup keeps the socket/process lifetime inside the test.
    let _ = push.0.kill();
    let _ = hub.0.kill();
    let _ = std::fs::remove_file(Path::new(&socket));
}
