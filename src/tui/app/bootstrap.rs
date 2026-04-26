use std::env;
use std::io;
use std::path::Path;

use tokio::sync::{mpsc, watch};

use crate::ServiceConfig;

use super::{
    App, DownloadEvent, NoCredentialsFallback, Popup, SharedAppState, SharedStateChannels,
    UiAction, WebOptions,
};

impl App {
    pub(crate) fn new_with_optional_service_config(
        event_tx: mpsc::UnboundedSender<DownloadEvent>,
        quit_enabled: bool,
        config_path: Option<&Path>,
        default_api_port: u16,
    ) -> io::Result<(Self, String, u16)> {
        if let Some(path) = config_path {
            let mut app = Self::new(0, event_tx, quit_enabled);
            let (host, port) = app.apply_service_config(path)?;
            app.api_port = port;
            return Ok((app, host, port));
        }

        let api_port = env::var("OCTO_API_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(default_api_port);
        Ok((
            Self::new(api_port, event_tx, quit_enabled),
            "127.0.0.1".to_string(),
            api_port,
        ))
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
                std::fs::create_dir_all(download_dir)?;
            }
            std::env::set_current_dir(download_dir)?;
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

        if service_config.api.api_key.is_none() {
            let key = uuid::Uuid::new_v4().simple().to_string();
            log::info!("Generated API key: {key}");
            service_config.api.api_key = Some(key);
            service_config.save(config_path)?;
            self.api_key.clone_from(&service_config.api.api_key);
        }

        Ok((service_config.api.host, service_config.api.port))
    }
}
