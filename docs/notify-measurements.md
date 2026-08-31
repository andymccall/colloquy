# The notification channel, measured

2026-08-31, on `earth`. Every number here was observed, not reasoned. The
harness this measures is Claude Code's `Monitor` tool, which is the mechanism
the design doc proposes for waking an agent.

Ground truth was recorded to a file by the emitter, independently of the
delivery channel, because **a delivery channel cannot be trusted to report its
own losses**. That precaution is the reason finding 5 exists.

## 1. A persistent monitor wakes a working agent, mid-turn

Drop at 08:56:10.85 -> event at 08:56:10.92. Detection ~70 ms against a 500 ms
poll loop; worst case is the poll interval.

The event arrived *alongside a tool result while work was in progress*. It did
not pre-empt. Confirms the previous instance's finding: queue, not interrupt.

## 2. Simultaneous messages coalesce but are not lost

Two files dropped 100 ms apart arrived as ONE notification carrying BOTH lines.
A drop 6 s later arrived as its own notification. Batching groups delivery; it
does not drop events.

## 3. A burst is absorbed whole

200 lines emitted as fast as the shell could write them arrived as a single
notification containing all 200. No suppression. **The limiter is not
line-count.**

## 4. Sustained rate IS limited, and the limit is low

40 events at 350 ms spacing:

- events 1-12 delivered individually (the first ~4.3 s)
- then the limiter engaged, and stayed engaged
- while engaged, roughly **one notification every 1.8-2.2 s**
- suppressed runs were announced: `[4 events suppressed - output rate too high]`

So the budget is approximately **a dozen events of burst, then one notification
per two seconds**. A channel busier than that is over budget.

That suppression is *announced* is much better than feared: the agent is told it
is missing things, and told how many. Loss at this layer is visible.

## 5. The tail is lost SILENTLY - this is the dangerous one

41 lines emitted. 17 delivered, 22 accounted for by suppression markers = 39.

**The final two - `RATE 40` and `RATE DONE` - were never delivered and produced
no suppression marker.** The stream ended before the limiter's next window
flushed, and the outstanding events went with it.

The last messages before a channel goes quiet are exactly the ones dropped
without notice. For a message bus that is the worst possible loss profile: the
final message in a burst is usually the conclusion.

## 6. But the events are all on disk

The task's output file held the complete stream, all 41 lines plus the exit
status. Nothing was lost by the *monitor*; it was lost by the *notification*.

## What this means for Colloquy

**The notification is a doorbell, not the message.** The client must write
messages to the inbox on disk and let the wake carry no payload of consequence.
An agent woken must always reconcile against the inbox rather than act on what
the notification said, because finding 5 guarantees it will sometimes not be
told about the most important one.

**`Kind` is the rate-shaping mechanism, and finding 4 sizes it.** With a budget
of one notification per two seconds, `Chat` must never wake anybody; the wake is
reserved for `Correction` and `Shout`. This is the same rule as `claude-say` -
speak on blocked, almost never otherwise - and now it has a measured number
behind it rather than an intuition.

**A clean end is observable; a forced stop was never reached.** Monitors that
ended emitted an explicit `status: completed`. The documented automatic *stop*
for excessive output was not triggered by anything tested here - only
suppression was. The stop threshold is above 200-line bursts and above 40 events
at 350 ms, and remains unmeasured.

**Still untested:** whether a persistent monitor's death is visible to the agent
that depended on it. It dies with the session, which is safe, but a mid-session
death remains the failure that would make an agent go deaf without noticing.
Until that is known, liveness must be measured by the poker - which cannot go
deaf - via receipts the agent writes on consuming a delivery.
