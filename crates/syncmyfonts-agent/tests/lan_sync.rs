use std::{
    fs,
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn saved_peer_sync_all_installs_matching_font_bytes() {
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_syncmyfonts-agent"));
    let root = unique_temp_dir("syncmyfonts-lan-sync");
    let peer_a_fonts = root.join("peer-a-fonts");
    let peer_a_config = root.join("peer-a-config");
    let peer_b_fonts = root.join("peer-b-fonts");
    let peer_b_config = root.join("peer-b-config");
    fs::create_dir_all(&peer_a_fonts).unwrap();
    fs::create_dir_all(&peer_a_config).unwrap();
    fs::create_dir_all(&peer_b_fonts).unwrap();
    fs::create_dir_all(&peer_b_config).unwrap();

    let source_font = peer_a_fonts.join("Workshop Test.ttf");
    fs::write(&source_font, b"SyncMyFonts integration test font\n").unwrap();

    let listen = free_local_addr();
    let mut child = Command::new(&bin);
    child
        .arg("lan-serve")
        .arg("--listen")
        .arg(listen.to_string())
        .arg("--lan-key")
        .arg("test-key");
    apply_isolated_env(&mut child, &peer_a_fonts, &peer_a_config);
    let server = child.spawn().unwrap();
    let _server = ChildGuard(server);
    wait_for_tcp(listen);

    let peer_url = format!("http://{listen}");
    let mut add_peer = Command::new(&bin);
    add_peer
        .arg("lan-add-peer")
        .arg("--name")
        .arg("Peer A")
        .arg("--url")
        .arg(&peer_url)
        .arg("--lan-key")
        .arg("test-key");
    apply_isolated_env(&mut add_peer, &peer_b_fonts, &peer_b_config);
    assert!(add_peer.status().unwrap().success());

    let mut sync_all = Command::new(&bin);
    sync_all.arg("lan-sync-all");
    apply_isolated_env(&mut sync_all, &peer_b_fonts, &peer_b_config);
    let output = sync_all.output().unwrap();
    assert!(
        output.status.success(),
        "lan-sync-all failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let installed = peer_b_fonts.join("SyncMyFonts").join("Workshop-Test.ttf");
    assert_eq!(fs::read(source_font).unwrap(), fs::read(installed).unwrap());

    let _ = fs::remove_dir_all(root);
}

fn apply_isolated_env(command: &mut Command, font_dir: &Path, config_dir: &Path) {
    command
        .env("SYNCMYFONTS_USER_FONT_DIR", font_dir)
        .env("SYNCMYFONTS_CONFIG_DIR", config_dir)
        .env("SYNCMYFONTS_SKIP_PLATFORM_FONT_REGISTRATION", "1");
}

fn unique_temp_dir(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "{}-{}-{}",
        prefix,
        std::process::id(),
        monotonic_nanos()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn monotonic_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

fn free_local_addr() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap()
}

fn wait_for_tcp(addr: SocketAddr) {
    let started = Instant::now();
    loop {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(100)).is_ok() {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "timed out waiting for {addr}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}
