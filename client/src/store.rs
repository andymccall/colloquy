//! The on-disk shape of the receipt loop.
//!
//! # Why there is a pending record at all
//!
//! The inbox is the agent's; it may read, rewrite or delete what is in it. The
//! poker therefore keeps its own record of what it delivered, in a directory
//! the agent has no reason to touch. Liveness is measured against that record
//! rather than against the inbox, because a measurement the subject can edit is
//! not a measurement.
//!
//! # The layout
//!
//! ```text
//! $COLLOQUY_HOME/                     (default ~/.colloquy)
//!   agents/<agent>/inbox/<id>.msg     the rendered envelope; the agent reads this
//!   agents/<agent>/receipts/<id>.json written by the agent when it consumes one
//!   agents/<agent>/pending/<id>.json  the poker's record; the agent never writes here
//!   alerts/<id>.alert                 an escalation that itself failed
//! ```
//!
//! Plain files, one per message, because the whole point of the prototype is to
//! validate the loop before a server exists — and because a human can `cat` any
//! of it when it misbehaves.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// What the poker recorded when it delivered. Owned by the poker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pending {
    pub id: i64,
    pub to: String,
    pub from: String,
    pub kind: String,
    pub subject: String,
    pub delivered_at: i64,
    /// Set once, the first time this delivery was escalated. Its presence is
    /// what stops a deaf agent producing an escalation every sweep — a channel
    /// that cries wolf gets muted, and then the one time it matters nobody
    /// hears it. The same reasoning as `claude-say`'s rate limit.
    pub escalated_at: Option<i64>,
}

/// What the agent wrote when it consumed a delivery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Receipt {
    pub id: i64,
    pub agent: String,
    pub acked_at: i64,
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Rooted at `$COLLOQUY_HOME`, or `~/.colloquy`.
#[derive(Debug, Clone)]
pub struct Store {
    pub root: PathBuf,
}

impl Store {
    pub fn open() -> Result<Self> {
        let root = match std::env::var_os("COLLOQUY_HOME") {
            Some(p) => PathBuf::from(p),
            None => {
                let home = std::env::var_os("HOME").context("HOME is not set")?;
                PathBuf::from(home).join(".colloquy")
            }
        };
        fs::create_dir_all(&root)
            .with_context(|| format!("creating the colloquy root at {}", root.display()))?;
        Ok(Store { root })
    }

    /// An agent name becomes a directory name, so it must not be able to leave
    /// the store. A message can name any recipient it likes.
    fn agent_dir(&self, agent: &str) -> Result<PathBuf> {
        anyhow::ensure!(!agent.is_empty(), "an empty agent name");
        anyhow::ensure!(
            agent
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'),
            "agent name {agent:?} has characters that are not [A-Za-z0-9._-]"
        );
        anyhow::ensure!(
            agent != "." && agent != ".." && !agent.starts_with('.'),
            "agent name {agent:?} would escape or hide in the store"
        );
        Ok(self.root.join("agents").join(agent))
    }

    pub fn inbox_dir(&self, agent: &str) -> Result<PathBuf> {
        Ok(self.agent_dir(agent)?.join("inbox"))
    }
    pub fn pending_dir(&self, agent: &str) -> Result<PathBuf> {
        Ok(self.agent_dir(agent)?.join("pending"))
    }
    pub fn receipts_dir(&self, agent: &str) -> Result<PathBuf> {
        Ok(self.agent_dir(agent)?.join("receipts"))
    }
    pub fn alerts_dir(&self) -> PathBuf {
        self.root.join("alerts")
    }

    pub fn ensure_agent(&self, agent: &str) -> Result<()> {
        for d in [
            self.inbox_dir(agent)?,
            self.pending_dir(agent)?,
            self.receipts_dir(agent)?,
        ] {
            fs::create_dir_all(&d).with_context(|| format!("creating {}", d.display()))?;
        }
        Ok(())
    }

    /// An id that sorts by time and does not collide on a fast sender.
    pub fn next_id(&self, agent: &str) -> Result<i64> {
        let mut id = now_millis();
        let dir = self.pending_dir(agent)?;
        while dir.join(format!("{id}.json")).exists() {
            id += 1;
        }
        Ok(id)
    }

    /// Write the envelope where the agent will find it, and open the pending
    /// record.
    ///
    /// Order matters: the envelope lands **first**. If the process dies between
    /// the two writes, the agent has a message the poker has forgotten — an
    /// unnecessary read. The other order would give a pending record for a
    /// message that was never delivered, and the poker would escalate about a
    /// message nobody could have acked. Of the two, wasting a read is the one
    /// to prefer.
    pub fn deliver(&self, p: &Pending, envelope: &str) -> Result<()> {
        self.ensure_agent(&p.to)?;
        let msg = self.inbox_dir(&p.to)?.join(format!("{}.msg", p.id));
        fs::write(&msg, envelope).with_context(|| format!("writing {}", msg.display()))?;
        self.write_pending(p)
    }

    pub fn write_pending(&self, p: &Pending) -> Result<()> {
        let path = self.pending_dir(&p.to)?.join(format!("{}.json", p.id));
        let json = serde_json::to_string_pretty(p)?;
        fs::write(&path, json).with_context(|| format!("writing {}", path.display()))
    }

    pub fn clear_pending(&self, agent: &str, id: i64) -> Result<()> {
        let path = self.pending_dir(agent)?.join(format!("{id}.json"));
        if path.exists() {
            fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        }
        Ok(())
    }

    pub fn read_pending(&self, agent: &str) -> Result<Vec<Pending>> {
        let dir = self.pending_dir(agent)?;
        let mut out = Vec::new();
        if !dir.exists() {
            return Ok(out);
        }
        for e in fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
            let path = e?.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let text = fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            match serde_json::from_str::<Pending>(&text) {
                Ok(p) => out.push(p),
                // A corrupt record must not stop the sweep: the whole job of
                // this loop is to keep working when something has gone wrong.
                Err(e) => eprintln!("colloquy: skipping unreadable {}: {e}", path.display()),
            }
        }
        out.sort_by_key(|p| p.id);
        Ok(out)
    }

    pub fn write_receipt(&self, r: &Receipt) -> Result<PathBuf> {
        self.ensure_agent(&r.agent)?;
        let path = self.receipts_dir(&r.agent)?.join(format!("{}.json", r.id));
        fs::write(&path, serde_json::to_string_pretty(r)?)
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(path)
    }

    pub fn read_receipt(&self, agent: &str, id: i64) -> Result<Option<Receipt>> {
        let path = self.receipts_dir(agent)?.join(format!("{id}.json"));
        if !path.exists() {
            return Ok(None);
        }
        let text = fs::read_to_string(&path)?;
        Ok(serde_json::from_str(&text).ok())
    }

    /// Agents that have ever been delivered to.
    pub fn known_agents(&self) -> Result<Vec<String>> {
        let dir = self.root.join("agents");
        let mut out = Vec::new();
        if !dir.exists() {
            return Ok(out);
        }
        for e in fs::read_dir(&dir)? {
            let e = e?;
            if e.file_type()?.is_dir() {
                if let Some(n) = e.file_name().to_str() {
                    out.push(n.to_string());
                }
            }
        }
        out.sort();
        Ok(out)
    }

    /// Record an escalation that itself failed — the failure the whole loop
    /// exists to prevent, one level up.
    pub fn write_alert(&self, id: i64, text: &str) -> Result<PathBuf> {
        let dir = self.alerts_dir();
        fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{id}.alert"));
        fs::write(&path, text)?;
        Ok(path)
    }

    pub fn envelope_path(&self, agent: &str, id: i64) -> Result<PathBuf> {
        Ok(self.inbox_dir(agent)?.join(format!("{id}.msg")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory unique to THIS test.
    ///
    /// Naming it from the clock alone was flaky: tests run in parallel, two
    /// that start in the same millisecond share a directory, and two of the
    /// tests below use the same agent name — so one would see the other's
    /// records and fail perhaps one run in ten. A test that fails
    /// intermittently is worse than no test, because it teaches you to re-run
    /// it until it is green.
    fn temp_store() -> (Store, PathBuf) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "colloquy-test-{}-{}-{n}",
            std::process::id(),
            now_millis()
        ));
        fs::create_dir_all(&dir).unwrap();
        (Store { root: dir.clone() }, dir)
    }

    fn pending(to: &str, id: i64) -> Pending {
        Pending {
            id,
            to: to.into(),
            from: "RSTAR".into(),
            kind: "finding".into(),
            subject: "s".into(),
            delivered_at: now(),
            escalated_at: None,
        }
    }

    #[test]
    fn an_agent_name_cannot_escape_the_store() {
        let (s, _d) = temp_store();
        for bad in ["../../etc", "a/b", ".hidden", "", ".."] {
            assert!(
                s.inbox_dir(bad).is_err(),
                "agent name {bad:?} should be refused"
            );
        }
        assert!(s.inbox_dir("RSTAR-1.0_x").is_ok());
    }

    #[test]
    fn deliver_then_ack_resolves() {
        let (s, _d) = temp_store();
        let p = pending("SHINOBI", 1);
        s.deliver(&p, "envelope").unwrap();
        assert_eq!(s.read_pending("SHINOBI").unwrap().len(), 1);
        assert!(s.read_receipt("SHINOBI", 1).unwrap().is_none());

        s.write_receipt(&Receipt { id: 1, agent: "SHINOBI".into(), acked_at: now() })
            .unwrap();
        assert!(s.read_receipt("SHINOBI", 1).unwrap().is_some());
        s.clear_pending("SHINOBI", 1).unwrap();
        assert!(s.read_pending("SHINOBI").unwrap().is_empty());
    }

    #[test]
    fn a_corrupt_pending_record_does_not_stop_the_sweep() {
        let (s, _d) = temp_store();
        s.deliver(&pending("A", 1), "e").unwrap();
        fs::write(s.pending_dir("A").unwrap().join("2.json"), "{ not json").unwrap();
        s.deliver(&pending("A", 3), "e").unwrap();
        let got = s.read_pending("A").unwrap();
        assert_eq!(got.len(), 2, "the readable records still come back");
        assert_eq!(got[0].id, 1);
        assert_eq!(got[1].id, 3);
    }

    #[test]
    fn ids_do_not_collide_within_a_millisecond() {
        let (s, _d) = temp_store();
        s.ensure_agent("A").unwrap();
        let a = s.next_id("A").unwrap();
        s.write_pending(&pending("A", a)).unwrap();
        let b = s.next_id("A").unwrap();
        assert_ne!(a, b);
    }
}
