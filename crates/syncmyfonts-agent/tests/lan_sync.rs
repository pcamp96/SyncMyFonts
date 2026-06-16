use std::{
    fs,
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command},
    sync::{Mutex, MutexGuard, OnceLock},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use sha2::{Digest, Sha256};
use syncmyfonts_core::DEFAULT_API_KEY_HEADER;

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

static LAN_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn lan_test_guard() -> MutexGuard<'static, ()> {
    LAN_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("LAN test lock should not be poisoned")
}

#[test]
fn saved_peer_sync_all_installs_matching_font_bytes() {
    let _guard = lan_test_guard();
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
    let add_peer_output = add_peer.output().unwrap();
    assert!(
        add_peer_output.status.success(),
        "lan-add-peer failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&add_peer_output.stdout),
        String::from_utf8_lossy(&add_peer_output.stderr)
    );
    let add_peer_json = parse_json(&add_peer_output);
    assert_eq!(add_peer_json["has_lan_key"], true);
    assert!(
        !String::from_utf8_lossy(&add_peer_output.stdout).contains("test-key"),
        "lan-add-peer output leaked the LAN key"
    );

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
fn bidirectional_saved_peer_sync_converges_without_duplicates() {
    let _guard = lan_test_guard();
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_syncmyfonts-agent"));
    let root = unique_temp_dir("syncmyfonts-lan-bidirectional");
    let peer_a_fonts = root.join("peer-a-fonts");
    let peer_a_config = root.join("peer-a-config");
    let peer_b_fonts = root.join("peer-b-fonts");
    let peer_b_config = root.join("peer-b-config");
    fs::create_dir_all(&peer_a_fonts).unwrap();
    fs::create_dir_all(&peer_a_config).unwrap();
    fs::create_dir_all(&peer_b_fonts).unwrap();
    fs::create_dir_all(&peer_b_config).unwrap();

    let peer_a_source = peer_a_fonts.join("MacBook Design Font.ttf");
    let peer_b_source = peer_b_fonts.join("Workshop Laser Font.ttf");
    fs::write(&peer_a_source, b"font installed first on the macbook\n").unwrap();
    fs::write(&peer_b_source, b"font installed first on the shop pc\n").unwrap();

    let peer_a_listen = free_local_addr();
    let peer_b_listen = free_local_addr();
    let mut peer_a_server = Command::new(&bin);
    peer_a_server
        .arg("lan-serve")
        .arg("--listen")
        .arg(peer_a_listen.to_string())
        .arg("--lan-key")
        .arg("shared-test-key");
    apply_isolated_env(&mut peer_a_server, &peer_a_fonts, &peer_a_config);
    let peer_a_server = peer_a_server.spawn().unwrap();
    let _peer_a_server = ChildGuard(peer_a_server);

    let mut peer_b_server = Command::new(&bin);
    peer_b_server
        .arg("lan-serve")
        .arg("--listen")
        .arg(peer_b_listen.to_string())
        .arg("--lan-key")
        .arg("shared-test-key");
    apply_isolated_env(&mut peer_b_server, &peer_b_fonts, &peer_b_config);
    let peer_b_server = peer_b_server.spawn().unwrap();
    let _peer_b_server = ChildGuard(peer_b_server);

    wait_for_tcp(peer_a_listen);
    wait_for_tcp(peer_b_listen);

    let peer_a_url = format!("http://{peer_a_listen}");
    let peer_b_url = format!("http://{peer_b_listen}");
    add_saved_peer(&bin, &peer_a_fonts, &peer_a_config, "Shop PC", &peer_b_url);
    add_saved_peer(&bin, &peer_b_fonts, &peer_b_config, "MacBook", &peer_a_url);

    let peer_a_first = sync_saved_peers(&bin, &peer_a_fonts, &peer_a_config);
    let peer_b_first = sync_saved_peers(&bin, &peer_b_fonts, &peer_b_config);
    assert_eq!(peer_a_first["dry_run"], false);
    assert_eq!(peer_b_first["dry_run"], false);
    assert_eq!(peer_a_first["peers"][0]["name"], "Shop PC");
    assert_eq!(peer_b_first["peers"][0]["name"], "MacBook");
    assert_eq!(peer_a_first["peers"][0]["ok"], true);
    assert_eq!(peer_b_first["peers"][0]["ok"], true);
    assert_eq!(
        peer_a_first["peers"][0]["installed"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        peer_b_first["peers"][0]["installed"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_font_bytes_present(&peer_a_fonts, fs::read(&peer_b_source).unwrap());
    assert_font_bytes_present(&peer_b_fonts, fs::read(&peer_a_source).unwrap());
    let peer_a_installed_after_first = installed_font_paths(&peer_a_fonts);
    let peer_b_installed_after_first = installed_font_paths(&peer_b_fonts);
    assert_eq!(peer_a_installed_after_first.len(), 2);
    assert_eq!(peer_b_installed_after_first.len(), 2);

    let peer_a_second = sync_saved_peers(&bin, &peer_a_fonts, &peer_a_config);
    let peer_b_second = sync_saved_peers(&bin, &peer_b_fonts, &peer_b_config);
    assert_eq!(
        peer_a_second["peers"][0]["installed"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        peer_b_second["peers"][0]["installed"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    assert_already_present_skip(&peer_a_second);
    assert_already_present_skip(&peer_b_second);
    assert_eq!(
        installed_font_paths(&peer_a_fonts),
        peer_a_installed_after_first
    );
    assert_eq!(
        installed_font_paths(&peer_b_fonts),
        peer_b_installed_after_first
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn lan_sync_dry_run_reports_missing_without_installing_fonts() {
    let _guard = lan_test_guard();
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
fn lan_pair_saves_key_then_sync_all_installs_font() {
    let _guard = lan_test_guard();
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_syncmyfonts-agent"));
    let root = unique_temp_dir("syncmyfonts-lan-pair");
    let peer_a_fonts = root.join("peer-a-fonts");
    let peer_a_config = root.join("peer-a-config");
    let peer_b_fonts = root.join("peer-b-fonts");
    let peer_b_config = root.join("peer-b-config");
    fs::create_dir_all(&peer_a_fonts).unwrap();
    fs::create_dir_all(&peer_a_config).unwrap();
    fs::create_dir_all(&peer_b_fonts).unwrap();
    fs::create_dir_all(&peer_b_config).unwrap();
    fs::write(peer_a_fonts.join("Pairing Path.ttf"), b"paired font\n").unwrap();

    let listen = free_local_addr();
    let mut child = Command::new(&bin);
    child
        .arg("lan-serve")
        .arg("--listen")
        .arg(listen.to_string())
        .arg("--pairing-code")
        .arg("1234-5678");
    apply_isolated_env(&mut child, &peer_a_fonts, &peer_a_config);
    let server = child.spawn().unwrap();
    let _server = ChildGuard(server);
    wait_for_tcp(listen);

    let mut pair = Command::new(&bin);
    pair.arg("lan-pair")
        .arg("--name")
        .arg("Paired Peer")
        .arg("--url")
        .arg(format!("http://{listen}"))
        .arg("--pairing-code")
        .arg("12345678");
    apply_isolated_env(&mut pair, &peer_b_fonts, &peer_b_config);
    let pair_output = pair.output().unwrap();
    assert!(
        pair_output.status.success(),
        "lan-pair failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&pair_output.stdout),
        String::from_utf8_lossy(&pair_output.stderr)
    );
    let pair_json = parse_json(&pair_output);
    assert_eq!(pair_json["name"], "Paired Peer");
    assert_eq!(pair_json["has_lan_key"], true);
    assert!(
        !String::from_utf8_lossy(&pair_output.stdout).contains("smf-"),
        "pair output leaked generated LAN key"
    );

    let mut sync_all = Command::new(&bin);
    sync_all.arg("lan-sync-all");
    apply_isolated_env(&mut sync_all, &peer_b_fonts, &peer_b_config);
    let output = sync_all.output().unwrap();
    assert!(
        output.status.success(),
        "paired lan-sync-all failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let json = parse_json(&output);
    assert_eq!(json["peers"][0]["ok"], true);
    assert_eq!(json["peers"][0]["installed"].as_array().unwrap().len(), 1);
    assert_eq!(installed_font_count(&peer_b_fonts), 1);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn lan_sync_with_wrong_key_fails_without_installing_fonts() {
    let _guard = lan_test_guard();
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
fn lan_sync_skips_system_font_conflict_without_installing_fonts() {
    let _guard = lan_test_guard();
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_syncmyfonts-agent"));
    let root = unique_temp_dir("syncmyfonts-lan-system-conflict");
    let peer_a_fonts = root.join("peer-a-fonts");
    let peer_a_config = root.join("peer-a-config");
    let peer_b_fonts = root.join("peer-b-fonts");
    let peer_b_config = root.join("peer-b-config");
    let peer_b_system_fonts = root.join("peer-b-system-fonts");
    fs::create_dir_all(&peer_a_fonts).unwrap();
    fs::create_dir_all(&peer_a_config).unwrap();
    fs::create_dir_all(&peer_b_fonts).unwrap();
    fs::create_dir_all(&peer_b_config).unwrap();
    fs::create_dir_all(&peer_b_system_fonts).unwrap();
    fs::write(peer_a_fonts.join("Existing.ttf"), b"peer font bytes\n").unwrap();
    fs::write(
        peer_b_system_fonts.join("existing.ttf"),
        b"system font bytes\n",
    )
    .unwrap();

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

    let mut sync = Command::new(&bin);
    sync.arg("lan-sync")
        .arg("--peer")
        .arg(format!("http://{listen}"))
        .arg("--lan-key")
        .arg("test-key");
    apply_isolated_env(&mut sync, &peer_b_fonts, &peer_b_config);
    sync.env("SYNCMYFONTS_SYSTEM_FONT_DIRS", &peer_b_system_fonts);
    let output = sync.output().unwrap();
    assert!(
        output.status.success(),
        "system-conflict lan-sync failed instead of reporting a skip\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let json = parse_json(&output);
    assert_eq!(json["dry_run"], false);
    assert_eq!(json["peer_fonts"], 1);
    assert_eq!(json["installed"].as_array().unwrap().len(), 0);
    assert!(json["skipped"].as_array().unwrap().iter().any(|value| {
        value
            .as_str()
            .is_some_and(|line| line.contains("system-font-conflict"))
    }));
    assert_eq!(installed_font_count(&peer_b_fonts), 0);
    assert_no_managed_manifest(&peer_b_config);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn lan_manifest_and_blobs_do_not_expose_system_fonts() {
    let _guard = lan_test_guard();
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_syncmyfonts-agent"));
    let root = unique_temp_dir("syncmyfonts-lan-system-exposure");
    let peer_fonts = root.join("peer-fonts");
    let peer_config = root.join("peer-config");
    let peer_system_fonts = root.join("peer-system-fonts");
    fs::create_dir_all(&peer_fonts).unwrap();
    fs::create_dir_all(&peer_config).unwrap();
    fs::create_dir_all(&peer_system_fonts).unwrap();

    let user_font_bytes = b"user font is shareable\n";
    let system_font_bytes = b"system font must never be served\n";
    let user_hash = sha256_hex(user_font_bytes);
    let system_hash = sha256_hex(system_font_bytes);
    fs::write(peer_fonts.join("Shareable User Font.ttf"), user_font_bytes).unwrap();
    fs::write(
        peer_system_fonts.join("Protected System Font.ttf"),
        system_font_bytes,
    )
    .unwrap();

    let listen = free_local_addr();
    let mut child = Command::new(&bin);
    child
        .arg("lan-serve")
        .arg("--listen")
        .arg(listen.to_string())
        .arg("--lan-key")
        .arg("test-key");
    apply_isolated_env(&mut child, &peer_fonts, &peer_config);
    child.env("SYNCMYFONTS_SYSTEM_FONT_DIRS", &peer_system_fonts);
    let server = child.spawn().unwrap();
    let _server = ChildGuard(server);
    wait_for_tcp(listen);

    let client = reqwest::blocking::Client::new();
    let manifest: serde_json::Value = client
        .get(format!("http://{listen}/api/lan/v1/manifest"))
        .header(DEFAULT_API_KEY_HEADER, "test-key")
        .send()
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .unwrap();
    let hashes = manifest["fonts"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|font| font["sha256"].as_str())
        .collect::<Vec<_>>();
    assert!(hashes.contains(&user_hash.as_str()));
    assert!(!hashes.contains(&system_hash.as_str()));

    let user_blob = client
        .get(format!("http://{listen}/api/lan/v1/blobs/{user_hash}"))
        .header(DEFAULT_API_KEY_HEADER, "test-key")
        .send()
        .unwrap()
        .error_for_status()
        .unwrap()
        .bytes()
        .unwrap();
    assert_eq!(&user_blob[..], user_font_bytes);

    let system_blob = client
        .get(format!("http://{listen}/api/lan/v1/blobs/{system_hash}"))
        .header(DEFAULT_API_KEY_HEADER, "test-key")
        .send()
        .unwrap();
    assert_eq!(system_blob.status(), reqwest::StatusCode::NOT_FOUND);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn saved_peer_sync_all_reports_offline_peer_without_installing_fonts() {
    let _guard = lan_test_guard();
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
        .env("SYNCMYFONTS_SKIP_PLATFORM_FONT_REGISTRATION", "1")
        .env("SYNCMYFONTS_DISABLE_SECRET_STORE", "1");
}

fn add_saved_peer(bin: &Path, font_dir: &Path, config_dir: &Path, name: &str, url: &str) {
    let mut add_peer = Command::new(bin);
    add_peer
        .arg("lan-add-peer")
        .arg("--name")
        .arg(name)
        .arg("--url")
        .arg(url)
        .arg("--lan-key")
        .arg("shared-test-key");
    apply_isolated_env(&mut add_peer, font_dir, config_dir);
    let output = add_peer.output().unwrap();
    assert!(
        output.status.success(),
        "lan-add-peer failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let json = parse_json(&output);
    assert_eq!(json["has_lan_key"], true);
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("shared-test-key"),
        "lan-add-peer output leaked the LAN key"
    );
}

fn sync_saved_peers(bin: &Path, font_dir: &Path, config_dir: &Path) -> serde_json::Value {
    let mut sync_all = Command::new(bin);
    sync_all.arg("lan-sync-all");
    apply_isolated_env(&mut sync_all, font_dir, config_dir);
    let output = sync_all.output().unwrap();
    assert!(
        output.status.success(),
        "lan-sync-all failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    parse_json(&output)
}

fn assert_font_bytes_present(root: &Path, expected_bytes: Vec<u8>) {
    assert!(
        installed_font_paths(root)
            .iter()
            .any(|path| fs::read(path).is_ok_and(|bytes| bytes == expected_bytes)),
        "no installed font under {} matched expected bytes",
        root.display()
    );
}

fn assert_already_present_skip(json: &serde_json::Value) {
    assert!(
        json["peers"][0]["skipped"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value
                .as_str()
                .is_some_and(|line| line.contains("already present")))
    );
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
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
