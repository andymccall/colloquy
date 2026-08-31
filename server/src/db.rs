//! The store: SQLite, one file, opened per connection thread.
//!
//! # Why the schema looks like this
//!
//! `proto` defines the wire types and this defines their durable shape. Where
//! the two differ it is deliberate and noted, because a schema that quietly
//! disagrees with the protocol is the failure the shared crate exists to
//! prevent.
//!
//! # Tokens are stored as hashes, never as tokens
//!
//! A token is a bearer credential: whoever holds it *is* that agent, and the
//! server establishes `from` from it. Storing the plaintext would mean anyone
//! who reads the database file can speak as any agent — including, eventually,
//! into another model's context, which is the whole thing `proto` warns about.
//!
//! No salt and no key stretching, deliberately: these are 256 bits of
//! `/dev/urandom`, not passwords. A dictionary does not exist to attack.

use anyhow::{Context, Result, bail};
use colloquy_proto::{AgentId, ChannelId, Kind, Knowledge, Message, Profile};
use rusqlite::{Connection, OptionalExtension, params};
use std::io::Read;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS agent (
    name        TEXT PRIMARY KEY,
    token_hash  TEXT NOT NULL,
    charter     TEXT NOT NULL DEFAULT '',
    created_at  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS channel (
    name TEXT PRIMARY KEY
);

CREATE TABLE IF NOT EXISTS membership (
    agent   TEXT NOT NULL REFERENCES agent(name) ON DELETE CASCADE,
    channel TEXT NOT NULL REFERENCES channel(name) ON DELETE CASCADE,
    PRIMARY KEY (agent, channel)
);

-- `sender` rather than `from`, because FROM is a SQL keyword and quoting it
-- everywhere invites the one place somebody forgets. The wire field is `from`;
-- the mapping happens here, once.
CREATE TABLE IF NOT EXISTS message (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    channel TEXT NOT NULL,
    sender  TEXT NOT NULL,
    kind    TEXT NOT NULL,
    subject TEXT NOT NULL,
    body    TEXT NOT NULL,
    at      INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS message_by_id ON message(id);

-- `name` is UNIQUE, and that is the whole retention policy for knowledge.
--
-- Claude Code's memory/ directory is one file per fact, updated in place when
-- the fact changes, and it works because of that: a claim that has been revised
-- reads as one corrected thing rather than as two contradictory ones with a
-- date to compare. An append-only knowledge table would accumulate every
-- superseded version and make the reader do the reconciling, which is exactly
-- the archaeology the design doc rejects.
--
-- So Learn upserts. Messages are the append-only stream; knowledge is the
-- curated document.
CREATE TABLE IF NOT EXISTS knowledge (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    name        TEXT NOT NULL UNIQUE,
    description TEXT NOT NULL,
    kind        TEXT NOT NULL,
    body        TEXT NOT NULL,
    author      TEXT NOT NULL,
    at          INTEGER NOT NULL
);

-- Keyword search, which the design says must be earned before anything cleverer
-- is reached for. FTS5 is compiled into the bundled SQLite here -- checked, not
-- assumed. `content=` makes this an index over the real table rather than a
-- second copy of the text that can drift from it.
CREATE VIRTUAL TABLE IF NOT EXISTS knowledge_fts USING fts5(
    name, description, body,
    content='knowledge', content_rowid='id'
);

-- The triggers are what stop the index and the table disagreeing. Without them
-- a search returns yesterday's text with today's confidence.
CREATE TRIGGER IF NOT EXISTS knowledge_ai AFTER INSERT ON knowledge BEGIN
    INSERT INTO knowledge_fts(rowid, name, description, body)
    VALUES (new.id, new.name, new.description, new.body);
END;
CREATE TRIGGER IF NOT EXISTS knowledge_ad AFTER DELETE ON knowledge BEGIN
    INSERT INTO knowledge_fts(knowledge_fts, rowid, name, description, body)
    VALUES ('delete', old.id, old.name, old.description, old.body);
END;
CREATE TRIGGER IF NOT EXISTS knowledge_au AFTER UPDATE ON knowledge BEGIN
    INSERT INTO knowledge_fts(knowledge_fts, rowid, name, description, body)
    VALUES ('delete', old.id, old.name, old.description, old.body);
    INSERT INTO knowledge_fts(rowid, name, description, body)
    VALUES (new.id, new.name, new.description, new.body);
END;
"#;

pub fn open(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)
        .with_context(|| format!("opening the store at {}", path.display()))?;
    conn.execute_batch(SCHEMA).context("applying the schema")?;
    Ok(conn)
}

// ---------------------------------------------------------------------------
// Tokens
// ---------------------------------------------------------------------------

/// 256 bits from `/dev/urandom`, hex encoded.
///
/// Read from the device rather than pulling in a crate: this is the one random
/// value the whole server needs, and on Linux the file is the interface.
pub fn mint_token() -> Result<String> {
    let mut buf = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .context("opening /dev/urandom")?
        .read_exact(&mut buf)
        .context("reading /dev/urandom")?;
    Ok(hex(&buf))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// SHA-256, implemented here rather than pulled in.
///
/// One small well-specified function against one dependency in the trust path
/// of authentication. It is tested against the published vectors below; if that
/// test ever fails, every token in the database is unverifiable and the failure
/// is loud rather than subtle.
pub fn sha256_hex(input: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    let mut msg = input.to_vec();
    let bitlen = (input.len() as u64) * 8;
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bitlen.to_be_bytes());

    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, v) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *slot = slot.wrapping_add(v);
        }
    }
    h.iter().map(|w| format!("{w:08x}")).collect()
}

/// Compare without leaking where two hashes first differ.
///
/// The leak is small — these are hashes, not secrets — but a comparison that
/// short-circuits is the kind of thing that gets copied into a place where it
/// matters.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// ---------------------------------------------------------------------------
// Agents
// ---------------------------------------------------------------------------

/// Register an agent and return its token — the only time the token exists in
/// readable form. It is not recoverable afterwards, only replaceable.
pub fn register(conn: &Connection, agent: &str, charter: &str) -> Result<String> {
    validate_agent_name(agent)?;
    let token = mint_token()?;
    conn.execute(
        "INSERT INTO agent (name, token_hash, charter, created_at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(name) DO UPDATE SET token_hash = excluded.token_hash",
        params![agent, sha256_hex(token.as_bytes()), charter, now()],
    )
    .with_context(|| format!("registering {agent}"))?;
    Ok(token)
}

/// A name that is going to appear in a rendered envelope and in a directory
/// name on the client side. Constrain it here, once, at the only place it
/// enters the system.
pub fn validate_agent_name(agent: &str) -> Result<()> {
    if agent.is_empty() || agent.len() > 64 {
        bail!("an agent name must be 1..64 characters");
    }
    if !agent
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        bail!("agent name {agent:?} has characters that are not [A-Za-z0-9._-]");
    }
    if agent.starts_with('.') {
        bail!("agent name {agent:?} would hide or escape on the client side");
    }
    Ok(())
}

/// Authenticate. Returns the agent name **established by the server**, which is
/// the only name any later message is allowed to be attributed to.
pub fn authenticate(conn: &Connection, agent: &str, token: &str) -> Result<String> {
    let stored: Option<String> = conn
        .query_row(
            "SELECT token_hash FROM agent WHERE name = ?1",
            params![agent],
            |r| r.get(0),
        )
        .optional()?;
    // The same error either way: distinguishing "no such agent" from "wrong
    // token" tells an attacker which half to work on.
    let Some(stored) = stored else {
        bail!("authentication failed");
    };
    if !constant_time_eq(&stored, &sha256_hex(token.as_bytes())) {
        bail!("authentication failed");
    }
    Ok(agent.to_string())
}

pub fn join_channel(conn: &Connection, agent: &str, channel: &str) -> Result<()> {
    conn.execute("INSERT OR IGNORE INTO channel (name) VALUES (?1)", params![channel])?;
    conn.execute(
        "INSERT OR IGNORE INTO membership (agent, channel) VALUES (?1, ?2)",
        params![agent, channel],
    )?;
    Ok(())
}

pub fn channels_of(conn: &Connection, agent: &str) -> Result<Vec<ChannelId>> {
    let mut stmt =
        conn.prepare("SELECT channel FROM membership WHERE agent = ?1 ORDER BY channel")?;
    let rows = stmt.query_map(params![agent], |r| r.get::<_, String>(0))?;
    Ok(rows.filter_map(|r| r.ok()).map(ChannelId).collect())
}

// ---------------------------------------------------------------------------
// The bootstrap
// ---------------------------------------------------------------------------

/// What an agent is handed when it joins.
///
/// Deliberately not "the last N messages" — see `docs/design.md`. The knowledge
/// is a curated, mutable document a human can open and correct, and the
/// channels tell it what it is in.
pub fn profile(conn: &Connection, agent: &str) -> Result<Profile> {
    let charter: String = conn
        .query_row("SELECT charter FROM agent WHERE name = ?1", params![agent], |r| r.get(0))
        .optional()?
        .unwrap_or_default();
    Ok(Profile {
        agent: AgentId(agent.to_string()),
        charter,
        knowledge: knowledge_all(conn)?,
        channels: channels_of(conn, agent)?,
    })
}

pub fn kind_from_str(s: &str) -> Kind {
    match s {
        "finding" => Kind::Finding,
        "heuristic" => Kind::Heuristic,
        "correction" => Kind::Correction,
        "shout" => Kind::Shout,
        _ => Kind::Chat,
    }
}

pub fn kind_to_str(k: Kind) -> &'static str {
    match k {
        Kind::Chat => "chat",
        Kind::Finding => "finding",
        Kind::Heuristic => "heuristic",
        Kind::Correction => "correction",
        Kind::Shout => "shout",
    }
}

fn knowledge_all(conn: &Connection) -> Result<Vec<Knowledge>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, description, kind, body, author, at FROM knowledge ORDER BY id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(Knowledge {
            id: r.get(0)?,
            name: r.get(1)?,
            description: r.get(2)?,
            kind: kind_from_str(&r.get::<_, String>(3)?),
            body: r.get(4)?,
            author: AgentId(r.get::<_, String>(5)?),
            at: r.get(6)?,
        })
    })?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

/// Append a message. `sender` is the **authenticated** name, never one taken
/// from the request body; the signature takes them separately so that the two
/// cannot be confused at a call site.
pub fn post(
    conn: &Connection,
    sender: &str,
    channel: &ChannelId,
    kind: Kind,
    subject: &str,
    body: &str,
) -> Result<i64> {
    conn.execute("INSERT OR IGNORE INTO channel (name) VALUES (?1)", params![channel.0])?;
    conn.execute(
        "INSERT INTO message (channel, sender, kind, subject, body, at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![channel.0, sender, kind_to_str(kind), subject, body, now()],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Messages after `since` that `agent` can see: the channels it is in, plus
/// `shout`, which reaches everyone by design.
pub fn messages_for(conn: &Connection, agent: &str, since: i64, limit: i64) -> Result<Vec<Message>> {
    let mut stmt = conn.prepare(
        "SELECT id, channel, sender, kind, subject, body, at
           FROM message
          WHERE id > ?1
            AND sender <> ?2
            AND (kind = 'shout' OR channel IN (SELECT channel FROM membership WHERE agent = ?2))
          ORDER BY id
          LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![since, agent, limit], |r| {
        Ok(Message {
            id: r.get(0)?,
            channel: ChannelId(r.get::<_, String>(1)?),
            from: AgentId(r.get::<_, String>(2)?),
            kind: kind_from_str(&r.get::<_, String>(3)?),
            subject: r.get(4)?,
            body: r.get(5)?,
            at: r.get(6)?,
        })
    })?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

pub fn high_water(conn: &Connection) -> Result<i64> {
    Ok(conn
        .query_row("SELECT COALESCE(MAX(id), 0) FROM message", [], |r| r.get(0))
        .unwrap_or(0))
}

/// Record something learned, at the moment it was learned.
///
/// Upserts on `name`. Recording a fact twice under one name is a revision, not
/// a second fact — see the schema comment. The author and time become those of
/// the revision, because the question a reader asks is "who says this now".
pub fn learn(
    conn: &Connection,
    author: &str,
    name: &str,
    description: &str,
    kind: Kind,
    body: &str,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO knowledge (name, description, kind, body, author, at)
              VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(name) DO UPDATE SET
              description = excluded.description,
              kind        = excluded.kind,
              body        = excluded.body,
              author      = excluded.author,
              at          = excluded.at",
        params![name, description, kind_to_str(kind), body, author, now()],
    )
    .with_context(|| format!("recording {name:?}"))?;
    Ok(conn.query_row("SELECT id FROM knowledge WHERE name = ?1", params![name], |r| r.get(0))?)
}

/// Keyword search over the curated knowledge.
///
/// The query is user text going into an FTS5 `MATCH`, where bare punctuation is
/// syntax rather than words — an unbalanced quote is a hard error, not a search
/// for a quote. So it is reduced to bare terms and re-quoted. That loses FTS5's
/// operators, which is the right trade for a query an agent typed from a symptom
/// rather than composed.
pub fn search(conn: &Connection, query: &str, limit: u32) -> Result<Vec<Knowledge>> {
    let terms: Vec<String> = query
        .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-'))
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{t}\""))
        .collect();
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    let match_expr = terms.join(" OR ");

    let mut stmt = conn.prepare(
        "SELECT k.id, k.name, k.description, k.kind, k.body, k.author, k.at
           FROM knowledge_fts f
           JOIN knowledge k ON k.id = f.rowid
          WHERE knowledge_fts MATCH ?1
          ORDER BY bm25(knowledge_fts)
          LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![match_expr, limit as i64], |r| {
        Ok(Knowledge {
            id: r.get(0)?,
            name: r.get(1)?,
            description: r.get(2)?,
            kind: kind_from_str(&r.get::<_, String>(3)?),
            body: r.get(4)?,
            author: AgentId(r.get::<_, String>(5)?),
            at: r.get(6)?,
        })
    })?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(SCHEMA).unwrap();
        c
    }

    #[test]
    fn sha256_matches_the_published_vectors() {
        // If this fails, every token in the database is unverifiable. It is here
        // so that failure is loud rather than subtle.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // Crosses the 64-byte block boundary and the length-padding edge.
        assert_eq!(
            sha256_hex(&b"a".repeat(1_000_000)),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn the_token_is_not_stored() {
        let c = mem();
        let token = register(&c, "RSTAR", "the API").unwrap();
        let stored: String = c
            .query_row("SELECT token_hash FROM agent WHERE name='RSTAR'", [], |r| r.get(0))
            .unwrap();
        assert_ne!(stored, token, "the plaintext token must not be in the row");
        assert_eq!(stored, sha256_hex(token.as_bytes()));
    }

    #[test]
    fn authentication_accepts_the_token_and_nothing_else() {
        let c = mem();
        let token = register(&c, "RSTAR", "").unwrap();
        assert_eq!(authenticate(&c, "RSTAR", &token).unwrap(), "RSTAR");
        assert!(authenticate(&c, "RSTAR", "wrong").is_err());
        assert!(authenticate(&c, "NOBODY", &token).is_err());
    }

    #[test]
    fn an_unknown_agent_and_a_bad_token_fail_identically() {
        // Telling them apart tells an attacker which half to work on.
        let c = mem();
        let token = register(&c, "RSTAR", "").unwrap();
        let a = authenticate(&c, "RSTAR", "wrong").unwrap_err().to_string();
        let b = authenticate(&c, "GHOST", &token).unwrap_err().to_string();
        assert_eq!(a, b);
    }

    #[test]
    fn tokens_differ_between_agents() {
        let c = mem();
        let a = register(&c, "A", "").unwrap();
        let b = register(&c, "B", "").unwrap();
        assert_ne!(a, b);
        assert_eq!(a.len(), 64, "256 bits, hex");
    }

    #[test]
    fn re_registering_replaces_the_token_and_the_old_one_stops_working() {
        let c = mem();
        let first = register(&c, "A", "").unwrap();
        let second = register(&c, "A", "").unwrap();
        assert!(authenticate(&c, "A", &second).is_ok());
        assert!(
            authenticate(&c, "A", &first).is_err(),
            "a replaced token must not still open the door"
        );
    }

    #[test]
    fn a_shout_reaches_an_agent_that_shares_no_channel() {
        let c = mem();
        register(&c, "RSTAR", "").unwrap();
        register(&c, "SHINOBI", "").unwrap();
        join_channel(&c, "RSTAR", "toolchains").unwrap();
        join_channel(&c, "SHINOBI", "pacman").unwrap();

        post(&c, "RSTAR", &ChannelId("toolchains".into()), Kind::Finding, "private", "b").unwrap();
        post(&c, "RSTAR", &ChannelId("shout".into()), Kind::Shout, "anyone?", "b").unwrap();

        let seen = messages_for(&c, "SHINOBI", 0, 100).unwrap();
        assert_eq!(seen.len(), 1, "the finding is not in SHINOBI's channels");
        assert_eq!(seen[0].subject, "anyone?");
    }

    #[test]
    fn an_agent_does_not_receive_its_own_messages() {
        let c = mem();
        register(&c, "RSTAR", "").unwrap();
        join_channel(&c, "RSTAR", "toolchains").unwrap();
        post(&c, "RSTAR", &ChannelId("toolchains".into()), Kind::Chat, "mine", "b").unwrap();
        assert!(
            messages_for(&c, "RSTAR", 0, 100).unwrap().is_empty(),
            "waking an agent with its own message is a mistake already made once"
        );
    }

    #[test]
    fn the_profile_is_not_a_message_tail() {
        let c = mem();
        register(&c, "RSTAR", "the API, and the thing everything else is built on").unwrap();
        join_channel(&c, "RSTAR", "toolchains").unwrap();
        post(&c, "RSTAR", &ChannelId("toolchains".into()), Kind::Chat, "s", "b").unwrap();

        let p = profile(&c, "RSTAR").unwrap();
        assert_eq!(p.agent.0, "RSTAR");
        assert!(p.charter.starts_with("the API"));
        assert_eq!(p.channels, vec![ChannelId("toolchains".into())]);
        assert!(p.knowledge.is_empty(), "knowledge is curated, not the recent traffic");
    }

    #[test]
    fn learning_the_same_name_twice_is_a_revision_not_a_duplicate() {
        // The property that makes memory/ readable: a corrected claim reads as
        // one corrected thing, not two contradictory ones with dates to compare.
        let c = mem();
        register(&c, "RSTAR", "").unwrap();
        register(&c, "SHINOBI", "").unwrap();
        let a = learn(&c, "RSTAR", "pce-lto", "d1", Kind::Finding, "-O0 is the pin").unwrap();
        let b = learn(&c, "SHINOBI", "pce-lto", "d2", Kind::Correction, "-fno-lto alone is the pin").unwrap();
        assert_eq!(a, b, "same name, same row");

        let all = knowledge_all(&c).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].body, "-fno-lto alone is the pin");
        assert_eq!(all[0].author.0, "SHINOBI", "who says this NOW");
        assert_eq!(all[0].kind, Kind::Correction);
    }

    #[test]
    fn search_finds_by_any_field_and_a_revision_replaces_the_old_text() {
        let c = mem();
        register(&c, "RSTAR", "").unwrap();
        learn(&c, "RSTAR", "pcfx-o2", "v810 collapses loops", Kind::Finding,
              "sixteen draw calls became one at -O2").unwrap();
        learn(&c, "RSTAR", "zp-determinism", "MEGA65 lottery", Kind::Finding,
              "MOSZeroPageAlloc pointer order").unwrap();

        assert_eq!(search(&c, "v810", 10).unwrap().len(), 1, "by description");
        assert_eq!(search(&c, "MOSZeroPageAlloc", 10).unwrap().len(), 1, "by body");
        assert_eq!(search(&c, "pcfx-o2", 10).unwrap().len(), 1, "by name");
        assert_eq!(search(&c, "nothing-matches-this", 10).unwrap().len(), 0);

        // The index must follow a revision, or search returns yesterday's text
        // with today's confidence.
        learn(&c, "RSTAR", "pcfx-o2", "retracted", Kind::Correction,
              "it was never the optimiser, it was the histogram").unwrap();
        assert_eq!(search(&c, "sixteen", 10).unwrap().len(), 0, "the old body is gone from the index");
        assert_eq!(search(&c, "histogram", 10).unwrap().len(), 1, "the new body is in it");
    }

    #[test]
    fn a_query_full_of_fts_syntax_does_not_blow_up() {
        // A query an agent typed from a symptom, not one it composed. Bare
        // punctuation is FTS5 syntax, and an unbalanced quote is a hard error
        // rather than a search for a quote.
        let c = mem();
        register(&c, "RSTAR", "").unwrap();
        learn(&c, "RSTAR", "n", "d", Kind::Finding, "the linker dropped a symbol").unwrap();
        for q in ["\"unbalanced", "AND OR NOT", "*", "", "()", "linker AND", "a\"b(c)"] {
            assert!(search(&c, q, 10).is_ok(), "query {q:?} should not error");
        }
        assert_eq!(search(&c, "  linker!!  ", 10).unwrap().len(), 1, "punctuation is stripped, terms survive");
    }

    #[test]
    fn an_agent_name_cannot_escape_on_the_client_side() {
        let c = mem();
        for bad in ["../etc", "a/b", ".hidden", ""] {
            assert!(register(&c, bad, "").is_err(), "{bad:?} should be refused");
        }
        assert!(register(&c, "RSTAR-1.0_x", "").is_ok());
    }
}
