use axum::{
    Json,
    extract::{
        FromRequest, FromRequestParts, OptionalFromRequest, Query, Request,
        rejection::{JsonRejection, QueryRejection},
    },
    http::{StatusCode, request::Parts},
    response::{IntoResponse, Response},
};
use serde::de::DeserializeOwned;

const INVALID_JSON_ERROR: &str = "Invalid JSON request.";
const INVALID_JSON_SYNTAX_ERROR: &str = "Invalid JSON syntax.";
const JSON_CONTENT_TYPE_ERROR: &str = "JSON content type is required.";
const JSON_TOO_LARGE_ERROR: &str = "JSON request is too large.";
const INVALID_QUERY_ERROR: &str = "Invalid query request.";

pub(super) struct ApiJson<T>(pub T);

impl<T, S> FromRequest<S> for ApiJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiExtractionRejection;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        <Json<T> as FromRequest<S>>::from_request(request, state)
            .await
            .map(|Json(value)| Self(value))
            .map_err(ApiExtractionRejection::from_json)
    }
}

impl<T, S> OptionalFromRequest<S> for ApiJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiExtractionRejection;

    async fn from_request(request: Request, state: &S) -> Result<Option<Self>, Self::Rejection> {
        <Json<T> as OptionalFromRequest<S>>::from_request(request, state)
            .await
            .map(|value| value.map(|Json(value)| Self(value)))
            .map_err(ApiExtractionRejection::from_json)
    }
}

pub(super) struct ApiQuery<T>(pub T);

impl<T, S> FromRequestParts<S> for ApiQuery<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiExtractionRejection;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        Query::<T>::from_request_parts(parts, state)
            .await
            .map(|Query(value)| Self(value))
            .map_err(ApiExtractionRejection::from_query)
    }
}

pub(super) struct ApiExtractionRejection {
    status: StatusCode,
    error: &'static str,
}

impl ApiExtractionRejection {
    fn from_json(rejection: JsonRejection) -> Self {
        let status = rejection.status();
        let error = match rejection {
            JsonRejection::JsonSyntaxError(_) => INVALID_JSON_SYNTAX_ERROR,
            JsonRejection::MissingJsonContentType(_) => JSON_CONTENT_TYPE_ERROR,
            _ if status == StatusCode::PAYLOAD_TOO_LARGE => JSON_TOO_LARGE_ERROR,
            _ => INVALID_JSON_ERROR,
        };
        Self { status, error }
    }

    fn from_query(rejection: QueryRejection) -> Self {
        Self {
            status: rejection.status(),
            error: INVALID_QUERY_ERROR,
        }
    }
}

impl IntoResponse for ApiExtractionRejection {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(serde_json::json!({ "error": self.error })),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        body::{Body, to_bytes},
        extract::State,
        http::{Method, Request, header},
        routing::{get, post},
    };
    use serde::Deserialize;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tower::ServiceExt;

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct JsonInput {
        value: usize,
    }

    #[derive(Deserialize)]
    struct QueryInput {
        value: usize,
    }

    async fn json_handler(
        State(effects): State<Arc<AtomicUsize>>,
        ApiJson(input): ApiJson<JsonInput>,
    ) -> StatusCode {
        effects.fetch_add(input.value, Ordering::SeqCst);
        StatusCode::NO_CONTENT
    }

    async fn optional_json_handler(
        State(effects): State<Arc<AtomicUsize>>,
        input: Option<ApiJson<JsonInput>>,
    ) -> StatusCode {
        effects.fetch_add(
            input.map_or(1, |ApiJson(input)| input.value),
            Ordering::SeqCst,
        );
        StatusCode::NO_CONTENT
    }

    async fn query_handler(
        State(effects): State<Arc<AtomicUsize>>,
        ApiQuery(input): ApiQuery<QueryInput>,
    ) -> StatusCode {
        effects.fetch_add(input.value, Ordering::SeqCst);
        StatusCode::NO_CONTENT
    }

    #[tokio::test]
    async fn p02_b03_contract_json_rejections_are_bounded_before_handler_execution() {
        let effects = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route("/json", post(json_handler))
            .with_state(effects.clone());
        let sensitive = "private-json-token";
        let oversized = format!(
            "{{\"value\":1,\"padding\":\"{}\"}}",
            "x".repeat(2 * 1024 * 1024)
        );
        for (content_type, body, status, error) in [
            (
                Some("application/json"),
                format!("{{\"value\":1}}{sensitive}"),
                StatusCode::BAD_REQUEST,
                INVALID_JSON_SYNTAX_ERROR,
            ),
            (
                Some("application/json"),
                format!("{{\"value\":1,\"unknown\":\"{sensitive}\"}}"),
                StatusCode::UNPROCESSABLE_ENTITY,
                INVALID_JSON_ERROR,
            ),
            (
                None,
                "{\"value\":1}".to_string(),
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                JSON_CONTENT_TYPE_ERROR,
            ),
            (
                Some("application/json"),
                oversized,
                StatusCode::PAYLOAD_TOO_LARGE,
                JSON_TOO_LARGE_ERROR,
            ),
        ] {
            let mut request = Request::builder().method(Method::POST).uri("/json");
            if let Some(content_type) = content_type {
                request = request.header(header::CONTENT_TYPE, content_type);
            }
            assert_rejection(
                app.clone(),
                request.body(Body::from(body)).unwrap(),
                status,
                error,
            )
            .await;
        }
        assert_eq!(effects.load(Ordering::SeqCst), 0);

        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/json")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{\"value\":2}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(effects.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn p02_b03_contract_cross_owner_query_and_optional_json_use_the_same_boundary() {
        let effects = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route("/query", get(query_handler))
            .route("/optional", post(optional_json_handler))
            .with_state(effects.clone());

        for uri in ["/query", "/query?value=private-query-token"] {
            assert_rejection(
                app.clone(),
                Request::builder().uri(uri).body(Body::empty()).unwrap(),
                StatusCode::BAD_REQUEST,
                INVALID_QUERY_ERROR,
            )
            .await;
        }
        assert_eq!(effects.load(Ordering::SeqCst), 0);

        let absent = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/optional")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(absent.status(), StatusCode::NO_CONTENT);
        assert_eq!(effects.load(Ordering::SeqCst), 1);

        assert_rejection(
            app,
            Request::builder()
                .method(Method::POST)
                .uri("/optional")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{\"value\":\"private-optional-token\"}"))
                .unwrap(),
            StatusCode::UNPROCESSABLE_ENTITY,
            INVALID_JSON_ERROR,
        )
        .await;
        assert_eq!(effects.load(Ordering::SeqCst), 1);
    }

    async fn assert_rejection(
        app: Router,
        request: Request<Body>,
        expected_status: StatusCode,
        expected_error: &'static str,
    ) {
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), expected_status);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
        let body = to_bytes(response.into_body(), 128).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            serde_json::json!({ "error": expected_error })
        );
        let rendered = String::from_utf8(body.to_vec()).unwrap();
        assert!(!rendered.contains("private"));
    }
}
