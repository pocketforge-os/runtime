# PocketForge compositor-independent session contract

This is the G2.2 runtime contract for consumers such as `pf-shell` and Steam Link. It does not
start a compositor, launch an app, broker input, or grant DRM ownership. The existing durable
session-authority socket remains a separate control-plane boundary.

## Canonical publication

The public session root is exactly:

```text
/run/pocketforge/session
```

The publisher creates an immutable generation below the parent-owned private store
`/run/pocketforge/.session-generations/<generation>/`, then atomically replaces the public root
with a symlink to that complete generation. A generation is consumable only when all of these
entries are present and valid:

```text
/run/pocketforge/session/environment
/run/pocketforge/session/readiness       # exactly: ready\n
/run/pocketforge/session/generation      # unsigned decimal + newline
/run/pocketforge/session/<WAYLAND_DISPLAY>
/run/pocketforge/session/xauthority     # only when Xwayland is published
/run/pocketforge/session/capabilities/<name>
```

`generation` increases monotonically. `environment` contains one `KEY=value` per line and must
contain:

```text
POCKETFORGE_SESSION=/run/pocketforge/session
POCKETFORGE_SESSION_GENERATION=<generation>
WAYLAND_DISPLAY=wayland-<n>
```

When Xwayland is available it additionally contains `DISPLAY=<display>` and
`XAUTHORITY=/run/pocketforge/session/xauthority`. `DISPLAY` and `XAUTHORITY` are an all-or-none
pair. The Wayland, Xauthority, and named capability entries are projections of endpoints owned
by the compositor/session publisher; the contract does not transfer endpoint or device
ownership to the app.

Consumers must resolve and read one generation, verify `readiness`, `generation`, and the
environment generation match, then retain that generation number. On reconnect, they call the
reader with the retained number: a different current number is `StaleGeneration` and requires a
fresh session read. Missing or partial publication is `NotReady`; consumers must not guess or
fall back to a previous generation.

## App-root projection and permissions

`SessionPublisher::project_app_root` creates the same contract under:

```text
<app-root>/run/pocketforge/session
```

Each projection is first written as an immutable generation below the private
`<app-root>/run/pocketforge/.session-projections/` store. The `session` path is then atomically
replaced with a symlink to that complete projection. Endpoint links in the projection target the
exact canonical session generation read during projection, not the moving canonical `session`
symlink. Republish of the canonical session therefore cannot mix generations in an existing app
projection; a consumer reconnects by requesting a new projection.

Only the requested, published non-privileged capability names are projected. Contract metadata
files are mode `0444`; contract directories are mode `0755`. Existing non-symlink files or
directories at projection targets are rejected. The projection is deliberately rooted under the
provided app root and never writes outside it.

## Explicit ownership denial

The capability namespace rejects `drm`, DRM master/control/render aliases, `protected-input`,
input-ownership aliases, `evdev`, and `uinput`. No app projection contains a DRM device/fd,
DRM master, protected-input owner, evdev node, or uinput node. Apps receive compositor protocol
access through the Wayland endpoint and must use the separately scoped input-broker contract for
input; G2.2 does not implement that broker.
