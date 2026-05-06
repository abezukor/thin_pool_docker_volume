# thin_pool_docker_volume

A Docker volume plugin that backs each Docker volume with a thin-provisioned LVM logical volume.

Each `docker volume create --driver lvm_thin_pool ...` allocates an LV inside a pre-existing LVM thin pool, formats it with the requested filesystem, and on mount exposes it to Docker. `docker volume rm` removes the LV.

## How it works

The plugin talks to LVM via `lvmdbus1` over the system D-Bus, so `lvmd` must be running on the host. It exposes the [Docker volume plugin protocol](https://docs.docker.com/engine/extend/plugins_volume/) over a Unix socket at `/run/docker/plugins/lvm_thin_pool.sock` (override with `DOCKER_LVM_TMPFS_SOCKET`). The driver name is derived from the socket name: `lvm_thin_pool`.

## Requirements

- Linux with LVM2 and the `lvmdbus1` service running.
- `mkfs.<fs_type>` available in `PATH` for whichever filesystems you intend to use (e.g. `xfsprogs` for `xfs`).
- A pre-created volume group and thin pool. The plugin will not create them.

## Configuration

Configure the plugin via the systemd unit's `EnvironmentFile`, which
defaults to `/etc/default/thin_pool_docker_volume`:

```sh
# /etc/default/thin_pool_docker_volume
DOCKER_LVM_THIN_POOL_VG_NAME=docker_thin_vg
DOCKER_LVM_THIN_POOL_NAME=docker_thin_pool
DOCKER_LVM_THIN_POOL_IMPORT_EXISTING=true
```

| Variable                                 | Description                                                       |
| ---------------------------------------- | ----------------------------------------------------------------- |
| `DOCKER_LVM_THIN_POOL_VG_NAME`           | Volume group containing the thin pool.                            |
| `DOCKER_LVM_THIN_POOL_NAME`              | Thin pool LV to allocate volumes from.                            |
| `DOCKER_LVM_THIN_POOL_IMPORT_EXISTING`   | Adopt LVs already in the pool as Docker volumes (default `true`). |
| `DOCKER_LVM_TMPFS_SOCKET`                | Override the listen path for the plugin socket. Development only — not recommended in production. |
| `RUST_LOG`                               | Standard `tracing-subscriber` filter.                             |

Reload after editing: `systemctl restart thin_pool_docker_volume`.

## Volume options

`docker volume create -o key=value` accepts:

| Option                | Type             | Description                                                          |
| --------------------- | ---------------- | -------------------------------------------------------------------- |
| `size`                | bytes            | Size of the LV.                                                      |
| `mount_options`       | `MS_*` bitmask   | Linux mount flags as a u32 bitmask.                                  |
| `fs_type`             | string           | Filesystem to format with (`xfs`, `ext4`, ...).                      |
| `format_options`      | comma-separated  | Extra args passed to `mkfs.<fs_type>`. Empty string means none.      |

Example:

```bash
docker volume create \
  --driver lvm_thin_pool \
  -o size=1073741824 \
  -o mount_options=0 \
  -o fs_type=xfs \
  -o format_options='' \
  myvol

docker run --rm -v myvol:/data alpine sh -c 'echo hi > /data/x'
docker volume rm myvol
```

## Caveats

- Volumes adopted via `--import-existing` have no recorded mount options, so the plugin will mount them with default options until they are recreated. Disable this flag if you do not want existing LVs claimed.
- `docker volume rm` deletes the underlying LV. Adopted volumes are not exempt.
- The plugin tracks mounted state in memory. With `--import-existing` enabled, restarting the plugin re-adopts existing LVs as `Provisioned`, but any in-flight `Mounted` state is lost — restart while no containers are actively using a volume.
