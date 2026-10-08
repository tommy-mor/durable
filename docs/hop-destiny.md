# hop destiny (vision sketch)

*Status: a glimpse, not a spec. Written 2026-10-05, after slug's replay chart
proved the baby version of the time-machine idea in production.*

One-liner: **hop's destiny is to be the first language where a whole
networked application — UI, state, history, agents, billing — is one program
with one tape, and every other way of building that thing looks like
handwriting assembly.**

Today hop says: write the function, mark where it crosses the network
(`server!()` / `browser!()`). The extreme of that is the death of the
distinction itself. "Frontend" and "backend" are accidents of physics, not
design. Glimpses of the full shape:

## Placement becomes inferred, not marked

Today placement is a static mark. Destiny-hop reads the dataflow and *knows*:
this segment touches the store, so it lives near the store; this one touches
the DOM, so it ships to a tab. Placement as a compiler optimization, like
register allocation. The programmer stops thinking about the network because
the network was never the point.

## Every program is its own time machine

Tape + deterministic reducer means history is first-class. "Show me the app
last Tuesday" becomes a GET request; every UI is scrubbable through its own
past. The replay chart on slug.social (`?v=history`) is the baby version —
one chart, one scope — and it already changes how the data feels. Truth is
what you can recompute; destiny-hop makes recomputation a user-facing
feature, not an operational trick.

## Humans and agents are the same kind of participant

`llm.stream` is already an effect; slug's delegates are already voters bound
to humans. Destiny-hop has `cast agents { }` beside `cast browsers { }` —
clicks and judgments recorded identically on one tape, auditable, and
*billable*: token metering stops being a Stripe integration and becomes a
**reducer projection**. The invoice is computed from history and provable by
replay. The most honest billing system ever shipped is a fold over events.

## Federation without the religion

"One writer per store" is not a limit; it is a sharding key. Destiny-hop is
tapes referencing tapes: one tape imports another, reducers fold over merged
histories, no platform owner, no consensus cosplay. Single-writer tapes
composing like functions — the honest version of every "decentralized app"
promise that ever lied.

## The first rung

The codebase indexer (per-repo `:needs-refactor` rankings over time) is the
flagship candidate because it is the smallest complete destiny: one store per
repo (tenant sharding from day one), agent judgments as tape effects,
rankings as reducers, the replay chart as the UI, billing as a projection.
Every piece of the extreme stack exercised by one product someone will pay
for. The demo that teaches the language why it exists.

## What must stay true

The tape is the asset. Whatever the app is written in — hop today, Rust on
durable if needed — the events outlive every implementation decision. That is
the religion that got us here: order computed from evidence, and evidence
never thrown away.
