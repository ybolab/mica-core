# 20260913-2230-independent-apid-upgrade PROPOSAL: mica-apid upgrades beside an unchanged micad

- **status**: proposed (not applied; pending the user's decision)
- **createdAt**: 2026-09-13 22:30
- **task**: `20260913-2200-build-env-release-v0.0.1` (notes)

## Problem

An API-only update must install a new `mica-apid` while the device keeps the
exact `micad` archive it already has. Today `mica-apid` declares
`Depends: micad (= @VERSION@)`, and the package gate (`scripts/deb/package-gate.sh`,
check b/i) requires that exact pin between packages built here, so every
`mica-apid` needs the `micad` of its own commit and dpkg refuses any other.
mica-build-env `RULES.md` 6 (v0.0.1 and v0.0.2 alike) lists the pool gates and does not name
the exact-pin rule; it lives in the reference gate this repository now owns.

## Proposal: the D-Bus interface is the dependency

The contract between the two is already named and versioned on the bus:
interface `com.mica.micad1` on `com.mica.micad` at `/com/mica/micad`.

1. `micad` declares `Provides: com.mica.micad1 (= N)`, where N is the
   interface revision. A revision only adds members (methods, signals,
   properties) or JSON fields a reader may ignore; it never changes or
   removes one. A breaking change is a new interface, `com.mica.micad2`, and a
   new virtual name, so nothing built for `1` is satisfied by it.
2. `mica-apid` declares `Depends: com.mica.micad1 (>= R)`, where R is the
   lowest revision that has every member its proxy calls
   (`crates/mica-apid/src/bus_client.rs`), and no longer depends on `micad`.
3. The revision is written once, in a committed introspection snapshot per
   revision, `crates/mica-core/dbus/com.mica.micad1.r<N>.xml`, and tests hold
   it: micad's live introspection equals the newest snapshot; each snapshot
   contains its predecessor's members with the same signatures; micad's
   control `Provides` names the newest N; mica-apid's proxy members all exist
   in snapshot R and its control `Depends` names R.
4. The owned package gate replaces the exact-pin rule for this pair with a
   versioned-virtual rule, fail-closed: a `Depends` on a name only provided
   in the pool must carry `(>= R)` and be satisfied by the pool's `Provides`
   version; an unversioned or unsatisfied one fails.
5. Whatever composes a root with dpkg runs a closure check over the dpkg
   database after its last install: every installed package is `ii`, and its
   `Pre-Depends` and `Depends` are satisfied (`dpkg-checkbuilddeps
   --admindir`). dpkg checks a package's own dependencies when it configures
   it, but does not re-check installed reverse dependencies when another
   package is replaced, so without this a lower-revision micad installs
   silently under a mica-apid that needs more.

## Acceptance (`scripts/gate/interface-dependency-test.sh`)

Synthetic archives packed by `scripts/deb/pack.sh` and installed with the
dpkg of the pinned `IMAGE_MICA_BUILD_BASE` (1.22.22) into a fresh
`--root`/`--admindir` per case:

| Installed | Installing | dpkg | closure |
| --- | --- | --- | --- |
| micad@A `Provides: com.mica.micad1 (= 3)` | mica-apid@B `Depends: com.mica.micad1 (>= 3)` | accepted, micad@A untouched | accepted |
| micad@A (= 3) | mica-apid `(>= 4)` | refused, apid `iU` | -- |
| micad `Provides: com.mica.micad2 (= 1)` | mica-apid@B | refused | -- |
| nothing | mica-apid@B | refused | -- |
| micad@A + mica-apid@B | micad `Provides: com.mica.micad1 (= 2)` | **accepted** | refused |
| micad@A | mica-apid `Depends: micad (= <its own commit>)` (today) | refused | -- |

Result on 2026-09-13: 10 passed, 0 failed.

## What it does not cover

- Semantics inside a member's JSON strings are held by review and by the
  existing bus tests, not by the snapshot; a change there that a reader cannot
  ignore is a breaking change and needs `com.mica.micad2`.
- `mica-mqttd` and `mica-mqtt-broker` pin `micad` exactly too; the same rule
  would apply to them, and this proposal does not change them.
- The micad archive published at 74235c3 carries no `Provides`, so it cannot
  be the retained micad@A; the first micad published under this rule is.
- Selecting micad@A and mica-apid@B from two pools is the assembly's lock
  (`mica-build`, `deps/packages/`), and the closure check in 5 is the
  composer's; both are outside this repository.

## Decision needed

Accept the D-Bus interface name and revision as the only compatibility unit
between micad and mica-apid (1-4 in this repository), and route 5 to the
root composer. Items 1-4 alone do not make the upgrade independent end to
end: without 5, the micad downgrade case above installs with both packages
`ii`. Complete acceptance is 1-4 here and 5 in the composer's final dpkg
database; JSON semantics stay a review and bus-test obligation.
