//! Image-pull and -list handlers (POL-32).
//!
//! Mirrors `/v1/images/pull` and `/v1/images` from `openapi/box.openapi.yaml`.
//! Both delegate to `state.runtime.images()` (the local backend) — the REST
//! server *is* a local runtime running behind HTTP, so the path that backs
//! `boxlite pull alpine:latest` over loopback is the same one that backs
//! `boxlite --profile remote pull alpine:latest` over the network.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use boxlite::runtime::types::ImageInfo;

use super::super::{AppState, error_from_boxlite, error_response};

/// `POST /v1/images/pull` request body — `PullImageRequest` in OpenAPI.
#[derive(Debug, Deserialize)]
pub(in crate::commands::serve) struct PullImageRequest {
    pub reference: String,
}

/// Wire-shape image metadata (the OpenAPI `ImageInfo` schema). Mirrors the
/// shape `boxes::BoxResponse` takes — a thin DTO layered over the in-process
/// `ImageInfo`, so the on-disk format and the wire format can drift
/// independently.
#[derive(Debug, Serialize)]
pub(in crate::commands::serve) struct ImageInfoResponse {
    pub reference: String,
    pub repository: String,
    pub tag: String,
    pub id: String,
    pub cached_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
}

impl From<&ImageInfo> for ImageInfoResponse {
    fn from(info: &ImageInfo) -> Self {
        Self {
            reference: info.reference.clone(),
            repository: info.repository.clone(),
            tag: info.tag.clone(),
            id: info.id.clone(),
            cached_at: info.cached_at.to_rfc3339(),
            size_bytes: info.size.map(|s| s.as_bytes()),
        }
    }
}

/// `GET /v1/images` response body — `ListImagesResponse` in OpenAPI.
#[derive(Debug, Serialize)]
pub(in crate::commands::serve) struct ListImagesResponse {
    pub images: Vec<ImageInfoResponse>,
}

pub(in crate::commands::serve) async fn pull_image(
    State(state): State<Arc<AppState>>,
    Json(req): Json<PullImageRequest>,
) -> Response {
    if req.reference.trim().is_empty() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "reference must not be empty".to_string(),
            "InvalidArgumentError",
            "invalid_argument",
        );
    }

    // Pull, then re-list to find the freshly-cached image's metadata. The
    // local `pull_image` returns an `ImageObject` tied to the host blob
    // store; converting that to the wire `ImageInfoResponse` over there
    // would require duplicating the manifest-digest → cached_at lookup
    // the list path already performs, so the small extra round-trip
    // through `list()` is the simplest correct way.
    //
    // The list is keyed on the *resolved* reference
    // (e.g. `docker.io/library/alpine:latest`), not the user's input
    // (`alpine`), so we correlate the just-pulled image by its
    // manifest digest — `ImageObject::manifest_digest()` is the same
    // `sha256:…` that `ImageInfo::id` carries — instead of by string
    // equality on the reference, which would 500 on every unqualified
    // pull (POL-32 hardening).
    let handle = match state.runtime.images() {
        Ok(h) => h,
        Err(e) => return error_from_boxlite(&e),
    };
    let pulled = match handle.pull(&req.reference).await {
        Ok(obj) => obj,
        Err(e) => return error_from_boxlite(&e),
    };
    let images = match handle.list().await {
        Ok(v) => v,
        Err(e) => return error_from_boxlite(&e),
    };
    match images.iter().find(|i| i.id == pulled.manifest_digest()) {
        Some(info) => Json(ImageInfoResponse::from(info)).into_response(),
        None => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "pull of {} succeeded (digest {}) but the cache listing did not include it",
                req.reference,
                pulled.manifest_digest(),
            ),
            "InternalError",
            "internal",
        ),
    }
}

pub(in crate::commands::serve) async fn list_images(
    State(state): State<Arc<AppState>>,
) -> Response {
    let handle = match state.runtime.images() {
        Ok(h) => h,
        Err(e) => return error_from_boxlite(&e),
    };
    match handle.list().await {
        Ok(images) => {
            let resp = ListImagesResponse {
                images: images.iter().map(ImageInfoResponse::from).collect(),
            };
            Json(resp).into_response()
        }
        Err(e) => error_from_boxlite(&e),
    }
}
