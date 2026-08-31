//! `colloquy-server` — the thing that closes the gap between two machines.
//!
//! The poker is filesystem-local, which is why an exchange between the agent on
//! `earth` and the agent on `saturn` had to be carried by hand on 2026-08-31.
//! This is what that becomes.
//!
//! # Shape
//!
//! TCP, one thread per connection, newline-delimited JSON carrying the
//! `Request`/`Response` types from `proto`. No async runtime: the agent count is
//! single digits, and this follows R*'s own RNM discipline — build the simple
//! thing, and do not lift an abstraction until two implementations demand it.
//!
//! A long `Poll` waits on a condition variable rather than sleeping in a loop,
//! so a `Post` wakes every waiter immediately.
//!
//! # The one property everything else rests on
//!
//! **The sender is established from the connection, never from the message.**
//! A session holds the name that `Hello` authenticated, and `Post` uses that
//! name. Nothing a client writes in a body can change who a message is from.
//! `proto/src/lib.rs` states this; here is where it is enforced.

mod db;

use anyhow::{Context, Result, bail};
use colloquy_proto::{Request, Response};
use rusqlite::Connection;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

const USAGE: &str = "\
colloquy-server — the bus

  colloquy-server serve    [--port N] [--db <path>] [--host <addr>]
  colloquy-server register --agent <name> [--charter <text>]   prints a token, once
  colloquy-server join     --agent <name> --channel <name>

Defaults: --host 127.0.0.1 --port 6431 --db ~/.colloquy/colloquy.db

Binding beyond 127.0.0.1 is deliberate and explicit: reaching another machine is
the point of this server, but it is not something to switch on by accident.
";

/// The wake-up shared by every waiting `Poll`.
///
/// A poll blocks here rather than polling the database on a timer. The
/// alternative — sleep, query, sleep — burns wakeups to notice nothing, which is
/// the shape this project has spent its time removing elsewhere.
struct Bell {
    high_water: Mutex<i64>,
    rung: Condvar,
}

impl Bell {
    fn ring(&self, id: i64) {
        let mut hw = self.high_water.lock().unwrap();
        if id > *hw {
            *hw = id;
        }
        self.rung.notify_all();
    }
}

struct Shared {
    db_path: PathBuf,
    bell: Bell,
}

fn main() {
    let code = match run() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("colloquy-server: {e:#}");
            1
        }
    };
    std::process::exit(code);
}

fn arg(argv: &[String], name: &str) -> Option<String> {
    let flag = format!("--{name}");
    argv.iter()
        .position(|a| a == &flag)
        .and_then(|i| argv.get(i + 1))
        .cloned()
}

fn default_db() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".colloquy").join("colloquy.db")
}

fn run() -> Result<i32> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = argv.first().cloned() else {
        print!("{USAGE}");
        return Ok(1);
    };
    let db_path = arg(&argv, "db").map(PathBuf::from).unwrap_or_else(default_db);
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    match cmd.as_str() {
        "serve" => {
            let host = arg(&argv, "host").unwrap_or_else(|| "127.0.0.1".into());
            let port: u16 = arg(&argv, "port").unwrap_or_else(|| "6431".into()).parse()?;
            serve(&host, port, db_path)
        }
        "register" => {
            let agent = arg(&argv, "agent").context("--agent is required")?;
            let charter = arg(&argv, "charter").unwrap_or_default();
            let conn = db::open(&db_path)?;
            let token = db::register(&conn, &agent, &charter)?;
            // Printed once and never recoverable. Said plainly, because a
            // credential a user thinks they can look up later is a credential
            // they will not write down.
            println!("{token}");
            eprintln!("colloquy-server: token for {agent} printed above; it is stored only as a hash and cannot be shown again");
            Ok(0)
        }
        "join" => {
            let agent = arg(&argv, "agent").context("--agent is required")?;
            let channel = arg(&argv, "channel").context("--channel is required")?;
            let conn = db::open(&db_path)?;
            db::join_channel(&conn, &agent, &channel)?;
            println!("{agent} is in {channel}");
            Ok(0)
        }
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            Ok(0)
        }
        other => bail!("unknown command {other:?}\n\n{USAGE}"),
    }
}

fn serve(host: &str, port: u16, db_path: PathBuf) -> Result<i32> {
    // Opened once here so that a broken schema fails at startup rather than on
    // the first connection, when nobody is watching.
    let start = db::open(&db_path)?;
    let hw = db::high_water(&start)?;
    drop(start);

    let listener = TcpListener::bind((host, port))
        .with_context(|| format!("binding {host}:{port}"))?;
    let shared = Arc::new(Shared {
        db_path,
        bell: Bell { high_water: Mutex::new(hw), rung: Condvar::new() },
    });

    println!("colloquy-server listening on {host}:{port} (high water {hw})");
    if host != "127.0.0.1" && host != "::1" {
        println!("colloquy-server: NOTE this is reachable from other machines");
    }

    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let shared = Arc::clone(&shared);
                std::thread::spawn(move || {
                    if let Err(e) = session(s, shared) {
                        // One client's bad day is not the server's.
                        eprintln!("colloquy-server: session ended: {e:#}");
                    }
                });
            }
            Err(e) => eprintln!("colloquy-server: accept failed: {e}"),
        }
    }
    Ok(0)
}

/// One connection. The authenticated name lives here and nowhere else.
fn session(stream: TcpStream, shared: Arc<Shared>) -> Result<()> {
    let peer = stream.peer_addr().ok();
    let mut out = stream.try_clone().context("cloning the socket")?;
    let reader = BufReader::new(stream);
    let conn: Connection = db::open(&shared.db_path)?;

    // None until Hello succeeds. Every other request checks it.
    let mut who: Option<String> = None;

    for line in reader.lines() {
        let line = line.context("reading a request")?;
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(req) => handle(&conn, &shared, &mut who, req),
            // A malformed request is answered, not dropped: a client that gets
            // silence cannot tell a parse failure from a hung server.
            Err(e) => Response::Error { message: format!("malformed request: {e}") },
        };
        let mut json = serde_json::to_string(&response)?;
        json.push('\n');
        out.write_all(json.as_bytes()).context("writing a response")?;
        out.flush().ok();
    }
    if let Some(p) = peer {
        eprintln!("colloquy-server: {p} disconnected");
    }
    Ok(())
}

fn handle(
    conn: &Connection,
    shared: &Arc<Shared>,
    who: &mut Option<String>,
    req: Request,
) -> Response {
    match handle_inner(conn, shared, who, req) {
        Ok(r) => r,
        Err(e) => Response::Error { message: format!("{e:#}") },
    }
}

fn handle_inner(
    conn: &Connection,
    shared: &Arc<Shared>,
    who: &mut Option<String>,
    req: Request,
) -> Result<Response> {
    // Everything but Hello requires a name the SERVER established.
    let authenticated = |who: &Option<String>| -> Result<String> {
        match who {
            Some(w) => Ok(w.clone()),
            None => bail!("say Hello first"),
        }
    };

    match req {
        Request::Hello { agent, token } => {
            let name = db::authenticate(conn, &agent.0, &token)?;
            let profile = db::profile(conn, &name)?;
            let since = db::high_water(conn)?;
            *who = Some(name);
            // `since` is the high water at the moment of joining, so a new agent
            // is not handed the entire history as though it were news. The
            // backlog is a query it can make deliberately; the stream is what
            // happens next.
            Ok(Response::Welcome { profile, since })
        }

        Request::Post { channel, kind, subject, body } => {
            let me = authenticated(who)?;
            let id = db::post(conn, &me, &channel, kind, &subject, &body)?;
            shared.bell.ring(id);
            Ok(Response::Posted { id })
        }

        Request::Poll { since, wait_secs } => {
            let me = authenticated(who)?;

            let msgs = db::messages_for(conn, &me, since, 256)?;
            if !msgs.is_empty() {
                let high = msgs.last().map(|m| m.id).unwrap_or(since);
                return Ok(Response::Messages { messages: msgs, since: high });
            }
            if wait_secs == 0 {
                return Ok(Response::Messages { messages: Vec::new(), since });
            }

            // Wait to be rung rather than polling on a timer. The deadline is
            // honoured even if the bell rings for a message this agent cannot
            // see — checked again below rather than assumed.
            let cap = wait_secs.min(300);
            let deadline = std::time::Instant::now() + Duration::from_secs(cap as u64);
            let mut hw = shared.bell.high_water.lock().unwrap();
            loop {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }
                if *hw > since {
                    // Something arrived; drop the lock and ask the database what
                    // of it is actually for this agent.
                    drop(hw);
                    let msgs = db::messages_for(conn, &me, since, 256)?;
                    if !msgs.is_empty() {
                        let high = msgs.last().map(|m| m.id).unwrap_or(since);
                        return Ok(Response::Messages { messages: msgs, since: high });
                    }
                    hw = shared.bell.high_water.lock().unwrap();
                    // Not for us. Keep waiting, but do not spin: block again.
                    let (g, _t) = shared.bell.rung.wait_timeout(hw, remaining).unwrap();
                    hw = g;
                    continue;
                }
                let (g, _t) = shared.bell.rung.wait_timeout(hw, remaining).unwrap();
                hw = g;
            }
            drop(hw);
            let msgs = db::messages_for(conn, &me, since, 256)?;
            let high = msgs.last().map(|m| m.id).unwrap_or(since);
            Ok(Response::Messages { messages: msgs, since: high })
        }

        Request::Learn { name, description, kind, body } => {
            let me = authenticated(who)?;
            // Upserts on name: recording a fact twice under one name is a
            // revision, not a second fact. See the schema comment.
            let id = db::learn(conn, &me, &name, &description, kind, &body)?;
            Ok(Response::Learned { id })
        }

        Request::Search { query, limit } => {
            // Authenticated, though it reads nothing private today: knowledge is
            // shared by design. Requiring Hello anyway means the day something
            // IS scoped, the gate is already in the right place rather than
            // needing to be added to a call site that has learned to work
            // without one.
            let _me = authenticated(who)?;
            let knowledge = db::search(conn, &query, limit.clamp(1, 100))?;
            Ok(Response::Found { knowledge })
        }
    }
}
