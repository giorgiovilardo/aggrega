//! Moving a whole profile between computers on the same network.
//!
//! While the Sync tab is open, a `Session` announces this copy of Aggrega
//! with a UDP broadcast beacon, lists the other copies it hears and serves a
//! snapshot of the database over TCP. `info` and `pull` are the client side:
//! pulling replaces this computer's database with the other one's. Nothing
//! runs outside a session: dropping it stops every thread within `POLL`.
//!
//! The protocol is line based. A client sends `AGGREGA-SYNC 1 INFO` or
//! `AGGREGA-SYNC 1 PULL`; the server answers `OK …` (followed by the
//! snapshot's bytes for `PULL`) or `ERR <code>: <detail>`. The code is
//! `incompatible` when the protocol version differs, and `failed` for any
//! other error, with its causes joined by ": " on one line. Builds that predate
//! the codes send plain text after `ERR`, which clients show as it is.

use std::collections::HashMap;
use std::hash::{BuildHasher, RandomState};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::db::{self, SCHEMA_VERSION, Stats, Store};
use crate::error::chain;

/// Where beacons are sent and heard.
pub const BEACON_PORT: u16 = 47811;
/// Where snapshots are served, unless another program already uses it.
pub const SERVICE_PORT: u16 = 47812;

const MAGIC: &str = "AGGREGA-SYNC";
const PROTOCOL: u32 = 1;
const BEACON_EVERY: Duration = Duration::from_millis(1500);
/// A peer that hasn't announced itself for this long is gone.
const PEER_TTL: Duration = Duration::from_secs(5);
/// How often the session threads check whether they should stop.
const POLL: Duration = Duration::from_millis(250);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Generous: the server compacts the database before it answers a pull.
const READ_TIMEOUT: Duration = Duration::from_secs(60);
/// Larger snapshots are refused rather than filling the disk.
const MAX_SNAPSHOT: u64 = 4 << 30;

/// Where a session listens and announces. Tests keep it on the loopback.
#[derive(Debug, Clone)]
pub struct Config {
    /// 0 picks a free port (and announces to it, so the session hears itself).
    pub beacon_port: u16,
    pub service: SocketAddr,
    pub announce_to: IpAddr,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            beacon_port: BEACON_PORT,
            service: (Ipv4Addr::UNSPECIFIED, SERVICE_PORT).into(),
            announce_to: Ipv4Addr::BROADCAST.into(),
        }
    }
}

/// Another copy of Aggrega heard on the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub name: String,
    pub addr: SocketAddr,
}

/// Something that doesn't work during a sync session, for the Sync tab.
#[derive(Debug, thiserror::Error)]
pub enum Problem {
    /// No port could be opened, so other computers can't pull from this one.
    #[error("can't serve this database to other computers")]
    CantServe(#[source] io::Error),
    /// Another program holds the beacon port: this copy announces itself but can't hear others.
    #[error("beacon port {0} is in use")]
    BeaconPortInUse(u16),
    #[error("can't look for other computers")]
    CantDiscover(#[source] io::Error),
}

/// Discovery and serving while the Sync tab is open.
pub struct Session {
    stop: Arc<AtomicBool>,
    /// The port snapshots are served on, if serving works at all.
    pub port: Option<u16>,
    /// What doesn't work (a port in use…), for the Sync tab.
    pub problems: Vec<Problem>,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Session {
    /// Starts serving `db` and announcing it as `name`. `on_peers` gets the
    /// full list of other copies whenever it changes, on a session thread.
    pub fn start(
        config: &Config,
        db: PathBuf,
        name: String,
        on_peers: impl Fn(Vec<Peer>) + Send + 'static,
    ) -> Session {
        let stop = Arc::new(AtomicBool::new(false));
        let mut problems = Vec::new();

        let port = match serve(config.service, db, name.clone(), stop.clone()) {
            Ok(port) => Some(port),
            Err(e) => {
                problems.push(Problem::CantServe(e));
                None
            }
        };
        match beacon(config, &name, port, stop.clone(), on_peers) {
            Ok(true) => {}
            Ok(false) => problems.push(Problem::BeaconPortInUse(config.beacon_port)),
            Err(e) => problems.push(Problem::CantDiscover(e)),
        }
        Session {
            stop,
            port,
            problems,
        }
    }
}

// ---- server -----------------------------------------------------------------

/// Why answering another computer failed; only ever logged.
#[derive(Debug, thiserror::Error)]
enum ServeError {
    #[error("not an Aggrega request")]
    NotARequest,
    #[error("unknown request “{0}”")]
    UnknownRequest(String),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Db(#[from] db::Error),
}

/// Listens on `addr` (or any free port if it's taken) and answers each
/// connection on its own thread. Returns the port.
fn serve(
    addr: SocketAddr,
    db: PathBuf,
    name: String,
    stop: Arc<AtomicBool>,
) -> Result<u16, io::Error> {
    let listener =
        TcpListener::bind(addr).or_else(|_| TcpListener::bind(SocketAddr::new(addr.ip(), 0)))?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let (db, name) = (db.clone(), name.clone());
                    std::thread::spawn(move || {
                        if let Err(e) = answer(stream, &db, &name) {
                            eprintln!("aggrega: sync: {}", chain(&e));
                        }
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(POLL),
                Err(e) => {
                    eprintln!("aggrega: sync: {e}");
                    std::thread::sleep(POLL);
                }
            }
        }
    });
    Ok(port)
}

fn answer(stream: TcpStream, db: &Path, name: &str) -> Result<(), ServeError> {
    // Accepted sockets may inherit non-blocking mode from the listener.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(CONNECT_TIMEOUT))?;
    stream.set_write_timeout(Some(READ_TIMEOUT))?;
    let mut line = String::new();
    BufReader::new((&stream).take(128)).read_line(&mut line)?;
    let mut out = &stream;
    let mut parts = line.split_whitespace();
    if parts.next() != Some(MAGIC) {
        return Err(ServeError::NotARequest);
    }
    if parts.next() != Some(&PROTOCOL.to_string()) {
        writeln!(
            out,
            "ERR incompatible: it runs an incompatible version of Aggrega"
        )?;
        return Ok(());
    }
    let reply = match parts.next() {
        Some("INFO") => Store::open(db)
            .and_then(|s| s.stats())
            .map(|s| {
                format!(
                    "OK {SCHEMA_VERSION} {} {} {} {name}",
                    s.feeds, s.articles, s.unread
                )
            })
            .map_err(ServeError::from),
        Some("PULL") => return send_snapshot(out, db),
        other => Err(ServeError::UnknownRequest(other.unwrap_or_default().into())),
    };
    match reply {
        Ok(ok) => writeln!(out, "{ok}")?,
        Err(e) => writeln!(out, "ERR failed: {}", one_line(&chain(&e)))?,
    }
    Ok(())
}

fn send_snapshot(mut out: &TcpStream, db: &Path) -> Result<(), ServeError> {
    let path = temp_path(&std::env::temp_dir(), "snapshot");
    let result = (|| -> Result<(), ServeError> {
        Store::open(db)?.snapshot_to(&path)?;
        let mut file = std::fs::File::open(&path)?;
        writeln!(out, "OK {}", file.metadata()?.len())?;
        std::io::copy(&mut file, &mut out)?;
        Ok(())
    })();
    let _ = std::fs::remove_file(&path);
    if let Err(e) = &result {
        // Only reaches the client if nothing was sent yet; harmless otherwise.
        let _ = writeln!(out, "ERR failed: {}", one_line(&chain(e)));
    }
    result
}

// ---- discovery --------------------------------------------------------------

/// Announces this copy every `BEACON_EVERY` and reports the others it hears.
/// Returns false if the beacon port is taken: this copy still announces
/// itself, but can't hear anyone.
fn beacon(
    config: &Config,
    name: &str,
    port: Option<u16>,
    stop: Arc<AtomicBool>,
    on_peers: impl Fn(Vec<Peer>) + Send + 'static,
) -> Result<bool, io::Error> {
    let (socket, listening) = match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, config.beacon_port)) {
        Ok(s) => (s, true),
        Err(_) => (UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?, false),
    };
    socket.set_broadcast(true)?;
    socket.set_read_timeout(Some(POLL))?;
    let target_port = match config.beacon_port {
        0 => socket.local_addr()?.port(),
        p => p,
    };
    let target = SocketAddr::new(config.announce_to, target_port);
    let me = instance_id();
    // Without a server there's nothing to pull: listen, but stay silent.
    let message = port.map(|p| format!("{MAGIC} {PROTOCOL} {me:016x} {p} {name}"));

    std::thread::spawn(move || {
        let mut peers: HashMap<String, (Peer, Instant)> = HashMap::new();
        let mut next_beacon = Instant::now();
        let mut buf = [0u8; 512];
        while !stop.load(Ordering::Relaxed) {
            if let Some(msg) = &message
                && Instant::now() >= next_beacon
            {
                // Fails without a network; the next round tries again.
                let _ = socket.send_to(msg.as_bytes(), target);
                next_beacon = Instant::now() + BEACON_EVERY;
            }
            let mut changed = false;
            if let Ok((n, from)) = socket.recv_from(&mut buf)
                && let Some((id, peer)) = parse_beacon(&buf[..n], from.ip())
                && id != format!("{me:016x}")
            {
                let known = peers.get(&id).map(|(p, _)| p);
                changed = known != Some(&peer);
                peers.insert(id, (peer, Instant::now()));
            }
            let before = peers.len();
            peers.retain(|_, (_, seen)| seen.elapsed() < PEER_TTL);
            changed |= peers.len() != before;
            if changed && !stop.load(Ordering::Relaxed) {
                let mut list: Vec<Peer> = peers.values().map(|(p, _)| p.clone()).collect();
                list.sort_by(|a, b| a.name.cmp(&b.name).then(a.addr.cmp(&b.addr)));
                on_peers(list);
            }
        }
    });
    Ok(listening)
}

/// `(instance id, peer)` from a beacon sent from `ip`.
fn parse_beacon(bytes: &[u8], ip: IpAddr) -> Option<(String, Peer)> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut parts = text.splitn(5, ' ');
    if parts.next()? != MAGIC || parts.next()?.parse::<u32>().ok()? != PROTOCOL {
        return None;
    }
    let id = parts.next()?.to_string();
    let port: u16 = parts.next()?.parse().ok()?;
    let name = parts.next().map(str::trim).filter(|n| !n.is_empty())?;
    Some((
        id,
        Peer {
            name: name.chars().take(64).collect(),
            addr: SocketAddr::new(ip, port),
        },
    ))
}

fn instance_id() -> u64 {
    RandomState::new().hash_one((std::process::id(), Instant::now()))
}

// ---- client -----------------------------------------------------------------

/// What another computer has, before pulling it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    /// The address it was reached at.
    pub addr: SocketAddr,
    pub name: String,
    pub schema: i32,
    pub stats: Stats,
}

/// Why reaching, asking or pulling from another computer failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no address given")]
    EmptyAddress,
    /// The name didn't resolve; `source` is the resolver's error, if it gave one.
    #[error("couldn't find “{input}” on the network")]
    NotFound {
        input: String,
        #[source]
        source: Option<io::Error>,
    },
    /// Connecting failed. `source.kind()` tells refused (Sync tab closed) from timed out.
    #[error("couldn't reach {addr}")]
    Unreachable {
        addr: SocketAddr,
        #[source]
        source: io::Error,
    },
    /// The connection failed after it was made (reset, timeout).
    #[error("the connection to {addr} was lost")]
    Connection {
        addr: SocketAddr,
        #[source]
        source: io::Error,
    },
    /// The answer wasn't in Aggrega's protocol, or was malformed.
    #[error("{addr} didn't answer like Aggrega does")]
    UnexpectedAnswer { addr: SocketAddr },
    #[error("{addr} runs an incompatible version of Aggrega")]
    Incompatible { addr: SocketAddr },
    /// The other computer reported an error; `message` is its text, as sent.
    #[error("{addr} reported an error: {message}")]
    Remote { addr: SocketAddr, message: String },
    #[error("the database from {addr} is too large to receive ({size} bytes)")]
    TooLarge { addr: SocketAddr, size: u64 },
    /// The connection ended cleanly before the whole database arrived.
    #[error("the connection to {addr} ended after {got} of {expected} bytes")]
    Truncated {
        addr: SocketAddr,
        got: u64,
        expected: u64,
    },
    /// Writing the received database to disk failed (a full disk…).
    #[error("couldn't save the received database to {}", path.display())]
    Save {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(transparent)]
    Db(#[from] db::Error),
}

/// Parses what the user typed: an IP address or host name, with or without
/// a port.
pub fn resolve(input: &str) -> Result<SocketAddr, Error> {
    let input = input.trim();
    if input.is_empty() {
        return Err(Error::EmptyAddress);
    }
    if let Ok(addr) = input.parse::<SocketAddr>() {
        return Ok(addr);
    }
    if let Ok(ip) = input.trim_matches(['[', ']']).parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, SERVICE_PORT));
    }
    let with_port = if input.contains(':') {
        input.to_socket_addrs()
    } else {
        (input, SERVICE_PORT).to_socket_addrs()
    };
    let not_found = |source| Error::NotFound {
        input: input.into(),
        source,
    };
    let mut addrs = with_port.map_err(|e| not_found(Some(e)))?;
    addrs.next().ok_or_else(|| not_found(None))
}

/// Reads what the other computer says after `ERR `: see the module docs.
fn remote_error(addr: SocketAddr, text: &str) -> Error {
    match text.split_once(": ") {
        Some(("incompatible", _)) => Error::Incompatible { addr },
        Some(("failed", message)) => Error::Remote {
            addr,
            message: message.into(),
        },
        _ => Error::Remote {
            addr,
            message: text.into(),
        },
    }
}

fn request(addr: SocketAddr, what: &str) -> Result<(BufReader<TcpStream>, String), Error> {
    let lost = |source: io::Error| Error::Connection { addr, source };
    let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
        .map_err(|source| Error::Unreachable { addr, source })?;
    stream.set_read_timeout(Some(READ_TIMEOUT)).map_err(lost)?;
    stream
        .set_write_timeout(Some(CONNECT_TIMEOUT))
        .map_err(lost)?;
    writeln!(&stream, "{MAGIC} {PROTOCOL} {what}").map_err(lost)?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    (&mut reader)
        .take(1024)
        .read_line(&mut line)
        .map_err(lost)?;
    let line = line.trim_end();
    if let Some(text) = line.strip_prefix("ERR ") {
        return Err(remote_error(addr, text));
    }
    let Some(rest) = line.strip_prefix("OK ") else {
        return Err(Error::UnexpectedAnswer { addr });
    };
    Ok((reader, rest.to_string()))
}

/// Asks the computer at `addr` what it would send.
pub fn info(addr: SocketAddr) -> Result<Remote, Error> {
    let (_, rest) = request(addr, "INFO")?;
    let mut parts = rest.splitn(5, ' ');
    let mut num = || -> Result<i64, Error> {
        parts
            .next()
            .and_then(|p| p.parse().ok())
            .ok_or_else(|| Error::UnexpectedAnswer { addr })
    };
    let (schema, feeds, articles, unread) = (num()?, num()?, num()?, num()?);
    Ok(Remote {
        addr,
        name: parts.next().unwrap_or("").to_string(),
        schema: schema as i32,
        stats: Stats {
            feeds,
            articles,
            unread,
        },
    })
}

/// Downloads the database of the computer at `addr` and replaces the one at
/// `db` with it. Returns what this computer has now.
pub fn pull(addr: SocketAddr, db: &Path) -> Result<Stats, Error> {
    let (mut reader, rest) = request(addr, "PULL")?;
    let len: u64 = rest
        .trim()
        .parse()
        .map_err(|_| Error::UnexpectedAnswer { addr })?;
    if len > MAX_SNAPSHOT {
        return Err(Error::TooLarge { addr, size: len });
    }
    let dir = db.parent().unwrap_or(Path::new("."));
    let path = temp_path(dir, "incoming");
    let result = receive(addr, &mut reader, len, &path).and_then(|()| {
        let mut store = Store::open(db)?;
        store.replace_with(&path)?;
        Ok(store.stats()?)
    });
    let _ = std::fs::remove_file(&path);
    result
}

/// Copies the `len` bytes of a snapshot from `reader` into `path`. Read
/// failures are the connection's; write failures are this computer's.
fn receive(
    addr: SocketAddr,
    reader: &mut impl BufRead,
    len: u64,
    path: &Path,
) -> Result<(), Error> {
    let save = |source| Error::Save {
        path: path.to_path_buf(),
        source,
    };
    let mut file = std::fs::File::create(path).map_err(save)?;
    let mut reader = reader.take(len);
    let mut got = 0;
    loop {
        let chunk = match reader.fill_buf() {
            Ok([]) => break,
            Ok(chunk) => chunk,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(source) => return Err(Error::Connection { addr, source }),
        };
        let n = chunk.len();
        file.write_all(chunk).map_err(save)?;
        reader.consume(n);
        got += n as u64;
    }
    if got != len {
        return Err(Error::Truncated {
            addr,
            got,
            expected: len,
        });
    }
    file.sync_all().map_err(save)?;
    Ok(())
}

// ---- helpers ----------------------------------------------------------------

/// A name for this computer that others will recognise.
pub fn device_name() -> String {
    #[cfg(target_os = "macos")]
    let name = std::process::Command::new("scutil")
        .args(["--get", "ComputerName"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok());
    #[cfg(windows)]
    let name = std::env::var("COMPUTERNAME").ok();
    #[cfg(not(any(target_os = "macos", windows)))]
    let name = std::fs::read_to_string("/proc/sys/kernel/hostname").ok();
    name.map(|n| one_line(n.trim()))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "Aggrega".into())
}

/// This computer's address on the local network, if it's on one. Connecting
/// a UDP socket only picks a route; nothing is sent.
pub fn local_ip() -> Option<IpAddr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_unspecified() && !ip.is_loopback()).then_some(ip)
}

/// "192.168.1.5", or "192.168.1.5:50123" when not on the usual port.
pub fn display_addr(addr: SocketAddr) -> String {
    if addr.port() == SERVICE_PORT {
        addr.ip().to_string()
    } else {
        addr.to_string()
    }
}

fn one_line(s: &str) -> String {
    s.replace(['\r', '\n'], " ")
}

fn temp_path(dir: &Path, what: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    dir.join(format!("aggrega-{what}-{}-{n}.db", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feed::{Fetched, NewArticle};
    use std::sync::mpsc;

    fn temp_dir(name: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("aggrega-sync-{name}-"))
            .tempdir()
            .unwrap()
    }

    fn feed(n: usize) -> Fetched {
        Fetched {
            title: "Sample".into(),
            site_url: None,
            etag: None,
            last_modified: None,
            articles: (0..n)
                .map(|i| NewArticle {
                    guid: format!("g{i}"),
                    title: format!("Post {i}"),
                    link: format!("https://x.org/{i}"),
                    snippet: String::new(),
                    image_url: None,
                    published: 1_700_000_000 + i as i64,
                    body: String::new(),
                })
                .collect(),
        }
    }

    fn loopback() -> Config {
        Config {
            beacon_port: 0,
            service: (Ipv4Addr::LOCALHOST, 0).into(),
            announce_to: Ipv4Addr::LOCALHOST.into(),
        }
    }

    #[test]
    fn beacons_round_trip() {
        let ip: IpAddr = Ipv4Addr::new(192, 168, 1, 7).into();
        let msg = format!("{MAGIC} {PROTOCOL} 00ab 47812 Salvo's MacBook Pro");
        let (id, peer) = parse_beacon(msg.as_bytes(), ip).unwrap();
        assert_eq!(id, "00ab");
        assert_eq!(peer.name, "Salvo's MacBook Pro");
        assert_eq!(peer.addr, SocketAddr::new(ip, 47812));

        for bad in [
            "hello",
            "AGGREGA-SYNC 2 00ab 47812 Future",
            "AGGREGA-SYNC 1 00ab notaport Name",
            "AGGREGA-SYNC 1 00ab 47812 ",
        ] {
            assert!(parse_beacon(bad.as_bytes(), ip).is_none(), "{bad}");
        }
    }

    #[test]
    fn resolves_typed_addresses() {
        assert_eq!(
            resolve(" 192.168.1.5 ").unwrap(),
            "192.168.1.5:47812".parse().unwrap()
        );
        assert_eq!(
            resolve("192.168.1.5:5000").unwrap(),
            "192.168.1.5:5000".parse().unwrap()
        );
        assert_eq!(resolve("::1").unwrap(), "[::1]:47812".parse().unwrap());
        assert_eq!(resolve("[::1]:7").unwrap(), "[::1]:7".parse().unwrap());
        assert!(matches!(resolve(" "), Err(Error::EmptyAddress)));
        assert_eq!(display_addr("10.0.0.2:47812".parse().unwrap()), "10.0.0.2");
        assert_eq!(display_addr("10.0.0.2:5".parse().unwrap()), "10.0.0.2:5");
    }

    #[test]
    fn pulls_another_database_over_tcp() -> Result<(), Error> {
        let dir = temp_dir("pull");
        let theirs = dir.path().join("theirs.db");
        let store = Store::open(&theirs)?;
        store.add_feed("https://x.org/rss", &feed(4))?;
        store.mark_all_read(None)?;
        let ours = dir.path().join("ours.db");
        Store::open(&ours)?.add_feed("https://y.org/rss", &feed(1))?;

        let session = Session::start(&loopback(), theirs, "Desk".into(), |_| {});
        assert!(session.problems.is_empty(), "{:?}", session.problems);
        let addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), session.port.unwrap());

        let remote = info(addr)?;
        assert_eq!(remote.name, "Desk");
        assert_eq!(remote.schema, SCHEMA_VERSION);
        assert_eq!(remote.stats, store.stats()?);

        let now = pull(addr, &ours)?;
        assert_eq!(now, store.stats()?);
        assert_eq!(Store::open(&ours)?.sources()?, store.sources()?);
        let leftovers = std::fs::read_dir(dir.path())
            .unwrap()
            .filter(|e| {
                let name = e.as_ref().unwrap().file_name();
                name.to_string_lossy().contains("incoming")
            })
            .count();
        assert_eq!(leftovers, 0, "the download is cleaned up");

        // Stopping the session closes the port.
        drop(session);
        std::thread::sleep(POLL * 3);
        assert!(matches!(info(addr), Err(Error::Unreachable { .. })));
        Ok(())
    }

    #[test]
    fn hears_beacons_but_not_its_own() {
        let dir = temp_dir("beacon");
        let db = dir.path().join("a.db");
        Store::open(&db).unwrap();
        let free = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let config = Config {
            beacon_port: free.local_addr().unwrap().port(),
            ..loopback()
        };
        drop(free);
        let (tx, rx) = mpsc::channel();
        let session = Session::start(&config, db, "Me".into(), move |p| {
            let _ = tx.send(p);
        });
        assert!(session.problems.is_empty(), "{:?}", session.problems);
        // Its own beacons come back over the loopback and are ignored…
        assert!(rx.recv_timeout(BEACON_EVERY * 2).is_err());

        // …but anyone else's are listed.
        let other = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let beacon = format!("{MAGIC} {PROTOCOL} 0123456789abcdef 5000 Laptop");
        other
            .send_to(beacon.as_bytes(), (Ipv4Addr::LOCALHOST, config.beacon_port))
            .unwrap();
        let peers = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(
            peers,
            [Peer {
                name: "Laptop".into(),
                addr: (Ipv4Addr::LOCALHOST, 5000).into(),
            }]
        );
        drop(session);
    }

    #[test]
    fn refuses_other_protocols() {
        let dir = temp_dir("protocol");
        let db = dir.path().join("a.db");
        Store::open(&db).unwrap();
        let session = Session::start(&loopback(), db, "Me".into(), |_| {});
        let addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), session.port.unwrap());
        let stream = TcpStream::connect(addr).unwrap();
        writeln!(&stream, "{MAGIC} 99 PULL").unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        let text = line.trim_end().strip_prefix("ERR ").unwrap();
        assert!(
            matches!(remote_error(addr, text), Error::Incompatible { .. }),
            "{line}"
        );
        drop(session);
    }

    /// A server that reads one request, sends `reply` and hangs up.
    fn fake_server(reply: &[u8]) -> SocketAddr {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let reply = reply.to_vec();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            stream.write_all(&reply).unwrap();
        });
        addr
    }

    #[test]
    fn tells_what_went_wrong_with_another_computer() {
        // Nothing listens on port 9 here, so the connection is refused.
        let closed: SocketAddr = (Ipv4Addr::LOCALHOST, 9).into();
        assert!(matches!(info(closed), Err(Error::Unreachable { .. })));

        let incompatible =
            fake_server(b"ERR incompatible: it runs an incompatible version of Aggrega\n");
        assert!(matches!(
            info(incompatible),
            Err(Error::Incompatible { .. })
        ));

        let plain = fake_server(b"ERR something went wrong\n");
        assert!(matches!(
            info(plain),
            Err(Error::Remote { message, .. }) if message == "something went wrong"
        ));

        let failed = fake_server(b"ERR failed: unknown request: a: b\n");
        assert!(matches!(
            info(failed),
            Err(Error::Remote { message, .. }) if message == "unknown request: a: b"
        ));

        let garbage = fake_server(b"hello\n");
        assert!(matches!(info(garbage), Err(Error::UnexpectedAnswer { .. })));
    }

    #[test]
    fn refuses_snapshots_that_are_cut_short_or_too_large() {
        let dir = temp_dir("bad-snapshot");
        let db = dir.path().join("a.db");

        let short = fake_server(b"OK 100\n0123456789");
        assert!(matches!(
            pull(short, &db),
            Err(Error::Truncated {
                got: 10,
                expected: 100,
                ..
            })
        ));

        let huge = fake_server(format!("OK {}\n", MAX_SNAPSHOT + 1).as_bytes());
        assert!(matches!(
            pull(huge, &db),
            Err(Error::TooLarge { size, .. }) if size == MAX_SNAPSHOT + 1
        ));
        assert!(!db.exists(), "nothing was written");
    }

    #[test]
    fn reports_a_beacon_port_in_use() {
        let dir = temp_dir("busy");
        let db = dir.path().join("a.db");
        Store::open(&db).unwrap();
        let busy = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap();
        let config = Config {
            beacon_port: busy.local_addr().unwrap().port(),
            ..loopback()
        };
        let session = Session::start(&config, db, "Me".into(), |_| {});
        assert!(matches!(
            session.problems.as_slice(),
            [Problem::BeaconPortInUse(port)] if *port == config.beacon_port
        ));
        drop(busy);
    }
}
