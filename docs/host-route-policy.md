# Host-route policy v1 (canonical static spec)

Status: normative. This document is the canonical host-route specification
referenced by `tests/fixtures/host-route-policy-v1.json` (`policy_id
host-route-policy-v1`, `schema_version 2` — bumped for §2.1, the
dynamic-container-value and image-operand rule from the pf-hwprobe#23
adjudication; the case JSONL object schema is unchanged, and the filenames
stay `*-v1*` because ports pin paths and sha256s). Ports in other repositories
vendor the policy JSON, the case JSONL and the runner host map from this
repository and pin them with a `.SOURCE` file; they must not restate or fork
the rules.

Origin: incident tsp-ihne1.56 (a CI harness typed into the owner's live laptop
through host uinput fake devices — `/dev/uinput` is host-global and a container
does not isolate it), bead tsp-ihne1.56.5, design pass pft-b9c90522 §3–§4.

Enforcement: `python3 tests/uinput_inventory.py` (tree check) and
`python3 tests/uinput_inventory.py --self-test` (seeded reds, positive
controls, mutants, case replay). CI runs the self-test via
`scripts/test-uinput-domain.sh` inside the pinned CI container. The scanner is
stdlib-only Python; the policy, the cases and the host map are data it loads.

## 1. Threat model

A "host route" is any static evidence that a build, test, workflow or product
path can reach the runner host's real input, display or clipboard subsystems:

- kernel input devices (`/dev/uinput`, `/dev/input/*`) — host-global, never
  isolated by containers;
- the host X11/Wayland session (`DISPLAY`, `WAYLAND_DISPLAY`, `XAUTHORITY`,
  `/tmp/.X11-unix`, `~/.Xauthority`, `/run/user/<uid>/wayland-*`);
- the host clipboard (`wl-copy`, `xclip`, `xsel`, clipboard crates);
- anything that cannot be resolved statically and might be one of the above.

Private, run-owned substitutes are allowed: xvfb-run, an Xvfb the test starts
itself, a headless weston on a generated socket under a run-private
`XDG_RUNTIME_DIR`, FIFOs, socketpairs and fake sinks.

## 2. Rule families

Every finding carries exactly one rule id. The seven rule families are:

| id | covers |
| --- | --- |
| `host_uinput_device` | opening, binding or passing `/dev/uinput` or `/dev/input/*` to a process or container (`--device=`, `--device`, compose `devices:`) |
| `host_input_bind` | bind-mounting host input, X11 socket, Xauthority or Wayland socket paths (`-v`, `--mount`, systemd `BindPaths=`/`BindReadOnlyPaths=`, compose `volumes:`), including `${XDG_RUNTIME_DIR}/${WAYLAND_DISPLAY}` spellings and literal `/tmp/.X11-unix`, `*.Xauthority`, `/run/user/*/wayland-*` sources |
| `input_major_wildcard` | device-cgroup rules granting major 13 (input) with any minor: `c 13:* rwm`, `a 13:* rwm`, quoted, `=`-joined or split-token spellings |
| `host_uinput_permission` | loading or permissioning the host uinput route: `modprobe`/`insmod uinput`, `chmod`/`chgrp`/`chown`/`setfacl`/`mknod`/`install` on `/dev/uinput` or `/dev/input`, `sg input`/`newgrp input`, systemd `DeviceAllow=`, `modules-load.d` entries loading `uinput` |
| `host_display_projection` | inheriting `${DISPLAY}`/`${WAYLAND_DISPLAY}`/`${XAUTHORITY}`, exporting a display without same-boundary private creation, bare host display paths, `xdotool`/`xhost`/`xwd`, source-level `set_var("DISPLAY"|"WAYLAND_DISPLAY"|"XAUTHORITY")` / `os.environ[...] =` / `setenv(...)` |
| `host_clipboard_write` | `wl-copy`/`xclip`/`xsel`, clipboard crates (`arboard`, `copypasta`, `x11-clipboard`, `wl-clipboard-rs`, `gtk::Clipboard`), `copy_text()`/`copy_image()` APIs |
| `dynamic_route` | fail-closed: any value containing a shell expansion that cannot be proven statically (§2.1: container device/volume/mount/env values, systemd `BindPaths=`, compose `devices:`, dynamic `docker run`/`create` image operands outside the `--` end-of-options position, option-shaped words after `--`, `runs-on` expressions, malformed shell/YAML/host map) and every contract violation (§6) |

Tokenisation: shell, YAML and systemd values are tokenised, not grepped.
Backslash-newline continuations, quoting, `$( )`/backticks, heredoc bodies and
whitespace variants are resolved before matching, so `--device=/dev/uinput`,
`--device /dev/uinput` and `--device=/dev/\`+newline+`uinput` all produce the
same finding. Files that cannot be tokenised (unbalanced quotes, tabs in YAML
indentation, trailing flow junk) fail closed as `dynamic_route` with detail
`malformed_file`.

### 2.1 Dynamic values and container image operands

A text scan cannot prove a shell variable's runtime value (pf-hwprobe#23
round-4 adjudication): `printf -v`, `read`, `eval`, sourced scripts and
`$GITHUB_ENV` writes all bypass assignment folding. This spec therefore has no
assignment-folding or proven-variable exception for dynamic container values:

- Any value containing an expansion (`$` or a backtick) is *dynamic*. Dynamic
  container `--device`, `-v`/`--volume`, `--mount` source, `-e`/`--env` name,
  `--device-cgroup-rule` value, systemd `BindPaths=`/`BindReadOnlyPaths=`
  source and compose `devices:` entries are `dynamic_route` (detail
  `unresolved_value`) regardless of same-file literal assignments.
- A dynamic `docker run`/`docker create` image operand — quoted or unquoted —
  is allowed only immediately after `--`, the end of options, where it can no
  longer be reinterpreted as a flag. Anywhere else it is `dynamic_route`
  (detail `dynamic_image_operand`).
- A static literal image needs no delimiter.
- `--` followed by an option-shaped word is still red (`dynamic_route`, detail
  `option_after_end_of_options`).
- To find the image operand reliably, the scanner parses supported CLI option
  arities: known docker/podman global options (for example `--context`) and
  known `run`/`create` options consume their split values. An *unknown* option
  taints the command, because its arity cannot be proven: the `--` delimiter
  is no longer trusted, no positional operand is trusted as the image, and any
  dynamic operand in the tainted `run`/`create` command is `dynamic_route`
  (detail `dynamic_image_operand`). Use inline `--opt=value` forms for options
  the scanner does not know to keep the command scannable. In the `run`
  context `-t`/`-i`/`-d` (and clusters such as `-it`) are booleans; `--pull`
  is string-valued for both `run` and `create` (docker and podman declare
  `--pull string`) and consumes a split or inline value (#147 review round 2).
  An unknown *global* option taints and its following non-subcommand word is
  treated as its possible split value, so a later `run`/`create` is still
  reached (`podman --root /tmp run "$IMAGE"` stays red). The taint fail-closed
  applies inside `run`/`create` only: a non-`run`/`create` subcommand after an
  unknown global option carries no image operand, so it reports nothing
  (`podman --root /tmp inspect "$CONTAINER"` is green).

Assignment folding survives only where it can never excuse anything: the typed
`private_compositor` creation-evidence exception (§3.1), and folding that
*indicts* a non-container `--device` argument whose recorded value is a
literal `/dev/uinput` or `/dev/input/*` path.

## 3. Typed exceptions

Exceptions are typed and evidence-bound. There is no free-form allowlist of
commands or paths.

### 3.1 private_compositor (creation + consumption in one boundary)

A display assignment is excused only when the creating command appears in the
same test boundary (one shell file, or one workflow `run:` block) and the
boundary owns its cleanup:

- `xvfb-run ...` in the boundary;
- `Xvfb :N` started in the boundary, literal `DISPLAY=:N`, and owned cleanup
  (`trap`/`kill` seen in the boundary);
- `weston --backend=headless*` with a socket that is either a literal name set
  as `WAYLAND_DISPLAY` in the same boundary, or a generated value
  (`mktemp`) shared by the weston command and the `WAYLAND_DISPLAY`
  assignment, plus `XDG_RUNTIME_DIR` from `mktemp` in the same boundary.

Never excused, in any boundary: `${DISPLAY}`, `${WAYLAND_DISPLAY}`,
`${XAUTHORITY}`, `$HOME/.Xauthority`, `/tmp/.X11-unix`, and any display value
that still contains an unresolved variable reference after substitution.
A `DISPLAY=:99` without same-boundary `Xvfb :99` creation is a finding
(detail `display_without_creation`); removing the creation line from a passing
case must turn it red — the self-test replays exactly these mutants.

### 3.2 guest_internal

Occurrences that only execute inside a pfvd guest. Classified in the inventory
(§4) with `class=guest_internal caller=<entry point>`. The call is invalid
unless the caller file exists, matches `guest_entry_points`
(`*/run-guest.sh`, `run-guest.sh`, `*/guest/*`) and references the occurrence
file by name.

### 3.3 product_session_capability

The product routes X11 through the producer-owned real capability source
`/run/pocketforge/x11/.X11-unix/X0` (see
`crates/pf-app-launch/src/policy.rs`). A *source-kind* occurrence that literally
carries that producer source path may also carry the `/tmp/.X11-unix/X0`
destination spelling; the scanner excuses that occurrence's
`host_display_projection` finding with detail `x11_socket_path`. The exception
is never file-wide, never applies to workflows, shell scripts or systemd units,
and never excuses a bind whose source is not producer-owned.

### 3.4 product_clipboard_adapter

Product clipboard adapters whose host route is gated off in CI. The occurrence
file must carry the gate marker `host_clipboard_disabled`; otherwise the
inventory row is an `invalid_exception`.

## 4. Inventory (manifest)

Findings that are not excused by §3 must be classified in
`tests/uinput-inventory.txt`. Host-route rows live beside the creator-scan
rows and are tagged:

```
<path>\t<sha256(rule + "\n" + stripped_line)>\t# host-route rule=<rule-id> class=<class> [caller=<path>] [wave2=<bead>] note: ...
```

The key binds path, rule and exact line text; editing the occurrence
invalidates the row (stale) and the new text fails until reclassified.
Bootstrap rows with `python3 tests/uinput_inventory.py --emit-host-routes`.

Classes: `dormant-k-consumer` (requires `wave2=`; kernel-fidelity consumer
kept stopped on runner hosts in wave 1), `product-reference`,
`product-session-fixture`, `guest_internal` (requires `caller=`, validated per
§3.2), `product_clipboard_adapter` (gate marker per §3.4). Occurrences inside
`.github/workflows/**` can never be classified by a manifest row — workflow
consumers must be deleted or made private, not inventoried.

## 5. Runner labels and the runs-on evaluator

The scanner evaluates every workflow job's `runs-on` against the vendored host
map `tests/fixtures/input-runner-host-map-v1.json`, validated against
`tests/fixtures/input-runner-host-map-v1.schema.json`. The authoritative map
is generated in pocketforge-automation
(`config/input-runner-host-map-v1.json`) from committed provisioning only; the
vendored copy is pinned by `tests/fixtures/input-runner-host-map-v1.SOURCE`
(`source_repo`, `source_commit`, `source_path`, `fixture_sha256`) and a sha
mismatch fails closed (`source_sha_mismatch`).

Entry fields: `runner` (unique id), `labels` (complete set) or
`label_patterns`, `host_class` (`github-hosted` | `vm` | `bare-metal`),
`physical_host`, `ephemeral`, `seat_policy` (`forbidden` | `present` |
`unknown`), `provisioning_sources` (citations, non-empty),
`may_launch_pfvd_guest`.

Matching uses the GitHub subset semantics: a job matches an entry when its
label set is a subset of the entry's complete `labels`, or every job label
fnmatches one of the entry's `label_patterns`. Matrix axes of literal scalars
expand; any other `${{ }}` expression in `runs-on` fails closed with
`dynamic_runs_on`. Reusable-workflow calls (`uses:` without `runs-on`) are
evaluated in the callee file.

Consumer policy: a job carrying any non-excused host-route finding that
matches at least one committed host fails with `host_consumer_job` (the
failure names the labels, the eligible hosts with class and seat policy, and
the finding rules); a consumer matching no committed host fails with
`no_eligible_host`. Every job is reported regardless:

```
runs_on=<workflow>#<job> labels=[...] eligible=[runner(class,seat=...),...]
```

## 6. Failure contract

Failures print one line each and exit non-zero:

```
host_route_inventory=FAIL reason=<rule> path=<path> line=<line> rule=<rule> detail=<detail> text=<stripped line>
```

Contract details (always failures, never classifiable): `stale_inventory`,
`duplicate_inventory_row`, `malformed_inventory_row`, `invalid_exception`,
`malformed_file`, `malformed_host_map`, `missing_host_map`,
`source_sha_mismatch`, `dynamic_runs_on`, `host_consumer_job`,
`no_eligible_host`, `policy_mismatch` (policy JSON disagrees with the
scanner's canonical rule/class tables or the schema disagrees with the
loader).

## 7. Self-test

`--self-test` replays, in one invocation:

1. the creator-scan seeds (stale manifest row, duplicate live occurrence,
   seeded violation, clean tree) — original behaviour, unchanged;
2. every case in `tests/fixtures/host-route-cases-v1.jsonl`: seeded reds for
   all seven rule families (including continuation, quoting and whitespace
   variants, and a dynamic image operand per mutation channel — `printf -v`,
   `read`, `eval`, a sourced script, `$GITHUB_ENV`, unquoted — plus `--`
   followed by an option-shaped word), positive controls (private xvfb-run,
   `Xvfb :99` + literal `DISPLAY=:99`, headless weston on a literal and on a
   generated socket, broker FIFO/socketpair, `guest_internal`, docs-only text,
   `product_session_capability`, clipboard adapter, clean workflow, private
   display inside a workflow run block, `.SOURCE`-pinned map, a `--`-delimited
   dynamic image operand, a static literal image) and mutants (removing the
   creation line from a private-compositor green turns it red);
3. the full tree check.

A case is `ok` when a red case produces a failure matching its declared rule
or contract reason and a green case produces none. Any `BROKEN` case exits 2.

## 8. Scope and wave policy

Scanned kinds: GitHub workflows (`*.yml`/`*.yaml` under
`.github/workflows/`, plus `*.yml.in`/`*.yaml.in` templates), other YAML
(compose), shell (`*.sh`, `*.bash`, `Makefile`, `Dockerfile`, `Containerfile`,
`#!sh|bash|dash` scripts), systemd units (`*.service`, `*.socket`, `*.mount`,
`*.target`, `*.conf`) and source (`*.py`, `*.rs`, `*.c`, `*.h`, `*.toml`,
`#!python` scripts). Never scanned: docs, data fixtures (`*.md`, `*.json`,
`*.jsonl`, `*.tsv`, `*.txt`), `tests/fixtures/**`, the scanner and the
manifest themselves. Docs-only text mentioning forbidden commands is a green
control, not a finding.

Wave 1 (this bead): kernel-fidelity (K) consumers stay stopped on runner
hosts; the existing uinput execution-domain guard and its fixture
(`tests/fixtures/uinput-domain-v1.tsv`) are unchanged and domain (c)
(transitional seatless pf-builder-vm) is still present. Deleting the host
routes guarded by `dormant-k-consumer` rows is wave 2 work, gated on the
tsp-ihne1.56.4 guest E2E. No shims, skips or "if `/dev/uinput` exists"
branches may be added: delete the host route or keep the job stopped.
