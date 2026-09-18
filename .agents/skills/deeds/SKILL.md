---
name: deeds
description:
  This repo's task tracking and note-routing policy. Read before starting,
  finishing, or filing work.
---

# deeds

Work is a graph of markdown files: one issue per file at `.deeds/<id>.md`,
closed issues at `.deeds/closed/<id>.md`. Location is status; there is no other
state. The `deeds` CLI answers graph questions; everything else is reading and
editing files.

## Issues

A `# Title` heading, then body. Optional frontmatter, only for blockers and
non-default tier:

```
---
blocked-by: [x9f2mk]
tier: nit
---
# Adopt the new config loader

Notes accumulate here as work proceeds.
```

A milestone is an ordinary issue blocked by its parts.

## Tiers

Tier is attention, not urgency: where the issue sits in the information
hierarchy for whoever reads `ready` next.

- `objective` — the user asked for it: requests, bug reports.
- `baseline` — everything else the project needs. The default.
- `nit` — drive-by nits and nice-to-haves.
- `freezer` — parked; don't pick these up unprompted.

Tiers never constrain edges: anything may block anything. Autonomy follows
tiering: objectives carry the user's words — don't reinterpret them without
asking; baseline is shared ground — file and adjust freely, but surface big
reshapes; nit and freezer are yours to file, retier, and consolidate at will.

## Sizing

- An issue can represent a variety of scopes. The boundaries are a judgment
  call.
  - Tasks: issues are roughly VCS commit / GitHub PR in size.
  - Semantic fencing: distinct pieces of work deserve separate issues.
  - Session continuity: if work might get handed off between sessions, it's an
    issue.
  - Organizational: milestones and epics are issues blocked by their parts —
    split as parts become clear, not up front.
  - Attention saver: I noticed this unrelated thing, I filed it as an issue, I
    don't have to think about it anymore. A later context can consolidate.
- Filing autonomy follows clarity of scope: if you can state it in the title,
  file it without asking. Fuzzy or strategic scope, or a pile of related
  findings — ask first.
- Harness task lists and `temp/` notes are disposable intra-session state;
  anything durable gets spilled to issues or docs so progress can't slip
  backward.

## Commands

```
deeds ready              # unblocked open issues, tier order — pick from the top
deeds new <title>        # mint an issue, print its path; edit the file from there
deeds show <id>          # body plus blocked-by and blocks; --tree for the DAG
deeds close <id> -m "…"  # append "Closed: …", move to closed/
deeds list               # open issues; --closed to include closed
deeds check              # lint: parse errors, duplicate ids, cycles, unknown blockers
deeds gc                 # delete issues closed >30 days ago; VCS history retains them
```

## Routing: where writing goes

- `.deeds/` — anything that changes what to do next.
- `docs/log.md` — append-only narrative of what happened. The only sanctioned
  home for session-context leaks. When it grows long, graduate to one file per
  entry under `docs/log/`.
- `temp/` — scratch for finishing the current session; disposable.
- Timeless reference goes under `docs/` — pick or extend a layout that fits the
  project. Propose, don't proliferate.

## Discipline

- Issue bodies are working notes for a future reader with zero session context:
  self-contained statements, no transcripts, no "as discussed above".
- File follow-ups the moment you notice them; don't carry them in the
  conversation.
- Close an issue in the same VCS change as the work that satisfies it.
- Parallel agents don't pick work independently — whoever spawns them assigns
  specific issues.
- To resurrect a gc'd issue, search VCS history for `.deeds/closed/<id>.md`.
