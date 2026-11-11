//! Bounded local Discord IPC. No sockets or named pipes are created here, and
//! errors retain neither local paths nor peer-authored text.
use serde_json::{Value, json};
use std::{fmt, future::Future, path::PathBuf, time::Duration};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub(super) const OP_HANDSHAKE: u32 = 0;
pub(super) const OP_FRAME: u32 = 1;
pub(super) const OP_CLOSE: u32 = 2;
const OP_PING: u32 = 3;
const OP_PONG: u32 = 4;
const MAX_FRAME_BYTES: usize = 64 * 1024;
const RESPONSE_FRAME_LIMIT: usize = 16;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(2);

#[cfg(unix)]
type Stream = tokio::net::UnixStream;
#[cfg(windows)]
type Stream = tokio::net::windows::named_pipe::NamedPipeClient;

#[derive(Debug)]
pub enum Error {
    Absent,
    Io,
    Json,
    Protocol,
    Timeout,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Absent => "Discord IPC is unavailable",
            Self::Io => "Discord IPC I/O failed",
            Self::Json => "Discord IPC returned invalid JSON",
            Self::Protocol => "Discord IPC protocol failed",
            Self::Timeout => "Discord IPC timed out",
        })
    }
}

impl From<std::io::Error> for Error {
    fn from(_: std::io::Error) -> Self {
        Self::Io
    }
}

impl From<serde_json::Error> for Error {
    fn from(_: serde_json::Error) -> Self {
        Self::Json
    }
}

pub struct Client {
    stream: Stream,
    nonce: u64,
}

impl Client {
    pub async fn connect(client_id: &str, paths: &[PathBuf]) -> Result<Self, Error> {
        bounded(async {
            let mut client = Self {
                stream: open(paths).await?,
                nonce: 0,
            };
            write_frame(
                &mut client.stream,
                OP_HANDSHAKE,
                &json!({ "v": 1, "client_id": client_id }),
            )
            .await?;
            match client.next_frame(|_| true).await?["evt"] == "READY" {
                true => Ok(client),
                false => Err(Error::Protocol),
            }
        })
        .await
    }

    pub async fn set_activity(&mut self, activity: &Value) -> Result<(), Error> {
        bounded(async {
            self.nonce = self.nonce.checked_add(1).ok_or(Error::Protocol)?;
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
            let reply = self
                .next_frame(|payload| payload["nonce"].as_str() == Some(&nonce))
                .await?;
            match reply["evt"] == "ERROR" || reply["cmd"] == "ERROR" {
                true => Err(Error::Protocol),
                false => Ok(()),
            }
        })
        .await
    }

    /// Answers pings while waiting for the first frame `matches` accepts.
    async fn next_frame(&mut self, matches: impl Fn(&Value) -> bool) -> Result<Value, Error> {
        for _ in 0..RESPONSE_FRAME_LIMIT {
            match read_frame(&mut self.stream).await? {
                (OP_FRAME, payload) if matches(&payload) => return Ok(payload),
                (OP_FRAME, _) => {}
                (OP_PING, payload) => write_frame(&mut self.stream, OP_PONG, &payload).await?,
                _ => return Err(Error::Protocol),
            }
        }
        Err(Error::Protocol)
    }

    pub async fn clear_and_close(mut self) {
        let _ = self.set_activity(&Value::Null).await;
        let _ = bounded(write_frame(&mut self.stream, OP_CLOSE, &json!({}))).await;
        // Dropping the connection also invalidates any partially read command.
    }
}

/// The whole command shares one deadline, so a trickling peer cannot extend it.
async fn bounded<T>(command: impl Future<Output = Result<T, Error>>) -> Result<T, Error> {
    tokio::time::timeout(COMMAND_TIMEOUT, command)
        .await
        .map_err(|_| Error::Timeout)?
}

async fn open(paths: &[PathBuf]) -> Result<Stream, Error> {
    for path in paths {
        #[cfg(unix)]
        if let Ok(stream) = Stream::connect(path).await {
            return Ok(stream);
        }
        #[cfg(windows)]
        if let Ok(stream) = tokio::net::windows::named_pipe::ClientOptions::new().open(path) {
            return Ok(stream);
        }
    }
    Err(Error::Absent)
}

pub(super) async fn write_frame(
    writer: &mut (impl AsyncWrite + Unpin),
    opcode: u32,
    payload: &Value,
) -> Result<(), Error> {
    let bytes = serde_json::to_vec(payload)?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(Error::Protocol);
    }
    writer.write_all(&opcode.to_le_bytes()).await?;
    writer
        .write_all(&(bytes.len() as u32).to_le_bytes())
        .await?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

pub(super) async fn read_frame(
    reader: &mut (impl AsyncRead + Unpin),
) -> Result<(u32, Value), Error> {
    let mut header = [0; 8];
    reader.read_exact(&mut header).await?;
    let opcode = u32::from_le_bytes(header[..4].try_into().expect("four-byte opcode"));
    let length = u32::from_le_bytes(header[4..].try_into().expect("four-byte length")) as usize;
    if length > MAX_FRAME_BYTES {
        return Err(Error::Protocol);
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).await?;
    Ok((opcode, serde_json::from_slice(&bytes)?))
}

#[cfg(unix)]
pub fn candidates() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = ["XDG_RUNTIME_DIR", "TMPDIR", "TMP", "TEMP"]
        .iter()
        .filter_map(|key| std::env::var_os(key))
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .collect();
    roots.push(PathBuf::from("/tmp"));
    candidates_from_roots(&roots)
}

#[cfg(unix)]
fn candidates_from_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for root in roots {
        for suffix in [
            "",
            "app/com.discordapp.Discord",
            "app/com.discordapp.DiscordCanary",
            "app/com.discordapp.DiscordPTB",
            "app/dev.vencord.Vesktop",
            ".flatpak/com.discordapp.Discord/xdg-run",
            ".flatpak/dev.vencord.Vesktop/xdg-run",
            "snap.discord",
            "snap.discord-canary",
        ] {
            for index in 0..10 {
                let path = root.join(suffix).join(format!("discord-ipc-{index}"));
                if !paths.contains(&path) {
                    paths.push(path);
                }
            }
        }
    }
    paths
}

#[cfg(windows)]
pub fn candidates() -> Vec<PathBuf> {
    (0..10)
        .map(|index| PathBuf::from(format!(r"\\?\pipe\discord-ipc-{index}")))
        .collect()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn pair() -> (Client, Stream) {
        let (stream, server) = Stream::pair().unwrap();
        (Client { stream, nonce: 0 }, server)
    }

    #[tokio::test]
    async fn frames_are_bounded_little_endian_json() {
        let payload = json!({ "nonce": "n1" });
        let mut bytes = Vec::new();
        write_frame(&mut bytes, OP_FRAME, &payload).await.unwrap();
        assert_eq!(&bytes[..4], &1_u32.to_le_bytes());
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize,
            bytes.len() - 8
        );
        assert_eq!(
            read_frame(&mut bytes.as_slice()).await.unwrap(),
            (OP_FRAME, payload)
        );
        let mut oversized = OP_FRAME.to_le_bytes().to_vec();
        oversized.extend_from_slice(&(MAX_FRAME_BYTES as u32 + 1).to_le_bytes());
        assert!(matches!(
            read_frame(&mut oversized.as_slice()).await,
            Err(Error::Protocol)
        ));
    }

    #[tokio::test]
    async fn activity_ping_clear_and_close_follow_the_wire_contract() {
        let (mut client, mut server) = pair();
        let peer = tokio::spawn(async move {
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
        client
            .set_activity(&json!({ "details": "Minecraft is running" }))
            .await
            .unwrap();
        client.clear_and_close().await;
        peer.await.unwrap();
    }

    #[tokio::test]
    async fn failures_never_echo_peer_text_or_local_paths() {
        let (mut client, mut server) = pair();
        let peer = tokio::spawn(async move {
            let (_, payload) = read_frame(&mut server).await.unwrap();
            write_frame(&mut server, OP_FRAME, &json!({ "evt": "ERROR", "nonce": payload["nonce"], "data": { "message": "private-server.example bearer secret" } })).await.unwrap();
        });
        let error = client.set_activity(&json!({})).await.unwrap_err();
        assert_eq!(error.to_string(), "Discord IPC protocol failed");
        peer.await.unwrap();
        let error = Error::from(std::io::Error::other("/Users/private-person/token"));
        assert_eq!(error.to_string(), "Discord IPC I/O failed");
    }

    #[tokio::test]
    async fn partial_response_is_bounded_by_whole_command_deadline() {
        let (mut client, mut server) = pair();
        let command = async {
            assert!(matches!(
                client.set_activity(&json!({})).await,
                Err(Error::Timeout)
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

    #[test]
    fn paths_cover_packaged_clients_without_duplicate_candidates() {
        let paths = candidates_from_roots(&["/tmp".into(), "/tmp".into()]);
        assert_eq!(paths, candidates_from_roots(&["/tmp".into()]));
        for path in [
            "/tmp/discord-ipc-9",
            "/tmp/app/com.discordapp.Discord/discord-ipc-0",
            "/tmp/.flatpak/dev.vencord.Vesktop/xdg-run/discord-ipc-0",
        ] {
            assert!(paths.contains(&PathBuf::from(path)));
        }
    }
}
