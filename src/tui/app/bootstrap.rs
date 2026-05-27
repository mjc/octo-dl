use std::collections::{HashMap, HashSet};
use std::env;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[cfg(test)]
#[path = "tests_bootstrap.rs"]
mod tests;

use indexmap::IndexMap;
use ratatui::widgets::ListState;
use tokio::sync::{mpsc, watch};

use crate::{
    DownloadConfig, ServiceConfig,
    core::{DownloadState, SavedMegaSession, SessionMeta, SessionSnapshot},
};

use crate::tui::dashboard::DashboardUiMode;

use super::{
    App, DownloadEvent, NoCredentialsFallback, Popup, SharedAppState, SharedStateChannels, UiAction,
};
use crate::tui::event::DownloadRequest;

const AUTO_LOGIN_IDLE_DELAY: Duration = Duration::from_millis(750);

fn path_io_error(action: &str, path: &Path, error: io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        format!("{action} {}: {error}", path.display()),
    )
}

fn state_dir_service_config_path() -> PathBuf {
    let mut path = SessionSnapshot::state_dir();
    path.pop();
    path.push("config.toml");
    path
}

fn default_service_config_path() -> PathBuf {
    env::current_dir()
        .map(|dir| dir.join("config.toml"))
        .unwrap_or_else(|_| state_dir_service_config_path())
}

fn distinct_fallback_service_config_path(primary: &Path) -> Option<PathBuf> {
    let fallback = state_dir_service_config_path();
    (fallback != primary).then_some(fallback)
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
            saved_mega_session: None,
            deferred_login_fallback: None,
            deferred_login_deadline: None,
            last_user_activity: Instant::now(),
            url_input: String::new(),
            url_input_cursor: 0,
            url_input_active: false,
            urls: Vec::new(),
            files: Vec::new(),
            cached_visible_rows: Vec::new(),
            cached_visible_rows_key: Default::default(),
            dashboard_revision: 0,
            dashboard_binary_cache_key: None,
            dashboard_binary_cache: bytes::Bytes::new(),
            visible_file_positions: HashMap::new(),
            overlay_files: IndexMap::new(),
            file_ui: HashMap::new(),
            queued_file_effects: IndexMap::new(),
            file_list_state: ListState::default(),
            expanded_packages: HashSet::new(),
            sort: super::SortState::new(),
            total_downloaded: 0,
            total_size: 0,
            files_completed: 0,
            files_total: 0,
            current_speed: 0,
            total_network_downloaded: 0,
            overlay_total_downloaded: 0,
            overlay_total_size: 0,
            overlay_files_completed: 0,
            overlay_files_total: 0,
            overlay_total_network_downloaded: 0,
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
            download_task_running: false,
            cancellation_tokens: HashMap::new(),
            file_attempt_ids: HashMap::new(),
            reset_pending_files: HashSet::new(),
            reverify_pending_files: HashSet::new(),
            verifying_files: HashSet::new(),
            verification_inflight_files: HashSet::new(),
            verification_targets: HashMap::new(),
            session: None,
            session_persistence: super::SessionPersistence::new(),
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
            visible_sync_defer_depth: 0,
            visible_sync_pending: false,
            pending_visible_selection: None,
            session_persist_defer_depth: 0,
            scheduler_pending_sync_defer_depth: 0,
            pending_core_state_session_persistence: false,
            pending_scheduler_pending_order_sync: false,
            pending_session_persistence: None,
            #[cfg(test)]
            visible_sync_count: 0,
            #[cfg(test)]
            session_persist_count: 0,
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
        let fallback_config_path = distinct_fallback_service_config_path(&config_path);
        let mut app = Self::new(env_api_port, event_tx, quit_enabled);
        app.persist_config_path = Some(config_path.clone());

        if config_path.exists() {
            let (host, mut port) = app.apply_service_config(&config_path)?;
            if !app.login.has_credentials()
                && let Some(fallback) = fallback_config_path.as_deref()
            {
                app.load_missing_credentials_from_config(fallback)?;
            }
            if env::var_os("OCTO_API_PORT").is_some() {
                port = env_api_port;
            }
            app.api_port = port;
            return Ok((app, host, port));
        }

        if let Some(fallback) = fallback_config_path
            && fallback.exists()
        {
            app.persist_config_path = Some(fallback.clone());
            let (host, mut port) = app.apply_service_config(&fallback)?;
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
    }

    pub(crate) fn prepare_headless_startup(&mut self) -> io::Result<()> {
        self.load_credentials_from_env();
        let config_path = self
            .persist_config_path
            .clone()
            .unwrap_or_else(default_service_config_path);
        self.require_credentials(&config_path)?;
        self.resume_latest_session();
        self.auto_login(NoCredentialsFallback::Silent);
        Ok(())
    }

    pub(crate) fn note_user_activity(&mut self) {
        self.last_user_activity = Instant::now();
        if self.deferred_login_fallback.is_some() {
            self.deferred_login_deadline = Some(self.last_user_activity + AUTO_LOGIN_IDLE_DELAY);
        }
    }

    pub(crate) fn schedule_auto_login(&mut self, fallback: NoCredentialsFallback) {
        if self.authenticated || self.login.logging_in || self.download_task_running {
            return;
        }
        self.deferred_login_fallback = Some(match (self.deferred_login_fallback, fallback) {
            (Some(NoCredentialsFallback::ShowPopup), _) | (_, NoCredentialsFallback::ShowPopup) => {
                NoCredentialsFallback::ShowPopup
            }
            _ => NoCredentialsFallback::Silent,
        });
        self.deferred_login_deadline = Some(self.last_user_activity + AUTO_LOGIN_IDLE_DELAY);
    }

    pub(crate) fn poll_deferred_auto_login(&mut self) -> bool {
        if self.authenticated || self.login.logging_in || self.download_task_running {
            self.deferred_login_fallback = None;
            self.deferred_login_deadline = None;
            return false;
        }
        let Some(deadline) = self.deferred_login_deadline else {
            return false;
        };
        if Instant::now() < deadline || self.url_input_active || self.popup != Popup::None {
            return false;
        }
        let fallback = self
            .deferred_login_fallback
            .take()
            .unwrap_or(NoCredentialsFallback::Silent);
        self.deferred_login_deadline = None;
        self.auto_login(fallback)
    }

    fn clear_deferred_auto_login(&mut self) {
        self.deferred_login_fallback = None;
        self.deferred_login_deadline = None;
    }

    fn matching_saved_mega_session(&self) -> Option<(String, String)> {
        let saved = self.saved_mega_session.as_ref()?.decrypt()?;
        if self.login.has_credentials() && !saved.0.eq_ignore_ascii_case(self.login.email()) {
            return None;
        }
        Some(saved)
    }

    pub(crate) fn shared_state_channels(
        &self,
        enabled: bool,
        ui_mode: DashboardUiMode,
    ) -> SharedStateChannels {
        let (action_tx, action_rx) = mpsc::unbounded_channel::<UiAction>();
        let initial_state = enabled
            .then(|| bytes::Bytes::from(self.borrowed_dashboard_postcard(ui_mode, false)))
            .unwrap_or_default();
        let (state_tx, state_rx) = watch::channel(initial_state);
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
        bookmarklet_host: Option<String>,
        shared_state: Option<SharedAppState>,
        remote_tui_stream: bool,
    ) {
        let api_tx = self.event_tx.clone();
        let api_key = self.api_key.clone();
        tokio::spawn(async move {
            if let Err(e) = super::super::api::run_api_server(
                api_tx,
                &host,
                port,
                bookmarklet_host.as_deref(),
                shared_state,
                remote_tui_stream,
                api_key,
            )
            .await
            {
                log::error!("API server error: {e}");
            }
        });
    }

    pub(crate) fn begin_login(&mut self) {
        self.clear_deferred_auto_login();
        self.begin_background_auth(None);
    }

    fn begin_background_auth(&mut self, resume_session: Option<(String, String)>) {
        if self.authenticated || self.login.logging_in || self.client_rx.is_some() {
            return;
        }

        self.login.error = None;
        self.login.logging_in = true;
        self.status = if resume_session.is_some() {
            "Resuming saved session...".to_string()
        } else {
            "Logging in...".to_string()
        };
        let tx = self.event_tx.clone();
        let email = self.login.email().to_owned();
        let password = self.login.password().to_owned();
        let mfa = self.login.mfa_option().map(str::to_owned);
        let resume_session = resume_session.clone();

        let (client_tx, client_rx) = tokio::sync::oneshot::channel();
        self.client_rx = Some(client_rx);

        tokio::spawn(async move {
            let mut clear_saved_session = false;
            let _ = tx.send(DownloadEvent::StatusMessage(if resume_session.is_some() {
                "Resuming saved session...".to_string()
            } else {
                "Logging in...".to_string()
            }));

            let http = match super::super::download::build_http_client() {
                Ok(http) => http,
                Err(e) => {
                    let _ = tx.send(DownloadEvent::LoginResult {
                        success: false,
                        error: Some(format!("Failed to build HTTP client: {e}")),
                        saved_session: None,
                        clear_saved_session: false,
                    });
                    return;
                }
            };

            let build_client = || mega::Client::builder().build(http.clone());

            let mut mega_client = match build_client() {
                Ok(client) => client,
                Err(e) => {
                    let _ = tx.send(DownloadEvent::LoginResult {
                        success: false,
                        error: Some(format!("Failed to create MEGA client: {e}")),
                        saved_session: None,
                        clear_saved_session: false,
                    });
                    return;
                }
            };

            if let Some((session_email, serialized_session)) = resume_session {
                match mega_client.resume_session(&serialized_session).await {
                    Ok(()) => {
                        let saved_session = mega_client
                            .serialize_session()
                            .await
                            .ok()
                            .map(|session| SavedMegaSession::encrypt(&session_email, &session));
                        let _ = client_tx.send((mega_client, http));
                        let _ = tx.send(DownloadEvent::LoginResult {
                            success: true,
                            error: None,
                            saved_session,
                            clear_saved_session: false,
                        });
                        return;
                    }
                    Err(_) => {
                        clear_saved_session = true;
                    }
                }

                mega_client = match build_client() {
                    Ok(client) => client,
                    Err(e) => {
                        let _ = tx.send(DownloadEvent::LoginResult {
                            success: false,
                            error: Some(format!("Failed to create MEGA client: {e}")),
                            saved_session: None,
                            clear_saved_session,
                        });
                        return;
                    }
                };
            }

            if email.is_empty() || password.is_empty() {
                let _ = tx.send(DownloadEvent::LoginResult {
                    success: false,
                    error: Some("No saved credentials available".to_string()),
                    saved_session: None,
                    clear_saved_session,
                });
                return;
            }

            if let Err(e) = mega_client.login(&email, &password, mfa.as_deref()).await {
                let _ = tx.send(DownloadEvent::LoginResult {
                    success: false,
                    error: Some(e.to_string()),
                    saved_session: None,
                    clear_saved_session,
                });
                return;
            }

            let saved_session = mega_client
                .serialize_session()
                .await
                .ok()
                .map(|session| SavedMegaSession::encrypt(&email, &session));
            let _ = client_tx.send((mega_client, http));
            let _ = tx.send(DownloadEvent::LoginResult {
                success: true,
                error: None,
                saved_session,
                clear_saved_session,
            });
        });
    }

    pub(crate) fn auto_login(&mut self, fallback: NoCredentialsFallback) -> bool {
        self.clear_deferred_auto_login();
        if let Some(saved_session) = self.matching_saved_mega_session() {
            self.begin_background_auth(Some(saved_session));
            true
        } else if self.login.has_credentials() {
            self.begin_background_auth(None);
            true
        } else {
            if fallback == NoCredentialsFallback::ShowPopup {
                self.popup = Popup::Login;
                return true;
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

    fn load_missing_credentials_from_config(&mut self, config_path: &Path) -> io::Result<()> {
        if self.login.has_credentials() || !config_path.exists() {
            return Ok(());
        }

        let service_config = ServiceConfig::load(config_path)?;
        if !service_config.credentials.has_credentials() {
            return Ok(());
        }

        if let Some((email, password, mfa)) = service_config.credentials.decrypt_if_needed() {
            log::info!("Loaded fallback credentials from {}", config_path.display());
            self.login
                .set_credentials_if_missing(&email, &password, &mfa);
            if self.api_key.is_none() {
                self.api_key.clone_from(&service_config.api.api_key);
            }
        } else {
            log::warn!(
                "Failed to decrypt fallback credentials from {}",
                config_path.display()
            );
        }

        Ok(())
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
            if let Some((email, password, mfa)) = service_config.credentials.decrypt_if_needed() {
                log::info!("Loaded credentials from config file");
                credentials_from_config = self.login.set_credentials(email, password, mfa);

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

        self.saved_mega_session =
            service_config
                .credentials
                .saved_session
                .clone()
                .filter(|saved_session| {
                    let Some((email, _)) = saved_session.decrypt() else {
                        log::warn!("Failed to decrypt saved MEGA session from config");
                        return false;
                    };
                    !self.login.has_credentials() || email.eq_ignore_ascii_case(self.login.email())
                });

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
        if self.login.has_credentials() {
            service_config.credentials = crate::ServiceCredentials {
                encrypted: false,
                email: self.login.email().to_string(),
                password: self.login.password().to_string(),
                mfa: String::new(),
                saved_session: self.saved_mega_session.clone(),
            };
            service_config.credentials.encrypt_in_place();
        } else {
            service_config.credentials.saved_session = self.saved_mega_session.clone();
        }
        service_config.api.port = self.api_port;
        service_config.api.api_key = self.api_key.clone();
        service_config.download = self.config.config.clone();
        service_config.save(config_path)
    }
}
