use axum::response::IntoResponse;
use std::str::FromStr;

use crate::core::{FileId, PackageId};
use crate::tui::dashboard::DownloadDashboardState;

use super::ApiState;

fn snapshot_state(
    state: &ApiState,
) -> Result<DownloadDashboardState, Box<axum::response::Response>> {
    let Some(shared) = state.shared.as_ref() else {
        return Err(Box::new(
            (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(serde_json::json!({"error": "interactive state not enabled"})),
            )
                .into_response(),
        ));
    };

    crate::tui::dashboard::dashboard_state_from_postcard(shared.state_rx.borrow().as_ref()).map_err(
        |_| {
            Box::new(
                (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(serde_json::json!({"error": "invalid app state"})),
                )
                    .into_response(),
            )
        },
    )
}

pub(super) fn resolve_package_id(
    state: &ApiState,
    id: Option<&str>,
    name: Option<&str>,
) -> Result<Option<PackageId>, Box<axum::response::Response>> {
    let Some(selector) = id.or(name) else {
        return Ok(None);
    };

    let Ok(snapshot) = snapshot_state(state) else {
        return Ok(None);
    };

    let matches: Vec<_> = snapshot
        .packages
        .into_iter()
        .filter(|package| package.id == selector || package.display_name == selector)
        .collect();
    match matches.as_slice() {
        [] => Ok(None),
        [package] => PackageId::from_str(&package.id).map(Some).map_err(|_| {
            Box::new(
                (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(serde_json::json!({"error": "invalid package id in app state"})),
                )
                    .into_response(),
            )
        }),
        _ => Err(Box::new(
            (
                axum::http::StatusCode::CONFLICT,
                axum::Json(serde_json::json!({"error": "ambiguous package name; use id"})),
            )
                .into_response(),
        )),
    }
}

pub(super) fn resolve_file_id(
    state: &ApiState,
    id: Option<String>,
    name: Option<String>,
) -> Result<FileId, Box<axum::response::Response>> {
    if let Some(id) = id {
        return Ok(id.into());
    }

    let Some(name) = name else {
        return Err(Box::new(
            (
                axum::http::StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({"error": "missing id or name"})),
            )
                .into_response(),
        ));
    };

    let snapshot = snapshot_state(state)?;

    let matches: Vec<_> = snapshot
        .files
        .into_iter()
        .filter(|file| file.name == name)
        .collect();
    match matches.as_slice() {
        [] => Err(Box::new(
            (
                axum::http::StatusCode::NOT_FOUND,
                axum::Json(serde_json::json!({"error": "file not found"})),
            )
                .into_response(),
        )),
        [file] => Ok(file.id.clone().into()),
        _ => Err(Box::new(
            (
                axum::http::StatusCode::CONFLICT,
                axum::Json(serde_json::json!({"error": "ambiguous file name; use id"})),
            )
                .into_response(),
        )),
    }
}
