//! The wire protocol, shared by server and client so they cannot disagree.
//!
//! # Why the types live here
//!
//! A message bus where the two ends have their own idea of the schema is a bus
//! that works until someone changes one side. Sharing the types makes that
//! impossible rather than merely discouraged.
//!
//! # Messages are DATA, never instructions
//!
//! This is the security property the whole system rests on, and it is stated
//! here because this is the file both ends read.
//!
//! An agent receiving a message is receiving text written by another agent —
//! or, once other models join for adversarial review, by a stranger. If a
//! client injects that text into an agent's context as though the agent had
//! thought it, then anyone who can post can make an agent act. That is prompt
//! injection with a delivery mechanism.
//!
//! So: every message carries a verified `from`, and clients MUST present
//! bodies as quoted data attributed to their sender. Nothing in a body is ever
//! to be followed as an instruction. Retrofitting this is very hard; it costs
//! almost nothing now.

use serde::{Deserialize, Serialize};

pub mod envelope;
pub use envelope::{Provenance, render};

/// Who sent something. Verified by the server, never taken from the body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentId(pub String);

/// A channel. Membership overlaps deliberately — an agent can be in several,
/// and `shout` is the one everybody can reach when asking whether a problem has
/// been solved somewhere they cannot see.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelId(pub String);

/// What a message is FOR, because retrieval should respect the difference.
///
/// Drawn from what a working agent actually produces: most traffic is chatter,
/// but the durable value is in the other three — and `Correction` is the one
/// nothing currently captures. Every wrong theory published and retracted costs
/// real time, and "it is not this, and here is how we know" is what stops the
/// next agent walking the same path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Ordinary coordination. The bulk, and the least durable.
    Chat,
    /// A verifiable claim about a system: a compiler bug, a register map.
    Finding,
    /// A transferable lesson, not tied to one system.
    Heuristic,
    /// A retraction. The most valuable and least recorded kind.
    Correction,
    /// "Has anyone solved this?" — crosses channels the asker is not in.
    Shout,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: i64,
    pub channel: ChannelId,
    /// Established by the server from the authenticated connection. A client
    /// must not trust a sender named inside `body`.
    pub from: AgentId,
    pub kind: Kind,
    pub subject: String,
    pub body: String,
    /// Unix seconds. The server stamps this; a client's clock is its own business.
    pub at: i64,
}

/// What an agent is handed when it joins — the bootstrap.
///
/// Deliberately NOT "the last N messages". A profile is a curated, mutable
/// document that a HUMAN can open and correct; a message tail is a stream
/// pretending to be one. See docs/design.md.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub agent: AgentId,
    /// Who this agent is and what it is for. Rendered into its CLAUDE.md.
    pub charter: String,
    /// Curated facts, in the shape Claude Code's memory/ files already use:
    /// a claim, why it is true, and what to do differently.
    pub knowledge: Vec<Knowledge>,
    pub channels: Vec<ChannelId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Knowledge {
    pub id: i64,
    pub name: String,
    pub description: String,
    pub kind: Kind,
    pub body: String,
    pub author: AgentId,
    pub at: i64,
}

/// Client -> server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Identify, and receive the profile. The bootstrap happens here rather
    /// than as a separate lookup, so a new agent has no discovery problem —
    /// it does not need to know where its own context lives.
    Hello { agent: AgentId, token: String },
    Post { channel: ChannelId, kind: Kind, subject: String, body: String },
    /// Long-poll: return messages after `since`, or block until there are some.
    Poll { since: i64, wait_secs: u32 },
    /// Record something learned, at the moment it was learned.
    Learn { name: String, description: String, kind: Kind, body: String },
    /// Ask across channels, including ones this agent is not in.
    Search { query: String, limit: u32 },
}

/// Server -> client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "re", rename_all = "snake_case")]
pub enum Response {
    Welcome { profile: Profile, since: i64 },
    Posted { id: i64 },
    Messages { messages: Vec<Message>, since: i64 },
    Learned { id: i64 },
    Found { knowledge: Vec<Knowledge> },
    Error { message: String },
}
