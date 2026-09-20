use axum::response::IntoResponse;
use std::str::FromStr;

use crate::core::{FileId, PackageId};
use crate::tui::dashboard::DownloadDashboardState;

use super::ApiState;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ActionTarget {
    Package(PackageId),
    File(FileId),
}

pub(super) fn resolve_action_target(
    state: &ApiState,
    id: Option<&str>,
    name: Option<&str>,
) -> Result<ActionTarget, Box<axum::response::Response>> {
    let Some(selector) = id.or(name) else {
        return Err(Box::new(
            (
                axum::http::StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({"error": "missing id or name"})),
            )
                .into_response(),
        ));
    };

    let snapshot = snapshot_state(state)?;
    let package_matches: Vec<_> = snapshot
        .packages
        .iter()
        .filter(|package| {
            id.is_some_and(|id| package.id == id)
                || name.is_some_and(|name| package.display_name == name)
        })
        .collect();
    let file_matches: Vec<_> = snapshot
        .files
        .iter()
        .filter(|file| {
            id.is_some_and(|id| file.id == id) || name.is_some_and(|name| file.name == name)
        })
        .collect();

    match (package_matches.as_slice(), file_matches.as_slice()) {
        ([package], []) => PackageId::from_str(&package.id)
            .map(ActionTarget::Package)
            .map_err(|_| invalid_package_id_response()),
        ([], [file]) => Ok(ActionTarget::File(file.id.clone().into())),
        ([], []) => Err(Box::new(
            (
                axum::http::StatusCode::NOT_FOUND,
                axum::Json(serde_json::json!({
                    "error": format!("no package or file found for {selector}")
                })),
            )
                .into_response(),
        )),
        _ => Err(Box::new(
            (
                axum::http::StatusCode::CONFLICT,
                axum::Json(serde_json::json!({
                    "error": "ambiguous selector; use an id"
                })),
            )
                .into_response(),
        )),
    }
}

fn invalid_package_id_response() -> Box<axum::response::Response> {
    Box::new(
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({"error": "invalid package id in app state"})),
        )
            .into_response(),
    )
}

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

    let snapshot = snapshot_state(state)?;

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
