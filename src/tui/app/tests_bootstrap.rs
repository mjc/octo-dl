use super::*;
use crate::ServiceConfig;
use crate::test_support::{CurrentDirGuard, StateDirectoryGuard};
use std::fs;
use std::io;
use tokio::sync::mpsc;

use tempfile::tempdir;

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
        },
        api: crate::ApiConfig::default(),
        download: crate::DownloadConfig {
            path: Some(blocked_child.display().to_string()),
            ..crate::DownloadConfig::default()
        },
    };
    config
        .save(&config_path)
        .expect("config should be writable");

    let (tx, _rx) = mpsc::unbounded_channel();
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
fn persist_login_credentials_creates_default_config_file() {
    let dir = tempdir().expect("temp dir should exist");
    let _guard = StateDirectoryGuard::set(dir.path());
    let _cwd = CurrentDirGuard::set(dir.path());
    let config_path = dir.path().join("config.toml");
    let mut config = ServiceConfig::load_or_create(&config_path).expect("config should exist");
    config.download.path = Some(dir.path().join("downloads").to_string_lossy().into_owned());
    config.save(&config_path).expect("config should save");

    let (tx, _rx) = mpsc::unbounded_channel();
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
    let (email, password, mfa) = saved
        .credentials
        .decrypt_if_needed()
        .expect("saved credentials should decrypt");
    assert_eq!(email, "user@example.com");
    assert_eq!(password, "super-secret");
    assert!(mfa.is_empty());
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
    };
    config.download.path = Some(dir.path().join("downloads").to_string_lossy().into_owned());
    config.credentials.encrypt_in_place();
    config.save(&config_path).expect("config should save");

    let (tx, _rx) = mpsc::unbounded_channel();
    let (app, _host, _port) =
        App::new_with_optional_service_config(tx, true, None, 9723).expect("app should initialize");

    assert_eq!(app.login.email(), "saved@example.com");
    assert_eq!(app.login.password(), "saved-secret");
    assert!(app.login.mfa().is_empty());
    assert_eq!(
        app.persist_config_path.as_deref(),
        Some(config_path.as_path())
    );
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
    };
    state_config.credentials.encrypt_in_place();
    state_config.api.api_key = Some("state-api-key".to_string());
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

    let (tx, _rx) = mpsc::unbounded_channel();
    let (app, _host, _port) =
        App::new_with_optional_service_config(tx, true, None, 9723).expect("app should initialize");

    assert_eq!(app.login.email(), "saved@example.com");
    assert_eq!(app.login.password(), "saved-secret");
    assert!(app.api_key.is_some());
    assert_eq!(
        app.persist_config_path.as_deref(),
        Some(cwd_config_path.as_path())
    );
}
