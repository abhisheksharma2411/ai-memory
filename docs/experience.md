# The experience pass (cross-session abstraction)

*2.0 item 6.* Per-session auto-improvement reviews one trajectory at a
time — it can never see that four sessions repeated the same workflow,
or that the operator keeps re-stating a preference no page records, or
that recent sessions quietly contradict a stored decision. The
experience pass reads the last N session summaries of a project **side
by side** and proposes exactly that cross-trajectory knowledge.

## Why "experience"

It is the Reflection→Experience step the 2026 agent-memory literature
converges on (the survey's third stage; TriMem's narrative layer): raw
capture → per-session summaries → durable knowledge distilled across
trajectories. The pass targets pattern/procedure/preference/
architecture pages — the pages that make session #50 cheaper than
session #5.

## When to turn it on — and when not to

Opt-in, off by default:

```toml
[auto_improve.scheduler]
experience_every_sessions = 5   # run after every 5 newly completed sessions
experience_sessions = 10        # read the last 10 session summaries
```

Turn it on when per-session auto-improve is already working for you and
the project has real session history (the pass skips scopes with fewer
session pages than the cadence floor). Leave it off when the project is
young — cross-session patterns need sessions to cross — or when no LLM
is configured: the pass is LLM-hosted and the zero-LLM default path
never runs it.

## What it costs and what guards it

One LLM call per project per cadence trigger, prompt-bounded like the
per-session reviewer. Every proposal flows through the **identical**
machinery: JSON-schema constrained output, validation, the confidence
floor, the eval gate, the rejection buffer (rejected ideas are shown to
later runs so they are not re-proposed), and pending-writes staging with
sidecars — reviewable, never silent. `require_approval` applies
unchanged. The system prompt demands evidence spanning **at least two
sessions**, naming them; single-session findings are the per-session
reviewer's job and are rejected here.

The cadence anchors on enablement: switching the pass on against an old
store does not re-digest history — it waits for the next
`experience_every_sessions` completed sessions.
