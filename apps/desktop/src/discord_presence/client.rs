use super::transport::{
    DiscordRpcError, OP_CLOSE, OP_FRAME, OP_HANDSHAKE, OP_PING, OP_PONG, connect_ipc, read_frame,
    write_frame,
};
use serde_json::{Value, json};

const DISCORD_RPC_VERSION: u8 = 1;
const COMMAND_RESPONSE_READ_LIMIT: usize = 8;

pub(super) struct DiscordRpcClient {
    stream: super::transport::IpcStream,
    nonce: u64,
}

impl DiscordRpcClient {
    pub(super) fn connect(client_id: &str) -> Result<Self, DiscordRpcError> {
        let stream = connect_ipc()?;
        let mut client = Self { stream, nonce: 0 };
        client.handshake(client_id)?;
        Ok(client)
    }

    #[cfg(test)]
    fn from_stream(stream: super::transport::IpcStream) -> Self {
        Self { stream, nonce: 0 }
    }

    pub(super) fn set_activity(&mut self, activity: &Value) -> Result<(), DiscordRpcError> {
        self.set_activity_payload(activity)
    }

    pub(super) fn clear_activity(&mut self) -> Result<(), DiscordRpcError> {
        self.set_activity_payload(&Value::Null)
    }

    pub(super) fn close(&mut self) -> Result<(), DiscordRpcError> {
        self.send_frame(OP_CLOSE, &json!({}))
    }

    fn handshake(&mut self, client_id: &str) -> Result<(), DiscordRpcError> {
        self.send_frame(
            OP_HANDSHAKE,
            &json!({
                "v": DISCORD_RPC_VERSION,
                "client_id": client_id,
            }),
        )?;
        let response = self.recv_rpc_payload()?;
        if let Some(error) = rpc_error_message(&response) {
            return Err(DiscordRpcError::Protocol(error));
        }
        if response.get("evt").and_then(Value::as_str) == Some("READY") {
            Ok(())
        } else {
            Err(DiscordRpcError::Protocol(
                "Discord RPC handshake did not return READY".to_string(),
            ))
        }
    }

    fn set_activity_payload(&mut self, activity: &Value) -> Result<(), DiscordRpcError> {
        self.command(
            "SET_ACTIVITY",
            json!({
                "pid": std::process::id(),
                "activity": activity,
            }),
        )
        .map(|_| ())
    }

    fn command(&mut self, command: &str, args: Value) -> Result<Value, DiscordRpcError> {
        let nonce = self.next_nonce();
        self.send_frame(
            OP_FRAME,
            &json!({
                "cmd": command,
                "args": args,
                "nonce": nonce,
            }),
        )?;

        for _ in 0..COMMAND_RESPONSE_READ_LIMIT {
            let response = self.recv_rpc_payload()?;
            if let Some(error) = rpc_error_message(&response) {
                return Err(DiscordRpcError::Protocol(error));
            }
            if response.get("nonce").and_then(Value::as_str) == Some(nonce.as_str()) {
                return Ok(response);
            }
        }

        Err(DiscordRpcError::Protocol(format!(
            "Discord RPC did not acknowledge {command}"
        )))
    }

    fn send_frame(&mut self, opcode: u32, payload: &Value) -> Result<(), DiscordRpcError> {
        write_frame(&mut self.stream, opcode, payload)
    }

    fn recv_rpc_payload(&mut self) -> Result<Value, DiscordRpcError> {
        loop {
            let (opcode, payload) = read_frame(&mut self.stream)?;
            match opcode {
                OP_FRAME => return Ok(payload),
                OP_PING => self.send_frame(OP_PONG, &payload)?,
                OP_CLOSE => {
                    return Err(DiscordRpcError::Protocol(
                        "Discord RPC connection closed".to_string(),
                    ));
                }
                _ => {
                    return Err(DiscordRpcError::Protocol(format!(
                        "unexpected Discord RPC opcode {opcode}"
                    )));
                }
            }
        }
    }

    fn next_nonce(&mut self) -> String {
        self.nonce = self.nonce.saturating_add(1);
        format!("croopor-{}-{}", std::process::id(), self.nonce)
    }
}

fn rpc_error_message(payload: &Value) -> Option<String> {
    let is_error = payload.get("evt").and_then(Value::as_str) == Some("ERROR")
        || payload.get("cmd").and_then(Value::as_str) == Some("ERROR");
    if !is_error {
        return None;
    }

    let code = payload
        .pointer("/data/code")
        .and_then(Value::as_i64)
        .map(|value| value.to_string());
    let message = payload
        .pointer("/data/message")
        .and_then(Value::as_str)
        .unwrap_or("Discord RPC returned an error");

    Some(match code {
        Some(code) => format!("{message} ({code})"),
        None => message.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::super::transport::{IpcStream, read_frame, write_frame};
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[cfg(unix)]
    fn ipc_pair() -> (DiscordRpcClient, IpcStream) {
        let (client, server) =
            std::os::unix::net::UnixStream::pair().expect("unix pair should be available");
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("client read timeout");
        client
            .set_write_timeout(Some(Duration::from_secs(2)))
            .expect("client write timeout");
        server
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("server read timeout");
        server
            .set_write_timeout(Some(Duration::from_secs(2)))
            .expect("server write timeout");
        (
            DiscordRpcClient::from_stream(IpcStream::Unix(client)),
            IpcStream::Unix(server),
        )
    }

    #[cfg(unix)]
    fn assert_handshake(server: &mut IpcStream) {
        let (opcode, payload) = read_frame(server).expect("handshake frame");
        assert_eq!(opcode, OP_HANDSHAKE);
        assert_eq!(payload["v"], DISCORD_RPC_VERSION);
        assert_eq!(payload["client_id"], "123456789012345678");
        write_frame(server, OP_FRAME, &json!({ "evt": "READY" })).expect("ready frame");
    }

    #[cfg(unix)]
    #[test]
    fn handshake_accepts_ready_response() {
        let (mut client, mut server) = ipc_pair();
        let server = thread::spawn(move || assert_handshake(&mut server));

        client
            .handshake("123456789012345678")
            .expect("handshake should succeed");
        server.join().expect("server should join");
    }

    #[cfg(unix)]
    #[test]
    fn set_activity_and_clear_activity_use_set_activity_command() {
        let (mut client, mut server) = ipc_pair();
        let server = thread::spawn(move || {
            assert_handshake(&mut server);

            let (opcode, payload) = read_frame(&mut server).expect("activity frame");
            assert_eq!(opcode, OP_FRAME);
            assert_eq!(payload["cmd"], "SET_ACTIVITY");
            assert_eq!(
                payload["args"]["activity"]["details"],
                "Minecraft is running"
            );
            let nonce = payload["nonce"].as_str().expect("nonce");
            write_frame(&mut server, OP_FRAME, &json!({ "nonce": nonce })).expect("activity ack");

            let (opcode, payload) = read_frame(&mut server).expect("clear frame");
            assert_eq!(opcode, OP_FRAME);
            assert_eq!(payload["cmd"], "SET_ACTIVITY");
            assert!(payload["args"]["activity"].is_null());
            let nonce = payload["nonce"].as_str().expect("nonce");
            write_frame(&mut server, OP_FRAME, &json!({ "nonce": nonce })).expect("clear ack");

            let (opcode, _) = read_frame(&mut server).expect("close frame");
            assert_eq!(opcode, OP_CLOSE);
        });

        client
            .handshake("123456789012345678")
            .expect("handshake should succeed");
        client
            .set_activity(&json!({ "details": "Minecraft is running" }))
            .expect("activity should succeed");
        client.clear_activity().expect("clear should succeed");
        client.close().expect("close should succeed");
        server.join().expect("server should join");
    }

    #[cfg(unix)]
    #[test]
    fn command_error_response_is_returned_as_protocol_error() {
        let (mut client, mut server) = ipc_pair();
        let server = thread::spawn(move || {
            assert_handshake(&mut server);
            let (_, payload) = read_frame(&mut server).expect("activity frame");
            let nonce = payload["nonce"].as_str().expect("nonce");
            write_frame(
                &mut server,
                OP_FRAME,
                &json!({
                    "evt": "ERROR",
                    "nonce": nonce,
                    "data": {
                        "code": 4000,
                        "message": "bad activity",
                    },
                }),
            )
            .expect("error frame");
        });

        client
            .handshake("123456789012345678")
            .expect("handshake should succeed");
        let error = client
            .set_activity(&json!({ "details": "Minecraft is running" }))
            .expect_err("activity should fail");

        assert!(matches!(
            error,
            DiscordRpcError::Protocol(message) if message.contains("bad activity")
        ));
        server.join().expect("server should join");
    }
}
