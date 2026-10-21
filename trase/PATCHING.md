# Making a patch

The procedure for a change to upstream code on this fork, and the checklist a
PR must answer. Branch and ledger mechanics are in [README.md](README.md) and
[PATCHES.md](PATCHES.md); this file is about getting the patch right. Every
item here came from a mistake made on the first patch stack, named in the
examples so the reason is not lost.

## 0. Decide whether it is a patch at all

A patch is the last option. In order of preference:

1. Configuration the renderer emits (`ai.routes`, `pathOverride`, policies).
2. A receiver-side change in proxy-authorizer or the recorder.
3. An engine patch.

Example: patch 006 taught an `ai` backend without `ai.routes` to classify
paths from the engine's own table. It was written, reviewed, measured and then
dropped, because the renderer must emit a full route table anyway for the
passthrough paths, and a non-empty table overrides the patch entirely. The
route table is config; it needed no fork surface.

The PR body states which config, policy or receiver-side change was considered
and why it was not enough. "None considered" is not an answer.

## 1. Research upstream before writing

Required, and recorded in the PR under "Upstream research". The first patch
stack skipped this step; the research afterwards found an open issue on the
exact code one patch builds on, an open cost-accounting bug the patch inherits,
and an upstream commit ahead of our base that rewrites the files the patch
touches.

Search issues, PRs and discussions for the subsystem and for the words a user
would use:

```bash
gh api -X GET search/issues -f q='repo:agentgateway/agentgateway "promptGuard"' \
  --jq '.items[] | "\(.number) [\(.state)] \(.title) (\(.updated_at[:10]))"'
```

List the upstream commits ahead of our base that touch the files the patch will
touch. Each one is a conflict at the next rebase and a sign the subsystem is
moving:

```bash
git fetch origin main
git log --oneline "$(git describe --tags --abbrev=0 --match 'v*' --exclude '*-trase*')..origin/main" -- <paths>
```

Record in the PR:

- Related issues, with the maintainers' stated position. An issue a
  maintainer called intentional (agentgateway #2956, streamed masking is "a
  purposeful gap") means the patch builds on behaviour upstream may change or
  remove.
- Open PRs on the same subsystem, and whether the patch should wait for them.
- Upstream commits ahead of the base on the same files, as the collision
  forecast for the rebase.
- Open upstream bugs the patch inherits (agentgateway #3739: a placeholder
  zero usage on an Anthropic stream can be recorded as final). The patch does
  not fix them; the PR names them so the consumer knows.

Put the primary issue in the `Upstream-Issue` trailer.

## 2. Shape

- **Additive and flag-gated.** With the flag off every byte is as before, and
  a test asserts it against the stock payload.
- **No default changes.** Patch 006 changed what `/v1/messages` meant on a
  backend without a route table. Correct, and still a behaviour change for
  anyone who relied on the old default. If a default must change, the PR has a
  "Behaviour change" section naming who is affected and how they notice.
- **Complete across every input format** the engine parses, buffered and
  streamed, when the patch touches a guard or a policy. A partial guard is a
  bypass.
- **Fields come from the engine's own objects.** A number the webhook sends
  must be the number the access log, the CEL variables and the rate-limit
  charge read, from the same object at the same point, never computed again
  beside them. Carry the status next to the number: a cost of zero with no
  status cannot be told from an unpriced model, which is why `usage` carries
  `costStatus`.
- **Strip what an earlier guard may have masked.** Text captured for logging
  before the guard chain runs (the prompt and completion copies in the usage
  context) must not travel in a new field; the stock fields already carry the
  post-mask text.

## 3. Side effects

Each item gets a sentence in the PR body, "none" included.

- **New outbound calls.** On the request path or detached; timeout; whether
  it is retried; what the receiver dedupes on; what happens to its verdict.
  The stream-end call in patch 002 is retried once, so the receiver must treat
  two `final: true` calls with the same `decision_id` as one. That is a
  contract the PR states, not a surprise the receiver discovers.
- **Every ending of a stream.** Normal end, client abort, guard block, engine
  error. For each: which calls fire, what is counted, what the log records.
  The first version of patch 002 lost the final call when the client
  disconnected during a window evaluation.
- **Memory and size.** How many copies of the payload exist during the call
  and what bounds them. `includeBody` holds a multimodal request three times
  (the typed struct, the JSON value built from it, the serialized bytes) and
  sends a webhook body as large as the client's request; a receiver with a
  smaller body limit rejects it and the engine fails closed. State the receiver
  limit the patch assumes.
- **Ordering.** Before or after the guard; before or after the log store and
  the rate-limit charge. A guard that rejects a buffered answer returns before
  the log store, so the webhook payload is the only place that call's cost
  exists.
- **Settings the consumer must set** for the patch to do anything
  (`promptGuard.streaming: Enabled` for every streamed field), with a contract
  test on the consumer's side.

## 4. Measure

Reasoning from the source is not evidence. Run the binary against a mock
upstream and a capturing webhook, one config per row, and put the table in the
PR: config, request, what the webhook received, what the client received,
what the log recorded. Include the abort and block rows for anything streamed.
`target/debug/agentgateway -f config.yaml --validate-only` checks a config
before the run.

## 5. Review before merge

Two separate reads, by someone or something that did not write the patch:

1. **Safety and correctness of the diff.** Policy bypass, data that leaks past
   a guard, drop and error paths, retries, future sizes, lock order.
2. **Fitness against the design.** Do the fields mean what ADR 0063, the plan
   and the Cedar schema assume? For cost: the table of cases (usage present,
   usage absent, model unknown, no catalog, blocked stream, abandoned stream)
   with what the field says in each.

A finding is fixed, or recorded in the PR under "Known holes" with the
upstream issue that tracks it.

## 6. PR body

Sections, in this order. Short is fine; missing is not.

1. Why Trase needs it, and the config or receiver-side alternative considered.
2. What changes with the flag on; the proof that nothing changes with it off.
3. Upstream research: issues, PRs, collision forecast, inherited bugs.
4. Behaviour change (or "none").
5. Side effects, per the list in § 3.
6. Measurements, per § 4.
7. Known holes.
8. Tests.
9. Upstream plan: which issue it goes under, when it is submitted, what is
   left out of the upstream version.

## 7. Upstream submission

The rules in [README.md § Patch guidelines](README.md#patch-guidelines)
apply: human-written issue, PR description and comments, AI assistance
disclosed, DCO sign-off, Conventional Commit subject. The upstream PR is
opened from a branch on this fork and its URL goes in the `Upstream-Status`
trailer. The upstream version may be smaller than ours; the ledger row says
what we carry that upstream did not take.
