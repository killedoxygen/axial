//! One profile composed through production startup for HTTP tests.

use crate::{DesktopServices, TestInputs, start_profile_for_test, transport};
use axial_app::instances::{
    create::{CreateInstanceRequest, CreateTarget},
    model::InstanceId,
};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use std::{path::PathBuf, time::Duration};

pub(crate) struct Profile {
    pub(crate) services: DesktopServices,
    pub(crate) api: Api,
    pub(crate) temporary: tempfile::TempDir,
}

impl Profile {
    pub(crate) async fn start() -> Self {
        Self::start_with(TestInputs::default()).await
    }

    pub(crate) async fn start_with(inputs: TestInputs) -> Self {
        let temporary = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let services = start_profile_for_test(temporary.path().join("profile"), inputs)
            .await
            .unwrap();
        let api = Api::new(&services);
        Self {
            services,
            api,
            temporary,
        }
    }

    /// A registered Fabric instance and its game directory.
    pub(crate) async fn create_instance(&self) -> (InstanceId, PathBuf) {
        let instances = &self.services.instances;
        let target = CreateTarget::loader_for_tests(
            axial_minecraft::LoaderComponentId::Fabric,
            "1.20.1",
            "0.16.9",
        )
        .unwrap();
        let created = instances
            .create(
                CreateInstanceRequest {
                    name: "Fixture".into(),
                    selection_id: target.selection_id().to_owned(),
                    ..Default::default()
                },
                target,
                instances.creation_admission_for_tests().await.unwrap(),
            )
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        let admission = instances.directories().admit(&created.id).unwrap();
        let game = self
            .services
            .library
            .admit()
            .unwrap()
            .read_projection()
            .unwrap()
            .join("instances")
            .join(&admission.record().directory_name);
        admission.validate_current().unwrap();
        (created.id, game)
    }

    pub(crate) async fn close(self) {
        self.services.server.shutdown().await.unwrap();
        assert!(self.services.server.is_shutdown_settled());
    }
}

pub(crate) struct Api {
    pub(crate) client: reqwest::Client,
    pub(crate) base: String,
    pub(crate) capability: String,
}

impl Api {
    pub(crate) fn new(services: &DesktopServices) -> Self {
        let bootstrap = services.server.bootstrap();
        Self {
            client: reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(45))
                .build()
                .unwrap(),
            base: bootstrap.base_url,
            capability: bootstrap.capability,
        }
    }

    pub(crate) fn builder(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.base))
            .header(transport::CAPABILITY_HEADER, &self.capability)
    }

    /// The status and JSON body of a request with an optional JSON body.
    pub(crate) async fn response(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut request = self.builder(method, path);
        if let Some(body) = body {
            request = request.json(&body);
        }
        Self::json(request).await
    }

    pub(crate) async fn json(request: reqwest::RequestBuilder) -> (StatusCode, Value) {
        let response = request.send().await.unwrap();
        let status = response.status();
        let body = response.bytes().await.unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    pub(crate) async fn read(&self, path: &str) -> (StatusCode, Value) {
        self.response(Method::GET, path, None).await
    }

    /// The JSON body of a request that must succeed.
    pub(crate) async fn request(&self, method: Method, path: &str, body: Option<Value>) -> Value {
        let kill = method == Method::POST && path.ends_with("/kill");
        let mut request = self.builder(method, path);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap_or_else(|error| {
            assert!(
                !kill,
                "Kill request failed before response headers; Stop acceptance is unknown"
            );
            panic!("{path}: {error}")
        });
        let status = response.status();
        let body = response.text().await.unwrap();
        assert!(status.is_success(), "{path}: {status}: {body}");
        serde_json::from_str(&body).unwrap()
    }

    pub(crate) async fn get(&self, path: &str) -> Value {
        self.request(Method::GET, path, None).await
    }

    pub(crate) async fn post(&self, path: &str, body: Value) -> Value {
        self.request(Method::POST, path, Some(body)).await
    }

    pub(crate) async fn put(&self, path: &str, body: Value) -> Value {
        self.request(Method::PUT, path, Some(body)).await
    }

    pub(crate) async fn events(&self, path: &str) -> Events {
        let ticket = self
            .post(
                "/api/v1/transport/tickets",
                json!({"audience":"stream","target":path}),
            )
            .await;
        let response = self
            .client
            .get(format!(
                "{}{path}?axial_ticket={}",
                self.base,
                ticket["ticket"].as_str().unwrap()
            ))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        assert!(
            response.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/event-stream")
        );
        Events {
            response,
            buffered: Vec::new(),
        }
    }
}

pub(crate) struct Events {
    response: reqwest::Response,
    buffered: Vec<u8>,
}

impl Events {
    pub(crate) async fn next(&mut self) -> (String, Value) {
        loop {
            if let Some(end) = self.buffered.windows(2).position(|bytes| bytes == b"\n\n") {
                let frame: Vec<_> = self.buffered.drain(..end + 2).collect();
                let frame = String::from_utf8(frame).unwrap();
                let event = frame
                    .lines()
                    .find_map(|line| line.strip_prefix("event:"))
                    .unwrap_or("message")
                    .trim();
                if let Some(data) = frame.lines().find_map(|line| line.strip_prefix("data:")) {
                    return (event.to_owned(), serde_json::from_str(data.trim()).unwrap());
                }
                continue;
            }
            self.buffered.extend_from_slice(
                &self
                    .response
                    .chunk()
                    .await
                    .unwrap()
                    .expect("stream ended before terminal state"),
            );
            assert!(self.buffered.len() <= 1 << 20, "bounded fixture stream");
        }
    }
}
