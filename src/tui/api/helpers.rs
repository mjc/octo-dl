use axum::http::HeaderMap;
use axum::response::IntoResponse;

use crate::extract_urls;

use super::super::app::UiAction;
use super::super::event::DownloadEvent;
use super::ApiState;

/// Sends a `UiAction` to the event loop, returning 503 if shared state is absent.
pub(super) fn send_ui_action(state: &ApiState, action: UiAction) -> axum::response::Response {
    state.shared.as_ref().map_or_else(
        || {
            (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                "interactive state not enabled",
            )
                .into_response()
        },
        |shared| {
            let _ = shared.action_tx.send(action);
            axum::Json(serde_json::json!({"ok": true})).into_response()
        },
    )
}

/// Dispatches extracted URLs — via `UiAction` if shared state is available,
/// otherwise directly as a `DownloadEvent`.
pub(super) fn dispatch_urls(state: &ApiState, urls: Vec<String>) {
    if urls.is_empty() {
        return;
    }
    if let Some(ref shared) = state.shared {
        let _ = shared.action_tx.send(UiAction::AddUrls(urls));
    } else {
        let _ = state.tx.send(DownloadEvent::UrlsReceived { urls });
    }
}

pub(super) fn extract_and_dispatch_urls(state: &ApiState, text: &str) -> (Vec<String>, usize) {
    let urls = extract_urls(text);
    let count = urls.len();
    dispatch_urls(state, urls.clone());
    (urls, count)
}

pub(super) fn provided_api_key(headers: &HeaderMap) -> Option<&str> {
    if let Some(key) = headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.trim().is_empty())
    {
        return Some(key);
    }

    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|value| !value.trim().is_empty())
}

pub(super) fn require_api_key(
    state: &ApiState,
    headers: &HeaderMap,
) -> Option<axum::response::Response> {
    let expected_key = state.api_key.as_ref()?;
    if provided_api_key(headers).is_some_and(|provided| provided == expected_key) {
        return None;
    }

    Some(
        (
            axum::http::StatusCode::UNAUTHORIZED,
            axum::Json(serde_json::json!({"error": "invalid api key"})),
        )
            .into_response(),
    )
}

fn header_to_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.trim().is_empty())
}

fn parse_forwarded_param(value: &str, key: &str) -> Option<String> {
    for entry in value.split(',') {
        for part in entry.split(';') {
            let mut segments = part.trim().splitn(2, '=');
            if let (Some(param), Some(raw_value)) = (segments.next(), segments.next())
                && param.eq_ignore_ascii_case(key)
            {
                let cleaned = raw_value.trim().trim_matches('"');
                if !cleaned.is_empty() {
                    return Some(cleaned.to_string());
                }
            }
        }
    }
    None
}

pub(super) fn infer_host(headers: &HeaderMap, state: &ApiState) -> String {
    if let Some(host) = header_to_str(headers, "x-forwarded-host") {
        return host.split(',').next().unwrap_or(host).trim().to_string();
    }
    if let Some(forwarded) = header_to_str(headers, "forwarded")
        && let Some(host) = parse_forwarded_param(forwarded, "host")
    {
        return host;
    }
    if let Some(host) = header_to_str(headers, "host") {
        return host.to_string();
    }
    state
        .bookmarklet_host
        .clone()
        .unwrap_or_else(|| state.host.clone())
}
