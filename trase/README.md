# The Trase build of agentgateway

This repository is a fork of [agentgateway/agentgateway](https://github.com/agentgateway/agentgateway)
that carries a short stack of patches Trase needs before upstream ships them. The
build that runs as the governance proxy in Trase OS comes from here.

## The one rule

A commit lives on `trase/main` only if it is submitted upstream, or its ledger
entry says why it cannot be. Every patch is written to be deleted: the moment
upstream merges it, the next rebase drops it. Nothing in this repository is a
long-term divergence. Documented fork failures all share one cause, a base that
stopped moving, not a large patch count; the controls below exist to keep the
base moving.

## Branches and tags

| Branch or tag | What it is | Who writes it |
|---|---|---|
| `main` | An exact mirror of upstream `main`. Never commit to it. | the sync job |
| `trase/main` | The latest upstream release tag plus the patch stack, one commit per patch, linear. | rebase PRs only |
| `patch/NNN-slug` | One patch under review, one commit, opened as a PR into `trase/main`. | the patch owner |
| `trase/vX.Y.Z-N` | A fork release: upstream `vX.Y.Z` plus the stack at that time, build `N`. The image is tagged `vX.Y.Z-trase.N`. | the release workflow |

The git tag does not start with `v`, so upstream's own release workflow,
which triggers on `v*.*.*`, never fires in this fork, and `git describe
--match 'v*'` finds the upstream base without excluding it. The image tag
`vX.Y.Z-trase.N` sorts below `vX.Y.Z` under semver (a pre-release string), so
consumers pin the image by digest, never by tag order.

## Adding a patch

1. Open an upstream issue or design discussion first when the change is over
   about a hundred lines or touches a policy surface. Upstream treats those as
   features and wants the design before the code.
2. Read [the patch guidelines](#patch-guidelines) below.
3. Branch from `trase/main` as `patch/NNN-slug`, make one commit, and put the
   ledger trailers on it (see [PATCHES.md](PATCHES.md)).
4. Open a PR into `trase/main`. The PR body explains why Trase needs it and how
   it is used; the commit message is what upstream will read, in the author's
   own words.
5. Merge by rebase only, so the stack stays linear.
6. Open the upstream PR from a branch on this fork, and record its URL in the
   trailer as `Upstream-Status: Submitted <url>`.

## Rebasing onto an upstream release

Upstream tags a minor roughly every five weeks and most minors carry breaking
changes, so the stack moves on every tag.

```bash
git fetch upstream --tags
git checkout -b rebase/vX.Y.Z trase/main
git rebase --onto vX.Y.Z $(git describe --tags --abbrev=0 --match 'v*')
make generate-schema format
make lint test
git range-diff <old-base>..trase/main vX.Y.Z..HEAD
```

Drop any patch upstream has merged (`git rebase --skip` on it, then mark the
ledger entry). Open a PR from `rebase/vX.Y.Z` into `trase/main`; the CI runs
upstream's own lint and tests plus the ledger check. The nightly rebase probe
tries this against upstream `main` every night and fails loudly on conflict, so
a conflict is found weeks before the tag.

## Releasing

```bash
trase/release.sh
```

On a clean, synced, green `trase/main` it runs the ledger check, tags the next
`trase/vX.Y.Z-N` for the current upstream base, pushes the tag, waits for the
release workflow (upstream's Dockerfile, pushed to Google Artifact Registry with
build provenance) and prints the image digest to pin in the platform. Pass a
build number to choose it, `--no-wait` to tag and leave.

Until the Artifact Registry inputs exist, `trase-build-ghcr.yml` (Actions, Run
workflow) builds any ref and pushes it to this org's GitHub Container Registry
with the job's own token, printing the digest; pull it and push it to Artifact
Registry by hand.

## Patch guidelines

What upstream accepts, and what it has declined, read from its contribution
rules, its maintainers' reviews and its code of conduct.

- **Extend through hooks and configuration, not core paths.** The engine's
  extension points are `ext_authz`, the prompt-guard webhook, the `ExtMcp`
  hook and `ext_proc`. A patch that adds a field to a hook payload or a flag
  to a policy is the right shape; a patch that adds a translation layer or a
  provider format is the shape upstream has declined (Bedrock Converse
  inbound, issue #3855).
- **A guard or policy feature is complete across every input format or it is
  a bypass.** Upstream has stalled external PRs on exactly this. Cover OpenAI
  chat, Responses, Anthropic Messages and Gemini, buffered and streamed.
- **Off by default, byte-identical when off.** A new field or flag changes
  nothing for anyone who did not ask for it, and a test proves it.
- **Tests at upstream's bar.** Unit tests beside the code, an end-to-end test
  with a mock upstream where behaviour is externally visible, and
  `make generate-schema` output committed.
- **Small and single-purpose.** One behaviour per patch, so it can be
  submitted, reviewed and dropped on its own.
- **Upstream's conventions.** Conventional Commit subjects, DCO sign-off with a
  real name, `make lint` and `make test` clean.
- **Human-written prose.** Upstream's code of conduct bans AI-written issues,
  PR descriptions and comments and requires disclosure of assistance. The
  commit message, the upstream issue and the upstream PR are written by the
  author. AI assistance with code and tests is disclosed in the PR.
- **Do not fix what upstream is removing.** Check open PRs and the roadmap
  before patching a subsystem; a patch on code that churns within a release
  will conflict on the next rebase.

## Compliance notes

- The image carries build provenance (GitHub artifact attestations) naming the
  source commit, so the patch set of any running build is recoverable.
- Crate names and versions are upstream's, so vulnerability scanners and VEX
  statements keep matching. The fork identity lives in the tag and the
  provenance, not in the crate metadata.
- Apache-2.0 section 4: modified files carry a notice, and the NOTICE file
  lists Trase's modifications. Until that file exists, PATCHES.md is the
  record.
