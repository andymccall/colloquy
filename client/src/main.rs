//! `colloquy` — the poker.
//!
//! The one thing this prototype is for: **liveness measured from the end that
//! cannot go deaf.**
//!
//! `docs/notify-measurements.md` established that the wake channel drops its
//! tail without saying so, and that an agent cannot detect its own deafness —
//! the absence of an event is not an event. So the agent does not watch itself.
//! It writes a receipt when it consumes a delivery, and this process, which is
//! not the one being woken, notices when a receipt does not arrive.
//!
//! ```text
//!   colloquy deliver ──> inbox/<id>.msg ──> the agent's Monitor fires
//!                    └─> pending/<id>.json
//!   the agent reads, then: colloquy ack ──> receipts/<id>.json
//!   colloquy sweep ──> receipt present?  resolve, and report the latency
//!                  └─> absent past the deadline?  escalate, exactly once
//! ```
//!
//! # The wake is a doorbell
//!
//! Nothing of consequence rides on the notification. The envelope is on disk
//! and a woken agent reconciles against the inbox, because the measured tail
//! loss guarantees it will sometimes not be told about the message that
//! mattered most.

mod store;

use anyhow::{Context, Result, bail};
use colloquy_proto::{AgentId, ChannelId, Kind, Message, Provenance, render};
use std::io::Read;
use std::process::Command;
use store::{Pending, Receipt, Store, now};

/// At least one agent is overdue and was escalated.
const EXIT_DEAF: i32 = 7;
/// The escalation itself failed — worse than a deaf agent, because it is the
/// failure this whole loop exists to prevent, one level up.
const EXIT_ESCALATION_FAILED: i32 = 8;

const USAGE: &str = "\
colloquy — the poker: deliver messages, and notice when nobody acknowledges them

  colloquy deliver --to <agent> --from <agent> --subject <text>
                   [--channel <name>] [--kind chat|finding|heuristic|correction|shout]
                   [--body <text> | --body-file <path> | --body -]

  colloquy ack     --agent <agent> --id <id>
  colloquy list    [--agent <agent>]
  colloquy sweep   [--agent <agent>] [--deadline-secs N] [--on-deaf <command>]
  colloquy watch-command --agent <agent>

Exit: 0 all well · 7 an agent was overdue · 8 the escalation failed

The escalation command may exit 75 (EX_TEMPFAIL) to mean suppressed-not-delivered;
it is then retried on the next sweep rather than recorded as done.
Store: $COLLOQUY_HOME, default ~/.colloquy
";

fn main() {
    let code = match run() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("colloquy: {e:#}");
            1
        }
    };
    std::process::exit(code);
}

/// Minimal flag parsing. A dependency would be more capable; this is a
/// prototype whose job is to validate a loop, and every crate added here is one
/// the eventual client may not want.
struct Args {
    flags: Vec<(String, String)>,
}

impl Args {
    fn parse(raw: &[String]) -> Result<Args> {
        let mut flags = Vec::new();
        let mut i = 0;
        while i < raw.len() {
            let a = &raw[i];
            if let Some(name) = a.strip_prefix("--") {
                let val = raw.get(i + 1).cloned().unwrap_or_default();
                if val.starts_with("--") || raw.get(i + 1).is_none() {
                    flags.push((name.to_string(), String::new()));
                    i += 1;
                } else {
                    flags.push((name.to_string(), val));
                    i += 2;
                }
            } else {
                bail!("unexpected argument {a:?}");
            }
        }
        Ok(Args { flags })
    }

    fn opt(&self, name: &str) -> Option<&str> {
        self.flags
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn req(&self, name: &str) -> Result<&str> {
        match self.opt(name) {
            Some(v) if !v.is_empty() => Ok(v),
            _ => bail!("--{name} is required"),
        }
    }
}

fn run() -> Result<i32> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = argv.first().cloned() else {
        print!("{USAGE}");
        return Ok(1);
    };
    let args = Args::parse(&argv[1..])?;
    let store = Store::open()?;

    match cmd.as_str() {
        "deliver" => cmd_deliver(&store, &args),
        "ack" => cmd_ack(&store, &args),
        "list" => cmd_list(&store, &args),
        "sweep" => cmd_sweep(&store, &args),
        "watch-command" => cmd_watch_command(&store, &args),
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            Ok(0)
        }
        other => bail!("unknown command {other:?}\n\n{USAGE}"),
    }
}

fn parse_kind(s: &str) -> Result<Kind> {
    Ok(match s {
        "chat" => Kind::Chat,
        "finding" => Kind::Finding,
        "heuristic" => Kind::Heuristic,
        "correction" => Kind::Correction,
        "shout" => Kind::Shout,
        other => bail!("unknown kind {other:?}"),
    })
}

fn kind_name(k: Kind) -> &'static str {
    match k {
        Kind::Chat => "chat",
        Kind::Finding => "finding",
        Kind::Heuristic => "heuristic",
        Kind::Correction => "correction",
        Kind::Shout => "shout",
    }
}

/// Whether a kind is allowed to wake anybody.
///
/// Measured: the wake channel sustains roughly one notification every two
/// seconds before it starts suppressing. `Chat` is the bulk of the traffic and
/// the least durable, so it must never spend that budget. The wake belongs to
/// the kinds that mean someone is currently acting on something wrong, or is
/// currently stuck — the same rule `claude-say` follows.
fn wakes(k: Kind) -> bool {
    matches!(k, Kind::Correction | Kind::Shout)
}

fn read_body(args: &Args) -> Result<String> {
    if let Some(text) = args.opt("body") {
        if text == "-" {
            let mut s = String::new();
            std::io::stdin().read_to_string(&mut s)?;
            return Ok(s);
        }
        if !text.is_empty() {
            return Ok(text.to_string());
        }
    }
    if let Some(path) = args.opt("body-file") {
        if !path.is_empty() {
            return std::fs::read_to_string(path)
                .with_context(|| format!("reading body from {path}"));
        }
    }
    bail!("one of --body, --body-file or --body - is required")
}

fn cmd_deliver(store: &Store, args: &Args) -> Result<i32> {
    let to = args.req("to")?;
    let from = args.req("from")?;
    let subject = args.req("subject")?;
    let kind = parse_kind(args.opt("kind").filter(|s| !s.is_empty()).unwrap_or("chat"))?;
    let channel = args.opt("channel").filter(|s| !s.is_empty()).unwrap_or("general");
    let body = read_body(args)?;

    store.ensure_agent(to)?;
    let id = store.next_id(to)?;

    let msg = Message {
        id,
        channel: ChannelId(channel.to_string()),
        from: AgentId(from.to_string()),
        kind,
        subject: subject.to_string(),
        body,
        at: now(),
    };

    // There is no server, so nothing verified this sender. Say so.
    let envelope = render(&msg, Provenance::Unverified);

    let pending = Pending {
        id,
        to: to.to_string(),
        from: from.to_string(),
        kind: kind_name(kind).to_string(),
        subject: subject.to_string(),
        delivered_at: now(),
        escalated_at: None,
    };
    store.deliver(&pending, &envelope)?;

    println!("{id}\t{}", store.envelope_path(to, id)?.display());
    if !wakes(kind) {
        println!(
            "note: kind '{}' does not warrant a wake; it is readable on demand",
            kind_name(kind)
        );
    }
    Ok(0)
}

fn cmd_ack(store: &Store, args: &Args) -> Result<i32> {
    let agent = args.req("agent")?;
    let id: i64 = args.req("id")?.parse().context("--id must be a number")?;

    // Acking something never delivered is a bug worth hearing about, not a
    // no-op to absorb quietly.
    let known = store.read_pending(agent)?.iter().any(|p| p.id == id);
    let already = store.read_receipt(agent, id)?.is_some();
    if !known && !already {
        bail!("no pending delivery {id} for {agent} — nothing to acknowledge");
    }

    let r = Receipt { id, agent: agent.to_string(), acked_at: now() };
    let path = store.write_receipt(&r)?;
    println!("acked {id}\t{}", path.display());
    Ok(0)
}

fn cmd_list(store: &Store, args: &Args) -> Result<i32> {
    let agents = match args.opt("agent").filter(|s| !s.is_empty()) {
        Some(a) => vec![a.to_string()],
        None => store.known_agents()?,
    };
    let t = now();
    for agent in agents {
        for p in store.read_pending(&agent)? {
            let acked = store.read_receipt(&agent, p.id)?.is_some();
            println!(
                "{}\t{}\tfrom {}\t{}\tage {}s\t{}{}",
                p.id,
                p.to,
                p.from,
                p.kind,
                t - p.delivered_at,
                if acked { "acked" } else { "UNACKED" },
                if p.escalated_at.is_some() { " (escalated)" } else { "" },
            );
        }
    }
    Ok(0)
}

/// The command run when an agent has gone quiet.
///
/// Defaults to the voice, because a deaf agent is exactly the case the voice
/// exists for: something needs a human and the human is not looking at the
/// screen.
fn escalation_command(args: &Args) -> String {
    if let Some(c) = args.opt("on-deaf").filter(|s| !s.is_empty()) {
        return c.to_string();
    }
    if let Ok(c) = std::env::var("COLLOQUY_ON_DEAF") {
        if !c.is_empty() {
            return c;
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    format!("{home}/bin/claude-say")
}

fn cmd_sweep(store: &Store, args: &Args) -> Result<i32> {
    let deadline: i64 = args
        .opt("deadline-secs")
        .filter(|s| !s.is_empty())
        .unwrap_or("300")
        .parse()
        .context("--deadline-secs must be a number")?;
    let agents = match args.opt("agent").filter(|s| !s.is_empty()) {
        Some(a) => vec![a.to_string()],
        None => store.known_agents()?,
    };

    let t = now();
    let mut exit = 0;

    for agent in agents {
        let mut overdue: Vec<Pending> = Vec::new();

        for p in store.read_pending(&agent)? {
            if let Some(r) = store.read_receipt(&agent, p.id)? {
                let latency = r.acked_at - p.delivered_at;
                println!("resolved\t{}\t{}\tacked after {latency}s", p.id, agent);
                store.clear_pending(&agent, p.id)?;
                continue;
            }
            // Overdue at the deadline, not one second after it: `age < deadline`
            // rather than `<=`. With `<=`, `--deadline-secs 0` could never fire
            // at all, because a delivery made this second has age 0 — which the
            // integration tests caught before it shipped.
            let age = t - p.delivered_at;
            if age < deadline {
                continue;
            }
            if p.escalated_at.is_some() {
                // Reported, never re-escalated. A channel that cries wolf is a
                // channel that gets muted.
                println!("still-deaf\t{}\t{}\tage {age}s (escalated already)", p.id, agent);
                exit = exit.max(EXIT_DEAF);
                continue;
            }
            overdue.push(p);
        }

        if overdue.is_empty() {
            continue;
        }

        // One escalation for the agent, not one per message: N utterances for
        // one deaf agent is the same wolf, cried N times.
        let oldest = overdue.iter().map(|p| t - p.delivered_at).max().unwrap_or(0);
        let text = escalation_text(&agent, overdue.len(), oldest);
        for p in &overdue {
            println!("DEAF\t{}\t{}\tage {}s\t{}", p.id, agent, t - p.delivered_at, p.subject);
        }

        let cmd = escalation_command(args);
        match escalate(&cmd, &text) {
            Ok(Escalation::Suppressed) => {
                // Deliberately NOT marked escalated: nobody was told, so the
                // next sweep must try again.
                println!(
                    "suppressed\t{agent}\t{cmd} declined to deliver (exit {EX_TEMPFAIL}); \
                     will retry next sweep"
                );
                exit = exit.max(EXIT_DEAF);
            }
            Ok(Escalation::Delivered) => {
                for p in overdue {
                    let marked = Pending { escalated_at: Some(t), ..p };
                    store.write_pending(&marked)?;
                }
                println!("escalated\t{agent}\tvia {cmd}");
                exit = exit.max(EXIT_DEAF);
            }
            Err(e) => {
                // Do NOT mark them escalated: an escalation that did not happen
                // must be retried, not recorded as done.
                let alert = format!(
                    "{text}\n\nESCALATION FAILED: {e:#}\ncommand: {cmd}\nat: {t}\n\
                     These deliveries are still unacknowledged and nobody has been told.\n"
                );
                let path = store.write_alert(overdue[0].id, &alert)?;
                eprintln!("colloquy: ESCALATION FAILED via {cmd}: {e:#}");
                eprintln!("colloquy: wrote {}", path.display());
                exit = EXIT_ESCALATION_FAILED;
            }
        }
    }

    Ok(exit)
}

/// What the human actually hears.
///
/// Written for the ear, because this is spoken. The first version said "in 0
/// seconds", which Andy heard live and which means nothing said aloud — an
/// alert whose only job is to be understood immediately cannot spend its words
/// on a duration that reads as a bug. So: say the trouble first, and give the
/// age only when it is long enough to be information.
fn escalation_text(agent: &str, count: usize, oldest_secs: i64) -> String {
    let what = if count == 1 {
        format!("a Colloquy message")
    } else {
        format!("{count} Colloquy messages")
    };
    if oldest_secs < 120 {
        format!("Andy. {agent} is not acknowledging {what}.")
    } else if oldest_secs < 3600 {
        format!(
            "Andy. {agent} has not acknowledged {what} for {} minutes.",
            oldest_secs / 60
        )
    } else {
        let hours = oldest_secs / 3600;
        format!(
            "Andy. {agent} has not acknowledged {what} for over {hours} hour{}.",
            if hours == 1 { "" } else { "s" }
        )
    }
}

/// Run the escalation and **check that it worked**.
///
/// Three separate tools in this codebase's history reported success while
/// dropping the thing they promised — the notification tail, `paplay --device`,
/// and `gh project item-add`. A liveness monitor that escalates into the void
/// is that same bug one level up, and it would be undetectable by construction.
fn escalate(cmd: &str, text: &str) -> Result<Escalation> {
    let out = Command::new(cmd)
        .arg(text)
        .output()
        .with_context(|| format!("running the escalation command {cmd:?}"))?;
    match out.status.code() {
        Some(0) => Ok(Escalation::Delivered),
        Some(EX_TEMPFAIL) => Ok(Escalation::Suppressed),
        _ => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            bail!("{cmd} exited {}: {}", out.status, stderr.trim())
        }
    }
}

/// The escalation command exited 75, the conventional EX_TEMPFAIL: it chose not
/// to deliver this one.
///
/// This is not hypothetical. `claude-say` is rate-limited to one utterance a
/// minute — deliberately, because a voice that speaks constantly gets muted —
/// and it exits 0 when it suppresses. Measured on 2026-08-31: the poker
/// recorded `escalated_at` for an alert nobody heard, and since a delivery is
/// escalated only once, that alert was gone for good.
///
/// So a suppressed escalation is NOT success. It is not failure either: the
/// right response is to leave the delivery unescalated and try again on the
/// next sweep, by which time the limit will have passed.
const EX_TEMPFAIL: i32 = 75;

#[derive(Debug, PartialEq, Eq)]
enum Escalation {
    Delivered,
    Suppressed,
}

/// Print the monitor the agent should arm at startup.
///
/// This exists because the wake is session-scoped: a persistent monitor
/// survives turns but dies with the session, so every new agent must arm its
/// own. That makes it a bootstrap concern, and `CLAUDE.md` is the bootstrap
/// channel — this line is what belongs in it.
fn cmd_watch_command(store: &Store, args: &Args) -> Result<i32> {
    let agent = args.req("agent")?;
    let dir = store.inbox_dir(agent)?;
    println!(
        "cd {} && seen=$(ls -1 2>/dev/null | sort); while true; do \
cur=$(ls -1 2>/dev/null | sort); comm -13 <(echo \"$seen\") <(echo \"$cur\"); \
seen=\"$cur\"; sleep 0.5; done",
        dir.display()
    );
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::escalation_text;

    #[test]
    fn a_spoken_alert_never_says_zero_seconds() {
        // Andy heard the first version say "in 0 seconds" and asked what it
        // meant, which is the correct reaction to an alert that sounds broken.
        let t = escalation_text("DEAFTEST", 1, 0);
        assert!(!t.contains('0'), "got: {t}");
        assert_eq!(t, "Andy. DEAFTEST is not acknowledging a Colloquy message.");
    }

    #[test]
    fn a_long_silence_says_how_long() {
        assert!(escalation_text("A", 2, 600).contains("10 minutes"));
        assert!(escalation_text("A", 1, 7200).contains("over 2 hours"));
        assert!(escalation_text("A", 1, 3700).contains("over 1 hour."));
    }
}
