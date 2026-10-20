# Host-route policy v1 (canonical static spec)

Status: normative. This document is the canonical host-route specification
referenced by `tests/fixtures/host-route-policy-v1.json` (`policy_id
host-route-policy-v1`, `schema_version 5` — bumped for the #153
fail-closed container-argv ruling (§2.5): `--env-file` takes its value and
even a literal path needs an inventory row, `exec` and the
`container run|create|exec` spellings are parsed, attached short values and
short-option clusters are decomposed, and undecomposable short tokens are
red. `schema_version 4` covered the #150 verification ruling: `env --`
ends option parsing only (§2.3), and adapter call sites are now
exact-occurrence rows of the new class `product_clipboard_adapter_call`
(§3.4) — the former optional `adapter=` inventory field is deleted, so the
inventory/case schema changed.
`schema_version 3` covered the 2026-10-16 amendments after worker C's census
(anchored bind sources §2.2, coreutils-`env` narrowing §2.3, nested-quote
lexing §2.4, clipboard adapter callers §3.4); `schema_version 2` covered
§2.1, the dynamic-container-value and image-operand rule from the
pf-hwprobe#23 adjudication. The filenames stay `*-v1*` because ports pin
paths and sha256s). Ports in other repositories
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

Repository inputs are part of the enforcement boundary. Before consuming any
candidate source, manifest, policy, case, host map, schema or `.SOURCE` file,
the scanner uses non-following metadata and `O_NOFOLLOW`, verifies the opened
object is the same regular file, and rejects files larger than 4 MiB before
reading. The read itself is bounded and a file that changes while being read
is rejected. Symlinks, FIFOs and other non-regular files therefore fail with
stable `repo_input_*` reasons instead of being followed, skipped, or allowed to
occupy a public runner until its job timeout.

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

### 2.2 Anchored dynamic bind sources (2026-10-16 amendment)

Worker C's census (pfvd 50 findings, image 21, qemu-tsp 1) showed the flat
"any dynamic bind source is `dynamic_route`" rule failed closed on legitimate
container-based CI with no available fix, because workflow occurrences cannot
be classified in the manifest at all. Under the accidents-not-adversaries
threat model for static CI checks, a dynamic `-v`/`--volume`/`--mount` source
is allowed **iff it is anchored at a runner workspace root**:

- it begins with `$GITHUB_WORKSPACE`, `${GITHUB_WORKSPACE}`, `$RUNNER_TEMP`,
  `${RUNNER_TEMP}`, `${{ github.workspace }}` or `${{ runner.temp }}`;
- the anchor is followed by end of word or `/` (so `$GITHUB_WORKSPACE_EVIL`
  and `${GITHUB_WORKSPACE}x` are *not* anchored);
- it contains no literal `..` path segment (so
  `$GITHUB_WORKSPACE/../dev/input` stays red).

These roots are provisioned per job by the runner and cannot name a host
device, display or clipboard socket. The anchored form is allowed in workflows
*and* scripts with **no manifest row**. Everything else dynamic stays
`dynamic_route` (detail `unresolved_value`): `$PWD`, `$(pwd)`, `$HOME`,
unanchored variables, and whole-argv variables such as
`docker run $CI_DOCKER_RUN`. Consumers rewrite those: `$PWD` becomes
`$GITHUB_WORKSPACE`; argv variables become literal flags or a committed wrapper
whose docker argv is literal. Static sources keep their existing rules:
`/dev*`, `/tmp/.X11-unix`, XDG runtime dirs, `.Xauthority` and Wayland socket
sources are always red.

### 2.3 Coreutils `env` is not a container projection (2026-10-16 amendment)

`dynamic_route` detail `unresolved_env_name` applies only to *container* env
options: `docker`/`podman` `run`/`create`/`exec` `-e`/`--env`/`--env-file` and
compose `environment`. A coreutils `env` invocation running a host process
(`env -u NAME ...`, `env -i PATH=... ...`, `env "${arr[@]}" ...`) is not a
projection: bare names, flags and dynamic non-assignment arguments are not
findings there. The scanner parses the `env` command line the way coreutils
does: options and their operands come first, then `NAME=VALUE` assignments,
and parsing of assignments stops at the first non-assignment word — the
utility being run. Every supported option spelling is parsed (#150 review
round 2): `-u`/`--unset`/`-C`/`--chdir` consume a separate operand or an
inline `=` value; short-option clusters consume the rest of the cluster, or
the next operand when a trailing `u`/`C` needs one
(`env -iu DISPLAY DISPLAY=$DISPLAY cmd` is red); a bare `-` is the `-i`
alias, not the utility (`env - DISPLAY=$DISPLAY cmd` is red); `--` ends
OPTION parsing only — GNU env runs `env -- DISPLAY=x cmd` with DISPLAY set,
so after `--` the assignment loop still runs until the first non-assignment
word (`env -- DISPLAY=$DISPLAY cmd` is red), while tokens after that utility
stay ignored (`env -- printf DISPLAY=$DISPLAY` is green, #150 verification);
and `-S`/`--split-string` operands are re-parsed as options plus assignments
(`env -S '-u DISPLAY DISPLAY=$DISPLAY cmd'` is red). Only `NAME=VALUE` assignments of
`DISPLAY`, `WAYLAND_DISPLAY` or `XAUTHORITY` inside the assignment prefix are
checked, so tokens after the utility (`env printf DISPLAY=$DISPLAY`), after
`--` (`env -- printf DISPLAY=$DISPLAY`) and option operands
(`env -C DISPLAY=$DISPLAY pwd`) are not projections, while
`env DISPLAY=$DISPLAY docker run ...` stays red
(`host_display_projection`/`display_inherited`).

### 2.4 Tokenizer fidelity (2026-10-16 amendment)

`malformed_file` must not fire on shell that passes `bash -n`. The lexer
therefore treats a command substitution `$(...)` or a backtick span *inside a
double-quoted string* as one balanced span (quotes inside the substitution
belong to it), skips `\`-escaped characters (including line continuations)
while depth-counting a substitution, and treats ANSI-C `$'...'` strings as one
token. Truly unbalanced input (an unclosed quote, substitution, backtick or
`$'...'` string) still fails closed as `dynamic_route`/`malformed_file`.

### 2.5 Container argv fail-closed coverage (2026-10-17 amendment)

The pf-app#7 review found three fail-opens in the container argv parse. The
property that holds now: **every `docker`/`podman` argv form that can project
a host input or display route is detected, or the parse fails closed.**

- *Route-capable subcommands* are `run`, `create` and `exec`, in both the
  direct (`docker run`) and the `docker container run|create|exec` spelling.
  `exec` never exits the option scan before `-e`/`--env`/`--env-file` are
  checked; it carries no image operand, so the dynamic-image-operand rule
  stays `run`/`create`-only (a dynamic container name is not a route).
- *`--env-file` always fails closed* (`host_display_projection`, detail
  `env_file`): the referenced file can carry
  `DISPLAY`/`WAYLAND_DISPLAY`/`XAUTHORITY` and its contents are not part of
  the scanned tree. A literal repo path does not make the occurrence green;
  it needs a manifest row like any other route. `--env-file` is not a
  skippable value flag.
- *Short-option tokens decompose structurally*: inside `run`/`create`/`exec`
  a single-dash token is a cluster of known boolean shorts (`-i -t -d -P -q`)
  that may end in a known value short (`-e -v -u -w -l -m -p -h`) with an
  attached value, so `-eDISPLAY=$DISPLAY`, `-v/dev/input:/x`, `-itv…`,
  `-ite DISPLAY=…` and pflag's `-e=VALUE` spelling are all checked as their
  decomposed flags. A short token containing any other character is
  *undecomposable*: it could hide an attached route value inside the token
  itself, where no operand taint can catch it, so it is red
  (`dynamic_route`, detail `undecomposable_option`) rather than skipped.
- *Unknown options fail closed on both spellings.* An unknown long option
  taints the command (§2.1) whether written `--opt value` or `--opt=value`;
  the inline spelling is no longer silently consumed. Unknown long options
  with fully static operands stay green per the existing §2.1 taint ruling
  (`docker run --mystery-flag value alpine:latest` — a long option's value is
  a separate or `=`-attached token the taint already covers); undecomposable
  short tokens are red because their would-be value is *inside* the token.

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

**Adapter call sites (#150 verification ruling).** A call site of a
capability-checked adapter (`shell.copy_text(...)`) is textually identical to
a direct framework/platform clipboard call. No call-site text heuristic can
prove which receiver a call targets, and a language-agnostic comment/string
stripper cannot cover every port language, so the earlier `adapter=`
citation machinery (path containment, stripped definition regex, import-stem
evidence binding, framework co-import refusal) is **deleted**. Calls to a
capability-checked adapter are handled by the exact-occurrence inventory this
spec already has:

- each adapter call site is a manifest row of class
  `product_clipboard_adapter_call`, bound by the occurrence line hash
  (`sha256(rule + "\n" + stripped_line)`) like every other row;
- a new or changed call site is unclassified, so the scan fails until a human
  classifies it in review — the same stale-hash contract as §4;
- the adapter's own definition file keeps its `product_clipboard_adapter`
  row, which still requires the `host_clipboard_disabled` gate marker in the
  occurrence file.

Detection is unchanged: `copy_text(`/`copy_image(` call sites and the listed
clipboard libraries (`arboard`, `copypasta`, `x11-clipboard`,
`wl-clipboard-rs`, `gtk::Clipboard`) stay detectable everywhere; only exact
classified occurrences pass. A direct framework call in a file that also
imports the adapter is red unless a human classifies that exact line.
Ports note: pfvd's port needs `product_clipboard_adapter_call` rows for its
~16 adapter call sites (worker C, at re-pin).

## 4. Inventory (manifest)

Findings that are not excused by §3 must be classified in
`tests/uinput-inventory.txt`. Host-route rows live beside the creator-scan
rows and are tagged:

```
<path>\t<sha256(rule + "\n" + stripped_line)>\t# host-route rule=<rule-id> class=<class> [caller=<path>] [wave2=<bead>] note: ...
```

The key binds path, rule and exact line text; editing the occurrence
invalidates the row (stale) and the new text fails until reclassified.
Line references inside diagnostic text (`... at line 1641`) are hashed as
`at line N` (`key_text`), so an edit that only moves an occurrence keeps its
row.
Bootstrap rows with `python3 tests/uinput_inventory.py --emit-host-routes`.

Classes: `dormant-k-consumer` (requires `wave2=`; kernel-fidelity consumer
kept stopped on runner hosts in wave 1), `product-reference`,
`product-session-fixture`, `guest_internal` (requires `caller=`, validated per
§3.2), `product_clipboard_adapter` (gate marker per §3.4),
`product_clipboard_adapter_call` (exact-occurrence row for a call site of the
gated adapter, per §3.4). Occurrences inside
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
