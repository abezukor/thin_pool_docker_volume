use std::{collections::HashMap, future::Future, io::ErrorKind, sync::Arc};

use anyhow::{Context, bail, ensure};
use docker_plugin::volume::{
    Capabilities, CapabilitiesResponse, CreateRequest, GetRequest, GetResponse, ListResponse,
    MountRequest, MountResponse, PathRequest, PathResponse, RemoveRequest,
};
use rustix::ffi;
use rustix::mount::MountFlags;
use scc::HashMap as SccHashMap;
use scc::hash_map::Entry;
use serde::{Deserialize, Serialize};
use serde_with::{DisplayFromStr, StringWithSeparator, formats::CommaSeparator, serde_as};
use tracing::{Instrument, debug, info, warn};

use crate::lvm::thin_pool::ThinPoolProxy;

pub use volume_state::VolumeState;
mod volume_state;

pub struct DockerLvmTmpFs {
    thin_pool: ThinPoolProxy<'static>,
    /// `scc::HashMap` is async-aware: holding an entry guard across `.await` is
    /// safe — other tasks contending for the same key suspend rather than
    /// blocking the OS thread.
    volumes: Arc<SccHashMap<String, VolumeState>>,
}

/// Runs `work` to completion on a detached task, so dockerd abandoning the RPC
/// at its deadline (dropping the handler future) can't strand a state
/// transition mid-way — an `Unmount` outliving its request during a long XFS
/// flush left volumes cached as `Mounted` and their `Remove` failing.
async fn run_detached<T: Send + 'static>(
    work: impl Future<Output = anyhow::Result<T>> + Send + 'static,
) -> anyhow::Result<T> {
    tokio::spawn(work.in_current_span()).await.unwrap()
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

    /// Permissions to apply to the volume root on mount, as an octal string
    /// (`"1777"`, `"0755"`). Absent leaves whatever `mkfs` produced.
    ///
    /// Lets a caller hand a world-writable volume to an unprivileged container
    /// without launching a separate `chmod` container first, which in turn
    /// makes anonymous volumes usable.
    #[serde(default, deserialize_with = "de_octal_mode")]
    pub root_mode: Option<u32>,
}

/// Docker passes every `driver_opt` as a string, and a mode is conventionally
/// written in octal, so `"1777"` has to be read base 8 — `DisplayFromStr`
/// would silently take it as decimal 1777.
fn de_octal_mode<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<u32>, D::Error> {
    use serde::Deserialize;

    let Some(mode) = Option::<String>::deserialize(deserializer)? else {
        return Ok(None);
    };
    u32::from_str_radix(mode.trim_start_matches("0o"), 8)
        .map(Some)
        .map_err(serde::de::Error::custom)
}

impl DockerLvmTmpFs {
    pub fn new(
        thin_pool: ThinPoolProxy<'static>,
        prexisting: impl Iterator<Item = (String, VolumeState)>,
    ) -> Self {
        Self {
            thin_pool,
            volumes: Arc::new(SccHashMap::from_iter(prexisting)),
        }
    }
}

impl docker_plugin::volume::Driver for DockerLvmTmpFs {
    type Opts = Opts;
    type Status = ();

    async fn create(&self, req: CreateRequest<Self::Opts>) -> anyhow::Result<()> {
        info!("Creating Volume {}", req.name);

        // Validate the opts here rather than at mount time, so a bad
        // `docker volume create` fails where the opts were written.
        let flags =
            MountFlags::from_bits(req.options.mount_options).context("Unrecognized mount flags")?;
        ensure!(
            req.options.root_mode.is_none() || !flags.contains(MountFlags::RDONLY),
            "root_mode cannot be applied to a read-only volume (MS_RDONLY in mount_options)"
        );

        let volumes = Arc::clone(&self.volumes);
        let thin_pool = self.thin_pool.clone();
        run_detached(async move {
            let mut entry = match volumes.entry_async(req.name.clone()).await {
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
            entry.get_mut().provision(&thin_pool, &req.name).await
        })
        .await
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

        let volumes = Arc::clone(&self.volumes);
        run_detached(async move {
            let Entry::Occupied(mut entry) = volumes.entry_async(req.name).await else {
                // A prior Remove that outlived docker's RPC deadline already
                // finished; ack so docker drops the volume from its store.
                warn!("Volume not in plugin state; treating remove as already done");
                return Ok(());
            };

            if let Err(e) = entry.get_mut().force_unmount().await {
                match e.kind() {
                    ErrorKind::ResourceBusy => {
                        bail!("Volume still in use, cannot remove");
                    }
                    ErrorKind::InvalidInput | ErrorKind::NotFound => {
                        // Volume was already removed, ignore
                    }
                    _ => {
                        return Err(e).context("Failed to force unmount the volume");
                    }
                }
            }

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
        })
        .await
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
        let volumes = Arc::clone(&self.volumes);
        run_detached(async move {
            let Entry::Occupied(mut entry) = volumes.entry_async(req.name.clone()).await else {
                bail!("Volume {} not found", req.name);
            };

            let mountpoint = entry.get_mut().mount().await?;
            Ok(MountResponse {
                mountpoint: mountpoint
                    .to_str()
                    .context("invalid mountpoint")?
                    .to_string(),
            })
        })
        .await
    }

    async fn unmount(&self, req: docker_plugin::volume::UnmountRequest) -> anyhow::Result<()> {
        info!("Unmounting Volume {}", req.name);

        let volumes = Arc::clone(&self.volumes);
        run_detached(async move {
            let Entry::Occupied(mut entry) = volumes.entry_async(req.name.clone()).await else {
                bail!("Volume {} not found", req.name);
            };
            entry.get_mut().unmount().await?;
            Ok(())
        })
        .await
    }

    async fn capabilities(&self) -> CapabilitiesResponse {
        CapabilitiesResponse {
            capabilities: Capabilities {
                scope: "local".to_owned(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::run_detached;

    #[tokio::test]
    async fn run_detached_survives_request_cancellation() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut handler = Box::pin(run_detached(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            tx.send(()).unwrap();
            Ok(())
        }));

        // First poll spawns the task; then drop the handler, as hyper does
        // when dockerd abandons the RPC at its deadline.
        tokio::select! {
            biased;
            res = &mut handler => panic!("finished early: {res:?}"),
            () = tokio::task::yield_now() => {}
        }
        drop(handler);

        rx.await.unwrap();
    }
}
