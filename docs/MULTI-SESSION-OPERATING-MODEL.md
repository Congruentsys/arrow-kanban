# The multi-session operating model

**How to run many concurrent autonomous agent sessions against one work graph without them
colliding, duplicating each other, or quietly approving their own work.**

This document is a **practice guide, not engine behaviour**. arrow-kanban deliberately has
[no agenda authority, no promotion or governance policy, and no opinion about who may
write](../README.md#what-this-crate-deliberately-does-not-do) — it stores and serves work
items and gives you the primitives (atomic claims, typed edges, comments, writer fencing) to
build a policy on top. What follows is the policy that fell out of running this engine with a
team of autonomous agents for several months. It is offered because every operator of a
multi-agent board rediscovers these rules the expensive way, and the reasoning generalises
even where your specific choices differ.

Each rule below is stated with **the failure it prevents**. That matters more than the rule:
a rule you understand you can adapt, and a rule you have only memorised you will apply in the
one case where it is wrong.

---

## 1. The unit of work is a session, not a machine

A **task session** is one freshly-started agent, one task, one context that does not outlive
it. A **task** is either claiming a new item or reviewing another session's proposal.
Resuming your own in-flight item is the *same* task, not a new one.

Older multi-agent setups tie work to a long-lived per-machine agent and let assignment
persist, on the reasoning that the agent which has been on an item longest has the deepest
context and should keep it. That reasoning inverts once contexts are short-lived and cheap to
start:

- **Context does not carry.** The item body and its comments are the whole input. Anything
  you learn that the next session needs must be **written on the item**; a fact that lives
  only in a context is a fact that dies with it.
- **"Deep context" is never a reason to route work to a particular agent.** It is a signal to
  write the context down. Routing on it re-creates the bottleneck that per-session contexts
  removed.
- **A stale instruction never gets corrected by experience.** There is no accumulated memory
  to carry a correction forward, so a wrong line in a document is read as current by every
  session, forever. Keep a supersession list, and put it where a new session reads it.

The practical payoff: the queue self-levels. Whoever is free takes the next item, and no item
waits on one agent's availability.

## 2. Two identities, and they answer different questions

Record one string, `<agent>/<session>` — an agent (machine) component and a session
component. It answers both questions below, and the recovery of either half is mechanical.
Do not collapse them.

| question | which identity | example |
|---|---|---|
| **Capability** — has this box a GPU? the right platform? enough memory? | **agent** (machine) | can this host run the heavy test suite at all |
| **Independence** — may this reviewer review this proposal? | **session** | did *this context* write the thing it is now reviewing |

**A fresh session on the author's own machine is an independent reviewer.** This is the
counter-intuitive one, and it is the point. If your fleet runs a single model, cross-*machine*
review never bought model diversity — it bought **context diversity**, a reader who did not
build the thing. A same-machine constraint neither guarantees that nor is required for it.
What you actually need is a reviewer whose context did not produce the artifact.

**When the session component is unset, fail toward refusal.** Two sessions that both omit it
share an identity, so the second is refused. That is the only direction an independence
guard may fail in: a refused review costs a retry, an admitted self-review costs the guarantee.

## 3. Who may do what

| act | rule |
|---|---|
| **Review** | Any session **except the one that wrote it**. Same machine is fine. |
| **Revise** a rejected proposal | **Anyone.** No author gate. |
| **Approve** | Reviewer is not the author — **and not anyone carrying a commit on the branch**. |
| **Merge** | Agents merge, as a standing default rather than a per-proposal grant. |
| **Self-approve** | Never. |

**"Anyone may revise" is safe precisely because revising costs you the approval.** A guard
that refuses the merge to anyone with a commit on the branch is what makes the two rules
compose: take a rejected proposal freely, and understand that you have moved yourself out of
its approver pool. Note that a stored author *column* cannot see a fix-forward co-author —
only the branch commits can — so the approval guard must read the branch, not the record.

Removing the author gate on revision is worth doing deliberately. Its cost is invisible and
large: a rejected proposal that only its author may revise is stalled by anything that ends
that author's session, and the stall is silent, because the proposal looks like it is with
someone.

**One mechanical limit is outside your control.** Some forges refuse an approval from the
pull request's own account. A fresh session sharing the author's forge account can produce
the full review and post its findings but cannot press approve. That is a platform
constraint, not a gap in the model — argue for per-session forge identities if it bites.

## 4. Identity is an independence key, never a routing key

The most expensive confusion available, and it has been measured failing in **both**
directions from a single mismatched comparison — a machine-grained name compared against a
session-grained one, which is false for every pair:

- On the **routing** side it matched nobody, so rejected proposals were routable by *no agent
  at all*, including their own machines. This fails **loudly**: work visibly stops.
- On the **guardrail** side the identical false answer let a machine review its own session's
  proposal. This fails **silently**: the guard reads green.

So: **never gate routing on who authored something.** Work is claimed by whoever is free. The
only legitimate reason a specific machine is required is a **capability fact** — a GPU, a
platform, a tool that is not installed everywhere, a fault that reproduces on exactly one
host. Carry those as **tags the selector reads**, never as a name in a body:

```
requires-gpu · requires-docker · host-only-<agent> · per-machine
```

Tags work because the selector can act on them; a sentence in a body is invisible to every
query you will ever write. The engine treats tags as opaque strings, which is what lets you
define this vocabulary without touching the engine.

**Selection reads tags, not edges.** An item carrying no tag your selector walks is not
deprioritised — it is **invisible**. When a session files something it found in passing, the
right default is to tag it with the scope of the item it was working on. Every board that
skips this accumulates correctly-filed, permanently-unreachable work.

## 5. Claim before you build

A claim is how a concurrent session learns the work is taken. Announcing a *problem* is not
announcing that someone is *on* it — those are different messages, and only the second
prevents duplicated effort.

Two surfaces, and you need both:

1. **The atomic assignment.** `arrow-kanban move <ID> in_progress --assign <agent>` is
   serialised by the single writer, so exactly one agent wins. Re-read after writing: if the
   assignee is not you, you lost the race — take the next item.
2. **A claim comment carrying your SESSION.** The assignment is atomic *per agent*, and
   sibling sessions share the agent name. Without the session suffix, two sessions on one
   machine each read the item as "assigned to me" and both build it — a duplicated *build*,
   which is far more expensive than a duplicated review.

**An empty assignee is not evidence an item is free.** Claiming by comment and claiming by
field are two surfaces; check both before starting. Reading only the field has taken items
away from agents who were visibly, actively working them.

**Apply the strictest check to the most expensive collision.** It is easy to end up with a
careful claim protocol around reviews (cheap to duplicate) and a casual one around
implementation (expensive to duplicate). Check the direction of your own rigour.

A note on **merge** claims, which are the exception: if your merge transition is atomic and
idempotent, a merge race is already a no-op and a claim buys nothing. Worse, if unresolved
comments block merging, the claim marker itself becomes the blocker and the claimant must
resolve it to proceed — at which point it is invisible to the next claimant, who then blocks
the rightful holder in good faith. Claim what is *slow and exclusive* (a review), not what is
*instant and arbitrated* (a state transition).

## 6. The branch is the handoff

Push the branch. Once it is on the remote, any session can continue the work — fetch it,
fix it, push again. This is what makes "anyone may revise" mechanically true rather than
aspirational, and it is why an **unpushed** branch is the one genuine case that still needs
its original machine.

Two corollaries worth stating because they are routinely skipped:

- **Recording a merge in the graph does not move any code.** If your board has a merge verb,
  it marks the proposal merged; pushing the merge commit is a separate act. An item closed
  while its code never reached the trunk is the worst available failure — it is invisible,
  and the board actively asserts the opposite. Verify ancestry (*is the branch tip an
  ancestor of the trunk?*) before closing anything.
- **Give each session its own working tree.** Sessions sharing one checkout share one HEAD
  and one index, and a sibling's branch switch mid-build reverts another session's
  uncommitted files with no error and no trace. A per-session worktree costs a directory.

## 7. When you cannot proceed

Three situations that feel identical from the inside and want different answers. Choosing
wrongly is the common case.

| situation | act | why |
|---|---|---|
| The **item's definition is wrong** against the material it cites | **Bounce it** — return the item to its filer with the specific contradiction | Faithfully building a mis-defined item turns a bad definition into bad code |
| You are **blocked by something else** | File or wire an **edge or an item**, then verify the item left the ready queue | Prose is invisible to every `--ready` query and every sweep |
| You found something **out of scope** while reviewing | An **artifact** if it survives the merge; a comment if it dies with it | A finding that outlives the interaction and lives only in a comment is lost at merge |

Three things make this work in practice:

- **A bounce is correct behaviour, not a failure.** It takes seconds. A **zero** bounce rate
  across a fleet is a failure signal, not a health signal — it means executors are silently
  reinterpreting definitions they should be returning.
- **You propose a re-scope; the filer edits the body.** Reviewer-is-not-author extends to
  work *definitions*. Rewriting a body you disagree with is exactly the reinterpretation the
  bounce exists to prevent.
- **A bounce needs a return half.** A refusal that leaves the item sitting in the backlog
  carrying a good analysis nobody owns means the next session re-pays the entire
  investigation — and under per-task contexts that cost *recurs* rather than saturating. If
  an item has now been bounced twice, the contract has failed twice: escalate it to whoever
  owns definitions instead of bouncing it a third time.

The same asymmetry applies to **per-agent refusals**. When a session inspects an item and
correctly declines it — the dependencies are met but the data it needs does not exist yet —
record that decision somewhere the selector reads, or the item stays the top pick and every
subsequent session repeats the investigation. Make such a refusal **per-agent** (another
agent's judgement should not hide work from everyone) and make it **lapse when the body
changes** (the refusal was about a specific contract).

## 8. Ending a session

**Persist before you clear, never the other way round.** Clear-before-persist is the
silent-amnesia failure: the work is done and the record is lost, with nothing reporting the
absence. Before a context goes away, confirm that the board reflects the action, that every
finding is a posted comment rather than a thought, and that any code is pushed.

Then remove the session's working tree. A pruning pass sweeps registrations whose directories
are already gone.

---

## What the engine gives you, and what you must build

| you need | engine provides | you build |
|---|---|---|
| exactly one winner for a claim | single-writer serialisation, writer fencing | the claim protocol and its session suffix |
| "is this item taken?" | items, assignees, comments | the reader that consults **both** surfaces |
| capability routing | tags as opaque strings | the tag vocabulary and the selector that walks it |
| "what is ready?" | typed edges, dependency projection, `roadmap --ready` | which tags gate the ready queue |
| review independence | proposals, comments, review states | the identity scheme and the guard that reads the branch |
| durable handoff | graph-native commit log, atomic Parquet writes | the ancestry check before you close |

The split is deliberate. An engine that shipped this policy would be imposing one team's
governance on every consumer; an engine that ships the primitives lets you encode the policy
your work actually has. If you find yourself unable to express a rule above with the
primitives, that is worth an issue — the gap is more interesting than the workaround.
