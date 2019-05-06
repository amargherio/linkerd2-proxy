use indexmap::IndexMap;
use std::{collections::HashMap, iter::IntoIterator, net::SocketAddr};

use futures::{Async, Stream};
use tower_grpc::{generic::client::GrpcService, BoxBody};

use api::{
    destination::{
        protocol_hint::Protocol, update::Update as PbUpdate2, TlsIdentity, WeightedAddr,
    },
    net::TcpAddress,
};

use control::{
    cache::{Cache, CacheChange, Exists},
    destination::{Metadata, ProtocolHint, Responder, Update},
    remote_stream::Remote,
};
use identity;
use NameAddr;

use super::{Client, Query, UpdateRx};

/// Holds the state of a single resolution.
pub(super) struct DestinationSet<T>
where
    T: GrpcService<BoxBody>,
{
    addrs: Exists<Cache<SocketAddr, Metadata>>,
    query: Option<Query<T>>,
    responders: Vec<Responder>,
}

// ===== impl DestinationSet =====

impl<T> DestinationSet<T>
where
    T: GrpcService<BoxBody>,
{
    /// Construct a new `DestinationSet` for the given `responder` by using
    /// `client` to query the destination service for `auth`.
    pub fn new(auth: &NameAddr, responder: Responder, client: &mut Client<T>) -> Self {
        let query = client.query(auth, "connect");
        Self {
            addrs: Exists::Unknown,
            query,
            responders: vec![responder],
        }
    }

    /// Drive the Destination service query for `auth`, returning `true` if the
    /// destination set will need to be queued to reconnect.
    pub fn poll_dst(&mut self, auth: &NameAddr) -> bool {
        let mut needs_reconnect = false;
        self.query = match self.query.take() {
            Some(Remote::ConnectedOrConnecting { rx }) => {
                let new_query = self.poll_query(auth, rx);
                if let Some(Remote::NeedsReconnect) = new_query {
                    self.reset_on_next_modification();
                    needs_reconnect = true;
                }
                new_query
            }
            None => {
                let exists = self.exists();
                self.no_endpoints(auth, exists);
                None
            }
            query => query,
        };
        needs_reconnect
    }

    /// Add a new responder that will be updated with changes in this
    /// destination set.
    pub fn add_responder(&mut self, responder: Responder) {
        match self.addrs {
            Exists::Yes(ref cache) => {
                for (&addr, meta) in cache {
                    let update = Update::Add(addr, meta.clone());
                    responder
                        .update_tx
                        .unbounded_send(update)
                        .expect("unbounded_send does not fail");
                }
            }
            Exists::No | Exists::Unknown => (),
        }
        self.responders.push(responder);
    }

    /// Reconnect the Destination service query for `auth`, using the
    /// provided `client`.
    pub fn reconnect(&mut self, auth: &NameAddr, client: &mut Client<T>) {
        self.query = client.query(auth, "reconnect")
    }

    /// Drops any inactive responders.
    pub fn retain_active(&mut self) -> &mut Self {
        self.responders.retain(Responder::is_active);
        self
    }

    /// Returns `true` if this destination set contains any active responders.
    pub fn is_active(&self) -> bool {
        self.responders.len() > 0
    }

    // Processes Destination service updates from `rx`, returning the new query
    // (or `None` if the destination service should not be queried.)
    fn poll_query(&mut self, auth: &NameAddr, mut rx: UpdateRx<T>) -> Option<Query<T>> {
        loop {
            match rx.poll() {
                Ok(Async::Ready(Some(update))) => match update.update {
                    Some(PbUpdate2::Add(a_set)) => {
                        let set_labels = a_set.metric_labels;
                        let addrs = a_set
                            .addrs
                            .into_iter()
                            .filter_map(|pb| pb_to_addr_meta(pb, &set_labels));
                        self.add(auth, addrs)
                    }
                    Some(PbUpdate2::Remove(r_set)) => {
                        let addrs = r_set.addrs.into_iter().filter_map(pb_to_sock_addr);
                        self.remove(auth, addrs);
                    }
                    Some(PbUpdate2::NoEndpoints(ref no_endpoints)) => {
                        self.no_endpoints(auth, no_endpoints.exists);
                    }
                    None => (),
                },
                Ok(Async::Ready(None)) => {
                    trace!(
                        "Destination.Get stream ended for {:?}, must reconnect",
                        auth
                    );
                    return Some(Remote::NeedsReconnect);
                }
                Ok(Async::NotReady) => {
                    return Some(Remote::ConnectedOrConnecting { rx });
                }
                Err(ref status) if status.code() == tower_grpc::Code::InvalidArgument => {
                    // Invalid Argument is returned to indicate that the
                    // requested name should *not* query the destination
                    // service. In this case, do not attempt to reconnect.
                    debug!(
                        "Destination.Get stream ended for {:?} with Invalid Argument",
                        auth
                    );
                    self.no_endpoints(auth, false);
                    return None;
                }
                Err(err) => {
                    warn!("Destination.Get stream errored for {:?}: {:?}", auth, err);
                    return Some(Remote::NeedsReconnect);
                }
            };
        }
    }

    fn exists(&self) -> bool {
        match self.addrs {
            Exists::Yes(_) => true,
            _ => false,
        }
    }

    fn reset_on_next_modification(&mut self) {
        match self.addrs {
            Exists::Yes(ref mut cache) => {
                cache.set_reset_on_next_modification();
            }
            Exists::No | Exists::Unknown => (),
        }
    }

    fn add<A>(&mut self, authority_for_logging: &NameAddr, addrs_to_add: A)
    where
        A: Iterator<Item = (SocketAddr, Metadata)>,
    {
        let mut cache = match self.addrs.take() {
            Exists::Yes(mut cache) => cache,
            Exists::Unknown | Exists::No => Cache::new(),
        };
        cache.update_union(addrs_to_add, &mut |change| {
            Self::on_change(&mut self.responders, authority_for_logging, change)
        });
        self.addrs = Exists::Yes(cache);
    }

    fn remove<A>(&mut self, authority_for_logging: &NameAddr, addrs_to_remove: A)
    where
        A: Iterator<Item = SocketAddr>,
    {
        let cache = match self.addrs.take() {
            Exists::Yes(mut cache) => {
                cache.remove(addrs_to_remove, &mut |change| {
                    Self::on_change(&mut self.responders, authority_for_logging, change)
                });
                cache
            }
            Exists::Unknown | Exists::No => Cache::new(),
        };
        self.addrs = Exists::Yes(cache);
    }

    fn no_endpoints(&mut self, authority_for_logging: &NameAddr, exists: bool) {
        trace!(
            "no endpoints for {:?} that is known to {}",
            authority_for_logging,
            if exists { "exist" } else { "not exist" }
        );
        self.responders.retain(|r| {
            let sent = r.update_tx.unbounded_send(Update::NoEndpoints);
            sent.is_ok()
        });
        match self.addrs.take() {
            Exists::Yes(mut cache) => {
                cache.clear(&mut |change| {
                    Self::on_change(&mut self.responders, authority_for_logging, change)
                });
            }
            Exists::Unknown | Exists::No => (),
        };
        self.addrs = if exists {
            Exists::Yes(Cache::new())
        } else {
            Exists::No
        };
    }

    fn on_change(
        responders: &mut Vec<Responder>,
        authority_for_logging: &NameAddr,
        change: CacheChange<SocketAddr, Metadata>,
    ) {
        let (update_str, update, addr) = match change {
            CacheChange::Insertion { key, value } => {
                ("insert", Update::Add(key, value.clone()), key)
            }
            CacheChange::Removal { key } => ("remove", Update::Remove(key), key),
            CacheChange::Modification { key, new_value } => (
                "change metadata for",
                Update::Add(key, new_value.clone()),
                key,
            ),
        };
        trace!("{} {:?} for {:?}", update_str, addr, authority_for_logging);
        // retain is used to drop any senders that are dead
        responders.retain(|r| {
            let sent = r.update_tx.unbounded_send(update.clone());
            sent.is_ok()
        });
    }
}

/// Construct a new labeled `SocketAddr `from a protobuf `WeightedAddr`.
fn pb_to_addr_meta(
    pb: WeightedAddr,
    set_labels: &HashMap<String, String>,
) -> Option<(SocketAddr, Metadata)> {
    let addr = pb.addr.and_then(pb_to_sock_addr)?;

    let meta = {
        let mut t = set_labels
            .iter()
            .chain(pb.metric_labels.iter())
            .collect::<Vec<(&String, &String)>>();
        t.sort_by(|(k0, _), (k1, _)| k0.cmp(k1));

        let mut m = IndexMap::with_capacity(t.len());
        for (k, v) in t.into_iter() {
            m.insert(k.clone(), v.clone());
        }

        m
    };

    let mut proto_hint = ProtocolHint::Unknown;
    if let Some(hint) = pb.protocol_hint {
        if let Some(proto) = hint.protocol {
            match proto {
                Protocol::H2(..) => {
                    proto_hint = ProtocolHint::Http2;
                }
            }
        }
    }

    let tls_id = pb.tls_identity.and_then(pb_to_id);
    let meta = Metadata::new(meta, proto_hint, tls_id, pb.weight);
    Some((addr, meta))
}

fn pb_to_id(pb: TlsIdentity) -> Option<identity::Name> {
    use api::destination::tls_identity::Strategy;

    let Strategy::DnsLikeIdentity(i) = pb.strategy?;
    match identity::Name::from_hostname(i.name.as_bytes()) {
        Ok(i) => Some(i),
        Err(_) => {
            warn!("Ignoring invalid identity: {}", i.name);
            None
        }
    }
}

fn pb_to_sock_addr(pb: TcpAddress) -> Option<SocketAddr> {
    use api::net::ip_address::Ip;
    use std::net::{Ipv4Addr, Ipv6Addr};
    /*
    current structure is:
    TcpAddress {
        ip: Option<IpAddress {
            ip: Option<enum Ip {
                Ipv4(u32),
                Ipv6(IPv6 {
                    first: u64,
                    last: u64,
                }),
            }>,
        }>,
        port: u32,
    }
    */
    match pb.ip {
        Some(ip) => match ip.ip {
            Some(Ip::Ipv4(octets)) => {
                let ipv4 = Ipv4Addr::from(octets);
                Some(SocketAddr::from((ipv4, pb.port as u16)))
            }
            Some(Ip::Ipv6(v6)) => {
                let octets = [
                    (v6.first >> 56) as u8,
                    (v6.first >> 48) as u8,
                    (v6.first >> 40) as u8,
                    (v6.first >> 32) as u8,
                    (v6.first >> 24) as u8,
                    (v6.first >> 16) as u8,
                    (v6.first >> 8) as u8,
                    v6.first as u8,
                    (v6.last >> 56) as u8,
                    (v6.last >> 48) as u8,
                    (v6.last >> 40) as u8,
                    (v6.last >> 32) as u8,
                    (v6.last >> 24) as u8,
                    (v6.last >> 16) as u8,
                    (v6.last >> 8) as u8,
                    v6.last as u8,
                ];
                let ipv6 = Ipv6Addr::from(octets);
                Some(SocketAddr::from((ipv6, pb.port as u16)))
            }
            None => None,
        },
        None => None,
    }
}
