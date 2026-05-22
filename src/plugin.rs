use std::collections::HashMap;

use anyhow::{Context, bail};
use docker_plugin::volume::{
    Capabilities, CapabilitiesResponse, CreateRequest, GetRequest, GetResponse, ListResponse,
    MountRequest, MountResponse, PathRequest, PathResponse, RemoveRequest,
};
use rustix::ffi;
use scc::HashMap as SccHashMap;
use scc::hash_map::Entry;
use serde::{Deserialize, Serialize};
use serde_with::{DisplayFromStr, StringWithSeparator, formats::CommaSeparator, serde_as};
use tracing::{debug, info};

use crate::lvm::thin_pool::ThinPoolProxy;

pub use volume_state::VolumeState;
mod volume_state;

pub struct DockerLvmTmpFs {
    thin_pool: ThinPoolProxy<'static>,
    /// `scc::HashMap` is async-aware: holding an entry guard across `.await` is
    /// safe — other tasks contending for the same key suspend rather than
    /// blocking the OS thread.
    volumes: SccHashMap<String, VolumeState>,
}

#[serde_as]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Opts {
    #[serde_as(as = "DisplayFromStr")]
    pub size: u64,
    #[serde_as(as = "DisplayFromStr")]
    pub mount_options: ffi::c_uint,
    pub fs_type: String,
    #[serde(default)]
    #[serde_as(as = "StringWithSeparator::<CommaSeparator, String>")]
    pub format_options: Vec<String>,
}

impl DockerLvmTmpFs {
    pub fn new(
        thin_pool: ThinPoolProxy<'static>,
        prexisting: impl Iterator<Item = (String, VolumeState)>,
    ) -> Self {
        Self {
            thin_pool,
            volumes: SccHashMap::from_iter(prexisting),
        }
    }
}

impl docker_plugin::volume::Driver for DockerLvmTmpFs {
    type Opts = Opts;
    type Status = ();

    async fn create(&self, req: CreateRequest<Self::Opts>) -> anyhow::Result<()> {
        info!("Creating Volume {}", req.name);

        let mut entry = match self.volumes.entry_async(req.name.clone()).await {
            Entry::Vacant(v) => v.insert_entry(VolumeState::UnProvisioned {
                creation_opts: req.options.clone(),
            }),
            Entry::Occupied(o) => o,
        };
        match entry.get_mut() {
            VolumeState::Mounted { .. } => bail!("Can not re-create mounted entry"),
            VolumeState::Provisioned { .. } => bail!("Cannot create an already created entry"),
            VolumeState::UnProvisioned { creation_opts } => *creation_opts = req.options,
        }
        entry.get_mut().provision(&self.thin_pool, &req.name).await
    }

    async fn list(&self) -> anyhow::Result<ListResponse<Self::Status>> {
        let mut volumes = Vec::new();
        self.volumes
            .iter_async(|name, state| {
                volumes.push(state.as_docker_volume(name));
                true
            })
            .await;
        debug!("Got Volume List {:?}", volumes);
        Ok(ListResponse { volumes })
    }

    async fn get(&self, req: GetRequest) -> anyhow::Result<GetResponse<Self::Status>> {
        let volume = self
            .volumes
            .read_async(&req.name, |name, state| state.as_docker_volume(name))
            .await;
        Ok(GetResponse { volume })
    }

    async fn remove(&self, req: RemoveRequest) -> anyhow::Result<()> {
        info!("Removing Volume {}", req.name);

        let Entry::Occupied(mut entry) = self.volumes.entry_async(req.name).await else {
            bail!("Entry does not exist")
        };

        entry.get_mut().force_unmount().await?;

        match entry.get() {
            VolumeState::Mounted { .. } => {
                unreachable!("force_unmount transitions out of Mounted")
            }
            VolumeState::UnProvisioned { .. } => {}
            VolumeState::Provisioned { lv, .. } => {
                let job = lv.remove(-1, HashMap::new()).await?;
                assert_eq!(job.as_str(), "/", "Job not empty, generate the api");
            }
        }
        let _ = entry.remove_entry();
        Ok(())
    }

    async fn path(&self, req: PathRequest) -> anyhow::Result<PathResponse> {
        let mountpoint = self
            .volumes
            .read_async(&req.name, |name, state| {
                state.as_docker_volume::<()>(name).mountpoint
            })
            .await
            .with_context(|| format!("Volume {} not found", req.name))?
            .context("Not mounted")?;

        Ok(PathResponse {
            mountpoint: mountpoint
                .to_str()
                .context("Invalid mountpoint")?
                .to_string(),
        })
    }

    async fn mount(&self, req: MountRequest) -> anyhow::Result<MountResponse> {
        info!("Mounting Volume {}", req.name);
        let Entry::Occupied(mut entry) = self.volumes.entry_async(req.name.clone()).await else {
            bail!("Volume {} not found", req.name);
        };

        let mountpoint = entry.get_mut().mount().await?;
        Ok(MountResponse {
            mountpoint: mountpoint
                .to_str()
                .context("invalid mountpoint")?
                .to_string(),
        })
    }

    async fn unmount(&self, req: docker_plugin::volume::UnmountRequest) -> anyhow::Result<()> {
        info!("Unmounting Volume {}", req.name);

        let Entry::Occupied(mut entry) = self.volumes.entry_async(req.name.clone()).await else {
            bail!("Volume {} not found", req.name);
        };
        entry.get_mut().unmount().await?;
        Ok(())
    }

    async fn capabilities(&self) -> CapabilitiesResponse {
        CapabilitiesResponse {
            capabilities: Capabilities {
                scope: "local".to_owned(),
            },
        }
    }
}
