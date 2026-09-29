# Agent bootstrap: start at the Council

This file is a bootstrap, not documentation. It holds the address of the contract, not the contract, and it loses every conflict with the cortex: where the two disagree, the cortex is right and this file is stale.

## Ground in this order

1. `cortex.ground` on instance `100monkeys-ai`; its payload lists every workspace with its UUID. Name the `council` workspace by that UUID and read the Council `home`, then `roster`.
2. Ground in this repository's workspace, `aegis-architecture`, whose project manager is the `zaru-fleet` seat on the roster, and read its landing page.
3. Read the brief you were handed, by your project manager where the roster names one.

AEGIS public documentation is the `aegis` workspace, which has no seat; the `zaru-fleet` seat may claim it.

## Every cortex call names its workspace

"Pass your task's workspace as `workspace` on every call from here on; never call `me.set_current_workspace`, whose pointer every session on the token shares." The UUIDs come from `cortex.ground`; this file carries none.

## Where the contract lives

In the workspace grounding `cortex.ground` returns, and in the pages the roster names for this repository's seat, where it has one: contract, ledger, directives, rulings. Nothing here restates them.

`CLAUDE.md` is a symlink to this file, `AGENTS.md`; edit only this file.

## AGENTS.md — Bootstrap for aegis-orchestrator

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

## The repository map is on the Council

The Council page `repository-bootstrap` maps every 100monkeys-ai repository to the workspace and seat that govern it. **`zaru-client` and `zaru-marketing` are governed by the `zaru` workspace, not by this one.**

## Modifying this file

Keep it to "where to look, how to attach, and what an agent must see before connecting". Anything longer belongs in a page in the AEGIS Architecture workspace, not here.
