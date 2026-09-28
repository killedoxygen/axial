//! Read-only standalone distribution of the build-verified frontend generation.

use axum::{
    Router,
    body::Body,
    http::{StatusCode, Uri, header},
    response::{IntoResponse, Response},
    routing::get,
};
use include_dir::{Dir, include_dir};

static EMBEDDED_FRONTEND: Dir<'_> = include_dir!("$OUT_DIR/embedded-frontend");

/// Add only the non-API fallback after the authenticated API routes are complete.
pub(crate) fn router(api: Router) -> Router {
    api.fallback(get(serve_embedded_frontend))
}

async fn serve_embedded_frontend(uri: Uri) -> Response {
    // The authenticated API router reserves this namespace. Keep the boundary
    // explicit here too so a future composition change cannot return HTML for API calls.
    if uri.path() == "/api" || uri.path().starts_with("/api/") {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({ "error": "API route was not found" })),
        )
            .into_response();
    }
    let path = uri.path().trim_start_matches('/');
    let file = EMBEDDED_FRONTEND
        .get_file(path)
        .or_else(|| EMBEDDED_FRONTEND.get_file("index.html"));
    match file {
        Some(file) => Response::builder()
            .status(StatusCode::OK)
            .header(
                header::CONTENT_TYPE,
                mime_guess::from_path(file.path())
                    .first_or_octet_stream()
                    .essence_str(),
            )
            .header(header::CACHE_CONTROL, "no-cache")
            .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
            .body(Body::from(file.contents()))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{CAPABILITY_HEADER, LocalApiAuthority, protected_router};
    use axum::http::{Method, Request};
    use http_body_util::BodyExt;
    use std::{fs, path::Path};
    use tower::ServiceExt;

    fn collect_files(directory: &Dir<'_>, paths: &mut Vec<String>) {
        paths.extend(
            directory
                .files()
                .map(|file| file.path().to_string_lossy().into_owned()),
        );
        for child in directory.dirs() {
            collect_files(child, paths);
        }
    }

    #[test]
    fn embedded_frontend_is_byte_exact_and_manifest_reachable() {
        let manifest_file = EMBEDDED_FRONTEND.get_file("generation.json").unwrap();
        let manifest: serde_json::Value = serde_json::from_slice(manifest_file.contents()).unwrap();
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../frontend/dist");
        let mut expected = vec!["generation.json".to_owned()];
        for record in manifest["files"].as_array().unwrap() {
            let relative = record["path"].as_str().unwrap();
            let file = EMBEDDED_FRONTEND.get_file(relative).unwrap();
            assert_eq!(
                file.contents(),
                fs::read(source.join(relative)).unwrap(),
                "{relative}"
            );
            expected.push(relative.to_owned());
        }
        assert_eq!(
            manifest_file.contents(),
            fs::read(source.join("generation.json")).unwrap()
        );
        let mut actual = Vec::new();
        collect_files(&EMBEDDED_FRONTEND, &mut actual);
        actual.sort();
        expected.sort();
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn static_mime_and_spa_fallback_do_not_change_the_api_authority_boundary() {
        let authority = LocalApiAuthority::new("127.0.0.1:43430".parse().unwrap(), None).unwrap();
        let app = router(protected_router(
            Router::new().route("/api/v1/example", get(|| async { "real API handler" })),
            authority.clone(),
        ));
        for (path, mime) in [
            ("/index.html", "text/html"),
            ("/app.js", "text/javascript"),
            ("/app.css", "text/css"),
            ("/fonts/GeistMono-Variable.woff2", "font/woff2"),
            ("/sounds/snd01/audioSprite.mp3", "audio/mpeg"),
            ("/sounds/snd01/audioSprite.json", "application/json"),
            ("/generation.json", "application/json"),
        ] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            assert_eq!(response.headers()[header::CONTENT_TYPE], mime, "{path}");
            assert_eq!(
                response.into_body().collect().await.unwrap().to_bytes(),
                EMBEDDED_FRONTEND.get_file(&path[1..]).unwrap().contents()
            );
        }
        for method in [Method::GET, Method::HEAD, Method::POST] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method.clone())
                        .uri("/instances/example/settings")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            if method == Method::POST {
                assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
            } else {
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(response.headers()[header::CONTENT_TYPE], "text/html");
                let body = response.into_body().collect().await.unwrap().to_bytes();
                if method == Method::HEAD {
                    assert!(body.is_empty());
                } else {
                    assert_eq!(
                        body,
                        EMBEDDED_FRONTEND.get_file("index.html").unwrap().contents()
                    );
                }
            }
        }
        for path in ["/api", "/api/", "/api/v1/missing", "/api/v2/missing"] {
            for authenticated in [false, true] {
                let mut request = Request::builder().uri(path);
                if authenticated {
                    request = request.header(CAPABILITY_HEADER, authority.capability_for_test());
                }
                let response = app
                    .clone()
                    .oneshot(request.body(Body::empty()).unwrap())
                    .await
                    .unwrap();
                assert_eq!(
                    response.status(),
                    if authenticated {
                        StatusCode::NOT_FOUND
                    } else {
                        StatusCode::UNAUTHORIZED
                    }
                );
                assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
                let body = response.into_body().collect().await.unwrap().to_bytes();
                let error: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(
                    error["error"],
                    if authenticated {
                        "API route was not found"
                    } else {
                        "API capability is required"
                    }
                );
            }
        }
        let denied = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn real_browser_server_serves_the_document_and_origin_scoped_bootstrap() {
        let temp = tempfile::tempdir().unwrap();
        let services = crate::start_in_profile(temp.path().join("browser"), None)
            .await
            .unwrap();
        let bootstrap = services.server.bootstrap();
        let client = reqwest::Client::new();
        let response = client.get(&bootstrap.base_url).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "text/html");
        assert_eq!(
            response.bytes().await.unwrap(),
            EMBEDDED_FRONTEND.get_file("index.html").unwrap().contents()
        );
        let response = client
            .post(format!("{}/api/v1/transport/bootstrap", bootstrap.base_url))
            .header(header::ORIGIN, &bootstrap.base_url)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let value: serde_json::Value = response.json().await.unwrap();
        assert_eq!(value["capability"], bootstrap.capability);
        let response = client
            .get(format!("{}/api/v1/config", bootstrap.base_url))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        services.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn native_mode_has_no_http_frontend_even_when_the_feature_is_unified() {
        let temp = tempfile::tempdir().unwrap();
        let services = crate::start_profile(temp.path().join("native"), None, true, None)
            .await
            .unwrap();
        let bootstrap = services.server.bootstrap();
        let response = reqwest::Client::new()
            .get(format!("{}/index.html", bootstrap.base_url))
            .header(CAPABILITY_HEADER, bootstrap.capability)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        services.server.shutdown().await.unwrap();
    }
}
