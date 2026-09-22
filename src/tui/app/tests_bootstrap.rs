use super::*;
use crate::ServiceConfig;
use crate::core::SavedMegaSession;
use crate::test_support::{CurrentDirGuard, StateDirectoryGuard};
use std::fs;
use std::io;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use tempfile::tempdir;

#[test]
fn api_host_policy_treats_bracketed_ipv6_loopback_as_loopback() {
    assert!(!api_host_requires_api_key("[::1]"));
}

#[test]
fn api_host_policy_preserves_non_loopback_host_support() {
    assert!(api_host_requires_api_key("192.0.2.1"));
    assert!(api_host_requires_api_key("[2001:db8::1]"));
    assert!(api_host_requires_api_key("api.example.test"));
    assert!(!api_host_requires_api_key("localhost"));
}

#[test]
fn apply_service_config_reports_download_directory_path() {
    let dir = tempdir().expect("temp dir should exist");
    let blocker = dir.path().join("not-a-directory");
    fs::write(&blocker, "block").expect("blocker file should be writable");
    let config_path = dir.path().join("config.toml");
    let blocked_child = blocker.join("child");
    let config = ServiceConfig {
        credentials: crate::ServiceCredentials {
            encrypted: false,
            email: String::new(),
            password: String::new(),
            mfa: String::new(),
            saved_session: None,
        },
        credential_key: None,
        api: crate::ApiConfig::default(),
        download: crate::DownloadConfig {
            path: Some(blocked_child.display().to_string()),
            ..crate::DownloadConfig::default()
        },
    };
    config
        .save(&config_path)
        .expect("config should be writable");

    let (tx, _rx) = mpsc::channel(64);
    let mut app = App::new(9723, tx, true);
    let error = app
        .apply_service_config(&config_path)
        .expect_err("invalid download dir should fail");
    let message = error.to_string();

    assert_eq!(error.kind(), io::ErrorKind::NotADirectory);
    assert!(message.contains("Failed to create download directory"));
    assert!(message.contains(&blocked_child.display().to_string()));
}

#[test]
fn explicit_relative_config_path_remains_rooted_after_download_directory_change() {
    let root = tempdir().expect("root directory should exist");
    let downloads = tempdir().expect("download directory should exist");
    let _cwd = CurrentDirGuard::set(root.path());
    let config_path = root.path().join("config.toml");
    let mut config = ServiceConfig::load_or_create(&config_path).expect("config should exist");
    config.download.path = Some(downloads.path().display().to_string());
    config.save(&config_path).expect("config should save");

    let (tx, _rx) = mpsc::channel(64);
    let relative_path = std::path::Path::new("config.toml");
    let _app = App::new_with_optional_service_config(tx, true, Some(relative_path), 9723)
        .expect("app should initialize");

    let saved = ServiceConfig::load(&config_path).expect("original config should remain readable");
    assert!(saved.api.api_key.is_some());
    assert!(!downloads.path().join("config.toml").exists());
}

#[test]
fn persist_login_credentials_creates_default_config_file() {
    let dir = tempdir().expect("temp dir should exist");
    let _guard = StateDirectoryGuard::set(dir.path());
    let _cwd = CurrentDirGuard::set(dir.path());
    let config_path = dir.path().join("config.toml");
    let mut config = ServiceConfig::load_or_create(&config_path).expect("config should exist");
    config.download.path = Some(dir.path().join("downloads").to_string_lossy().into_owned());
    config.save(&config_path).expect("config should save");

    let (tx, _rx) = mpsc::channel(64);
    let (mut app, _host, _port) =
        App::new_with_optional_service_config(tx, true, None, 9723).expect("app should initialize");
    assert!(app.login.set_credentials(
        "user@example.com".to_string(),
        "super-secret".to_string(),
        "123456".to_string()
    ));

    app.persist_login_credentials_to_config()
        .expect("credentials should persist");

    assert!(config_path.exists());

    let saved = ServiceConfig::load(&config_path).expect("config should load");
    assert!(saved.credentials.encrypted);
    let key = crate::core::decode_credential_key(saved.credential_key.as_deref().unwrap())
        .expect("credential key should load");
    let (email, password, mfa) = saved
        .credentials
        .decrypt_if_needed_with_key(&key)
        .expect("saved credentials should decrypt");
    assert_eq!(email, "user@example.com");
    assert_eq!(password, "super-secret");
    assert!(mfa.is_empty());
    assert!(saved.credentials.saved_session.is_none());
}

#[test]
fn new_without_explicit_config_loads_default_saved_credentials() {
    let dir = tempdir().expect("temp dir should exist");
    let _guard = StateDirectoryGuard::set(dir.path());
    let _cwd = CurrentDirGuard::set(dir.path());
    let config_path = dir.path().join("config.toml");
    let mut config = ServiceConfig::load_or_create(&config_path).expect("config should exist");
    config.credentials = crate::ServiceCredentials {
        encrypted: false,
        email: "saved@example.com".to_string(),
        password: "saved-secret".to_string(),
        mfa: "654321".to_string(),
        saved_session: None,
    };
    config.download.path = Some(dir.path().join("downloads").display().to_string());
    config.download.path = Some(dir.path().join("downloads").to_string_lossy().into_owned());
    config.credentials.encrypt_in_place();
    config.save(&config_path).expect("config should save");

    let (tx, _rx) = mpsc::channel(64);
    let (app, _host, _port) =
        App::new_with_optional_service_config(tx, true, None, 9723).expect("app should initialize");

    assert_eq!(app.login.email(), "saved@example.com");
    assert_eq!(app.login.password(), "saved-secret");
    assert_eq!(app.login.mfa(), "654321");
    assert_eq!(
        app.persist_config_path.as_deref(),
        Some(config_path.as_path())
    );
}

#[test]
fn interactive_startup_defers_auto_login_until_terminal_draws() {
    let dir = tempdir().expect("state dir should exist");
    let _guard = StateDirectoryGuard::set(dir.path());
    let (tx, _rx) = mpsc::channel(64);
    let mut app = App::new(9723, tx, true);
    assert!(app.login.set_credentials(
        "saved@example.com".to_string(),
        "saved-secret".to_string(),
        String::new()
    ));

    app.prepare_interactive_startup();

    assert!(!app.login.logging_in);
    assert!(app.client_rx.is_none());
}

#[test]
fn persist_login_credentials_preserves_existing_credentials_when_only_session_changes() {
    let dir = tempdir().expect("temp dir should exist");
    let _guard = StateDirectoryGuard::set(dir.path());
    let _cwd = CurrentDirGuard::set(dir.path());
    let config_path = dir.path().join("config.toml");
    let mut config = ServiceConfig::load_or_create(&config_path).expect("config should exist");
    config.credentials = crate::ServiceCredentials {
        encrypted: false,
        email: "saved@example.com".to_string(),
        password: "saved-secret".to_string(),
        mfa: String::new(),
        saved_session: None,
    };
    config.download.path = Some(dir.path().join("downloads").display().to_string());
    config.credentials.encrypt_in_place();
    config.save(&config_path).expect("config should save");

    let (tx, _rx) = mpsc::channel(64);
    let (mut app, _host, _port) =
        App::new_with_optional_service_config(tx, true, None, 9723).expect("app should initialize");
    app.saved_mega_session = Some(SavedMegaSession::encrypt(
        "saved@example.com",
        "serialized-session",
    ));

    app.persist_login_credentials_to_config()
        .expect("session should persist");

    let saved = ServiceConfig::load(&config_path).expect("config should load");
    let key = crate::core::decode_credential_key(saved.credential_key.as_deref().unwrap())
        .expect("credential key should load");
    let (email, password, _mfa) = saved
        .credentials
        .decrypt_if_needed_with_key(&key)
        .expect("saved credentials should decrypt");
    assert_eq!(email, "saved@example.com");
    assert_eq!(password, "saved-secret");
    let (session_email, session) = saved
        .credentials
        .saved_session
        .expect("saved session should exist")
        .decrypt()
        .expect("saved session should decrypt");
    assert_eq!(session_email, "saved@example.com");
    assert_eq!(session, "serialized-session");
}

#[test]
fn new_without_explicit_config_loads_saved_mega_session() {
    let dir = tempdir().expect("temp dir should exist");
    let _guard = StateDirectoryGuard::set(dir.path());
    let _cwd = CurrentDirGuard::set(dir.path());
    let config_path = dir.path().join("config.toml");
    let mut config = ServiceConfig::load_or_create(&config_path).expect("config should exist");
    config.credentials = crate::ServiceCredentials {
        encrypted: false,
        email: "saved@example.com".to_string(),
        password: "saved-secret".to_string(),
        mfa: String::new(),
        saved_session: Some(SavedMegaSession::encrypt(
            "saved@example.com",
            "serialized-session",
        )),
    };
    config.download.path = Some(dir.path().join("downloads").display().to_string());
    config.credentials.encrypt_in_place();
    config.save(&config_path).expect("config should save");

    let (tx, _rx) = mpsc::channel(64);
    let (app, _host, _port) =
        App::new_with_optional_service_config(tx, true, None, 9723).expect("app should initialize");

    let (session_email, session) = app
        .saved_mega_session
        .expect("saved session should load")
        .decrypt()
        .expect("saved session should decrypt");
    assert_eq!(session_email, "saved@example.com");
    assert_eq!(session, "serialized-session");
}

#[test]
fn deferred_auto_login_waits_for_idle_before_showing_popup() {
    let (tx, _rx) = mpsc::channel(64);
    let mut app = App::new(9723, tx, true);

    app.schedule_auto_login(NoCredentialsFallback::ShowPopup);
    assert_eq!(app.popup, Popup::None);
    assert!(!app.poll_deferred_auto_login());

    app.deferred_login_deadline = Some(
        Instant::now()
            .checked_sub(Duration::from_millis(1))
            .unwrap(),
    );

    assert!(app.poll_deferred_auto_login());
    assert_eq!(app.popup, Popup::Login);
}

#[test]
fn disabled_shared_state_skips_initial_dashboard_snapshot() {
    let (tx, _rx) = mpsc::channel(64);
    let app = App::new(9723, tx, true);

    let SharedStateChannels {
        state_tx,
        shared_state,
        ..
    } = app.shared_state_channels(false, DashboardUiMode::Tui);

    assert!(shared_state.is_none());
    assert!(state_tx.borrow().is_empty());
}

#[test]
fn implicit_cwd_template_falls_back_to_state_config_credentials() {
    let state_dir = tempdir().expect("state dir should exist");
    let cwd_dir = tempdir().expect("cwd should exist");
    let _guard = StateDirectoryGuard::set(state_dir.path());
    let _cwd = CurrentDirGuard::set(cwd_dir.path());

    let state_config_path = state_dir.path().join("config.toml");
    let mut state_config =
        ServiceConfig::load_or_create(&state_config_path).expect("state config should exist");
    state_config.credentials = crate::ServiceCredentials {
        encrypted: false,
        email: "saved@example.com".to_string(),
        password: "saved-secret".to_string(),
        mfa: "654321".to_string(),
        saved_session: None,
    };
    state_config.credentials.encrypt_in_place();
    state_config.api.api_key =
        Some(crate::config::ApiKey::new("state-api-key").expect("test API key"));
    state_config
        .save(&state_config_path)
        .expect("state config should save");

    let cwd_config_path = cwd_dir.path().join("config.toml");
    let mut cwd_config =
        ServiceConfig::load_or_create(&cwd_config_path).expect("cwd config should exist");
    cwd_config.download.path = Some(
        cwd_dir
            .path()
            .join("downloads")
            .to_string_lossy()
            .into_owned(),
    );
    cwd_config
        .save(&cwd_config_path)
        .expect("cwd config should save");

    let (tx, _rx) = mpsc::channel(64);
    let (app, _host, _port) =
        App::new_with_optional_service_config(tx, true, None, 9723).expect("app should initialize");

    assert_eq!(app.login.email(), "saved@example.com");
    assert_eq!(app.login.password(), "saved-secret");
    assert_eq!(app.login.mfa(), "654321");
    assert!(app.api_key.is_some());
    assert_eq!(
        app.persist_config_path.as_deref(),
        Some(cwd_config_path.as_path())
    );
}
