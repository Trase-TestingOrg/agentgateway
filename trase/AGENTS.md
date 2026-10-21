# Working in the Trase build of agentgateway

Read `trase/README.md` first. This file is for an agent making or rebasing a
patch here.

## Build and test

- Toolchain: `rust-toolchain.toml` pins it; `rustup` picks it up.
- `make lint` (fmt check and clippy with the denied methods list), `make test`
  (library, integration, examples and benches). Both must pass before a PR.
- `make generate-schema format` after any change to a `schema!` struct; commit
  `schema/config.json` and `schema/config.md`.
- The binary: `cargo build -p agentgateway-app`; run with
  `target/debug/agentgateway -f config.yaml`, `--validate-only` to check a config.
- Measure behaviour with a mock upstream and a capturing webhook rather than
  reasoning from the source alone; a result table with the config used is what
  goes into a PR or a design document.

## Rules for a patch

- One behaviour per commit, on a `patch/NNN-slug` branch, with the ledger
  trailers from `trase/PATCHES.md`. `trase/check-patches.sh` fails CI without them.
- Touch upstream files only inside a patch commit. Fork-only files live under
  `trase/`, the root `CLAUDE.md`, and `.github/workflows/trase-*.yml`; nothing
  else is ours.
- Off by default, byte-identical when off, with a test that proves it.
- Complete across every input format the engine parses (OpenAI chat,
  Responses, Anthropic Messages, Gemini; buffered and streamed) when the patch
  touches a guard or a policy.
- Match the surrounding code's comment density and naming; upstream reviews
  style.
- Do not write upstream-facing prose. The commit message, the upstream issue
  and the upstream PR description are the author's own words (upstream's code
  of conduct). Draft them only when asked, and mark the draft as one.
- Before patching a subsystem, check upstream's open PRs and issues for work
  that removes or rewrites it; a patch on churning code conflicts on the next
  rebase.

## Release

`trase/release.sh` on `trase/main`; never tag by hand.

## Rebase

Follow the procedure in `trase/README.md`. Review the result with
`git range-diff`; a patch whose diff changed in the rebase needs its owner's
eyes. Drop a patch upstream has merged and update `trase/PATCHES.md`.
