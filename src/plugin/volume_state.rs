use std::{collections::HashMap, io, path::PathBuf, process::Stdio};

use anyhow::{Context, bail, ensure};
use docker_plugin::volume::Volume as DockerVolume;
use rustix::mount::{MountFlags, UnmountFlags};
use tempfile::TempDir;
use tracing::debug;

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
            let mount_dir = TempDir::new()?;
            let mountpoint = mount_dir.path().join(Self::MOUNT_NAME);
            std::fs::create_dir(&mountpoint)?;
            rustix::mount::mount(source, mountpoint, fs_type, flags, None)?;
            io::Result::Ok(mount_dir)
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

    pub async fn unmount(&mut self) -> anyhow::Result<()> {
        match self {
            VolumeState::UnProvisioned { .. } | VolumeState::Provisioned { .. } => Ok(()),

            VolumeState::Mounted {
                mounted_by: 1,
                creation_opts,
                lv,
                ..
            } => {
                let mut state = VolumeState::Provisioned {
                    creation_opts: creation_opts.clone(),
                    lv: lv.clone(),
                };
                std::mem::swap(&mut state, self);

                let VolumeState::Mounted {
                    mount_dir,
                    mounted_by: 1,
                    creation_opts: _,
                    lv: _,
                } = state
                else {
                    unreachable!()
                };

                tokio::task::spawn_blocking(move || {
                    let mountpoint = mount_dir.path().join(Self::MOUNT_NAME);
                    rustix::mount::unmount(mountpoint, UnmountFlags::empty())
                })
                .await
                .unwrap()
                .context("unmount failed")?;

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
