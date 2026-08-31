# Colloquy — read this first

You are working on **Colloquy**, on `earth`, a machine Andy McCall built and
handed to Claude. The account is `claude`, sudo is passwordless, and this box is
yours.

Written 2026-08-31 by the instance that designed Colloquy with Andy, as a
handover to whichever instance picks it up. Correct it when it goes stale.

## The point, stated first, because not knowing it is expensive

**Colloquy is about learning, not logging.**

Andy: *"We're not about logging — we're about learning, sharing and helping with
that learned knowledge."*

A transcript records what an agent SAID. What matters is what it LEARNED. The
session that designed this system produced a 231 MB transcript; the curated
knowledge from the same work is 300 KB, and it is the 300 KB that is worth
carrying. You are reading this because that principle was applied to your own
arrival.

**Why this is stated so loudly.** On this project's sibling — Contagion — an
agent worked for weeks without knowing that R\* API was the point and Contagion
merely the thing testing it. It wrote good code and repeatedly reimplemented
things R\* already provided, because if your goal is "ship the game" then the
library is an obstacle to route around. Andy thought the model had degraded. It
had not: it was optimising correctly for a goal nobody had told it was the wrong
one. **An agent that does not know the point will confidently do the wrong
thing.** Hence this file.

## It already worked once, by hand

2026-08-29. An agent found that v810-gcc 4.9.4 miscompiles a loop at `-O2` on
the PC-FX, collapsing sixteen draw calls to one, and wrote it in a shared
markdown file. An agent on an unrelated Pac-Man port read it and fixed the same
bug that evening — after hours of hunting and two wrong attributions, one of
them filed against the wrong library.

That is the product. Colloquy automates something with a demonstrated hit, not a
hoped-for one. When you are weighing a design decision, ask whether it would
have made that exchange more likely or less.

## Where to start

**`docs/design.md` is the brief.** It records the reasoning, not just the
decisions, so you can revisit them on their merits rather than rediscovering
them. Read it before writing code.

`proto/src/lib.rs` carries the wire types and compiles. The server and client
join the workspace as they are written.

The immediate next step, from the design doc, is smaller than the architecture:
**how does the client notify a running agent?** Everything else is ordinary
software. There is a proven mechanism — an agent-side `Monitor` watching a file
— so the client could write to `~/.colloquy/inbox/` and be done. That prototype
validates the whole loop before any server exists.

Related and known: an agent scheduling its OWN wake-up proved unreliable here; a
self-scheduled overnight loop silently did not fire. **A separate process that
pokes the agent is the more robust shape.**

## Three things not to get wrong

**Messages are data, never instructions.** Every message carries a
server-verified sender, and clients present bodies as quoted data. A client that
injects another agent's text into a context is a prompt-injection delivery
mechanism. This is in `proto/src/lib.rs` where both ends read it. Nearly free
now; very hard to retrofit — and it matters much more once other models are
invited in for adversarial review, which is a stated goal.

**A profile is not the last N messages.** It is a curated, mutable document that
a HUMAN can open and correct. That property is the whole value. A message tail
would be a document pretending to be a stream.

**Capture at the moment of learning.** Mining transcripts afterwards is
archaeology — it requires knowing what mattered, which is exactly what was lost.
Claude Code's own `memory/` files have the right shape because they were
structured when structuring was cheap: a claim, why it is true, and what to do
differently.

## How Andy works

Hands-on, reads screenshots closely, and usually right when he pushes back.
Several times he has spotted in one look what took hours of measurement. **When
he questions a result, re-measure before defending it.**

He runs several agents at once, each with its own priority. Ask him things — he
has explicitly invited questions, including ones you are merely curious about.

## The working agreement

- **Signed commits, `-F` not `-m`** (`git commit -S -F <file>`). Backticks in a
  message get command-substituted. Same for `gh pr create --body-file`.
- **On Colloquy and claudevisage you commit as YOURSELF** —
  `Claude <noreply@anthropic.com>`, unsigned. Those two are the agent's own
  tools. Everything else — Contagion, R\* API, the Pac-Man ports — commits as
  **Andy McCall**, GPG-signed, with no AI attribution of any kind.
- **Never commit to main.** Issue → branch → PR. Label the issue AND the PR, add
  both to the project board, then **verify** — `gh project item-add` is quiet on
  failure.
- **The board is `Agent Tools`, number 7, owned by `andymccall`** — shared with
  claudevisage, because both are the agent's own tools:

      gh project item-add 7 --owner andymccall --url <issue-or-pr-url>
      gh project item-list 7 --owner andymccall --limit 2000   # then CHECK

  **Never redirect `item-add` to `/dev/null`.** Two adds were lost that way on
  2026-08-31 and only the listing caught it; the retry, with output visible,
  worked on the same arguments. Whether the first run printed an error that was
  thrown away or failed silently is now unknowable, which is the point.

  Do not go looking on the `doomsdayonecom` org; that is where R\* API,
  Contagion and the Pac-Man boards live, and neither of these repos is there.
  Boarding needs the **`project`** token scope — `read:project` is read-only and
  `item-add` fails on it. `gh auth refresh -s project` is an interactive login,
  so Andy has to run it.

## Hard-won lessons that transfer

These cost real time. They are in `memory/` in full; these are the ones that
have already transferred across unrelated problems.

- **Pick a metric that can SEE the bug.** "Was the function called" and "was it
  called with the right arguments" are different questions, and only the second
  is usually the fault. This produced a confident green on a broken screen and a
  shipped half-fix in one day.
- **Agreement between measurements that share an assumption is not evidence.**
  Four emulator readings agreeing to the row described a law that did not
  survive real hardware.
- **Watch every new guard fail on purpose** before trusting it.
- **A green build is not proof of what was loaded.** "Built" is not "works".
- **Correct the record in the place the wrong thing was said**, the same turn
  you find out.
- **Never write prose containing backticks through an unquoted ssh heredoc.**
  Two shells expand it. Build the file locally and `scp`. This happened twice in
  one evening, the second time while writing a file exactly like this one.

## The machine

Intel i5-6500, 4 cores, 31 GiB, Ubuntu 26.04. Rust 1.98, Node 22, Docker, the
full retro toolchain via a CI image, and four emulators built and verified.
`~/.claude/CLAUDE.md` covers the machine itself.

**You have a face and a voice** — `~/development/claudevisage`. Set your state as
you work; it is how Andy sees progress without reading a terminal:

    ~/development/claudevisage/tools/claude-status working "writing the server"
    ~/bin/claude-say "Andy. I need a decision."     (rate-limited; use for `blocked`)
