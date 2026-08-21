use std::{collections::HashMap, io::ErrorKind, path::PathBuf, process::Stdio};

use anyhow::{Context, bail, ensure};
use docker_plugin::volume::Volume as DockerVolume;
use rustix::mount::{MountFlags, UnmountFlags};
use tempfile::TempDir;
use tracing::{debug, trace};

use crate::lvm::{
    lv::LvProxy, lv_common::LvCommonProxy, owned_proxy, proxy_convert, thin_pool::ThinPoolProxy,
};

pub enum VolumeState {
    UnProvisioned {
        creation_opts: super::Opts,
    },
    Provisioned {
        creation_opts: super::Opts,
        lv: LvProxy<'static>,
    },
    Mounted {
        creation_opts: super::Opts,
        mount_dir: TempDir,
        mounted_by: usize,
        lv: LvProxy<'static>,
    },
}

impl VolumeState {
    const MOUNT_NAME: &str = "mount";

    pub fn as_docker_volume<S: Default>(&self, name: &str) -> DockerVolume<S> {
        DockerVolume {
            name: name.to_owned(),
            mountpoint: match self {
                VolumeState::UnProvisioned { .. } | VolumeState::Provisioned { .. } => None,
                VolumeState::Mounted {
                    mount_dir,
                    mounted_by: _,
                    lv: _,
                    creation_opts: _,
                } => Some(mount_dir.path().join(Self::MOUNT_NAME)),
            },
            status: S::default(),
        }
    }

    pub async fn provision(
        &mut self,
        thin_pool: &ThinPoolProxy<'static>,
        name: &str,
    ) -> anyhow::Result<()> {
        let Self::UnProvisioned { creation_opts } = self else {
            return Ok(());
        };

        debug!("Creating LV {name:}");
        let (lv, completion) = thin_pool
            .lv_create(name, creation_opts.size, -1, HashMap::new())
            .await?;

        ensure!(
            completion.as_str() == "/",
            "thin pool data has no completion object"
        );
        let lv: LvProxy = owned_proxy(thin_pool.as_ref().connection().clone(), lv).await?;

        let lv_common: LvCommonProxy = proxy_convert(&lv).await?;

        debug!("Making Filesystem on {:?}", lv_common.path().await?);
        ensure!(
            tokio::process::Command::new(format!("mkfs.{:}", creation_opts.fs_type))
                .args(&creation_opts.format_options)
                .arg(lv_common.path().await?)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .stdin(Stdio::null())
                .status()
                .await
                .context("Formatting lv failed")?
                .success(),
            "Formatting failed"
        );
        *self = Self::Provisioned {
            creation_opts: creation_opts.clone(),
            lv,
        };
        Ok(())
    }

    pub async fn mount(&mut self) -> anyhow::Result<PathBuf> {
        let (opts, lv) = match self {
            VolumeState::UnProvisioned { .. } => {
                bail!("Cannot mount unprovisioned volume");
            }
            VolumeState::Mounted {
                mounted_by,
                mount_dir,
                ..
            } => {
                *mounted_by += 1;
                return Ok(mount_dir.path().join(Self::MOUNT_NAME));
            }
            VolumeState::Provisioned { creation_opts, lv } => (creation_opts, lv),
        };

        let common_proxy = proxy_convert::<_, LvCommonProxy>(lv).await?;
        let source = common_proxy.path().await?;

        let flags =
            MountFlags::from_bits(opts.mount_options).context("Unrecognized mount flags")?;
        let fs_type = opts.fs_type.clone();

        let mount_dir = tokio::task::spawn_blocking(move || {
            let mount_dir = TempDir::new().context("Failed to create mount dir")?;
            let mountpoint = mount_dir.path().join(Self::MOUNT_NAME);
            trace!("Mounting to {:?}", mountpoint);
            std::fs::create_dir(&mountpoint).context("Failed to create mountpoint")?;
            trace!(
                "Mounting {} at {:?} with type {} and flags {:?}",
                source, mountpoint, fs_type, flags
            );
            rustix::mount::mount(source, mountpoint, fs_type, flags, None)
                .context("Mount failed")?;
            Ok::<_, anyhow::Error>(mount_dir)
        })
        .await
        .unwrap()
        .context("mount failed")?;

        let mountpoint = mount_dir.path().join(Self::MOUNT_NAME);

        *self = Self::Mounted {
            mount_dir,
            mounted_by: 1,
            lv: lv.clone(),
            creation_opts: opts.clone(),
        };

        Ok(mountpoint)
    }

    /// Tears down the mount regardless of the in-memory refcount.
    ///
    /// `Driver::remove` calls this so a stale `Mounted` state — left over when
    /// Docker skipped the `Unmount` call (container SIGKILL/OOM, daemon
    /// restart) — doesn't permanently wedge `docker volume rm`. Docker only
    /// issues `Remove` when no live container references the volume, so
    /// forcing through the kernel unmount here is safe.
    ///
    /// `EINVAL`/`ENOENT` from `umount(2)` means the kernel disagrees with our
    /// cached `Mounted` state; transition to `Provisioned` anyway so the caller
    /// doesn't fall through to a state-machine `unreachable!`.
    pub async fn force_unmount(&mut self) -> std::io::Result<()> {
        let VolumeState::Mounted {
            creation_opts,
            mount_dir,
            lv,
            mounted_by,
        } = self
        else {
            return Ok(());
        };
        if *mounted_by != 1 {
            tracing::warn!(
                "Force unmounting volume with stale refcount {}",
                *mounted_by
            );
        }
        let mountpoint = mount_dir.path().join(Self::MOUNT_NAME);
        let result = tokio::task::spawn_blocking(move || {
            rustix::mount::unmount(mountpoint, UnmountFlags::empty())
        })
        .await
        .unwrap();

        match result.map_err(|e| e.kind()) {
            Ok(()) => {}
            Err(kind @ (ErrorKind::InvalidInput | ErrorKind::NotFound)) => {
                tracing::warn!(
                    ?kind,
                    "unmount reported not-mounted; reconciling cached state"
                );
            }
            Err(kind) => return Err(kind.into()),
        }

        *self = VolumeState::Provisioned {
            creation_opts: creation_opts.clone(),
            lv: lv.clone(),
        };

        Ok(())
    }

    pub async fn unmount(&mut self) -> anyhow::Result<()> {
        match self {
            VolumeState::UnProvisioned { .. } | VolumeState::Provisioned { .. } => Ok(()),

            VolumeState::Mounted {
                mounted_by: 1,
                creation_opts,
                mount_dir,
                lv,
                ..
            } => {
                let mountpoint = mount_dir.path().join(Self::MOUNT_NAME);
                tokio::task::spawn_blocking(move || {
                    rustix::mount::unmount(mountpoint, UnmountFlags::empty())
                })
                .await
                .unwrap()
                .context("unmount failed")?;

                *self = VolumeState::Provisioned {
                    creation_opts: creation_opts.clone(),
                    lv: lv.clone(),
                };

                Ok(())
            }
            VolumeState::Mounted { mounted_by, .. } => {
                *mounted_by -= 1;
                tracing::debug!("Mounted by {} other containers", mounted_by);
                Ok(())
            }
        }
    }
}
