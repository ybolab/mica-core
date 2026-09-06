# `pkgs/mosd/tests/apid-api` — the apid black-box HTTP suite

A bun + TypeScript suite that talks to **apid** in a booted mos guest over the
network, the way a browser would, and asserts on what comes back. It has **no
runtime dependencies**: a client that follows redirects, manages cookies
invisibly and normalises request targets would hide the exact behaviours this
suite exists to observe.

Nothing here modifies `pkgs/mosd/`. Any apid defect this suite finds is **reported,
never fixed from inside the test tree**.

## One boot, phased, ordered

A TCG boot of the image reaches apid's `APID_LISTENING` line in **60–66 s** and
both readiness signals in **65–72 s** — measured on this host on 2026-08-24,
across this campaign's eight runs, under TCG with no `/dev/kvm` and on a quiet
machine. That is a measurement of one host on one day, not a property of the
image: a contended host is materially slower, which is why the harness's
readiness deadline stays at 900 s and is not trimmed to fit these numbers.

### Both boards, measured the same way

Since PLAN-085 the harness boots either UEFI board, selected by `MOS_BOARD`.
Wall clock from the `docker run` to the `APID_LISTENING` line, fresh disk,
5 s polling, 2026-09-06:

| board | wall clock | guest time | machine |
|---|---|---|---|
| `x64` | **75 s** | 39.3 s | `qemu-system-x86_64 -machine q35`, OVMF |
| `virt-arm64` | **95 s** | 57.8 s | `qemu-system-aarch64 -machine virt`, AAVMF |

**The arm64 board costs 1.27× the amd64 one, not an order of magnitude.** That
is worth stating because the opposite is the natural assumption about an
emulated foreign architecture, and PLAN-085 declined to predict a multiplier
for exactly this reason. Both run under TCG here — there is no `/dev/kvm` for
either — so the comparison is like for like.

The readiness deadline is **not** changed for `virt-arm64`: 900 s over a
measured 95 s is 9.5× headroom, and a board-specific deadline would be
machinery for a problem the measurement says does not exist.

**A boot per test is still not viable on that figure.** The suite therefore
runs against one factory-fresh boot and hands credentials and state between
ordered phases. The current suite runs the shipped SPA/API boundary first, then
the onboarding, update and recovery surfaces:

| id | what it covers |
|----|----------------|
| `01-spa-boundary` | `/` → `/_ui/`, embedded SPA assets, API JSON errors, and retired form routes |
| `02-session` | JSON setup/login/logout, the session cookie, and CSRF enforcement |
| `03-api-management` | cookie and bearer API reads, a CSRF-protected write/task, and inert legacy paths |
| `04-network-observation` | configured intent plus current interface count, details and states |
| `05c-kernel-net` | the live kernel creating VLAN, bridge and WireGuard links, plus key-store permissions |
| `06-onboarding-claim` | the claim record `POST /api/v1/setup` wrote, the empty provisioning-import record, and setup refusing to run twice |
| `07-update-rollback` | the `rollback` verdict mosd computes from RAUC's live slot state, and the 409 a refused rollback answers with |
| `08-reset-recovery` | staging a tier-1 reset and reading the intent back, and the presence gate refusing tier 3 and credential recovery |

State coupling between phases is **accepted**, and then made structural. Every
phase declares an `assumes` string saying what it expects the previous phase to
have left behind; the runner **refuses to run a phase whose `assumes` is empty**,
and prints it above the phase's output. Once a phase fails, every later phase is
`SKIP`ped with a line naming the phase that failed and this phase's own
assumption — so a phase-3 failure is never read as a phase-4 bug.

A `SKIP` is not a `PASS`. It is excluded from both sides of the `RESULT` count.

## The address: three doors, and none of them is loopback

`APID_HOST` has **no default**, and specifically no default of `127.0.0.1`.
Reaching the guest crosses three separate hops, and getting any one wrong
presents identically as "connection refused":

1. QEMU's `hostfwd` binds **inside the container running QEMU**.
2. That container must **publish** the forwarded port.
3. `-p 127.0.0.1:...` publishes on the **docker host's** loopback — which is not
   the loopback of whatever container you are running this suite from.

So `APID_HOST` is the **QEMU container's address on the shared docker network**
(this campaign's is `traefik`, `172.18.0.0/16`).

Related: apid's `:80 → :443` redirect is a `308` whose `Location` names the
**guest's** port 443. Through a port forward that authority is unreachable, so
the client **asserts the redirect and never follows it**. `follow()` throws
`CrossAuthorityRedirectError` rather than hanging; a client that follows blindly
here looks exactly like apid being down.

## Environment

| variable | required | default | meaning |
|----------|----------|---------|---------|
| `APID_HOST` | **yes** | — | address of the container running QEMU. Not loopback. |
| `APID_HTTPS_PORT` | no | `18443` | published port reaching the guest's `:443` |
| `APID_HTTP_PORT` | no | `18080` | published port reaching the guest's `:80` |
| `APID_CONSOLE` | no | — | path to the captured QEMU console log |
| `APID_ADMIN_PASSWORD` | no | `mos-e2e-admin-pw` | password sent to `/api/v1/setup` and `/api/v1/session` |
| `APID_HOSTNAME_TARGET` | no | `mos-e2e-renamed` | hostname sent to `/api/v1/setup` and read back through `/api` |
| `APID_PHASES` | no | all | comma-separated phase ids; a partial run says so, loudly |
| `APID_RESULT_JSON` | no | — | path for the machine-readable result |
| `APID_NEGATIVE` | no | — | invert the first matching check, to prove a run can go red |

## Running

The whole thing, image and all:

```sh
make os-apid-api-test
```

The self-test — **no network, no docker, no QEMU, no image**. Measured under
`oven/bun:1` (bun 1.4.0) on 2026-08-24: `selftest` itself takes **~0.17 s** and
`typecheck` **~2 s**, so the whole block below is a couple of seconds after the
first `bun install`:

```sh
bun install
bun run typecheck
bun run selftest
```

`selftest.ts` drives every assertion helper, the cookie jar, the redirect
refusal, the verbatim request writer and the phase runner against inputs that
are **deliberately wrong**, and requires each one to fail *with its own
message*. Its `RESULT: PASS (n/m checks)` means "n wrong answers were correctly
rejected". Positive controls sit beside every negative, so a helper hardwired to
always fail does not satisfy it either.

Run it before and after touching anything in `src/`. An assertion that stays
green on a wrong input is the defect this file exists to catch.

The other no-boot check, and the only one CI runs:

```sh
bash spec-pins.sh          # or: make os-apid-api-spec-pins, from the repo root
```

`src/spec-pins.ts` asserts that every literal a phase pins which
`pkgs/mosd/apid/openapi.json` ALSO states agrees with the document. It covers
the session, setup, settings, UI and observed-network contracts, reading the
phase files' own bytes rather than importing them, so both directions of drift
go red. It exists because the phases below only run under a
booted run: a milestone that moves a shipped status otherwise leaves every
phase pinning the old one green until somebody boots the image.
Its header states what is out of scope and why; the full black-box suite covers
the remaining runtime contracts, which are most of this file's pins.

It runs on a host bun when there is one and in the bun pinned as `IMAGE_BUN_1`
otherwise, and says which. `MOS_APID_CONTAINER=1` forces the pinned container.

bun is not required on the host — run it in a container, mounting the
**repository** (a `/tmp` mount does not propagate to the docker daemon here and
silently yields an empty directory):

```sh
docker run --rm -v "$(git rev-parse --show-toplevel):/w" -w /w/pkgs/mosd/tests/apid-api \
  oven/bun:1 sh -c "bun install && bun run typecheck && bun run selftest"
```

## Layout

```
src/spec-pins.ts  the build-time pin check: the phase literals openapi.json
                  also states, asserted against it with no boot
src/config.ts     the environment contract; validates once, then freezes
src/client.ts     the browser-simulating client: jar, manual redirects,
                  per-request TLS scoped to APID_HOST, and raw() for
                  byte-for-byte request targets
src/report.ts     the house PASS/FAIL/RESULT format and the assertion helpers
src/runner.ts     phases, `assumes`, and failure containment
src/main.ts       the ordered registry — every phase imported up front, so a
                  phase author edits exactly one file
src/selftest.ts   the proof the machinery can go red
src/phases/       one module per phase
```
