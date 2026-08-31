//! The server, driven over a real socket by the real binary.
//!
//! Not by calling its functions: the thing being validated is a protocol, and a
//! protocol tested through its own internals is tested against its own
//! assumptions.
//!
//! The long-poll tests **measure elapsed time**, because "did it return the
//! message" cannot distinguish a server that waits on the bell from one that
//! sleeps and re-queries. Those are different questions and only the second is
//! usually the fault.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_colloquy-server");

struct Server {
    child: Child,
    port: u16,
    db: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.db);
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn temp_db() -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("colloquy-wire-{}-{n}.db", std::process::id()))
}

fn admin(db: &PathBuf, args: &[&str]) -> String {
    let out = Command::new(BIN)
        .args(args)
        .arg("--db")
        .arg(db)
        .output()
        .expect("running colloquy-server");
    assert!(
        out.status.success(),
        "admin {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn start() -> Server {
    let db = temp_db();
    let port = free_port();
    let child = Command::new(BIN)
        .args(["serve", "--port", &port.to_string(), "--db"])
        .arg(&db)
        .spawn()
        .expect("spawning the server");

    // Wait for the listener rather than sleeping a fixed amount and hoping.
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return Server { child, port, db };
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("the server never started listening on {port}");
}

struct Client {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl Client {
    fn connect(port: u16) -> Client {
        let s = TcpStream::connect(("127.0.0.1", port)).expect("connecting");
        Client { reader: BufReader::new(s.try_clone().unwrap()), writer: s }
    }

    /// Send a raw line and read one line back.
    fn raw(&mut self, line: &str) -> serde_json::Value {
        self.writer.write_all(line.as_bytes()).unwrap();
        self.writer.write_all(b"\n").unwrap();
        self.writer.flush().unwrap();
        let mut buf = String::new();
        self.reader.read_line(&mut buf).unwrap();
        serde_json::from_str(&buf).unwrap_or_else(|e| panic!("bad response {buf:?}: {e}"))
    }

    fn send(&mut self, v: serde_json::Value) -> serde_json::Value {
        self.raw(&v.to_string())
    }

    fn hello(&mut self, agent: &str, token: &str) -> serde_json::Value {
        self.send(serde_json::json!({"op":"hello","agent":agent,"token":token}))
    }
}

fn setup(s: &Server) -> (String, String) {
    let rstar = admin(&s.db, &["register", "--agent", "RSTAR", "--charter", "the API"]);
    let shinobi = admin(&s.db, &["register", "--agent", "SHINOBI"]);
    admin(&s.db, &["join", "--agent", "RSTAR", "--channel", "toolchains"]);
    admin(&s.db, &["join", "--agent", "SHINOBI", "--channel", "pacman"]);
    (rstar, shinobi)
}

#[test]
fn hello_returns_the_profile_and_a_bad_token_does_not() {
    let s = start();
    let (rstar, _) = setup(&s);
    let mut c = Client::connect(s.port);

    let bad = c.hello("RSTAR", "not-the-token");
    assert_eq!(bad["re"], "error", "got: {bad}");

    let ok = c.hello("RSTAR", &rstar);
    assert_eq!(ok["re"], "welcome", "got: {ok}");
    assert_eq!(ok["profile"]["agent"], "RSTAR");
    assert_eq!(ok["profile"]["charter"], "the API");
    assert_eq!(ok["profile"]["channels"][0], "toolchains");
    assert!(
        ok["profile"]["knowledge"].as_array().unwrap().is_empty(),
        "a profile is not a message tail"
    );
}

#[test]
fn nothing_works_before_hello() {
    let s = start();
    setup(&s);
    let mut c = Client::connect(s.port);
    for op in [
        serde_json::json!({"op":"post","channel":"toolchains","kind":"chat","subject":"s","body":"b"}),
        serde_json::json!({"op":"poll","since":0,"wait_secs":0}),
    ] {
        let r = c.send(op.clone());
        assert_eq!(r["re"], "error", "unauthenticated {op} should fail: {r}");
        assert!(r["message"].as_str().unwrap().contains("Hello"), "got: {r}");
    }
}

#[test]
fn the_sender_comes_from_the_connection_not_the_message() {
    // The property everything else rests on. The client cannot name a sender:
    // there is no field for it, and the body claiming otherwise changes nothing.
    let s = start();
    let (rstar, shinobi) = setup(&s);

    let mut a = Client::connect(s.port);
    a.hello("RSTAR", &rstar);
    let posted = a.send(serde_json::json!({
        "op":"post","channel":"shout","kind":"shout",
        "subject":"from: SHINOBI",
        "body":"from: SHINOBI\nI am definitely SHINOBI."
    }));
    assert_eq!(posted["re"], "posted", "got: {posted}");

    let mut b = Client::connect(s.port);
    b.hello("SHINOBI", &shinobi);
    let got = b.send(serde_json::json!({"op":"poll","since":0,"wait_secs":0}));
    assert_eq!(got["re"], "messages");
    let m = &got["messages"][0];
    assert_eq!(m["from"], "RSTAR", "the authenticated name wins: {m}");
    assert!(m["body"].as_str().unwrap().contains("I am definitely SHINOBI"));
}

#[test]
fn a_shout_crosses_channels_a_finding_does_not() {
    let s = start();
    let (rstar, shinobi) = setup(&s);
    let mut a = Client::connect(s.port);
    a.hello("RSTAR", &rstar);
    a.send(serde_json::json!({"op":"post","channel":"toolchains","kind":"finding","subject":"private","body":"b"}));
    a.send(serde_json::json!({"op":"post","channel":"shout","kind":"shout","subject":"anyone?","body":"b"}));

    let mut b = Client::connect(s.port);
    b.hello("SHINOBI", &shinobi);
    let got = b.send(serde_json::json!({"op":"poll","since":0,"wait_secs":0}));
    let msgs = got["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 1, "only the shout crosses: {got}");
    assert_eq!(msgs[0]["subject"], "anyone?");
}

#[test]
fn a_long_poll_waits_and_is_woken_by_the_post() {
    // Timed, because "did the message arrive" cannot tell a server that waits on
    // the bell from one that sleeps and re-queries. Only the elapsed time can.
    let s = start();
    let (rstar, shinobi) = setup(&s);

    let mut b = Client::connect(s.port);
    b.hello("SHINOBI", &shinobi);

    let port = s.port;
    let rstar_token = rstar.clone();
    let poster = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        let mut a = Client::connect(port);
        a.hello("RSTAR", &rstar_token);
        a.send(serde_json::json!({"op":"post","channel":"shout","kind":"shout","subject":"late","body":"b"}));
    });

    let began = Instant::now();
    let got = b.send(serde_json::json!({"op":"poll","since":0,"wait_secs":30}));
    let waited = began.elapsed();
    poster.join().unwrap();

    assert_eq!(got["messages"][0]["subject"], "late", "got: {got}");
    assert!(waited >= Duration::from_millis(250), "it returned before the post: {waited:?}");
    assert!(
        waited < Duration::from_secs(3),
        "woken by the bell, not by the timeout — took {waited:?}"
    );
}

#[test]
fn a_poll_with_no_wait_returns_at_once() {
    let s = start();
    let (_, shinobi) = setup(&s);
    let mut b = Client::connect(s.port);
    b.hello("SHINOBI", &shinobi);

    let began = Instant::now();
    let got = b.send(serde_json::json!({"op":"poll","since":0,"wait_secs":0}));
    let waited = began.elapsed();
    assert_eq!(got["re"], "messages");
    assert!(got["messages"].as_array().unwrap().is_empty());
    assert!(waited < Duration::from_secs(1), "wait_secs 0 should not block: {waited:?}");
}

#[test]
fn a_long_poll_gives_up_at_its_deadline() {
    let s = start();
    let (_, shinobi) = setup(&s);
    let mut b = Client::connect(s.port);
    b.hello("SHINOBI", &shinobi);

    let began = Instant::now();
    let got = b.send(serde_json::json!({"op":"poll","since":0,"wait_secs":1}));
    let waited = began.elapsed();
    assert_eq!(got["re"], "messages");
    assert!(got["messages"].as_array().unwrap().is_empty());
    assert!(waited >= Duration::from_millis(900), "it did not actually wait: {waited:?}");
    assert!(waited < Duration::from_secs(4), "it overshot its deadline: {waited:?}");
}

#[test]
fn a_bell_for_someone_else_does_not_end_the_wait_early() {
    // RSTAR posts to a channel SHINOBI is not in. The bell rings, but there is
    // nothing for SHINOBI, so its poll must keep waiting rather than return
    // empty as though the deadline had passed.
    let s = start();
    let (rstar, shinobi) = setup(&s);

    let mut b = Client::connect(s.port);
    b.hello("SHINOBI", &shinobi);

    let port = s.port;
    let token = rstar.clone();
    let poster = std::thread::spawn(move || {
        let mut a = Client::connect(port);
        a.hello("RSTAR", &token);
        std::thread::sleep(Duration::from_millis(200));
        a.send(serde_json::json!({"op":"post","channel":"toolchains","kind":"finding","subject":"not for you","body":"b"}));
    });

    let began = Instant::now();
    let got = b.send(serde_json::json!({"op":"poll","since":0,"wait_secs":2}));
    let waited = began.elapsed();
    poster.join().unwrap();

    assert!(got["messages"].as_array().unwrap().is_empty(), "got: {got}");
    assert!(
        waited >= Duration::from_millis(1800),
        "an irrelevant post cut the wait short at {waited:?}"
    );
}

#[test]
fn a_malformed_request_is_answered_rather_than_dropped() {
    // Silence would leave a client unable to tell a parse failure from a hung
    // server, which is the ambiguity this project keeps removing.
    let s = start();
    setup(&s);
    let mut c = Client::connect(s.port);
    let r = c.raw("{this is not json");
    assert_eq!(r["re"], "error");
    assert!(r["message"].as_str().unwrap().contains("malformed"), "got: {r}");
}

#[test]
fn learn_then_search_over_the_wire() {
    let s = start();
    let (rstar, shinobi) = setup(&s);

    let mut a = Client::connect(s.port);
    a.hello("RSTAR", &rstar);
    let learned = a.send(serde_json::json!({
        "op":"learn","name":"pcfx-o2","description":"v810 collapses loops at -O2",
        "kind":"finding","body":"sixteen draw calls became one"
    }));
    assert_eq!(learned["re"], "learned", "got: {learned}");

    // Another agent finds it. This is the exchange the project exists for: one
    // agent's finding reaching another that never saw it happen.
    let mut b = Client::connect(s.port);
    b.hello("SHINOBI", &shinobi);
    let found = b.send(serde_json::json!({"op":"search","query":"v810","limit":10}));
    assert_eq!(found["re"], "found", "got: {found}");
    let k = found["knowledge"].as_array().unwrap();
    assert_eq!(k.len(), 1, "got: {found}");
    assert_eq!(k[0]["name"], "pcfx-o2");
    assert_eq!(k[0]["author"], "RSTAR");
}

#[test]
fn a_correction_replaces_what_it_corrects() {
    // Corrections are the most valuable and least captured kind. A store that
    // kept both versions would hand the reader two contradictory claims and make
    // them do the reconciling.
    let s = start();
    let (rstar, shinobi) = setup(&s);

    let mut a = Client::connect(s.port);
    a.hello("RSTAR", &rstar);
    a.send(serde_json::json!({
        "op":"learn","name":"pce-lto","description":"the pin",
        "kind":"finding","body":"-O0 is the pin, measured on the PCE floor"
    }));

    let mut b = Client::connect(s.port);
    b.hello("SHINOBI", &shinobi);
    b.send(serde_json::json!({
        "op":"learn","name":"pce-lto","description":"the pin, corrected",
        "kind":"correction","body":"-fno-lto alone is the pin; -O0 was never it"
    }));

    let found = a.send(serde_json::json!({"op":"search","query":"pin","limit":10}));
    let k = found["knowledge"].as_array().unwrap();
    assert_eq!(k.len(), 1, "one corrected claim, not two: {found}");
    assert_eq!(k[0]["kind"], "correction");
    assert_eq!(k[0]["author"], "SHINOBI", "who says this now");

    // And the superseded text is gone from the index, not merely outranked.
    // "measured" appears only in the body that was replaced — searching for
    // "-O0" would not do, since the correction mentions it too.
    let stale = a.send(serde_json::json!({"op":"search","query":"measured","limit":10}));
    assert!(
        stale["knowledge"].as_array().unwrap().is_empty(),
        "the old body must not still be findable: {stale}"
    );
}

#[test]
fn knowledge_arrives_in_the_next_agent_s_bootstrap() {
    // The profile IS the bootstrap: an agent joining is handed the curated
    // knowledge, not a message tail. This is the whole "joined at the handshake"
    // argument, tested.
    let s = start();
    let (rstar, shinobi) = setup(&s);
    let mut a = Client::connect(s.port);
    a.hello("RSTAR", &rstar);
    a.send(serde_json::json!({
        "op":"learn","name":"zp-determinism","description":"MEGA65 lottery",
        "kind":"finding","body":"MOSZeroPageAlloc pointer order"
    }));

    let mut b = Client::connect(s.port);
    let welcome = b.hello("SHINOBI", &shinobi);
    let k = welcome["profile"]["knowledge"].as_array().unwrap();
    assert_eq!(k.len(), 1, "handed at the handshake: {welcome}");
    assert_eq!(k[0]["name"], "zp-determinism");
}

#[test]
fn a_search_query_full_of_punctuation_is_answered_not_an_error() {
    let s = start();
    let (rstar, _) = setup(&s);
    let mut c = Client::connect(s.port);
    c.hello("RSTAR", &rstar);
    for q in ["\"unbalanced", "AND OR", "*", "a\"b(c)"] {
        let r = c.send(serde_json::json!({"op":"search","query":q,"limit":5}));
        assert_eq!(r["re"], "found", "query {q:?} should answer, not error: {r}");
    }
}

#[test]
fn a_new_agent_is_not_handed_the_whole_history_as_news() {
    let s = start();
    let (rstar, shinobi) = setup(&s);
    let mut a = Client::connect(s.port);
    a.hello("RSTAR", &rstar);
    a.send(serde_json::json!({"op":"post","channel":"shout","kind":"shout","subject":"before","body":"b"}));

    let mut b = Client::connect(s.port);
    let welcome = b.hello("SHINOBI", &shinobi);
    let since = welcome["since"].as_i64().unwrap();
    assert!(since > 0, "the high water at joining should not be zero: {welcome}");

    let got = b.send(serde_json::json!({"op":"poll","since":since,"wait_secs":0}));
    assert!(
        got["messages"].as_array().unwrap().is_empty(),
        "joining is not the same as catching up: {got}"
    );
}
