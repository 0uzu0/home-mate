# Release packages: signing, bootstrap, install, rollback

M8a (2026-11-13). Design: [docs/d/m8a-design-20261113.md](../../docs/d/m8a-design-20261113.md).
This file is the operator's runbook: where the keys live, how a package is built and signed offline,
how the installer gets onto the device, and how activation/rollback are observed.

## 1. Trust root and key custody

| Material | Location | Notes |
|---|---|---|
| Ed25519 **public** key | `deploy/release/release-pubkey.hex` (committed) | compiled into `hm-release` with `include_str!` |
| Ed25519 **private** key (seed, hex) | `~/.home-mate/signing/release-signing-key.hex`, mode `0600`, **outside Git** | never copied to the device |
| Fingerprint | `sha256(pubkey)[:16]` = **`7e0182c7b1f14154`** (2026-11-13 key) | printed by `hm-release version` / `pubkey` |

- Rotating the key requires rebuilding `hm-release` with the new public key and reinstalling it on
  the device. Device data, releases and the database are untouched by a key change.
- The device build (`tools/build_release.sh`) is the only binary that carries the trust root. A
  **debug** build additionally accepts `HM_RELEASE_PUBKEY_OVERRIDE` (used by host tests only);
  release builds ignore it.

Generate a key (once; refuses to overwrite):

```sh
cargo build -p hm-release
./target/debug/hm-release keygen --out ~/.home-mate/signing/release-signing-key.hex
./target/debug/hm-release pubkey --key ~/.home-mate/signing/release-signing-key.hex
# put the printed public key into deploy/release/release-pubkey.hex, then rebuild the release tool
```

## 2. Build and sign a release (offline on the dev Mac)

1. Build the three artifacts through their supported scripts (never by hand):

```sh
tools/build_core.sh        # target/aarch64-unknown-linux-musl/release/hm-core
sh ../home-mate-gateway-linux/tools/build_firmware.sh
   # ../home-mate-gateway-linux/target/armv7-unknown-linux-musleabihf/release/home-mate-device-gateway
(cd web && npm run build) # web/dist (console)
```

2. Assemble the release directory. `hm-release pack` reads versions/build ids from markers inside
   the binaries — they are never typed by hand:

```sh
VER=2026.11.14-1
REL=build/release-$VER
mkdir -p "$REL/components/model"
cp target/aarch64-unknown-linux-musl/release/hm-core          "$REL/components/core-aarch64"
cp ../home-mate-gateway-linux/target/armv7-unknown-linux-musleabihf/release/home-mate-device-gateway \
   "$REL/components/firmware-armv7"
cp -a web/dist "$REL/components/web"
cp /path/to/silero_vad.onnx "$REL/components/model/silero_vad.onnx"   # the 643,854 B 16 kHz export
```

3. Pack + sign. `--serial` is monotonic (a package with `serial <= installed` is rejected as a
   replay). **A core whose build id ends in `-dirty` is refused** (uncommitted work; a release must
   come from a committed tree — `--allow-dirty` exists for drills only). The command prints the
   exact manifest summary and self-verifies the package before it is shipped:

```sh
./target/debug/hm-release pack \
  --dir "$REL" --out "build/release/home-mate-release-$VER" \
  --serial 1 --version "$VER" --key ~/.home-mate/signing/release-signing-key.hex \
  --model AEOBC --profile dot2d-emos --firmware-min 0.0.44 --firmware-max 0.0.99
```

Outputs (the three files the brief names):

```
build/release/home-mate-release-<ver>.tar            # components + manifest.json + manifest.sig
build/release/home-mate-release-<ver>.manifest.json  # the exact signed bytes
build/release/home-mate-release-<ver>.manifest.sig   # detached Ed25519 signature (hex)
```

The signature is over the exact bytes of `manifest.json`; the manifest lists:
`manifest_version`, `release {serial, version, created_ms, notes}`, `target {model, profile, arch}`,
`firmware_range {min, max}`, per-component `{file, size, sha256}` (core also `version`, `build_id`;
firmware also `version`, `gateway_version`, `roles`; console a per-file list + entry `index`;
model `sha256` + `name`), and `total_size`. **All hashes are real SHA-256**;
`fwupdate::sha256_prefix()` (FNV-1a) is not used anywhere in the release path.

Inspect or re-verify later (works on the device too):

```sh
./target/debug/hm-release verify --package build/release/home-mate-release-<ver>.tar
```

## 3. Bootstrap the installer on the device (once)

`hm-release` is bootstrap infrastructure and is **not** part of a release (an installer cannot
replace itself safely). On the dev Mac:

```sh
tools/build_release.sh    # target/aarch64-unknown-linux-musl/release/hm-release
```

Serve the directory over the LAN and pull it with the device's own wget (emOS has no adb; the
console deployment uses the same channel):

```sh
# on the Mac, in a directory holding hm-release:
python3 -m http.server 8111
# on the device (serial /dev/cu.usbmodem<port>, root shell):
busybox wget -O /data/hm/bin/hm-release http://<mac-ip>:8111/hm-release
chmod 755 /data/hm/bin/hm-release
/data/hm/bin/hm-release version          # prints its version + the compiled-in key fingerprint
/data/hm/bin/hm-release status           # "active: none" before the first release
```

The core looks for the tool at `<data-dir>/../bin/hm-release` (`/data/hm/bin/hm-release`);
`HM_RELEASE_TOOL` overrides it on a host. The installer derives its roots from `HM_RELEASE_ROOT`
(default `/data/hm`) and `HM_FW_DIR` (default `/data/local/tmp/hm-direct`).

> **不明重启的排查顺序**：先热复位，再读 `/proc/last_kmsg`（上一次启动的内核日志，跨热复位保留，
> 下一次重启即毁）；**不要先拔电**。当前 p10 是原始镜像，未经演练不要重刷。
> 完整说明：[pre-m8b-freeze-checklist §3.1](../../docs/d/pre-m8b-freeze-checklist-20261114.md)。

## 4. Install and activate

Either through the console (**发布 / Releases**) or by hand on the device:

```sh
/data/hm/bin/hm-release install --package http://<mac-ip>:8111/home-mate-release-<ver>.tar
/data/hm/bin/hm-release install --package <url> --activate   # install + activate in one job
/data/hm/bin/hm-release activate --id <serial>-<version>
/data/hm/bin/hm-release rollback                             # back to the recorded previous
```

What activation does (one layer at a time, each step observable in `installer.log`):

1. re-verifies the on-disk release directory (manifest signature + every file hash);
2. preflights the device: model (`/mode`), profile (`/etc/os-release` → `dot2d-emos`), firmware
   range, and ELF architecture of core (aarch64) and firmware (ARM);
3. asks the core for a DB snapshot (`POST /api/v0/release/snapshot`, `VACUUM INTO` + WAL
   checkpoint) — no snapshot, no activation;
4. records `status: running` in `state.json` **before** moving anything;
5. adopts the current console dir / model file on first use (`.bootstrap/`), writes core+firmware
   into the inactive A/B slots (**hard links to the verified release files when `/data` is the same
   filesystem**, so a release does not store a second 16 MiB core + 6.4 MiB firmware), then flips
   `bin/current`, `hm-direct/current`, `releases/current`, `web` and the model symlink;
6. restarts the **firmware** first, waits for `/mode` to report its version; then the **core**
   (`POST /api/v0/system/restart`, graceful exit 0 → supervisor respawn);
7. waits up to 90 s for all health criteria: core build id, gateway connected + device online,
   device model, firmware version, `kws.state=running`, Silero endpoint loaded (not the energy
   fallback), and the served console index pointing at this release's bundle;
8. success → `active`/`previous` updated (the receipt records `from` and `to`), reflex words
   re-pushed (idempotent), and every release directory except active+previous is pruned;
9. failure/timeout → everything flips back, the firmware and core are restarted into the old
   release, and the result is recorded as `rolled_back` (or `rollback_failed`).

State lives in `/data/hm/releases/state.json`; the last 10 runs are kept in `history`.

## 5. Database compatibility

- The core writes `/data/hm/data/home-mate.schema` after migrations (schema + build id).
- A rollback that would hand a **newer schema** to the old core writes
  `releases/restore-on-boot.json`; the core restores the pre-activation snapshot **before**
  opening the DB (it deletes `-wal`/`-shm` with it). If the schema did not advance, the current DB
  is kept — writes from the failed window are not thrown away.
- A power cut / supervisor swap during an activation is handled by the same guard at the next
  boot, keyed on `state.last.status == "running"` and a DB modified after the snapshot.

## 6. Drills and troubleshooting

```sh
hm-release status            # active/previous/last + per-criterion facts + DB decision
hm-release health            # evaluate the active release right now
hm-release verify --package <tar>            # reject a corrupted/tampered file before installing
hm-release install --package <tar> --drill-stall-ms 30000   # half-install drill (kill it here)
```

- **Rejected install**: the package is never committed; `state.json` and every pointer are
  unchanged; `installer.log` names the failing check (`sha256 mismatch`, `model mismatch`,
  `signature does not verify`, `replay rejected`, ...).
- **Half install**: staging lives under `releases/.staging-*`; `current` is not touched until the
  commit rename. The next install (holding the same lock) deletes stale `.staging-*`/`.incoming-*`
  before fetching, together with release dirs other than active/previous and snapshots beyond two.
  `/data` is shared with everything else; the space check names the numbers when it refuses.
- **Failed activation**: `state.last.status = rolled_back`, `criteria` carries the failed checks,
  and `rollback_*` checks show the old release serving again. If `rollback_failed`, use the serial
  console: the two core slots and the `current` symlinks are still there, and
  `/data/hm/releases/<id>/` holds every verified file.
- **Boot guard record**: `releases/boot-guard.json` says what was restored at the last boot.
- The installer log is capped at 1 MiB (`releases/installer.log`); the console shows the last 200
  lines.

## 7. Boundaries

- The system image (kernel/rootfs, `console=` removal, read-only application layer) is **M8b**;
  M8a only versions the application layer on the writable `/data` layout.
- Distribution is LAN-only HTTP (user decision); there is no fleet push, no delta package and no
  installer self-update. Signing the SBOM/dependencies is not part of M8a.
- The management API is unauthenticated on the home LAN today (same trust domain as the console);
  the installer runs as root because the slots and supervisors are root-owned.
