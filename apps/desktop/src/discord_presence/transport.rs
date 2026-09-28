//! Bounded local Discord IPC. No sockets or named pipes are created here.
use serde_json::Value;
use std::fmt;
use std::path::PathBuf;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub(super) const OP_HANDSHAKE: u32 = 0;
pub(super) const OP_FRAME: u32 = 1;
pub(super) const OP_CLOSE: u32 = 2;
pub(super) const OP_PING: u32 = 3;
pub(super) const OP_PONG: u32 = 4;
const MAX_FRAME_BYTES: usize = 64 * 1024;

pub(super) trait AsyncIpc: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> AsyncIpc for T {}
pub(super) type IpcStream = Box<dyn AsyncIpc>;

#[derive(Debug)]
pub(super) enum DiscordRpcError {
    Absent,
    Io,
    Json,
    Protocol,
    Timeout,
}

impl fmt::Display for DiscordRpcError {
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

// Errors intentionally retain neither local paths nor peer-authored text.
impl From<std::io::Error> for DiscordRpcError {
    fn from(_: std::io::Error) -> Self {
        Self::Io
    }
}
impl From<serde_json::Error> for DiscordRpcError {
    fn from(_: serde_json::Error) -> Self {
        Self::Json
    }
}

pub(super) async fn connect_ipc() -> Result<IpcStream, DiscordRpcError> {
    for path in ipc_path_candidates() {
        #[cfg(unix)]
        if let Ok(stream) = tokio::net::UnixStream::connect(&path).await {
            return Ok(Box::new(stream));
        }
        #[cfg(windows)]
        if let Ok(stream) = tokio::net::windows::named_pipe::ClientOptions::new().open(&path) {
            return Ok(Box::new(stream));
        }
    }
    Err(DiscordRpcError::Absent)
}

pub(super) async fn write_frame(
    writer: &mut (impl AsyncWrite + Unpin + ?Sized),
    opcode: u32,
    payload: &Value,
) -> Result<(), DiscordRpcError> {
    let bytes = serde_json::to_vec(payload)?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(DiscordRpcError::Protocol);
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
    reader: &mut (impl AsyncRead + Unpin + ?Sized),
) -> Result<(u32, Value), DiscordRpcError> {
    let mut header = [0; 8];
    reader.read_exact(&mut header).await?;
    let opcode = u32::from_le_bytes(header[..4].try_into().expect("four-byte header"));
    let length = u32::from_le_bytes(header[4..].try_into().expect("four-byte length")) as usize;
    if length > MAX_FRAME_BYTES {
        return Err(DiscordRpcError::Protocol);
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).await?;
    Ok((opcode, serde_json::from_slice(&bytes)?))
}

#[cfg(unix)]
fn ipc_path_candidates() -> Vec<PathBuf> {
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
    let mut seen = std::collections::HashSet::new();
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
                if seen.insert(path.clone()) {
                    paths.push(path);
                }
            }
        }
    }
    paths
}

#[cfg(windows)]
fn ipc_path_candidates() -> Vec<PathBuf> {
    (0..10)
        .map(|index| PathBuf::from(format!(r"\\?\pipe\discord-ipc-{index}")))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn frames_preserve_little_endian_header_and_json() {
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
    }

    #[tokio::test]
    async fn oversized_frames_fail_before_reading_payload() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&OP_FRAME.to_le_bytes());
        bytes.extend_from_slice(&(MAX_FRAME_BYTES as u32 + 1).to_le_bytes());
        assert!(matches!(
            read_frame(&mut bytes.as_slice()).await,
            Err(DiscordRpcError::Protocol)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn paths_cover_packaged_clients_without_duplicate_candidates() {
        let paths = candidates_from_roots(&["/tmp".into(), "/tmp".into()]);
        assert_eq!(paths.len(), 90);
        for path in [
            "/tmp/discord-ipc-9",
            "/tmp/app/com.discordapp.Discord/discord-ipc-0",
            "/tmp/.flatpak/dev.vencord.Vesktop/xdg-run/discord-ipc-0",
        ] {
            assert!(paths.contains(&PathBuf::from(path)));
        }
    }

    #[test]
    fn error_text_never_echoes_a_local_path_or_peer_message() {
        let error = DiscordRpcError::from(std::io::Error::other("/Users/private-person/token"));
        assert_eq!(error.to_string(), "Discord IPC I/O failed");
    }
}
