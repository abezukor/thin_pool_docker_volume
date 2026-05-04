use std::{
    collections::HashMap, env, fs::Permissions, os::unix::fs::PermissionsExt, path::PathBuf,
    sync::Arc,
};

use anyhow::{Context, Result, bail, ensure};
use clap::{ArgAction, Parser};
use tracing::{Level, info, trace, warn};
use tracing_subscriber::{EnvFilter, FmtSubscriber};
use zbus::Connection;

use crate::lvm::{
    lv::LvProxy, lv_common::LvCommonProxy, owned_proxy, proxy_convert, thin_pool::ThinPoolProxy,
    vg::VgProxy,
};

mod lvm;
mod plugin;

#[derive(Debug, Parser)]
pub struct Config {
    /// Volume group where the thin pool lives
    #[arg(long, env = "DOCKER_LVM_THIN_POOL_VG_NAME")]
    vg_name: String,
    /// Thin pool to use to allocate and de-allocate docker volumes.
    #[arg(long, env = "DOCKER_LVM_THIN_POOL_NAME")]
    thin_pool_name: String,
    /// Import Existing LVs in the thin pool as docker volumes. They will be mounted with no mount options.
    /// Note that this means that docker volume remove will delete these LVs
    #[arg(long, env = "DOCKER_LVM_THIN_POOL_IMPORT_EXISTING", action = ArgAction::Set, default_value_t = true)]
    import_existing: bool,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let config = Config::parse();

    // a builder for `FmtSubscriber`.
    let subscriber = FmtSubscriber::builder()
        // all spans/events with a level higher than TRACE (e.g, debug, info, warn, etc.)
        // will be written to stdout.
        .with_max_level(Level::TRACE)
        .with_env_filter(EnvFilter::from_default_env())
        // completes the builder.
        .finish();

    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");

    info!("Initializing LVM Setup");
    let connection = Connection::system()
        .await
        .context("Could not connect to system dbus")?;

    let lvm_manager = lvm::manager::ManagerProxy::new(&connection)
        .await
        .context("Could not connect to LVM Manager proxy")?;

    let vg = lvm_manager
        .look_up_by_lvm_id(&config.vg_name)
        .await
        .context("cound not find vg")?;
    let vg = VgProxy::builder(&connection)
        .path(vg)?
        .build()
        .await
        .context("Failed to make vg proxy")?;

    let thin_pool = find_thin_pool(&config, &connection, &vg).await?;
    tracing::debug!("Thin pool {:?}.", thin_pool,);

    let preexisting_volumes = match config.import_existing {
        true => get_existing(&connection, &thin_pool, &vg)
            .await
            .context("Failed to get pre-existing volume groups")?,
        false => {
            clean_thin_pool(&connection, &thin_pool, &vg)
                .await
                .context("Failed to clean the thin pool")?;
            Vec::new()
        }
    };

    info!("Setting Up Docker Volume Driver");
    let driver = Arc::new(plugin::DockerLvmTmpFs::new(
        thin_pool,
        preexisting_volumes.into_iter(),
    ));

    let app = docker_plugin::router(vec![docker_plugin::volume::IMPLEMENTS_VOLUME.to_string()])
        .merge(docker_plugin::volume::router(driver));

    let socket = match env::var("DOCKER_LVM_TMPFS_SOCKET") {
        Ok(val) => PathBuf::from(val),
        Err(_) => PathBuf::from("/run/docker/plugins/lvm_thin_pool.sock"),
    };
    let _ = tokio::fs::remove_file(&socket).await;

    info!("Starting Docker Plugin");
    let listener = tokio::net::UnixListener::bind(&socket)?;
    tokio::fs::set_permissions(&socket, Permissions::from_mode(0o666)).await?;
    axum::serve(listener, app.into_make_service()).await?;

    Ok(())
}

async fn find_thin_pool<'a>(
    config: &Config,
    connection: &'a Connection,
    vg: &VgProxy<'a>,
) -> anyhow::Result<ThinPoolProxy<'static>> {
    for lv_path in vg.lvs().await.context("Could not get LVs from VG")? {
        let lv = LvCommonProxy::builder(connection)
            .path(&lv_path)?
            .build()
            .await
            .context("Failed to make lv proxy")?;
        let lv_name = lv.name().await.context("could not get lv name")?;
        trace!("Checking if {lv_name} is the thin pool");
        if lv_name == config.thin_pool_name {
            ensure!(lv.is_thin_pool().await?, "LV {lv_name} is not a thin pool");

            return Ok(owned_proxy(connection.clone(), lv_path).await?);
        }
    }
    bail!(
        "Could not find thin pool with name {}",
        config.thin_pool_name
    )
}

async fn clean_thin_pool<'a>(
    connection: &'a Connection,
    thin_pool: &ThinPoolProxy<'a>,
    vg: &VgProxy<'a>,
) -> anyhow::Result<()> {
    let mut delete_lvs = tokio::task::JoinSet::new();
    for lv_path in vg.lvs().await.context("Could not get LVs from VG")? {
        let lv = LvCommonProxy::builder(connection)
            .path(&lv_path)?
            .build()
            .await
            .context("Failed to make lv proxy")?;
        trace!("lv {:?}. Pool {:?}", lv_path, lv.pool_lv().await?);
        if &lv.pool_lv().await?.as_ref() != thin_pool.inner().path() {
            trace!("Skipping lv {:?}", lv_path);
            continue;
        }
        warn!("Deleting LVM Volume {lv_path:?}");
        let lv = LvProxy::builder(connection)
            .path(lv_path)?
            .build()
            .await
            .context("Failed to make lv proxy")?;
        delete_lvs.spawn(async move { lv.remove(0, HashMap::new()).await });
    }
    delete_lvs
        .join_all()
        .await
        .into_iter()
        .collect::<zbus::Result<Vec<_>>>()
        .context("Could clear thin pool")?;
    Ok(())
}

async fn get_existing<'a>(
    connection: &'a Connection,
    thin_pool: &ThinPoolProxy<'a>,
    vg: &VgProxy<'a>,
) -> anyhow::Result<Vec<(String, plugin::VolumeState)>> {
    let mut existing_lvs = Vec::new();
    for lv_path in vg.lvs().await.context("Could not get LVs from VG")? {
        let lv = LvCommonProxy::builder(connection)
            .path(lv_path.clone())?
            .build()
            .await
            .context("Failed to make lv proxy")?;
        trace!("lv {:?}. Pool {:?}", lv_path, lv.pool_lv().await?);
        if &lv.pool_lv().await?.as_ref() != thin_pool.inner().path() {
            trace!("Skipping lv {:?}", lv_path);
            continue;
        }
        let name = lv.name().await?;
        trace!("Importing {} as a docker volume.", name);
        existing_lvs.push((
            name,
            plugin::VolumeState::Provisioned {
                creation_opts: plugin::Opts {
                    size: lv.size_bytes().await?,
                    mount_options: 0,
                    fs_type: String::new(),
                    format_options: Vec::new(),
                },
                lv: proxy_convert(&lv).await?,
            },
        ));
    }

    Ok(existing_lvs)
}
