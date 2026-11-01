//! Versioned, length-bounded control plane. Never carried on plugin stdout.
use super::super::{ParamDesc, Placement, PluginInfo, Spot};
use super::surface::SurfaceDescriptor;
use crate::plugins::surface::EditorPointer;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::io::{Read, Write};
use std::net::TcpStream;

pub const VERSION: u32 = 2;
pub const MAX_FRAME: usize = 8 * 1024 * 1024;
pub const MAX_STATE: usize = 1024 * 1024;
pub const MAX_PARAMS: usize = 8192;

#[derive(Serialize, Deserialize)]
pub struct Hello {
    pub version: u32,
    pub token: String,
}
#[derive(Serialize, Deserialize)]
pub struct Init {
    pub version: u32,
    pub info: PluginInfo,
    pub sr: f32,
    pub quantum: usize,
    pub delay: usize,
    pub slots: usize,
    pub generation: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Metadata {
    pub params: Vec<ParamDesc>,
    pub values: Vec<(u32, f64, String)>,
    pub latency: u32,
    pub midi: (bool, bool),
    pub instrument: bool,
    pub editor: bool,
    pub editor_size: (u32, u32),
    pub editor_open: bool,
    pub editor_id: u64,
    pub editor_error: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
pub enum Command {
    Poll,
    SetParam(u32, f64),
    Display(u32, f64),
    Load(Vec<u8>),
    Checkpoint,
    Open {
        token: u64,
        placement: Placement,
        captured: bool,
    },
    SetSurface(u64, SurfaceDescriptor),
    Pointer {
        generation: u64,
        token: u64,
        event: EditorPointer,
    },
    Place(u64, Option<Spot>),
    Zoom(u64, f32),
    Close(u64),
    Shutdown,
}
#[derive(Serialize, Deserialize)]
pub struct Request {
    pub version: u32,
    pub generation: u64,
    pub id: u64,
    pub command: Command,
}
#[derive(Serialize, Deserialize)]
pub struct Reply {
    pub version: u32,
    pub generation: u64,
    pub id: u64,
    pub metadata: Option<Metadata>,
    pub state: Option<Vec<u8>>,
    pub display: Option<(u32, f64, String)>,
    pub error: Option<String>,
}

pub fn send<T: Serialize>(stream: &mut TcpStream, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > MAX_FRAME {
        bail!("remote control message exceeds limit");
    }
    let timeout = stream
        .write_timeout()?
        .unwrap_or(std::time::Duration::from_secs(3));
    let deadline = std::time::Instant::now() + timeout;
    write_bounded(stream, &(bytes.len() as u32).to_le_bytes(), deadline)?;
    write_bounded(stream, &bytes, deadline)?;
    stream.set_write_timeout(Some(timeout))?;
    Ok(())
}
pub fn receive<T: DeserializeOwned>(stream: &mut TcpStream) -> Result<T> {
    let timeout = stream
        .read_timeout()?
        .unwrap_or(std::time::Duration::from_secs(3));
    let deadline = std::time::Instant::now() + timeout;
    let mut len = [0; 4];
    read_bounded(stream, &mut len, deadline)?;
    let len = u32::from_le_bytes(len) as usize;
    if len == 0 || len > MAX_FRAME {
        bail!("invalid remote control frame length");
    }
    let mut bytes = vec![0; len];
    read_bounded(stream, &mut bytes, deadline)?;
    stream.set_read_timeout(Some(timeout))?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn read_bounded(
    stream: &mut TcpStream,
    mut data: &mut [u8],
    deadline: std::time::Instant,
) -> Result<()> {
    while !data.is_empty() {
        let remaining = deadline
            .checked_duration_since(std::time::Instant::now())
            .ok_or_else(|| anyhow::anyhow!("control read deadline exceeded"))?;
        stream.set_read_timeout(Some(remaining))?;
        let n = stream.read(data)?;
        if n == 0 {
            bail!("control connection closed")
        }
        data = &mut data[n..];
    }
    Ok(())
}
fn write_bounded(
    stream: &mut TcpStream,
    mut data: &[u8],
    deadline: std::time::Instant,
) -> Result<()> {
    while !data.is_empty() {
        let remaining = deadline
            .checked_duration_since(std::time::Instant::now())
            .ok_or_else(|| anyhow::anyhow!("control write deadline exceeded"))?;
        stream.set_write_timeout(Some(remaining))?;
        let n = stream.write(data)?;
        if n == 0 {
            bail!("control connection closed")
        }
        data = &data[n..];
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_oversized_frame_before_body_allocation() {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut sender = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        sender
            .write_all(&((MAX_FRAME + 1) as u32).to_le_bytes())
            .unwrap();
        let (mut receiver, _) = listener.accept().unwrap();
        assert!(receive::<Hello>(&mut receiver).is_err());
    }
}
