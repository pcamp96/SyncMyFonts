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
    let first_json = parse_json(&output);
    assert_eq!(first_json["dry_run"], false);
    assert_eq!(first_json["peers"][0]["name"], "Peer A");
    assert_eq!(first_json["peers"][0]["url"], peer_url);
    assert_eq!(first_json["peers"][0]["ok"], true);
    assert_eq!(
        first_json["peers"][0]["installed"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        first_json["peers"][0]["skipped"].as_array().unwrap().len(),
        0
    );

    let expected_bytes = fs::read(source_font).unwrap();
    let install_dirs = [peer_b_fonts.join("SyncMyFonts"), peer_b_fonts.clone()];
    let matching_install = install_dirs.iter().any(|install_dir| {
        fs::read_dir(install_dir)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .any(|entry| fs::read(entry.path()).is_ok_and(|bytes| bytes == expected_bytes))
    });
    assert!(
        matching_install,
        "no installed font under {} matched source bytes",
        peer_b_fonts.display()
    );
    let installed_after_first = installed_font_paths(&peer_b_fonts);
    assert_eq!(installed_after_first.len(), 1);

    let mut second_sync = Command::new(&bin);
    second_sync.arg("lan-sync-all");
    apply_isolated_env(&mut second_sync, &peer_b_fonts, &peer_b_config);
    let second_output = second_sync.output().unwrap();
    assert!(
        second_output.status.success(),
        "second lan-sync-all failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&second_output.stdout),
        String::from_utf8_lossy(&second_output.stderr)
    );
    let second_json = parse_json(&second_output);
    assert_eq!(second_json["dry_run"], false);
    assert_eq!(second_json["peers"][0]["name"], "Peer A");
    assert_eq!(second_json["peers"][0]["url"], peer_url);
    assert_eq!(second_json["peers"][0]["ok"], true);
    assert_eq!(
        second_json["peers"][0]["installed"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    assert!(
        second_json["peers"][0]["skipped"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value
                .as_str()
                .is_some_and(|line| line.contains("already present")))
    );
    assert_eq!(installed_font_paths(&peer_b_fonts), installed_after_first);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn lan_sync_dry_run_reports_missing_without_installing_fonts() {
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_syncmyfonts-agent"));
    let root = unique_temp_dir("syncmyfonts-lan-dry-run");
    let peer_a_fonts = root.join("peer-a-fonts");
    let peer_a_config = root.join("peer-a-config");
    let peer_b_fonts = root.join("peer-b-fonts");
    let peer_b_config = root.join("peer-b-config");
    fs::create_dir_all(&peer_a_fonts).unwrap();
    fs::create_dir_all(&peer_a_config).unwrap();
    fs::create_dir_all(&peer_b_fonts).unwrap();
    fs::create_dir_all(&peer_b_config).unwrap();
    fs::write(peer_a_fonts.join("Preview Only.ttf"), b"preview font\n").unwrap();

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

    let mut dry_run = Command::new(&bin);
    dry_run
        .arg("lan-sync")
        .arg("--peer")
        .arg(format!("http://{listen}"))
        .arg("--lan-key")
        .arg("test-key")
        .arg("--dry-run");
    apply_isolated_env(&mut dry_run, &peer_b_fonts, &peer_b_config);
    let output = dry_run.output().unwrap();
    assert!(
        output.status.success(),
        "dry-run lan-sync failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let json = parse_json(&output);
    assert_eq!(json["dry_run"], true);
    assert_eq!(json["peer_fonts"], 1);
    assert_eq!(json["installed"].as_array().unwrap().len(), 0);
    assert!(json["skipped"].as_array().unwrap().iter().any(|value| {
        value
            .as_str()
            .is_some_and(|line| line.contains("would install"))
    }));
    assert_eq!(installed_font_count(&peer_b_fonts), 0);
    assert_no_managed_manifest(&peer_b_config);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn lan_sync_with_wrong_key_fails_without_installing_fonts() {
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_syncmyfonts-agent"));
    let root = unique_temp_dir("syncmyfonts-lan-wrong-key");
    let peer_a_fonts = root.join("peer-a-fonts");
    let peer_a_config = root.join("peer-a-config");
    let peer_b_fonts = root.join("peer-b-fonts");
    let peer_b_config = root.join("peer-b-config");
    fs::create_dir_all(&peer_a_fonts).unwrap();
    fs::create_dir_all(&peer_a_config).unwrap();
    fs::create_dir_all(&peer_b_fonts).unwrap();
    fs::create_dir_all(&peer_b_config).unwrap();
    fs::write(
        peer_a_fonts.join("Protected Workshop.ttf"),
        b"private font\n",
    )
    .unwrap();

    let listen = free_local_addr();
    let mut child = Command::new(&bin);
    child
        .arg("lan-serve")
        .arg("--listen")
        .arg(listen.to_string())
        .arg("--lan-key")
        .arg("correct-key");
    apply_isolated_env(&mut child, &peer_a_fonts, &peer_a_config);
    let server = child.spawn().unwrap();
    let _server = ChildGuard(server);
    wait_for_tcp(listen);

    let mut sync = Command::new(&bin);
    sync.arg("lan-sync")
        .arg("--peer")
        .arg(format!("http://{listen}"))
        .arg("--lan-key")
        .arg("wrong-key");
    apply_isolated_env(&mut sync, &peer_b_fonts, &peer_b_config);
    let output = sync.output().unwrap();

    assert!(
        !output.status.success(),
        "lan-sync unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !font_dir_has_entries(&peer_b_fonts),
        "wrong-key sync wrote files under {}",
        peer_b_fonts.display()
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn saved_peer_sync_all_reports_offline_peer_without_installing_fonts() {
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_syncmyfonts-agent"));
    let root = unique_temp_dir("syncmyfonts-lan-offline");
    let peer_b_fonts = root.join("peer-b-fonts");
    let peer_b_config = root.join("peer-b-config");
    fs::create_dir_all(&peer_b_fonts).unwrap();
    fs::create_dir_all(&peer_b_config).unwrap();

    let peer_url = format!("http://{}", free_local_addr());
    let mut add_peer = Command::new(&bin);
    add_peer
        .arg("lan-add-peer")
        .arg("--name")
        .arg("Offline Peer")
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
        "lan-sync-all should report peer error without failing whole command\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let json = parse_json(&output);
    assert_eq!(json["dry_run"], false);
    assert_eq!(json["peers"][0]["ok"], false);
    assert_eq!(json["peers"][0]["installed"].as_array().unwrap().len(), 0);
    assert_eq!(json["peers"][0]["skipped"].as_array().unwrap().len(), 0);
    assert!(json["peers"][0]["error"].as_str().is_some());
    assert_eq!(installed_font_count(&peer_b_fonts), 0);
    assert_no_managed_manifest(&peer_b_config);
    let config_json: serde_json::Value =
        serde_json::from_slice(&fs::read(peer_b_config.join("config.json")).unwrap()).unwrap();
    assert_eq!(config_json["peers"].as_array().unwrap().len(), 1);

    let _ = fs::remove_dir_all(root);
}

fn apply_isolated_env(command: &mut Command, font_dir: &Path, config_dir: &Path) {
    command
        .env("SYNCMYFONTS_USER_FONT_DIR", font_dir)
        .env("SYNCMYFONTS_CONFIG_DIR", config_dir)
        .env("SYNCMYFONTS_SKIP_PLATFORM_FONT_REGISTRATION", "1");
}

fn font_dir_has_entries(path: &Path) -> bool {
    fs::read_dir(path)
        .ok()
        .into_iter()
        .flatten()
        .any(|entry| entry.is_ok())
}

fn parse_json(output: &std::process::Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).unwrap()
}

fn installed_font_count(root: &Path) -> usize {
    installed_font_paths(root).len()
}

fn installed_font_paths(root: &Path) -> Vec<PathBuf> {
    let mut paths = [root.join("SyncMyFonts"), root.to_path_buf()]
        .iter()
        .filter_map(|dir| fs::read_dir(dir).ok())
        .flat_map(|entries| entries.filter_map(Result::ok))
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

fn assert_no_managed_manifest(config_dir: &Path) {
    let path = config_dir.join("managed-fonts.json");
    assert!(
        !path.exists(),
        "unexpected managed manifest at {}",
        path.display()
    );
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
