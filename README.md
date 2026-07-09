<p align="center">
  <img src="assets/flotswarm-logo.png" alt="flotswarm logo" width="720">
</p>

# flotswarm

Webhooks, distributed and run on your infra. A flotilla of hosts, a swarm of
agents pulling dispatched work — corralling the flotsam of your infrastructure.

This repo holds everything flotswarm is made of: the Rust crates (below), the
packaging that ships the agent as a deb/rpm, and a reusable **Terraform module**
(`terraform/flotswarm/`) that stands up the AWS side (API Gateway, Lambda, SQS,
IAM). Your infrastructure repo consumes that module from a thin root.

## Shape

```
GitHub / event ─▶ API Gateway ─▶ flotswarm-distributor (Lambda)
                                    verify HMAC, then SendMessage (broadcast)
                                        │
                        per-host SQS queues (flotswarm-<host>)
                                        │
                                 flotswarm-agent (per host)
                                    long-poll → allowlist → run → delete
```

No SNS: the distributor writes straight to the host queues, and each agent's
local **allowlist** (`/etc/flotswarm/actions.d/*.toml`) decides what it will run.
Routing (which queues exist) is data the distributor reads from SSM, so adding a
host never redeploys the Lambda.

## Crates

| Crate | What it is |
|-------|------------|
| `flotswarm-types` | The shared contract: the message [`Envelope`] and the `actions.d` config schema, with **format** validation + argv assembly. Pure, dependency-light, unit-tested. |
| `flotswarm-distributor` | The ingress Lambda (`bootstrap` binary for `provided.al2023`). Verifies the HMAC signature and `SendMessage`s the envelope to the host queues. |
| `flotswarm-agent` | The per-host daemon. Long-polls its SQS queue, validates each message against the local allowlist, runs the mapped command, deletes on success. |

`flotswarm-types` is real and tested today; the distributor and agent are
compiling skeletons — the AWS SDK wiring (SendMessage, ReceiveMessage, HMAC,
SSM) is the next increment, marked `TODO` in each `main.rs`.

## Security model (three layers)

1. **HMAC-SHA256** at the distributor — only callers who know a hook's secret can
   inject a message.
2. **IAM** — only the distributor may send; each host may only read *its own*
   queue.
3. **Agent allowlist** — a message can only name a pre-registered action; args
   are validated per-action (argv-only, format + repo-state checks, no shell).

## Build

```sh
cargo test                       # runs the flotswarm-types validation tests
cargo build                      # builds all three crates
cargo run -p flotswarm-agent     # prints the skeleton validation demo
```

Packaging (see `packaging/`): `make deb` / `make rpm` build the agent package
(nfpm) with its systemd unit; `make lambda` builds the distributor zip
(`cargo lambda build --arm64`). On a tag push, CI
(`.github/workflows/release.yml`) publishes all three as GitHub Release assets;
point your own package repo and Terraform at those.

## Layout

```
crates/
  flotswarm-types/         Envelope + ActionConfig + validation (+ tests)
  flotswarm-distributor/   Lambda ingress (bootstrap)
  flotswarm-agent/         per-host SQS consumer
packaging/
  actions.d/               example allowlist entries
  systemd/                 flotswarm-agent.service
  agent.conf.example       per-host config (queue url, region, creds)
  sudoers.example          scoped privilege for run_as
```
