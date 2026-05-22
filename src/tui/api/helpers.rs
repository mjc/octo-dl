use axum::http::HeaderMap;
use axum::response::IntoResponse;
use std::collections::HashSet;

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

pub(super) fn extract_urls_from_parse_payload(page: &str, fallback: &str) -> Vec<String> {
    let mut urls = Vec::new();
    let mut seen = HashSet::new();
    append_unique_urls(&mut urls, &mut seen, extract_urls(page));
    if urls.is_empty() {
        append_unique_urls(&mut urls, &mut seen, extract_urls(&html_to_text(page)));
    }
    if !fallback.is_empty() {
        append_unique_urls(&mut urls, &mut seen, extract_urls(fallback));
    }
    urls
}

fn append_unique_urls(urls: &mut Vec<String>, seen: &mut HashSet<String>, candidates: Vec<String>) {
    for url in candidates {
        if seen.insert(url.clone()) {
            urls.push(url);
        }
    }
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

fn html_to_text(html: &str) -> String {
    let mut text = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut chars = html.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '<' => {
                in_tag = true;
            }
            '>' => {
                in_tag = false;
            }
            '&' if !in_tag => {
                let mut entity = String::new();
                while let Some(&next) = chars.peek() {
                    entity.push(next);
                    chars.next();
                    if next == ';' || entity.len() > 10 {
                        break;
                    }
                }
                text.push(decode_html_entity(&entity));
            }
            _ if !in_tag => text.push(ch),
            _ => {}
        }
    }

    text
}

fn decode_html_entity(entity: &str) -> char {
    if let Some(value) = entity
        .strip_prefix("#x")
        .or_else(|| entity.strip_prefix("#X"))
        .and_then(|value| value.strip_suffix(';'))
        .and_then(|value| u32::from_str_radix(value, 16).ok())
        .and_then(char::from_u32)
    {
        return value;
    }
    if let Some(value) = entity
        .strip_prefix('#')
        .and_then(|value| value.strip_suffix(';'))
        .and_then(|value| value.parse::<u32>().ok())
        .and_then(char::from_u32)
    {
        return value;
    }
    match entity {
        "amp;" => '&',
        "lt;" => '<',
        "gt;" => '>',
        "quot;" => '"',
        "#39;" | "apos;" => '\'',
        _ => ' ',
    }
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

pub(super) fn infer_origin(headers: &HeaderMap, state: &ApiState) -> String {
    let host = infer_host(headers, state);
    if host.contains("://") {
        return host;
    }

    let proto = header_to_str(headers, "x-forwarded-proto")
        .map(str::to_string)
        .or_else(|| {
            header_to_str(headers, "forwarded")
                .and_then(|forwarded| parse_forwarded_param(forwarded, "proto"))
        })
        .unwrap_or_else(|| "http".to_string());

    format!("{proto}://{host}")
}
