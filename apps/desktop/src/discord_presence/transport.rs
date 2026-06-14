use serde_json::Value;
#[cfg(unix)]
use std::collections::HashSet;
use std::fmt;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::time::Duration;

pub(super) const OP_HANDSHAKE: u32 = 0;
pub(super) const OP_FRAME: u32 = 1;
pub(super) const OP_CLOSE: u32 = 2;
pub(super) const OP_PING: u32 = 3;
pub(super) const OP_PONG: u32 = 4;

const IPC_SEARCH_LIMIT: usize = 10;
const MAX_FRAME_BYTES: usize = 64 * 1024;
#[cfg(unix)]
const IPC_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) fn connect_ipc() -> Result<IpcStream, DiscordRpcError> {
    let mut last_error: Option<std::io::Error> = None;
    for path in ipc_path_candidates() {
        match IpcStream::connect(&path) {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = Some(error),
        }
    }

    Err(DiscordRpcError::Connect(
        last_error
            .map(|error| error.to_string())
            .unwrap_or_else(|| "Discord IPC socket was not found".to_string()),
    ))
}

pub(super) fn write_frame(
    writer: &mut impl Write,
    opcode: u32,
    payload: &Value,
) -> Result<(), DiscordRpcError> {
    let data = serde_json::to_vec(payload)?;
    if data.len() > MAX_FRAME_BYTES {
        return Err(DiscordRpcError::Protocol(
            "Discord RPC frame payload is too large".to_string(),
        ));
    }
    writer.write_all(&opcode.to_le_bytes())?;
    writer.write_all(&(data.len() as u32).to_le_bytes())?;
    writer.write_all(&data)?;
    writer.flush()?;
    Ok(())
}

pub(super) fn read_frame(reader: &mut impl Read) -> Result<(u32, Value), DiscordRpcError> {
    let mut header = [0_u8; 8];
    reader.read_exact(&mut header)?;
    let opcode = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
    let len = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(DiscordRpcError::Protocol(
            "Discord RPC frame payload is too large".to_string(),
        ));
    }
    let mut payload = vec![0_u8; len];
    reader.read_exact(&mut payload)?;
    Ok((opcode, serde_json::from_slice(&payload)?))
}

#[cfg(unix)]
fn ipc_path_candidates() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for key in ["XDG_RUNTIME_DIR", "TMPDIR", "TMP", "TEMP"] {
        if let Some(value) = std::env::var_os(key)
            && !value.is_empty()
        {
            roots.push(PathBuf::from(value));
        }
    }
    roots.push(PathBuf::from("/tmp"));
    ipc_path_candidates_from_roots(&roots)
}

#[cfg(unix)]
fn ipc_path_candidates_from_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    let suffixes = [
        "",
        "app/com.discordapp.Discord",
        "app/com.discordapp.DiscordCanary",
        "app/com.discordapp.DiscordPTB",
        "app/dev.vencord.Vesktop",
        ".flatpak/com.discordapp.Discord/xdg-run",
        ".flatpak/dev.vencord.Vesktop/xdg-run",
        "snap.discord",
        "snap.discord-canary",
    ];
    let mut seen = HashSet::new();
    let mut paths = Vec::new();
    for root in roots {
        for suffix in suffixes {
            let base = if suffix.is_empty() {
                root.clone()
            } else {
                root.join(suffix)
            };
            for index in 0..IPC_SEARCH_LIMIT {
                let path = base.join(format!("discord-ipc-{index}"));
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
    (0..IPC_SEARCH_LIMIT)
        .map(|index| PathBuf::from(format!(r"\\?\pipe\discord-ipc-{index}")))
        .collect()
}

pub(super) enum IpcStream {
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixStream),
    #[cfg(windows)]
    Windows(std::fs::File),
}

impl IpcStream {
    fn connect(path: &Path) -> Result<Self, std::io::Error> {
        #[cfg(unix)]
        {
            let stream = std::os::unix::net::UnixStream::connect(path)?;
            stream.set_read_timeout(Some(IPC_TIMEOUT))?;
            stream.set_write_timeout(Some(IPC_TIMEOUT))?;
            Ok(Self::Unix(stream))
        }

        #[cfg(windows)]
        {
            let stream = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)?;
            Ok(Self::Windows(stream))
        }
    }
}

impl Read for IpcStream {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, std::io::Error> {
        match self {
            #[cfg(unix)]
            IpcStream::Unix(stream) => stream.read(buf),
            #[cfg(windows)]
            IpcStream::Windows(stream) => stream.read(buf),
        }
    }
}

impl Write for IpcStream {
    fn write(&mut self, buf: &[u8]) -> Result<usize, std::io::Error> {
        match self {
            #[cfg(unix)]
            IpcStream::Unix(stream) => stream.write(buf),
            #[cfg(windows)]
            IpcStream::Windows(stream) => stream.write(buf),
        }
    }

    fn flush(&mut self) -> Result<(), std::io::Error> {
        match self {
            #[cfg(unix)]
            IpcStream::Unix(stream) => stream.flush(),
            #[cfg(windows)]
            IpcStream::Windows(stream) => stream.flush(),
        }
    }
}

#[derive(Debug)]
pub(super) enum DiscordRpcError {
    Connect(String),
    Io(std::io::Error),
    Json(serde_json::Error),
    Protocol(String),
}

impl fmt::Display for DiscordRpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect(message) => write!(f, "failed to connect to Discord IPC: {message}"),
            Self::Io(error) => write!(f, "Discord IPC I/O failed: {error}"),
            Self::Json(error) => write!(f, "Discord IPC JSON failed: {error}"),
            Self::Protocol(message) => write!(f, "Discord RPC protocol error: {message}"),
        }
    }
}

impl From<std::io::Error> for DiscordRpcError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for DiscordRpcError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Cursor;

    #[test]
    fn frames_use_little_endian_header_and_json_payload() {
        let payload = json!({ "cmd": "SET_ACTIVITY", "nonce": "n1" });
        let mut bytes = Vec::new();

        write_frame(&mut bytes, OP_FRAME, &payload).expect("frame should serialize");

        assert_eq!(&bytes[0..4], &OP_FRAME.to_le_bytes());
        assert_eq!(
            u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize,
            bytes.len() - 8
        );

        let (opcode, decoded) =
            read_frame(&mut Cursor::new(bytes)).expect("frame should deserialize");
        assert_eq!(opcode, OP_FRAME);
        assert_eq!(decoded, payload);
    }

    #[test]
    fn oversized_frames_are_rejected_before_allocation() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&OP_FRAME.to_le_bytes());
        bytes.extend_from_slice(&((MAX_FRAME_BYTES as u32) + 1).to_le_bytes());

        let error = read_frame(&mut Cursor::new(bytes)).expect_err("frame should be rejected");
        assert!(matches!(error, DiscordRpcError::Protocol(_)));
    }

    #[cfg(unix)]
    #[test]
    fn unix_candidates_cover_standard_and_packaged_discord_paths() {
        let paths = ipc_path_candidates_from_roots(&[
            PathBuf::from("/run/user/1000"),
            PathBuf::from("/tmp"),
        ]);

        assert!(paths.contains(&PathBuf::from("/run/user/1000/discord-ipc-0")));
        assert!(paths.contains(&PathBuf::from(
            "/run/user/1000/app/com.discordapp.Discord/discord-ipc-0"
        )));
        assert!(paths.contains(&PathBuf::from(
            "/run/user/1000/.flatpak/dev.vencord.Vesktop/xdg-run/discord-ipc-0"
        )));
        assert!(paths.contains(&PathBuf::from("/tmp/discord-ipc-9")));
    }
}
