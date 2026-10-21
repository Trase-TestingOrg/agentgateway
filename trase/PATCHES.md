# Patch ledger

One row per commit on `trase/main` above the upstream base. The row is derived
from the commit's trailers; `trase/check-patches.sh` fails CI when a commit
lacks them.

Trailers on every patch commit:

```
Patch-Id: NNN-slug
Upstream-Status: Pending | Submitted <url> | Backport <sha> | Denied <reason> | Inappropriate <reason>
Upstream-Issue: <url>
Owner: @github-handle
Added: YYYY-MM-DD
Drop-When: <condition>
```

`Upstream-Status` uses the Yocto vocabulary. `Pending` means not yet sent;
`Submitted` carries the PR link; `Backport` means merged upstream and carried
until the next tag; `Denied` and `Inappropriate` require the reason.

| Patch | Purpose | Upstream status | Owner | Added | Drop when |
|---|---|---|---|---|---|
| `000-fork-files` | The fork's own files: this directory, the root `CLAUDE.md`, the `trase-*` workflows. | Inappropriate: fork-only files | @jaketrase | 2026-10-21 | never; carried on every rebase |
| `001-webhook-include-body` | `includeBody` on the prompt-guard webhook: the request payload gains `request` and `normalized_messages`, the response payload gains `response`, so proxy-authorizer fills the model call's context without parsing provider formats. | Pending; thread [agentgateway #2955](https://github.com/agentgateway/agentgateway/issues/2955) | @jaketrase | 2026-10-08 | merged upstream |
| `002-webhook-usage-cost` | With `includeBody`, the response webhook carries `usage` (the engine's priced usage context, the access log's `llm` fields) on a buffered answer; a streamed answer's windows carry `index` and `stream`, and one detached final call after the stream ends carries `usage`, `final: true` and the window count. Byte-identical with the flag off. | Pending; the stream-end call is a contract change to raise on [agentgateway #2955](https://github.com/agentgateway/agentgateway/issues/2955) | @jaketrase | 2026-10-21 | merged upstream |
| `006-backend-path-table` | A backend with no `ai.routes` classifies the standard LLM paths with the engine's own table instead of treating every path as chat completions, so `/v1/messages`, `/v1/responses` and `/v1/embeddings` parse as what they are and the renderer carries no route map. | Pending | @jaketrase | 2026-10-21 | merged upstream |

Base: the upstream tag `trase/main` sits on, found with
`git describe --tags --abbrev=0 --match 'v*'`.
