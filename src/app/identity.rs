use futures::{Async, Future, Poll, Stream};
use futures_watch::{Store, Watch};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio_timer::{clock, Delay};
use tower_grpc::{self as grpc, generic::client::GrpcService, BoxBody};

use api::identity as api;
use never::Never;

use identity;
pub use identity::{Crt, CrtKey, InvalidName, Key, Name, TokenSource, TrustAnchors, CSR};

#[derive(Debug)]
pub struct Config {
    pub svc_addr: super::config::ControlAddr,
    pub trust_anchors: TrustAnchors,
    pub key: Key,
    pub csr: CSR,
    pub local_name: Name,
    pub token: TokenSource,
    pub min_refresh: Duration,
    pub max_refresh: Duration,
}

#[derive(Clone)]
pub struct Local {
    trust_anchors: TrustAnchors,
    name: Name,
    crt_key: Watch<Option<CrtKey>>,
}

/// Drives updates.
pub struct Daemon<T>
where
    T: GrpcService<BoxBody>,
    T::ResponseBody: grpc::Body,
{
    config: Config,
    client: api::client::Identity<T>,
    crt_key: Store<Option<CrtKey>>,
    expiry: SystemTime,
    inner: Inner<T>,
}

enum Inner<T>
where
    T: GrpcService<BoxBody>,
    T::ResponseBody: grpc::Body,
{
    Waiting(Delay),
    ShouldRefresh,
    Pending(grpc::client::unary::ResponseFuture<api::CertifyResponse, T::Future, T::ResponseBody>),
}

pub fn new<T>(config: Config, client: T) -> (Local, Daemon<T>)
where
    T: GrpcService<BoxBody>,
{
    let (ck_watch, ck_store) = Watch::new(None);
    let id = Local {
        name: config.local_name.clone(),
        trust_anchors: config.trust_anchors.clone(),
        crt_key: ck_watch,
    };
    let d = Daemon {
        config,
        crt_key: ck_store,
        inner: Inner::ShouldRefresh,
        expiry: UNIX_EPOCH,
        client: api::client::Identity::new(client),
    };
    (id, d)
}

// === impl LocalIdentity ===

impl LocalIdentity {}

// === impl Daemon ===

impl Config {
    fn refresh(&self, expiry: SystemTime) -> Delay {
        let now = clock::now();

        let refresh = match expiry
            .duration_since(SystemTime::now())
            .ok()
            .map(|d| d * 7 / 10)
        {
            None => self.min_refresh,
            Some(lifetime) if lifetime < self.min_refresh => self.min_refresh,
            Some(lifetime) if self.max_refresh < lifetime => self.max_refresh,
            Some(lifetime) => lifetime,
        };

        Delay::new(now + refresh)
    }
}

impl<T> Future for Daemon<T>
where
    T: GrpcService<BoxBody> + Clone,
{
    type Item = ();
    type Error = Never;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            self.inner = match self.inner {
                Inner::Waiting(ref mut d) => {
                    if let Ok(Async::NotReady) = d.poll() {
                        return Ok(Async::NotReady);
                    }
                    Inner::ShouldRefresh
                }
                Inner::ShouldRefresh => {
                    let req = grpc::Request::new(api::CertifyRequest {
                        identity: self.config.local_name.as_ref().to_owned(),
                        token: self.config.token.load().expect("FIXME"),
                        certificate_signing_request: self.config.csr.to_vec(),
                    });
                    let f = self.client.certify(req);
                    Inner::Pending(f)
                }
                Inner::Pending(ref mut p) => match p.poll() {
                    Ok(Async::NotReady) => return Ok(Async::NotReady),
                    Ok(Async::Ready(rsp)) => {
                        let api::CertifyResponse {
                            leaf_certificate,
                            intermediate_certificates,
                            valid_until,
                        } = rsp.into_inner();

                        match valid_until.and_then(|d| Result::<SystemTime, Duration>::from(d).ok())
                        {
                            None => error!(
                                "Identity service did not specify a ceritificate expiration."
                            ),
                            Some(expiry) => {
                                let key = self.config.key.clone();
                                let crt = Crt::new(
                                    self.config.local_name.clone(),
                                    leaf_certificate,
                                    intermediate_certificates,
                                    expiry,
                                );

                                match self.config.trust_anchors.certify(key, crt) {
                                    Err(e) => {
                                        error!("Received invalid ceritficate: {}", e);
                                    }
                                    Ok(crt_key) => {
                                        if self.crt_key.store(Some(crt_key)).is_err() {
                                            // If we can't store a value, than all observations
                                            // have been dropped and we can stop refreshing.
                                            return Ok(Async::Ready(()));
                                        }

                                        self.expiry = expiry;
                                    }
                                }
                            }
                        }

                        Inner::Waiting(self.config.refresh(self.expiry))
                    }
                    Err(e) => {
                        error!("Failed to certify identity: {}", e);
                        Inner::Waiting(self.config.refresh(self.expiry))
                    }
                },
            };
        }
    }
}
