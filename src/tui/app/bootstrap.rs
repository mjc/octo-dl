use std::collections::{HashMap, HashSet};
use std::env;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;

use indexmap::IndexMap;
use ratatui::widgets::ListState;
use tokio::sync::{mpsc, watch};

use crate::tui::WebOptions;
use crate::{
    DownloadConfig, ServiceConfig,
    core::{DownloadState, SessionMeta, SessionSnapshotV3},
};

use super::{
    App, DownloadEvent, NoCredentialsFallback, Popup, SharedAppState, SharedStateChannels, UiAction,
};
use crate::tui::event::DownloadRequest;

fn path_io_error(action: &str, path: &Path, error: io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        format!("{action} {}: {error}", path.display()),
    )
}

fn default_service_config_path() -> PathBuf {
    let mut path = SessionSnapshotV3::state_dir();
    path.pop();
    path.push("config.toml");
    path
}

impl App {
    pub fn new(
        api_port: u16,
        event_tx: mpsc::UnboundedSender<DownloadEvent>,
        quit_enabled: bool,
    ) -> Self {
        let (url_tx, url_rx) = mpsc::unbounded_channel::<DownloadRequest>();
        let (pause_tx, pause_rx) = watch::channel(false);
        let (token_tx, token_rx) = mpsc::unbounded_channel::<super::TokenMessage>();
        Self {
            popup: super::Popup::None,
            pending_confirmation: None,
            should_quit: false,
            quit_policy: super::QuitPolicy::from_bool(quit_enabled),
            login: super::LoginState::new(),
            authenticated: false,
            url_input: String::new(),
            url_input_active: false,
            urls: Vec::new(),
            files: Vec::new(),
            overlay_files: IndexMap::new(),
            file_ui: HashMap::new(),
            file_list_state: ListState::default(),
            expanded_packages: HashSet::new(),
            sort: super::SortState::new(),
            total_downloaded: 0,
            total_size: 0,
            files_completed: 0,
            files_total: 0,
            current_speed: 0,
            total_network_downloaded: 0,
            aggregate_rate: Default::default(),
            status: String::new(),
            paused: false,
            config: super::ConfigState::new(),
            event_tx,
            url_tx,
            url_rx: Some(url_rx),
            pause_tx,
            pause_rx: Some(pause_rx),
            token_rx,
            token_tx: Some(token_tx),
            client_rx: None,
            cancellation_tokens: HashMap::new(),
            file_attempt_ids: HashMap::new(),
            deleted_files: HashSet::new(),
            reset_pending_files: HashSet::new(),
            session: None,
            core_state: DownloadState::new(SessionMeta {
                config: DownloadConfig::default(),
                ..SessionMeta::default()
            }),
            api_port,
            api_key: None,
            persist_config_path: None,
            cpu_usage: 0.0,
            last_tick: Instant::now(),
            memory_rss: 0,
        }
    }

    pub(crate) fn new_with_optional_service_config(
        event_tx: mpsc::UnboundedSender<DownloadEvent>,
        quit_enabled: bool,
        config_path: Option<&Path>,
        default_api_port: u16,
    ) -> io::Result<(Self, String, u16)> {
        if let Some(path) = config_path {
            let mut app = Self::new(0, event_tx, quit_enabled);
            app.persist_config_path = Some(path.to_path_buf());
            let (host, port) = app.apply_service_config(path)?;
            app.api_port = port;
            return Ok((app, host, port));
        }

        let env_api_port = env::var("OCTO_API_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(default_api_port);
        let config_path = default_service_config_path();
        let mut app = Self::new(env_api_port, event_tx, quit_enabled);
        app.persist_config_path = Some(config_path.clone());

        if config_path.exists() {
            let (host, mut port) = app.apply_service_config(&config_path)?;
            if env::var_os("OCTO_API_PORT").is_some() {
                port = env_api_port;
            }
            app.api_port = port;
            return Ok((app, host, port));
        }

        Ok((app, "127.0.0.1".to_string(), env_api_port))
    }

    pub(crate) fn require_credentials(&self, config_path: &Path) -> io::Result<()> {
        if self.login.has_credentials() {
            return Ok(());
        }

        log::error!(
            "No credentials configured. Edit {} and set email/password under [credentials], then restart.",
            config_path.display()
        );
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("No credentials in {}", config_path.display()),
        ))
    }

    pub(crate) fn prepare_interactive_startup(&mut self) {
        self.resume_latest_session();
        self.load_credentials_from_env();
        self.auto_login(NoCredentialsFallback::ShowPopup);
    }

    pub(crate) fn prepare_headless_startup(&mut self, config_path: &Path) -> io::Result<()> {
        self.load_credentials_from_env();
        self.require_credentials(config_path)?;
        self.resume_latest_session();
        self.auto_login(NoCredentialsFallback::Silent);
        Ok(())
    }

    pub(crate) fn shared_state_channels(&self, enabled: bool) -> SharedStateChannels {
        let (action_tx, action_rx) = mpsc::unbounded_channel::<UiAction>();
        let (state_tx, state_rx) = watch::channel(self.to_json());
        let shared_state = enabled.then_some(SharedAppState {
            action_tx,
            state_rx,
        });
        SharedStateChannels {
            action_rx,
            state_tx,
            shared_state,
        }
    }

    pub(crate) fn spawn_api_server(
        &self,
        host: String,
        port: u16,
        web_opts: Option<WebOptions>,
        shared_state: Option<SharedAppState>,
    ) {
        let api_tx = self.event_tx.clone();
        let api_key = self.api_key.clone();
        tokio::spawn(async move {
            if let Err(e) = super::super::api::run_api_server(
                api_tx,
                &host,
                port,
                web_opts.as_ref(),
                shared_state,
                api_key,
            )
            .await
            {
                log::error!("API server error: {e}");
            }
        });
    }

    pub(crate) fn begin_login(&mut self) {
        self.login.error = None;
        self.login.logging_in = true;
        self.status = "Logging in...".to_string();
        let tx = self.event_tx.clone();
        let email = self.login.email().to_owned();
        let password = self.login.password().to_owned();
        let mfa = self.login.mfa_option().map(str::to_owned);

        let (client_tx, client_rx) = tokio::sync::oneshot::channel();
        self.client_rx = Some(client_rx);

        tokio::spawn(async move {
            let _ = tx.send(DownloadEvent::StatusMessage("Logging in...".to_string()));

            let http = match super::super::download::build_http_client() {
                Ok(http) => http,
                Err(e) => {
                    let _ = tx.send(DownloadEvent::LoginResult {
                        success: false,
                        error: Some(format!("Failed to build HTTP client: {e}")),
                    });
                    return;
                }
            };

            let mut mega_client = match mega::Client::builder().build(http.clone()) {
                Ok(client) => client,
                Err(e) => {
                    let _ = tx.send(DownloadEvent::LoginResult {
                        success: false,
                        error: Some(format!("Failed to create MEGA client: {e}")),
                    });
                    return;
                }
            };

            if let Err(e) = mega_client.login(&email, &password, mfa.as_deref()).await {
                let _ = tx.send(DownloadEvent::LoginResult {
                    success: false,
                    error: Some(format!("Login failed: {e}")),
                });
                return;
            }

            let _ = client_tx.send((mega_client, http));
            let _ = tx.send(DownloadEvent::LoginResult {
                success: true,
                error: None,
            });
        });
    }

    pub(crate) fn auto_login(&mut self, fallback: NoCredentialsFallback) -> bool {
        if self.login.has_credentials() {
            self.begin_login();
            true
        } else {
            if fallback == NoCredentialsFallback::ShowPopup {
                self.popup = Popup::Login;
            }
            false
        }
    }

    pub(crate) fn load_credentials_from_env(&mut self) {
        let email = env::var("MEGA_EMAIL").unwrap_or_default();
        let password = env::var("MEGA_PASSWORD").unwrap_or_default();
        let mfa = env::var("MEGA_MFA").unwrap_or_default();
        if !email.is_empty() || !password.is_empty() {
            log::info!("Using MEGA credentials from environment variables");
        }
        self.login
            .set_credentials_if_missing(&email, &password, &mfa);
    }

    pub(crate) fn apply_service_config(&mut self, config_path: &Path) -> io::Result<(String, u16)> {
        let mut service_config = ServiceConfig::load_or_create(config_path)?;
        log::info!("Loaded config from {}", config_path.display());

        if let Some(ref dl_path) = service_config.download.path {
            let download_dir = Path::new(dl_path);
            if !download_dir.exists() {
                std::fs::create_dir_all(download_dir).map_err(|error| {
                    path_io_error("Failed to create download directory", download_dir, error)
                })?;
            }
            std::env::set_current_dir(download_dir).map_err(|error| {
                path_io_error("Failed to change directory to", download_dir, error)
            })?;
            log::info!("Download directory: {dl_path}");
        }

        self.config.config = service_config.download.clone();
        self.api_key.clone_from(&service_config.api.api_key);

        let mut credentials_from_config = false;
        if service_config.credentials.has_credentials() {
            if let Some((email, password, _mfa)) = service_config.credentials.decrypt_if_needed() {
                log::info!("Loaded credentials from config file");
                credentials_from_config =
                    self.login.set_credentials(email, password, String::new());

                if !service_config.credentials.encrypted {
                    log::info!("Encrypting plaintext credentials in config file");
                    service_config.credentials.encrypt_in_place();
                    service_config.save(config_path)?;
                }
            } else {
                log::warn!(
                    "Failed to decrypt credentials from config (machine key mismatch?). Falling back to environment variables."
                );
            }
        }

        if !credentials_from_config {
            if let (Ok(email), Ok(password)) = (env::var("MEGA_EMAIL"), env::var("MEGA_PASSWORD")) {
                log::info!(
                    "Using credentials from MEGA_EMAIL and MEGA_PASSWORD environment variables"
                );
                self.login.set_credentials(
                    email,
                    password,
                    env::var("MEGA_MFA").unwrap_or_default(),
                );
            } else if service_config.credentials.has_credentials() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Failed to decrypt credentials from config file. Set MEGA_EMAIL and MEGA_PASSWORD environment variables, or re-create the config file as the current user.",
                ));
            }
        }

        if service_config.api.api_key.is_none() {
            let key = uuid::Uuid::new_v4().simple().to_string();
            log::info!("Generated API key: {key}");
            service_config.api.api_key = Some(key);
            service_config.save(config_path)?;
            self.api_key.clone_from(&service_config.api.api_key);
        }

        Ok((service_config.api.host, service_config.api.port))
    }

    pub(crate) fn persist_login_credentials_to_config(&self) -> io::Result<()> {
        let Some(config_path) = self.persist_config_path.as_deref() else {
            return Ok(());
        };

        let mut service_config = ServiceConfig::load_or_create(config_path)?;
        service_config.credentials = crate::ServiceCredentials {
            encrypted: false,
            email: self.login.email().to_string(),
            password: self.login.password().to_string(),
            mfa: String::new(),
        };
        service_config.credentials.encrypt_in_place();
        service_config.api.port = self.api_port;
        service_config.api.api_key = self.api_key.clone();
        service_config.download = self.config.config.clone();
        service_config.save(config_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    struct StateDirectoryGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        previous: Option<std::ffi::OsString>,
    }

    impl StateDirectoryGuard {
        fn set(path: &Path) -> Self {
            let lock = crate::core::session::STATE_DIRECTORY_TEST_LOCK
                .lock()
                .unwrap();
            let previous = env::var_os("STATE_DIRECTORY");
            unsafe { env::set_var("STATE_DIRECTORY", path) };
            Self {
                _lock: lock,
                previous,
            }
        }
    }

    impl Drop for StateDirectoryGuard {
        fn drop(&mut self) {
            if let Some(ref value) = self.previous {
                unsafe { env::set_var("STATE_DIRECTORY", value) };
            } else {
                unsafe { env::remove_var("STATE_DIRECTORY") };
            }
        }
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
        let config_path = dir.path().join("config.toml");
        let mut config = ServiceConfig::load_or_create(&config_path).expect("config should exist");
        config.download.path = Some(dir.path().join("downloads").to_string_lossy().into_owned());
        config.save(&config_path).expect("config should save");

        let (tx, _rx) = mpsc::unbounded_channel();
        let (mut app, _host, _port) = App::new_with_optional_service_config(tx, true, None, 9723)
            .expect("app should initialize");
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
        let (app, _host, _port) = App::new_with_optional_service_config(tx, true, None, 9723)
            .expect("app should initialize");

        assert_eq!(app.login.email(), "saved@example.com");
        assert_eq!(app.login.password(), "saved-secret");
        assert!(app.login.mfa().is_empty());
        assert_eq!(
            app.persist_config_path.as_deref(),
            Some(config_path.as_path())
        );
    }
}
