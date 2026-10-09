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
//! snapshot's bytes for `PULL`) or `ERR <message>`.

use std::collections::HashMap;
use std::hash::{BuildHasher, RandomState};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

use crate::db::{SCHEMA_VERSION, Stats, Store};
use crate::i18n::tr;

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

/// Discovery and serving while the Sync tab is open.
pub struct Session {
    stop: Arc<AtomicBool>,
    /// The port snapshots are served on, if serving works at all.
    pub port: Option<u16>,
    /// What doesn't work (a port in use…), for the Sync tab.
    pub problems: Vec<String>,
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
                problems.push(tr!(
                    "Other computers can't pull from this one: {}",
                    format!("{e:#}")
                ));
                None
            }
        };
        match beacon(config, &name, port, stop.clone(), on_peers) {
            Ok(true) => {}
            Ok(false) => problems.push(tr!(
                "Can't look for other computers: port {} is in use. Enter an address instead.",
                config.beacon_port
            )),
            Err(e) => problems.push(tr!("Can't look for other computers: {}", format!("{e:#}"))),
        }
        Session {
            stop,
            port,
            problems,
        }
    }
}

// ---- server -----------------------------------------------------------------

/// Listens on `addr` (or any free port if it's taken) and answers each
/// connection on its own thread. Returns the port.
fn serve(addr: SocketAddr, db: PathBuf, name: String, stop: Arc<AtomicBool>) -> Result<u16> {
    let listener = TcpListener::bind(addr)
        .or_else(|_| TcpListener::bind(SocketAddr::new(addr.ip(), 0)))
        .context("couldn't open a port")?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let (db, name) = (db.clone(), name.clone());
                    std::thread::spawn(move || {
                        if let Err(e) = answer(stream, &db, &name) {
                            eprintln!("aggrega: sync: {e:#}");
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

fn answer(stream: TcpStream, db: &Path, name: &str) -> Result<()> {
    // Accepted sockets may inherit non-blocking mode from the listener.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(CONNECT_TIMEOUT))?;
    stream.set_write_timeout(Some(READ_TIMEOUT))?;
    let mut line = String::new();
    BufReader::new((&stream).take(128)).read_line(&mut line)?;
    let mut out = &stream;
    let mut parts = line.split_whitespace();
    if parts.next() != Some(MAGIC) {
        bail!("not an Aggrega request");
    }
    if parts.next() != Some(&PROTOCOL.to_string()) {
        writeln!(out, "ERR it runs an incompatible version of Aggrega")?;
        return Ok(());
    }
    let reply = match parts.next() {
        Some("INFO") => Store::open(db).and_then(|s| s.stats()).map(|s| {
            format!(
                "OK {SCHEMA_VERSION} {} {} {} {name}",
                s.feeds, s.articles, s.unread
            )
        }),
        Some("PULL") => return send_snapshot(out, db),
        _ => Err(anyhow!("unknown request")),
    };
    match reply {
        Ok(ok) => writeln!(out, "{ok}")?,
        Err(e) => writeln!(out, "ERR {}", one_line(&format!("{e:#}")))?,
    }
    Ok(())
}

fn send_snapshot(mut out: &TcpStream, db: &Path) -> Result<()> {
    let path = temp_path(&std::env::temp_dir(), "snapshot");
    let result = (|| {
        Store::open(db)?.snapshot_to(&path)?;
        let mut file = std::fs::File::open(&path)?;
        writeln!(out, "OK {}", file.metadata()?.len())?;
        std::io::copy(&mut file, &mut out)?;
        Ok(())
    })();
    let _ = std::fs::remove_file(&path);
    if let Err(e) = &result {
        // Only reaches the client if nothing was sent yet; harmless otherwise.
        let _ = writeln!(out, "ERR {}", one_line(&format!("{e:#}")));
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
) -> Result<bool> {
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
    pub name: String,
    pub schema: i32,
    pub stats: Stats,
}

/// Parses what the user typed: an IP address or host name, with or without
/// a port.
pub fn resolve(input: &str) -> Result<SocketAddr> {
    let input = input.trim();
    if input.is_empty() {
        bail!("enter the other computer's address");
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
    with_port
        .ok()
        .and_then(|mut addrs| addrs.next())
        .with_context(|| format!("couldn't find “{input}” on the network"))
}

fn request(addr: SocketAddr, what: &str) -> Result<(BufReader<TcpStream>, String)> {
    let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).map_err(|e| {
        anyhow!("couldn't reach {addr} ({e}). Is Aggrega's Sync tab open over there?")
    })?;
    stream.set_read_timeout(Some(READ_TIMEOUT))?;
    stream.set_write_timeout(Some(CONNECT_TIMEOUT))?;
    writeln!(&stream, "{MAGIC} {PROTOCOL} {what}")?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    (&mut reader).take(1024).read_line(&mut line)?;
    let line = line.trim_end();
    if let Some(err) = line.strip_prefix("ERR ") {
        bail!("{err}");
    }
    let Some(rest) = line.strip_prefix("OK ") else {
        bail!("{addr} didn't answer like Aggrega does");
    };
    Ok((reader, rest.to_string()))
}

/// Asks the computer at `addr` what it would send.
pub fn info(addr: SocketAddr) -> Result<Remote> {
    let (_, rest) = request(addr, "INFO")?;
    let mut parts = rest.splitn(5, ' ');
    let mut num = || -> Result<i64> {
        parts
            .next()
            .and_then(|p| p.parse().ok())
            .context("unexpected answer")
    };
    let (schema, feeds, articles, unread) = (num()?, num()?, num()?, num()?);
    Ok(Remote {
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
pub fn pull(addr: SocketAddr, db: &Path) -> Result<Stats> {
    let (mut reader, rest) = request(addr, "PULL")?;
    let len: u64 = rest.trim().parse().context("unexpected answer")?;
    if len > MAX_SNAPSHOT {
        bail!("the database is too large to receive");
    }
    let dir = db.parent().unwrap_or(Path::new("."));
    let path = temp_path(dir, "incoming");
    let result = (|| {
        let mut file = std::fs::File::create(&path)?;
        let got = std::io::copy(&mut (&mut reader).take(len), &mut file)?;
        if got != len {
            bail!("the connection dropped before the whole database arrived");
        }
        file.sync_all()?;
        drop(file);
        let mut store = Store::open(db)?;
        store.replace_with(&path)?;
        store.stats()
    })();
    let _ = std::fs::remove_file(&path);
    result
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
    #[cfg(not(target_os = "macos"))]
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

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aggrega-sync-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
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
        assert!(resolve("").is_err());
        assert_eq!(display_addr("10.0.0.2:47812".parse().unwrap()), "10.0.0.2");
        assert_eq!(display_addr("10.0.0.2:5".parse().unwrap()), "10.0.0.2:5");
    }

    #[test]
    fn pulls_another_database_over_tcp() -> Result<()> {
        let dir = temp_dir("pull");
        let theirs = dir.join("theirs.db");
        let store = Store::open(&theirs)?;
        store.add_feed("https://x.org/rss", &feed(4))?;
        store.mark_all_read(None)?;
        let ours = dir.join("ours.db");
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
        let leftovers = std::fs::read_dir(&dir)?
            .filter(|e| {
                let name = e.as_ref().unwrap().file_name();
                name.to_string_lossy().contains("incoming")
            })
            .count();
        assert_eq!(leftovers, 0, "the download is cleaned up");

        // Stopping the session closes the port.
        drop(session);
        std::thread::sleep(POLL * 3);
        assert!(info(addr).is_err());
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[test]
    fn hears_beacons_but_not_its_own() {
        let dir = temp_dir("beacon");
        let db = dir.join("a.db");
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
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn refuses_other_protocols() -> Result<()> {
        let dir = temp_dir("protocol");
        let db = dir.join("a.db");
        Store::open(&db)?;
        let session = Session::start(&loopback(), db, "Me".into(), |_| {});
        let addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), session.port.unwrap());
        let stream = TcpStream::connect(addr)?;
        writeln!(&stream, "{MAGIC} 99 PULL")?;
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line)?;
        assert!(line.starts_with("ERR"), "{line}");
        drop(session);
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }
}
