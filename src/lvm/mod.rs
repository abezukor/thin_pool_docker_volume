use zbus::{
    Proxy,
    proxy::{Defaults, ProxyImpl},
};

pub mod lv;
pub mod lv_common;
pub mod manager;
pub mod thin_pool;
pub mod vg;

pub async fn owned_proxy<T, P>(connection: zbus::Connection, path: P) -> zbus::Result<T>
where
    T: From<zbus::Proxy<'static>> + Defaults,
    P: TryInto<zbus::zvariant::ObjectPath<'static>>,
    P::Error: Into<zbus::Error>,
{
    let proxy = zbus::Proxy::new_owned(
        connection,
        T::DESTINATION.as_ref().unwrap(),
        path,
        T::INTERFACE.as_ref().unwrap(),
    )
    .await?;
    Ok(T::from(proxy))
}

pub async fn proxy_convert<
    'p,
    From: AsRef<Proxy<'p>>,
    To: ProxyImpl<'p> + std::convert::From<Proxy<'p>>,
>(
    from: &From,
) -> zbus::Result<To> {
    let connection = from.as_ref().connection();
    To::builder(connection)
        .path(from.as_ref().path())?
        .build()
        .await
}
