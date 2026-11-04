//! One nodeplayer per user. The first one started listens on a loopback port of its own;
//! starting another hands its files and folders to it (which opens them and comes to the
//! front) and exits. `--new-instance` opts out.
//!
//! The port is picked from the user's name (and `--data-root`), so each user, and each
//! separate profile, has its own. If something else holds it and doesn't answer like
//! nodeplayer, this one starts on its own instead.
//!
//! What the system hands over by other means (macOS open-document events) arrives here too,
//! through [`deliver`].

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Result, ensure};
use crossbeam_channel::{Receiver, Sender};

const HELLO: &str = "nodeplayer-open 1";
const REPLY: &str = "ok";
/// How much a request may be, file list included.
const MAX_REQUEST: u64 = 1 << 20;

pub enum Claim {
    /// This is the nodeplayer running: what others hand over arrives through [`take`].
    Primary,
    /// The running nodeplayer took the files; this one exits.
    Forwarded,
    /// The port is held by something that isn't nodeplayer: this one runs on its own.
    Alone,
}

/// Files and folders handed over, and that the window should come to the front.
type Request = Vec<PathBuf>;

fn channel() -> &'static (Sender<Request>, Receiver<Request>) {
    static CHANNEL: OnceLock<(Sender<Request>, Receiver<Request>)> = OnceLock::new();
    CHANNEL.get_or_init(crossbeam_channel::unbounded)
}

/// Hands `files` to the running window (it also comes to the front).
pub fn deliver(files: Vec<PathBuf>) {
    let _ = channel().0.send(files);
}

/// What was handed over since the last call, oldest first: `None` when nothing was, else
/// the files (perhaps none: then just come to the front).
pub fn take() -> Option<Vec<PathBuf>> {
    let mut requests = channel().1.try_iter().peekable();
    requests.peek()?;
    Some(requests.flatten().collect())
}

/// The loopback port for this user and profile: outside the range Windows hands out (and
/// reserves for Hyper-V and the like) and the well-known ports.
fn port() -> u16 {
    let user = std::env::var("USERNAME").or_else(|_| std::env::var("USER")).unwrap_or_default();
    let profile = crate::runtime::get().data_root.as_ref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    port_for(&format!("{user}\n{profile}"))
}

fn port_for(key: &str) -> u16 {
    // FNV-1a: the same in every build.
    let hash = key.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3));
    37_000 + (hash % 10_000) as u16
}

/// Becomes the nodeplayer running, or hands `files` to the one there is.
pub fn claim(files: &[PathBuf]) -> Claim {
    claim_at(SocketAddr::from((Ipv4Addr::LOCALHOST, port())), files)
}

fn claim_at(addr: SocketAddr, files: &[PathBuf]) -> Claim {
    match TcpListener::bind(addr) {
        Ok(listener) => {
            let started = std::thread::Builder::new().name("instance".into()).spawn(move || serve(listener));
            if let Err(e) = started {
                log::warn!("can't listen for files from other starts: {e}");
            }
            Claim::Primary
        }
        Err(bind) => match forward(addr, files) {
            Ok(()) => Claim::Forwarded,
            Err(e) => {
                log::warn!("port {} is taken ({bind}) by something that didn't take the files ({e:#}); starting on its own", addr.port());
                Claim::Alone
            }
        },
    }
}

fn serve(listener: TcpListener) {
    for stream in listener.incoming() {
        match stream.map_err(anyhow::Error::from).and_then(receive) {
            Ok(files) => deliver(files),
            Err(e) => log::debug!("ignored a request to open files: {e:#}"),
        }
    }
}

fn receive(stream: TcpStream) -> Result<Request> {
    // Briefly: a stalled client can't hold up the next one for long.
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut reader = BufReader::new((&stream).take(MAX_REQUEST));
    let mut line = String::new();
    reader.read_line(&mut line)?;
    ensure!(line.trim_end() == HELLO, "not a nodeplayer request");
    line.clear();
    reader.read_line(&mut line)?;
    let files: Request = serde_json::from_str(&line)?;
    (&stream).write_all(format!("{REPLY}\n").as_bytes())?;
    Ok(files)
}

fn forward(addr: SocketAddr, files: &[PathBuf]) -> Result<()> {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(1))?;
    // The running one may be busy starting up; it answers once its listener thread runs.
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    // Paths relative to where this one was started mean nothing to the other.
    let files: Request = files.iter().map(|f| std::path::absolute(f).unwrap_or_else(|_| f.clone())).collect();
    allow_foreground();
    stream.write_all(format!("{HELLO}\n{}\n", serde_json::to_string(&files)?).as_bytes())?;
    let mut reply = String::new();
    BufReader::new(&stream).read_line(&mut reply)?;
    ensure!(reply.trim_end() == REPLY, "unexpected answer {reply:?}");
    Ok(())
}

/// Windows lets only the process the user just started take the foreground: pass that on to
/// the running nodeplayer, so its window can come to the front.
fn allow_foreground() {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::{ASFW_ANY, AllowSetForegroundWindow};
        let _ = AllowSetForegroundWindow(ASFW_ANY);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_depends_on_the_user_and_stays_in_its_range() {
        let (a, b) = (port_for("alice\n"), port_for("bob\n"));
        assert_ne!(a, b);
        assert_eq!(a, port_for("alice\n"));
        for p in [a, b, port_for(""), port_for("alice\n/profile")] {
            assert!((37_000..47_000).contains(&p), "{p}");
        }
    }

    #[test]
    fn a_second_start_hands_its_files_to_the_first() {
        // A free port of the test's own, not the user's.
        let addr = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap().local_addr().unwrap();
        assert!(matches!(claim_at(addr, &[]), Claim::Primary));
        let files = vec![std::env::temp_dir().join("a song.flac"), PathBuf::from("relative/b.mp3")];
        assert!(matches!(claim_at(addr, &files), Claim::Forwarded));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let got = loop {
            if let Some(got) = take() {
                break got;
            }
            assert!(std::time::Instant::now() < deadline, "the files didn't arrive");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(got[0], files[0]);
        assert!(got[1].is_absolute() && got[1].ends_with("relative/b.mp3"), "{:?}", got[1]);

        // Something else on the port: it doesn't answer like nodeplayer.
        let other = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let other_addr = other.local_addr().unwrap();
        std::thread::spawn(move || {
            for s in other.incoming().flatten() {
                let _ = (&s).write_all(b"HTTP/1.0 400 Bad Request\r\n\r\n");
            }
        });
        assert!(matches!(claim_at(other_addr, &files), Claim::Alone));
        // And a stranger's request isn't taken for files.
        let mut s = TcpStream::connect(addr).unwrap();
        s.write_all(b"GET / HTTP/1.0\r\n\r\n").unwrap();
        drop(s);
        std::thread::sleep(Duration::from_millis(100));
        assert!(take().is_none());
    }
}
