use super::transport::{
    DiscordRpcError, IpcStream, OP_CLOSE, OP_FRAME, OP_HANDSHAKE, OP_PING, OP_PONG, connect_ipc,
    read_frame, write_frame,
};
use serde_json::{Value, json};
use std::time::Duration;

pub(super) const RPC_TIMEOUT: Duration = Duration::from_secs(2);
const RESPONSE_FRAME_LIMIT: usize = 16;

pub(super) struct DiscordRpcClient {
    stream: IpcStream,
    nonce: u64,
}

impl DiscordRpcClient {
    pub(super) async fn connect(client_id: &str) -> Result<Self, DiscordRpcError> {
        bounded(async {
            let mut client = Self {
                stream: connect_ipc().await?,
                nonce: 0,
            };
            client.handshake(client_id).await?;
            Ok(client)
        })
        .await
    }

    async fn handshake(&mut self, client_id: &str) -> Result<(), DiscordRpcError> {
        write_frame(
            &mut self.stream,
            OP_HANDSHAKE,
            &json!({ "v": 1, "client_id": client_id }),
        )
        .await?;
        for _ in 0..RESPONSE_FRAME_LIMIT {
            if let Some(payload) = self.receive().await? {
                return if payload["evt"] == "READY" {
                    Ok(())
                } else {
                    Err(DiscordRpcError::Protocol)
                };
            }
        }
        Err(DiscordRpcError::Protocol)
    }

    pub(super) async fn set_activity(&mut self, activity: &Value) -> Result<(), DiscordRpcError> {
        bounded(self.set_activity_inner(activity)).await
    }

    async fn set_activity_inner(&mut self, activity: &Value) -> Result<(), DiscordRpcError> {
        self.nonce = self.nonce.checked_add(1).ok_or(DiscordRpcError::Protocol)?;
        let nonce = format!("axial-{}-{}", std::process::id(), self.nonce);
        write_frame(
            &mut self.stream,
            OP_FRAME,
            &json!({
                "cmd": "SET_ACTIVITY", "nonce": nonce,
                "args": { "pid": std::process::id(), "activity": activity },
            }),
        )
        .await?;
        for _ in 0..RESPONSE_FRAME_LIMIT {
            let Some(payload) = self.receive().await? else {
                continue;
            };
            if payload["nonce"].as_str() == Some(nonce.as_str()) {
                return if payload["evt"] == "ERROR" || payload["cmd"] == "ERROR" {
                    Err(DiscordRpcError::Protocol)
                } else {
                    Ok(())
                };
            }
        }
        Err(DiscordRpcError::Protocol)
    }

    async fn receive(&mut self) -> Result<Option<Value>, DiscordRpcError> {
        let (opcode, payload) = read_frame(&mut self.stream).await?;
        match opcode {
            OP_FRAME => Ok(Some(payload)),
            OP_PING => {
                write_frame(&mut self.stream, OP_PONG, &payload).await?;
                Ok(None)
            }
            _ => Err(DiscordRpcError::Protocol),
        }
    }

    pub(super) async fn clear_and_close(mut self) {
        let _ = self.set_activity(&Value::Null).await;
        let _ = bounded(write_frame(&mut self.stream, OP_CLOSE, &json!({}))).await;
        // Dropping the connection also invalidates any partially read command.
    }
}

async fn bounded<T>(
    future: impl std::future::Future<Output = Result<T, DiscordRpcError>>,
) -> Result<T, DiscordRpcError> {
    tokio::time::timeout(RPC_TIMEOUT, future)
        .await
        .map_err(|_| DiscordRpcError::Timeout)?
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn pair() -> (DiscordRpcClient, tokio::io::DuplexStream) {
        let (stream, server) = tokio::io::duplex(1024);
        (
            DiscordRpcClient {
                stream: Box::new(stream),
                nonce: 0,
            },
            server,
        )
    }

    #[tokio::test]
    async fn handshake_activity_ping_clear_and_close_follow_the_wire_contract() {
        let (mut client, mut server) = pair();
        let peer = tokio::spawn(async move {
            let (opcode, payload) = read_frame(&mut server).await.unwrap();
            assert_eq!(opcode, OP_HANDSHAKE);
            assert_eq!(
                payload,
                json!({ "v": 1, "client_id": "123456789012345678" })
            );
            write_frame(&mut server, OP_FRAME, &json!({ "evt": "READY" }))
                .await
                .unwrap();
            let (opcode, payload) = read_frame(&mut server).await.unwrap();
            assert_eq!(opcode, OP_FRAME);
            assert_eq!(payload["cmd"], "SET_ACTIVITY");
            assert_eq!(
                payload["args"]["activity"]["details"],
                "Minecraft is running"
            );
            write_frame(&mut server, OP_PING, &json!({ "ping": 1 }))
                .await
                .unwrap();
            assert_eq!(
                read_frame(&mut server).await.unwrap(),
                (OP_PONG, json!({ "ping": 1 }))
            );
            write_frame(&mut server, OP_FRAME, &json!({ "nonce": "unrelated" }))
                .await
                .unwrap();
            write_frame(&mut server, OP_FRAME, &json!({ "nonce": payload["nonce"] }))
                .await
                .unwrap();
            let (_, payload) = read_frame(&mut server).await.unwrap();
            assert!(payload["args"]["activity"].is_null());
            write_frame(&mut server, OP_FRAME, &json!({ "nonce": payload["nonce"] }))
                .await
                .unwrap();
            assert_eq!(read_frame(&mut server).await.unwrap().0, OP_CLOSE);
        });
        client.handshake("123456789012345678").await.unwrap();
        client
            .set_activity(&json!({ "details": "Minecraft is running" }))
            .await
            .unwrap();
        client.clear_and_close().await;
        peer.await.unwrap();
    }

    #[tokio::test]
    async fn rejected_activity_is_failure_without_echoing_peer_text() {
        let (mut client, mut server) = pair();
        let peer = tokio::spawn(async move {
            let (_, payload) = read_frame(&mut server).await.unwrap();
            write_frame(&mut server, OP_FRAME, &json!({ "evt": "ERROR", "nonce": payload["nonce"], "data": { "message": "private-server.example bearer secret" } })).await.unwrap();
        });
        let error = client.set_activity(&json!({})).await.unwrap_err();
        assert_eq!(error.to_string(), "Discord IPC protocol failed");
        peer.await.unwrap();
    }

    #[tokio::test]
    async fn partial_response_is_bounded_by_whole_command_deadline() {
        let (mut client, mut server) = pair();
        let command = async {
            assert!(matches!(
                client.set_activity(&json!({})).await,
                Err(DiscordRpcError::Timeout)
            ));
        };
        let peer = async {
            read_frame(&mut server).await.unwrap();
            server.write_all(&[1]).await.unwrap();
            let mut byte = [0];
            let _ = server.read(&mut byte).await;
        };
        tokio::select! {
            _ = command => {}
            _ = peer => panic!("peer unexpectedly ended"),
        }
    }
}
