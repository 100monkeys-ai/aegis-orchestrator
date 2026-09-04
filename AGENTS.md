# AGENTS.md — Bootstrap for aegis-orchestrator

This file is a **bootstrap**, not the engineering contract. The contract — architecture, decisions, operating principles, testing, commit workflow, the autonomy boundary, every lesson already learned — lives in the AEGIS Architecture workspace of the cortex at `https://100monkeys-ai.cortex.page/aegis-architecture/`. Read this file once, ground against that workspace, and work from there. **Where this file and the workspace disagree, the workspace wins and this file is stale.**

## Ground first

1. `cortex_ground` with `workspace: "c8b0fadf-176c-418a-a8a4-a148dcf9fd1d"`. Read the instance grounding and the workspace grounding it returns, in full.
2. Read the workspace landing page `readme`. It is the navigation table: which page to read for which kind of work.
3. Read the pages `readme` names for your task before writing code — the process pages it links in the `project-management` workspace, then the ADR itself: the record, not a summary of it.

**Pass `workspace: "c8b0fadf-176c-418a-a8a4-a148dcf9fd1d"` on every cortex call** — page, atom, search, tag, comment, knowledge graph, reads and writes alike. The MCP token has one current-workspace pointer, shared by every session presenting that token; any of them can move it between two of your calls, and a call that omits `workspace` resolves against wherever the pointer happens to be, with no error. Prefer the UUID over the slug `aegis-architecture`.

## The rules you must see before you connect

**AEGIS is pre-alpha in its code paths and published in its artefacts.** No backward-compatibility shims, no legacy code paths, no deprecation cycles inside a repository — remove any you find. The carve-out is anything holding state a real tenant depends on: user volumes, tenant records, billing state, and the Postgres schemas underneath them. A change there is a forward-only migration, never a removal.

**Never bump a version string, create a tag, or push a tag without Jeshua's exact trigger phrase.** See `guidelines/version-management` in the workspace. If you are not sure whether versioning was requested, it was not.

**An agent's job ends at commit and push.** CI builds and publishes the image. Jeshua handles every deployment, dev and production alike.

## The repository map is one directory up

`../CLAUDE.md` in the local monorepo directory maps every 100monkeys-ai repository to the workspace that governs it, and carries the instructions for attaching the cortex MCP server if it is not present in your session. **`zaru-client` and `zaru-marketing` are governed by the `zaru` workspace, not by this one.**

## Modifying this file

Keep it to "where to look, how to attach, and what an agent must see before connecting". Anything longer belongs in a page in the AEGIS Architecture workspace, not here.
