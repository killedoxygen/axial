//! Local HTTP fixtures only. No real account, keyring or provider mutations.

use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{mpsc, oneshot},
};

pub(super) struct Reply {
    pub status: u16,
    pub body: Vec<u8>,
    pub resume: Option<oneshot::Receiver<()>>,
}

impl Reply {
    pub fn json(value: impl serde::Serialize) -> Self {
        Self {
            status: 200,
            body: serde_json::to_vec(&value).unwrap(),
            resume: None,
        }
    }
}

pub(super) async fn server(
    replies: Vec<Reply>,
) -> (
    String,
    mpsc::UnboundedReceiver<String>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let (send, receive) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        for reply in replies {
            let (mut stream, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let mut request = Vec::new();
            let header_end;
            loop {
                let mut bytes = [0_u8; 4096];
                let count = stream.read(&mut bytes).await.unwrap();
                assert!(count > 0);
                request.extend_from_slice(&bytes[..count]);
                assert!(request.len() < 1024 * 1024);
                if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                    header_end = end + 4;
                    break;
                }
            }
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let line = headers.lines().next().unwrap().to_owned();
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            while request.len() < header_end + length {
                let mut bytes = [0_u8; 4096];
                let count = stream.read(&mut bytes).await.unwrap();
                assert!(count > 0);
                request.extend_from_slice(&bytes[..count]);
                assert!(request.len() < 1024 * 1024);
            }
            let _ = send.send(line);
            if let Some(resume) = reply.resume {
                resume.await.unwrap();
            }
            let headers = format!(
                "HTTP/1.1 {} Fixture\r\nContent-Length: {}\r\nConnection: close\r\nContent-Type: application/json\r\n\r\n",
                reply.status,
                reply.body.len()
            );
            stream.write_all(headers.as_bytes()).await.unwrap();
            stream.write_all(&reply.body).await.unwrap();
        }
    });
    (address, receive, task)
}

pub(super) fn png(red: u8) -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, 64, 64);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .unwrap()
            .write_image_data(&[red, 20, 30, 255].repeat(64 * 64))
            .unwrap();
    }
    bytes
}

#[tokio::test]
async fn texture_cache_keeps_exact_url_and_image_kind_identity() {
    use super::{delivery::TextureDelivery, lookup::ProfileLookup};
    let first = png(11);
    let second = png(22);
    let (base, mut requests, server) = server(vec![
        Reply {
            status: 200,
            body: first.clone(),
            resume: None,
        },
        Reply {
            status: 200,
            body: second.clone(),
            resume: None,
        },
    ])
    .await;
    let delivery = TextureDelivery::new(ProfileLookup::fixture(&base));
    let first_url = format!("{base}/texture/first");
    let second_url = format!("{base}/texture/second");
    assert_eq!(
        delivery.skin(&first_url).await.unwrap(),
        crate::media::normalize_skin_png(&first).unwrap().png_bytes
    );
    assert_eq!(
        delivery.skin(&second_url).await.unwrap(),
        crate::media::normalize_skin_png(&second).unwrap().png_bytes
    );
    assert_eq!(
        delivery.skin(&first_url).await.unwrap(),
        crate::media::normalize_skin_png(&first).unwrap().png_bytes
    );
    server.await.unwrap();
    assert!(requests.recv().await.unwrap().contains("/texture/first"));
    assert!(requests.recv().await.unwrap().contains("/texture/second"));
    assert!(requests.try_recv().is_err());
    // Skin and cape formats have separate cache identities: a skin cache hit
    // cannot cause an unvalidated skin image to be returned as a cape.
    assert!(delivery.cape(&first_url).await.is_err());
}

#[tokio::test]
async fn lookup_binds_session_uuid_and_rejects_external_texture_authority() {
    use super::{
        lookup::{ProfileLookup, ProfileMediaError},
        tests::{Reply, server},
    };
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    let profile_id = "12345678123442348234123456789abc";
    let property = STANDARD.encode(
        serde_json::to_vec(&serde_json::json!({"profileId": profile_id,
        "textures":{"SKIN":{"url":"http://127.0.0.1:1/private"}}}))
        .unwrap(),
    );
    let (base, _, server) = server(vec![Reply::json(serde_json::json!({"id":profile_id,"name":"PlayerOne"})),
        Reply::json(serde_json::json!({"id":profile_id,"properties":[{"name":"textures","value":property}]}))]).await;
    assert_eq!(
        ProfileLookup::fixture(&base)
            .lookup("PlayerOne")
            .await
            .unwrap_err(),
        ProfileMediaError::InvalidTexture
    );
    server.await.unwrap();
}
