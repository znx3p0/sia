#![cfg(not(target_arch = "wasm32"))]

use compact_str::CompactStr;
use serde_repr::{Deserialize_repr, Serialize_repr};
use std::sync::{Arc, Weak};

use camino::Utf8Path;
use dashmap::DashMap;
use igcp::{err, BareChannel, Channel};
use once_cell::sync::Lazy;

use crate::runtime::spawn;
use crate::service::{Service, Svc};
use crate::Result;

type RouteKey = CompactStr;
type InnerRoute = DashMap<RouteKey, Storable>;

/// used for discovering services.
/// it stores services inside with a key and it can introduce channels to services.
pub enum Route {
    Owned(InnerRoute),
    Static(&'static InnerRoute),
    Dynamic(Weak<InnerRoute>)
}

enum Storable {
    Route(Route),
    Service(Svc),
}

/// context associated with a service
pub struct Ctx {
    top_route: RouteRef,
}

impl Ctx {
    fn new(top_route: RouteRef) -> Self {
        Ctx {
            top_route
        }
    }
}

impl std::ops::Deref for Ctx {
    type Target = Route;

    fn deref(&self) -> &Self::Target {
        self.top_route.deref()
    }
}

enum RouteRef {
    Static(&'static Route), // global
    Dynamic(Arc<Route>), // arc cannot outlive due to tree structure
}

impl RouteRef {
    fn new_static(route: &'static Route) -> Self {
        RouteRef::Static(route)
    }
    fn new_dynamic(route: impl Into<Arc<Route>>) -> Self {
        RouteRef::Dynamic(route.into())
    }
}

impl std::ops::Deref for RouteRef {
    type Target = Route;

    fn deref(&self) -> &Self::Target {
        match self {
            RouteRef::Static(route) => route,
            RouteRef::Dynamic(route) => route,
        }
    }
}

/// has an endpoint at which a type should be registered
pub trait RegisterEndpoint {
    /// inner endpoint
    const ENDPOINT: &'static str;
}

/// Register is how a specific type should be registered on a route,
/// and the metadata needed for it.
pub trait Register: RegisterEndpoint {
    /// metadata of type
    type Meta;
    /// register implementation of type
    fn register(top_route: &Route, meta: Self::Meta) -> Result<()>;
}

#[derive(Serialize_repr, Deserialize_repr)]
#[repr(u8)]
/// used for discovery
pub enum Status {
    /// indicates a service has been found
    Found = 1,
    /// indicates a service has not been found
    NotFound = 2,
}

/// global route on which initial services are laid on
pub static GLOBAL_ROUTE: Lazy<Route> = Lazy::new(Default::default);

trait Context {
    fn context(self) -> Ctx;
}

impl Context for &'static Route {
    fn context(self) -> Ctx {
        Ctx::new(RouteRef::new_static(self))
    }
}

impl Context for Arc<Route> {
    fn context(self) -> Ctx {
        Ctx::new(RouteRef::new_dynamic(self))
    }
}

impl Route {
    /// adds a service at a specific id to the route
    /// ```norun
    /// #[service]
    /// async fn ping_service(mut channel: Channel) -> Result<()> {
    ///     let ping: String = channel.receive().await?;
    ///     println!("received {}", ping);
    ///     channel.send("Pong!").await?;
    ///     Ok(())
    /// }
    ///
    /// GLOBAL_ROUTE.add_service_at::<ping_service>("ping", ())?;
    /// ```
    pub fn add_service_at<T: Service>(&self, at: &str, meta: T::Meta) -> Result<()> {
        match self
            .0
            .insert(at.into(), Storable::Service(T::service(meta)))
        {
            Some(_) => err!((in_use, format!("service `{}` already exists", at))),
            None => Ok(()),
        }
    }
    /// adds a service to the route
    /// ```norun
    /// #[service]
    /// async fn ping_service(mut channel: Channel) -> Result<()> {
    ///     let ping: String = channel.receive().await?;
    ///     println!("received {}", ping);
    ///     channel.send("Pong!").await?;
    ///     Ok(())
    /// }
    ///
    /// GLOBAL_ROUTE.add_service::<ping_service>(())?;
    /// ```
    pub fn add_service<T: Service>(&self, meta: T::Meta) -> Result<()> {
        self.add_service_at::<T>(T::ENDPOINT, meta)
    }
    /// removes a service from the route
    /// ```norun
    /// GLOBAL_ROUTE.remove_service::<my_service>()?;
    /// ```
    pub fn remove_service<T: Service>(&self) -> Result<()> {
        match self.0.remove(T::ENDPOINT) {
            Some(_) => Ok(()),
            None => err!((
                not_found,
                format!("service `{}` doesn't exist", T::ENDPOINT)
            )),
        }
    }
    /// remove the register endpoint from the route
    /// ```norun
    /// GLOBAL_ROUTE.remove_register::<my_custom_register>()?
    /// ```
    pub fn remove_register<T: Register>(&self) -> Result<()> {
        match self.0.remove(T::ENDPOINT) {
            Some(_) => Ok(()),
            None => err!((not_found, format!("route `{}` doesn't exist", T::ENDPOINT))),
        }
    }
    /// remove the specified id from the route
    /// ```norun
    /// GLOBAL_ROUTE.remove_at("my_service")?
    /// ```
    pub fn remove_at(&self, at: &str) -> Result<()> {
        match self.0.remove(at) {
            Some(_) => Ok(()),
            None => err!((
                not_found,
                format!("route or service `{}` doesn't exist", at)
            )),
        }
    }
    /// add a route into the route at the specified id.
    /// ```norun
    /// GLOBAL_ROUTE.add_route_at("MyRoute", Route::default())?;
    /// ```
    pub fn add_route_at(&self, at: &str, route: impl Into<Arc<Route>>) -> Result<()> {
        match self.0.insert(at.into(), Storable::Route(route.into())) {
            Some(_) => err!((in_use, format!("route `{}` already exists", at))),
            None => Ok(()),
        }
    }
    /// register into a new route and add the new route at the specified id
    /// ```norun
    /// GLOBAL_ROUTE.register_route_at::<MyType>("MyRoute", ())?;
    /// ```
    /// the global route now looks like this:
    ///
    /// GLOBAL_ROUTE:
    /// - MyRoute:
    ///   - ... whatever the register implementation added
    pub fn register_route_at<T: Register>(&self, at: &str, meta: T::Meta) -> Result<()> {
        let route = Route::default();
        T::register(&route, meta)?;
        self.add_route_at(at, route)?;
        Ok(())
    }
    /// register into a new route and add it
    /// ```norun
    /// GLOBAL_ROUTE.register_route::<MyRoute>(())?;
    /// ```
    /// the global route now looks like this:
    ///
    /// GLOBAL_ROUTE:
    /// - MyRoute:
    ///   - ... whatever the register implementation added
    pub fn register_route<T: Register>(&self, meta: T::Meta) -> Result<()> {
        self.register_route_at::<T>(T::ENDPOINT, meta)
    }
    /// registers the type on the route
    /// ```norun
    /// GLOBAL_ROUTE.register::<MyRoute>(())?;
    /// ```
    pub fn register<T: Register>(&self, meta: T::Meta) -> Result<()> {
        T::register(self, meta)
    }

    fn static_switch(
        &'static self,
        id: impl AsRef<Utf8Path>,
        chan: impl Into<BareChannel>,
    ) -> ::core::result::Result<(), (igcp::Error, BareChannel)> {
        let mut id = id.as_ref().into_iter();
        let chan = chan.into();
        let first = match id.next() {
            Some(id) => id,
            None => return Err((err!(invalid_data, "service name is empty"), chan))?,
        };
        let value = match self.0.get(first) {
            Some(id) => id,
            None => {
                return Err((
                    err!(invalid_data, format!("service `{:?}` not found", id)),
                    chan,
                ))?
            }
        };
        let ctx = self.context_static();
        match value.value() {
            Storable::Route(r) => {
                let mut map = r.clone();
                loop {
                    let next = match id.next() {
                        Some(id) => id,
                        None => {
                            return Err((
                                err!(not_found, format!("service `{:?}` not found", id)),
                                chan,
                            ))
                        }
                    };
                    let next_map = {
                        let val = match map.0.get(next) {
                            Some(val) => val,
                            None => {
                                return Err((
                                    err!(not_found, format!("service `{:?}` not found", id)),
                                    chan,
                                ))
                            }
                        };
                        match val.value() {
                            Storable::Route(r) => r.clone(),
                            Storable::Service(f) => {
                                f(chan, ctx);
                                return Ok(());
                            }
                        }
                    };
                    map = next_map;
                }
            }
            Storable::Service(f) => {
                f(chan, ctx);
                Ok(())
            }
        }
    }

    // all next are used for the routing system

    pub(crate) fn introduce_static(&'static self, c: BareChannel) {
        let mut c: Channel = c.into();
        spawn(async move {
            let id = match c.receive::<RouteKey>().await {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("found error receiving id of service: {:?}", &e);
                    err!((other, e))?
                }
            };
            self.introduce_service_static(id.as_ref(), c.bare()).await?;
            Ok::<_, igcp::Error>(())
        });
    }

    pub(crate) async fn introduce_static_unspawn(&'static self, c: BareChannel) -> Result<()> {
        let mut c: Channel = c.into();
        let id = match c.receive::<RouteKey>().await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("found error receiving id of service: {:?}", &e);
                err!((other, e))?
            }
        };
        self.introduce_service_static(id.as_ref(), c.bare()).await?;
        Ok(())
    }

    pub(crate) async fn introduce_service_static(
        &'static self,
        id: impl AsRef<Utf8Path>,
        bare: BareChannel,
    ) -> Result<()> {
        let id = id.as_ref();
        if let Err((e, c)) = self.__introduce_inner_static(id, bare).await {
            let mut chan: Channel = c.into();
            chan.send(Status::NotFound).await?;
            err!((e))?
        }
        Ok(())
    }

    async fn __introduce_inner_static(
        &'static self,
        id: impl AsRef<Utf8Path>,
        chan: BareChannel,
    ) -> ::core::result::Result<(), (igcp::Error, BareChannel)> {
        let mut id = id.as_ref().into_iter();
        let first = match id.next() {
            Some(id) => id,
            None => return Err((err!(invalid_data, "service name is empty"), chan))?,
        };
        let value = match self.0.get(first) {
            Some(id) => id,
            None => {
                return Err((
                    err!(invalid_data, format!("service `{:?}` not found", id)),
                    chan,
                ))?
            }
        };
        let ctx = self.context_static();
        match value.value() {
            Storable::Route(r) => {
                let mut map = r.clone();
                loop {
                    let next = match id.next() {
                        Some(id) => id,
                        None => {
                            return Err((
                                err!(not_found, format!("service `{:?}` not found", id)),
                                chan,
                            ))
                        }
                    };
                    let next_map = {
                        let val = match map.0.get(next) {
                            Some(val) => val,
                            None => {
                                return Err((
                                    err!(not_found, format!("service `{:?}` not found", id)),
                                    chan,
                                ))
                            }
                        };
                        match val.value() {
                            Storable::Route(r) => r.clone(),
                            Storable::Service(f) => {
                                let mut chan: Channel = chan.into();
                                chan.tx(Status::Found).await.ok();
                                f(chan.bare(), ctx);
                                return Ok(());
                            }
                        }
                    };
                    map = next_map;
                }
            }
            Storable::Service(f) => {
                let mut chan: Channel = chan.into();
                chan.tx(Status::Found).await.ok();
                f(chan.bare(), ctx);
                Ok(())
            }
        }
    }
    // pub(crate) async fn introduce_service(
    //     &self,
    //     id: impl AsRef<Utf8Path>,
    //     bare: BareChannel,
    // ) -> Result<()> {
    //     let id = id.as_ref();
    //     if let Err((e, c)) = self.__introduce_inner(id, bare).await {
    //         let mut chan: Channel = c.into();
    //         chan.send(Status::NotFound).await?;
    //         err!((e))?
    //     }
    //     Ok(())
    // }
    // pub(crate) async fn introduce_service(
    //     &self,
    //     id: impl AsRef<Utf8Path>,
    //     bare: BareChannel,
    // ) -> Result<()> {
    //     let id = id.as_ref();
    //     if let Err((e, c)) = self.__introduce_inner(id, bare).await {
    //         let mut chan: Channel = c.into();
    //         chan.send(Status::NotFound).await?;
    //         err!((e))?
    //     }
    //     Ok(())
    // }
    // async fn __introduce_inner(
    //     &self,
    //     id: impl AsRef<Utf8Path>,
    //     chan: BareChannel,
    // ) -> ::core::result::Result<(), (igcp::Error, BareChannel)> {
    //     let mut id = id.as_ref().into_iter();
    //     let first = match id.next() {
    //         Some(id) => id,
    //         None => return Err((err!(invalid_data, "service name is empty"), chan))?,
    //     };
    //     let value = match self.0.get(first) {
    //         Some(id) => id,
    //         None => {
    //             return Err((
    //                 err!(invalid_data, format!("service `{:?}` not found", id)),
    //                 chan,
    //             ))?
    //         }
    //     };
    //     match value.value() {
    //         Storable::Route(r) => {
    //             let mut map = r.clone();
    //             loop {
    //                 let next = match id.next() {
    //                     Some(id) => id,
    //                     None => {
    //                         return Err((
    //                             err!(not_found, format!("service `{:?}` not found", id)),
    //                             chan,
    //                         ))
    //                     }
    //                 };
    //                 let next_map = {
    //                     let val = match map.0.get(next) {
    //                         Some(val) => val,
    //                         None => {
    //                             return Err((
    //                                 err!(not_found, format!("service `{:?}` not found", id)),
    //                                 chan,
    //                             ))
    //                         }
    //                     };
    //                     match val.value() {
    //                         Storable::Route(r) => r.clone(),
    //                         Storable::Service(f) => {
    //                             let mut chan: Channel = chan.into();
    //                             chan.tx(Status::Found).await.ok();
    //                             f(chan.bare());
    //                             return Ok(());
    //                         }
    //                     }
    //                 };
    //                 map = next_map;
    //             }
    //         }
    //         Storable::Service(f) => {
    //             let mut chan: Channel = chan.into();
    //             chan.tx(Status::Found).await.ok();
    //             f(chan.bare());
    //             Ok(())
    //         }
    //     }
    // }
}
