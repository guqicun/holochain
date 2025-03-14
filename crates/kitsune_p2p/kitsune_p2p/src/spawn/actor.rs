// this is largely a passthrough that routes to a specific space handler

use crate::actor::*;
use crate::event::*;
use crate::gossip::sharded_gossip::BandwidthThrottles;
use crate::gossip::sharded_gossip::KitsuneDiagnostics;
use crate::types::gossip::GossipModuleType;
use crate::wire::MetricExchangeMsg;
use crate::*;
use futures::future::FutureExt;
use kitsune_p2p_bootstrap_client::BootstrapNet;
use kitsune_p2p_fetch::*;
use kitsune_p2p_types::agent_info::AgentInfoSigned;
use kitsune_p2p_types::async_lazy::AsyncLazy;
use kitsune_p2p_types::config::{KitsuneP2pConfig, TransportConfig};
use kitsune_p2p_types::dht::Arq;
use kitsune_p2p_types::*;
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::sync::Arc;

/// Default webrtc config if set to `None`.
/// TODO - set this to holochain stun servers once they exist!
const DEFAULT_WEBRTC_CONFIG: &str = r#"{
  "iceServers": [
    { "urls": ["stun:stun-0.main.infra.holo.host:443"] },
    { "urls": ["stun:stun-1.main.infra.holo.host:443"] }
  ]
}"#;

/// The bootstrap service is much more thoroughly documented in the default service implementation.
/// See <https://github.com/holochain/bootstrap>
mod discover;
pub(crate) mod meta_net;
use meta_net::*;
mod fetch;
mod meta_net_task;
mod space;
use ghost_actor::dependencies::tracing;
use space::*;

#[cfg(feature = "test_utils")]
pub mod test_util;

type EvtRcv = futures::channel::mpsc::Receiver<KitsuneP2pEvent>;
type KSpace = Arc<KitsuneSpace>;
type KAgent = Arc<KitsuneAgent>;
type KBasis = Arc<KitsuneBasis>;
type VecMXM = Vec<MetricExchangeMsg>;
type Payload = Box<[u8]>;
type OpHashList = Vec<OpHashSized>;
type MaybeDelegate = Option<(KBasis, u32, u32)>;

/// Random number.
const UNAUTHORIZED_DISCONNECT_CODE: u32 = 0x59ea599e;
const UNAUTHORIZED_DISCONNECT_REASON: &str = "unauthorized";

ghost_actor::ghost_chan! {
    #[allow(clippy::too_many_arguments)]
    pub chan Internal<crate::KitsuneP2pError> {
        /// Notification that we have a new address to be identified at
        fn new_address(local_url: String) -> ();

        /// Register space event handler
        fn register_space_event_handler(recv: EvtRcv) -> ();

        /// Incoming Delegate Broadcast
        /// We are being requested to delegate a broadcast to our neighborhood
        /// on behalf of an author. `mod_idx` / `mod_cnt` inform us which
        /// neighbors we are responsible for.
        /// (See comments in actual method impl for more detail.)
        fn incoming_delegate_broadcast(
            space: KSpace,
            basis: KBasis,
            to_agent: KAgent,
            mod_idx: u32,
            mod_cnt: u32,
            data: BroadcastData,
        ) -> ();

        /// This should be invoked instead of incoming_delegate_broadcast
        /// in the case of a publish data variant. It will, in turn, call
        /// into incoming_delegate_broadcast once we have the data to act
        /// as a fetch responder for the op data.
        fn incoming_publish(
            space: KSpace,
            to_agent: KAgent,
            source: KAgent,
            transfer_method: kitsune_p2p_fetch::TransferMethod,
            op_hash_list: OpHashList,
            context: kitsune_p2p_fetch::FetchContext,
            maybe_delegate: MaybeDelegate,
        ) -> ();

        /// We just received data for an op_hash. Check if we had a pending
        /// delegation action we need to continue now that we have the data.
        fn resolve_publish_pending_delegates(space: KSpace, op_hash: KOpHash) -> ();

        /// Incoming Gossip
        fn incoming_gossip(space: KSpace, con: MetaNetCon, remote_url: String, data: Payload, module_type: crate::types::gossip::GossipModuleType) -> ();

        /// Incoming Metric Exchange
        fn incoming_metric_exchange(space: KSpace, msgs: VecMXM) -> ();

        /// New Con
        fn new_con(url: String, con: MetaNetCon) -> ();

        /// Del Con
        fn del_con(url: String) -> ();

        /// Fetch an op from a remote
        fn fetch(key: FetchKey, space: KSpace, source: FetchSource) -> ();

        /// Get all local joined agent infos across all spaces.
        fn get_all_local_joined_agent_infos() -> Vec<AgentInfoSigned>;
    }
}

pub(crate) struct KitsuneP2pActor {
    channel_factory: ghost_actor::actor_builder::GhostActorChannelFactory<Self>,
    internal_sender: ghost_actor::GhostSender<Internal>,
    ep_hnd: MetaNet,
    host_api: HostApiLegacy,
    #[allow(clippy::type_complexity)]
    spaces: HashMap<
        Arc<KitsuneSpace>,
        AsyncLazy<(
            ghost_actor::GhostSender<KitsuneP2p>,
            ghost_actor::GhostSender<space::SpaceInternal>,
        )>,
    >,
    config: Arc<KitsuneP2pConfig>,
    bootstrap_net: BootstrapNet,
    bandwidth_throttles: BandwidthThrottles,
    parallel_notify_permit: Arc<tokio::sync::Semaphore>,
    fetch_pool: FetchPool,
    local_url: Arc<std::sync::Mutex<Option<String>>>,
}

impl KitsuneP2pActor {
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        config: KitsuneP2pConfig,
        channel_factory: ghost_actor::actor_builder::GhostActorChannelFactory<Self>,
        internal_sender: ghost_actor::GhostSender<Internal>,
        direct_host_api: HostApiLegacy,
        self_host_api: HostApiLegacy,
        ep_hnd: MetaNet,
        ep_evt: MetaNetEvtRecv,
        bootstrap_net: BootstrapNet,
        maybe_peer_url: Option<String>,
    ) -> KitsuneP2pResult<Self> {
        let local_url = Arc::new(std::sync::Mutex::new(maybe_peer_url));

        crate::types::metrics::init();

        let fetch_response_queue =
            FetchResponseQueue::new(FetchResponseConfig::new(config.tuning_params.clone()));

        // TODO - use a real config
        let fetch_pool = FetchPool::new_bitwise_or();

        // Start a loop to handle our fetch queue fetch items.
        FetchTask::spawn(
            config.clone(),
            fetch_pool.clone(),
            self_host_api.clone(),
            internal_sender.clone(),
        );

        let i_s = internal_sender.clone();

        let bandwidth_throttles = BandwidthThrottles::new(&config.tuning_params);
        let parallel_notify_permit = Arc::new(tokio::sync::Semaphore::new(
            config.tuning_params.concurrent_limit_per_thread,
        ));

        MetaNetTask::new(
            self_host_api.clone(),
            config.clone(),
            fetch_pool.clone(),
            fetch_response_queue,
            ep_evt,
            i_s,
        )
        .spawn();

        Ok(Self {
            channel_factory,
            internal_sender,
            ep_hnd,
            host_api: direct_host_api,
            spaces: HashMap::new(),
            config: Arc::new(config),
            bootstrap_net,
            bandwidth_throttles,
            parallel_notify_permit,
            fetch_pool,
            local_url,
        })
    }
}

pub(super) async fn create_meta_net(
    config: &KitsuneP2pConfig,
    _tls_config: tls::TlsConfig,
    internal_sender: ghost_actor::GhostSender<Internal>,
    host: HostApiLegacy,
    preflight_user_data: PreflightUserData,
) -> KitsuneP2pResult<(MetaNet, MetaNetEvtRecv, BootstrapNet, Option<String>)> {
    let mut ep_hnd = None;
    let mut ep_evt = None;
    let mut bootstrap_net = None;
    let mut maybe_peer_url = None;

    if ep_hnd.is_none() && config.is_tx5() {
        tracing::trace!("tx5");
        let mut tune: kitsune_p2p_types::config::tuning_params_struct::KitsuneP2pTuningParams =
            (*config.tuning_params).clone();
        let (signal_url, webrtc_config) = match config.transport_pool.first().unwrap() {
            TransportConfig::WebRTC {
                signal_url,
                webrtc_config,
            } => {
                let webrtc_config = webrtc_config
                    .as_ref()
                    .map(|c| serde_json::to_string(&c).expect("Can Serialize JSON"))
                    .unwrap_or_else(|| DEFAULT_WEBRTC_CONFIG.to_string());
                (signal_url.clone(), webrtc_config)
            }
            TransportConfig::Mem {} => {
                tune.tx5_backend_module = "mem".to_string();
                ("wss://fake.fake".to_string(), "{}".to_string())
            }
        };
        let (h, e, p) = MetaNet::new_tx5(
            Arc::new(tune),
            host.clone(),
            internal_sender.clone(),
            signal_url,
            webrtc_config,
            preflight_user_data,
        )
        .await?;
        ep_hnd = Some(h);
        ep_evt = Some(e);
        bootstrap_net = Some(BootstrapNet::Tx5);
        maybe_peer_url = p;
    }

    match (ep_hnd, ep_evt, bootstrap_net) {
        (Some(h), Some(e), Some(n)) => Ok((h, e, n, maybe_peer_url)),
        _ => Err("Network config has no valid transport".into()),
    }
}

use crate::spawn::actor::fetch::{FetchResponseConfig, FetchTask};
use crate::spawn::actor::meta_net_task::MetaNetTask;
use ghost_actor::dependencies::must_future::MustBoxFuture;
use kitsune_p2p_types::bootstrap::AgentInfoPut;

impl ghost_actor::GhostControlHandler for KitsuneP2pActor {
    fn handle_ghost_actor_shutdown(mut self) -> MustBoxFuture<'static, ()> {
        use futures::sink::SinkExt;
        use ghost_actor::GhostControlSender;
        async move {
            // The line below was added when migrating to rust edition 2021, per
            // https://doc.rust-lang.org/edition-guide/rust-2021/disjoint-capture-in-closures.html#migration
            let _ = &self;
            // this is a courtesy, ok if fails
            let _ = self.host_api.legacy.close().await;
            self.ep_hnd.close(500, "").await;
            for (_, space) in self.spaces.into_iter() {
                let (space, _) = space.get().await;
                let _ = space.ghost_actor_shutdown_immediate().await;
            }
        }
        .boxed()
        .into()
    }
}

impl ghost_actor::GhostHandler<Internal> for KitsuneP2pActor {}

impl InternalHandler for KitsuneP2pActor {
    fn handle_new_address(&mut self, local_url: String) -> InternalHandlerResult<()> {
        let spaces = self.spaces.values().map(|s| s.get()).collect::<Vec<_>>();
        Ok(async move {
            let mut all = Vec::new();
            for (_, space) in futures::future::join_all(spaces).await {
                all.push(space.new_address(local_url.clone()));
            }
            let _ = futures::future::join_all(all).await;
            Ok(())
        }
        .boxed()
        .into())
    }

    fn handle_register_space_event_handler(
        &mut self,
        recv: futures::channel::mpsc::Receiver<KitsuneP2pEvent>,
    ) -> InternalHandlerResult<()> {
        let f = self.channel_factory.attach_receiver(recv);
        Ok(async move {
            f.await?;
            Ok(())
        }
        .boxed()
        .into())
    }

    fn handle_incoming_delegate_broadcast(
        &mut self,
        space: Arc<KitsuneSpace>,
        basis: Arc<KitsuneBasis>,
        to_agent: Arc<KitsuneAgent>,
        mod_idx: u32,
        mod_cnt: u32,
        data: BroadcastData,
    ) -> InternalHandlerResult<()> {
        let space_sender = match self.spaces.get_mut(&space) {
            None => {
                tracing::warn!(
                    "received delegate_broadcast for unhandled space: {:?}",
                    space
                );
                return unit_ok_fut();
            }
            Some(space) => space.get(),
        };
        Ok(async move {
            let (_, space_inner) = space_sender.await;
            space_inner
                .incoming_delegate_broadcast(space, basis, to_agent, mod_idx, mod_cnt, data)
                .await
        }
        .boxed()
        .into())
    }

    fn handle_incoming_publish(
        &mut self,
        space: KSpace,
        to_agent: KAgent,
        source: KAgent,
        transfer_method: kitsune_p2p_fetch::TransferMethod,
        op_hash_list: OpHashList,
        context: kitsune_p2p_fetch::FetchContext,
        maybe_delegate: MaybeDelegate,
    ) -> InternalHandlerResult<()> {
        let space_sender = match self.spaces.get_mut(&space) {
            None => {
                tracing::warn!("received publish for unhandled space: {:?}", space);
                return unit_ok_fut();
            }
            Some(space) => space.get(),
        };
        Ok(async move {
            let (_, space_inner) = space_sender.await;
            space_inner
                .incoming_publish(
                    space,
                    to_agent,
                    source,
                    transfer_method,
                    op_hash_list,
                    context,
                    maybe_delegate,
                )
                .await
        }
        .boxed()
        .into())
    }

    fn handle_resolve_publish_pending_delegates(
        &mut self,
        space: KSpace,
        op_hash: KOpHash,
    ) -> InternalHandlerResult<()> {
        let space_sender = match self.spaces.get_mut(&space) {
            None => {
                return unit_ok_fut();
            }
            Some(space) => space.get(),
        };
        Ok(async move {
            let (_, space_inner) = space_sender.await;
            space_inner
                .resolve_publish_pending_delegates(space, op_hash)
                .await
        }
        .boxed()
        .into())
    }

    fn handle_incoming_gossip(
        &mut self,
        space: Arc<KitsuneSpace>,
        con: MetaNetCon,
        remote_url: String,
        data: Box<[u8]>,
        module_type: GossipModuleType,
    ) -> InternalHandlerResult<()> {
        let space_sender = match self.spaces.get_mut(&space) {
            None => {
                tracing::warn!("received gossip for unhandled space: {:?}", space);
                return unit_ok_fut();
            }
            Some(space) => space.get(),
        };
        Ok(async move {
            let (_, space_inner) = space_sender.await;
            space_inner
                .incoming_gossip(space, con, remote_url, data, module_type)
                .await
        }
        .boxed()
        .into())
    }

    fn handle_incoming_metric_exchange(
        &mut self,
        space: Arc<KitsuneSpace>,
        msgs: Vec<MetricExchangeMsg>,
    ) -> InternalHandlerResult<()> {
        let space_sender = match self.spaces.get_mut(&space) {
            None => {
                return unit_ok_fut();
            }
            Some(space) => space.get(),
        };
        Ok(async move {
            let (_, space_inner) = space_sender.await;
            space_inner.incoming_metric_exchange(space, msgs).await
        }
        .boxed()
        .into())
    }

    fn handle_new_con(&mut self, url: String, con: MetaNetCon) -> InternalHandlerResult<()> {
        let spaces = self.spaces.values().map(|s| s.get()).collect::<Vec<_>>();
        Ok(async move {
            let mut all = Vec::new();
            for (_, space) in futures::future::join_all(spaces).await {
                all.push(space.new_con(url.clone(), con.clone()));
            }
            let _ = futures::future::join_all(all).await;
            Ok(())
        }
        .boxed()
        .into())
    }

    fn handle_del_con(&mut self, url: String) -> InternalHandlerResult<()> {
        let spaces = self.spaces.values().map(|s| s.get()).collect::<Vec<_>>();
        Ok(async move {
            let mut all = Vec::new();
            for (_, space) in futures::future::join_all(spaces).await {
                all.push(space.del_con(url.clone()));
            }
            let _ = futures::future::join_all(all).await;
            Ok(())
        }
        .boxed()
        .into())
    }

    fn handle_fetch(
        &mut self,
        key: FetchKey,
        space: KSpace,
        source: FetchSource,
    ) -> InternalHandlerResult<()> {
        let FetchSource::Agent(agent) = source;

        let space_sender = match self.spaces.get_mut(&space) {
            None => {
                tracing::warn!("received fetch for unhandled space: {:?}", space);
                return unit_ok_fut();
            }
            Some(space) => space.get(),
        };
        Ok(async move {
            let (_, space_inner) = space_sender.await;
            let payload = wire::Wire::fetch_op(vec![(space, vec![key])]);
            space_inner.notify(agent, payload).await
        }
        .boxed()
        .into())
    }

    /// Best effort to retrieve all local agent infos across all spaces. If there
    /// is an error for some space we simply log it and ignore the error for that
    /// space and return local joined agent infos from the other spaces.
    fn handle_get_all_local_joined_agent_infos(
        &mut self,
    ) -> InternalHandlerResult<Vec<AgentInfoSigned>> {
        let spaces = self.spaces.values().map(|s| s.get()).collect::<Vec<_>>();
        Ok(async move {
            let mut all = Vec::new();
            for (_, space) in futures::future::join_all(spaces).await {
                all.push(space.get_all_local_joined_agent_infos());
            }
            let agent_infos = futures::future::join_all(all)
                .await
                .into_iter()
                .filter_map(|maybe_agent_infos| {
                    if let Err(err) = &maybe_agent_infos {
                        tracing::warn!(?err, "error reading agent infos from spaces");
                    }
                    maybe_agent_infos.ok()
                })
                .flatten()
                .collect();
            Ok(agent_infos)
        }
        .boxed()
        .into())
    }
}

impl ghost_actor::GhostHandler<KitsuneP2pEvent> for KitsuneP2pActor {}

impl KitsuneP2pEventHandler for KitsuneP2pActor {
    fn handle_put_agent_info_signed(
        &mut self,
        input: crate::event::PutAgentInfoSignedEvt,
    ) -> KitsuneP2pEventHandlerResult<Vec<AgentInfoPut>> {
        let legacy_host = self.host_api.legacy.clone();
        let ep_hnd = self.ep_hnd.clone();

        Ok(async move {
            let puts = legacy_host.put_agent_info_signed(input).await?;

            for removed_url in puts.iter().flat_map(|r| r.removed_urls.clone()) {
                tracing::debug!(?removed_url, "peer URL changed, closing connection");
                if let Err(e) = ep_hnd.close_peer_con(removed_url.clone()) {
                    tracing::debug!(?e, ?removed_url, "could not close peer connection");
                }
            }

            Ok(puts)
        }
        .boxed()
        .into())
    }

    fn handle_query_agents(
        &mut self,
        input: crate::event::QueryAgentsEvt,
    ) -> KitsuneP2pEventHandlerResult<Vec<crate::types::agent_store::AgentInfoSigned>> {
        Ok(self.host_api.legacy.query_agents(input))
    }

    fn handle_query_peer_density(
        &mut self,
        space: Arc<KitsuneSpace>,
        arq: kitsune_p2p_types::dht_arc::DhtArc,
    ) -> KitsuneP2pEventHandlerResult<kitsune_p2p_types::dht::PeerView> {
        Ok(self.host_api.legacy.query_peer_density(space, arq))
    }

    fn handle_call(
        &mut self,
        space: Arc<KitsuneSpace>,
        to_agent: Arc<KitsuneAgent>,
        payload: Vec<u8>,
    ) -> KitsuneP2pEventHandlerResult<Vec<u8>> {
        Ok(self.host_api.legacy.call(space, to_agent, payload))
    }

    fn handle_notify(
        &mut self,
        space: Arc<KitsuneSpace>,
        to_agent: Arc<KitsuneAgent>,
        payload: Vec<u8>,
    ) -> KitsuneP2pEventHandlerResult<()> {
        Ok(self.host_api.legacy.notify(space, to_agent, payload))
    }

    fn handle_receive_ops(
        &mut self,
        space: Arc<KitsuneSpace>,
        ops: Vec<KOp>,
        context: Option<FetchContext>,
    ) -> KitsuneP2pEventHandlerResult<()> {
        Ok(self.host_api.legacy.receive_ops(space, ops, context))
    }

    fn handle_query_op_hashes(
        &mut self,
        input: QueryOpHashesEvt,
    ) -> KitsuneP2pEventHandlerResult<Option<(Vec<Arc<KitsuneOpHash>>, TimeWindowInclusive)>> {
        Ok(self.host_api.legacy.query_op_hashes(input))
    }

    fn handle_fetch_op_data(
        &mut self,
        input: FetchOpDataEvt,
    ) -> KitsuneP2pEventHandlerResult<Vec<(Arc<KitsuneOpHash>, KOp)>> {
        Ok(self.host_api.legacy.fetch_op_data(input))
    }

    fn handle_sign_network_data(
        &mut self,
        input: SignNetworkDataEvt,
    ) -> KitsuneP2pEventHandlerResult<KitsuneSignature> {
        Ok(self.host_api.legacy.sign_network_data(input))
    }
}

impl ghost_actor::GhostHandler<KitsuneP2p> for KitsuneP2pActor {}

impl KitsuneP2pHandler for KitsuneP2pActor {
    fn handle_join(
        &mut self,
        space: Arc<KitsuneSpace>,
        agent: Arc<KitsuneAgent>,
        maybe_agent_info: Option<AgentInfoSigned>,
        initial_arq: Option<Arq>,
    ) -> KitsuneP2pHandlerResult<()> {
        let internal_sender = self.internal_sender.clone();
        let space2 = space.clone();
        let ep_hnd = self.ep_hnd.clone();
        let host = self.host_api.clone().api;
        let config = Arc::clone(&self.config);
        let bootstrap_net = self.bootstrap_net;
        let bandwidth_throttles = self.bandwidth_throttles.clone();
        let parallel_notify_permit = self.parallel_notify_permit.clone();
        let fetch_pool = self.fetch_pool.clone();
        let local_url = self.local_url.clone();

        let space_sender = match self.spaces.entry(space.clone()) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(AsyncLazy::new(async move {
                let (send, send_inner, evt_recv) = spawn_space(
                    space2,
                    ep_hnd,
                    host,
                    config,
                    bootstrap_net,
                    bandwidth_throttles,
                    parallel_notify_permit,
                    fetch_pool,
                    local_url,
                )
                .await
                .expect("cannot fail to create space");
                internal_sender
                    .register_space_event_handler(evt_recv)
                    .await
                    .expect("FAIL");
                (send, send_inner)
            })),
        };
        let space_sender = space_sender.get();
        Ok(async move {
            let (space_sender, _) = space_sender.await;
            space_sender
                .join(space, agent, maybe_agent_info, initial_arq)
                .await
        }
        .boxed()
        .into())
    }

    fn handle_leave(
        &mut self,
        space: Arc<KitsuneSpace>,
        agent: Arc<KitsuneAgent>,
    ) -> KitsuneP2pHandlerResult<()> {
        let space_sender = match self.spaces.get_mut(&space) {
            None => return unit_ok_fut(),
            Some(space) => space.get(),
        };
        Ok(async move {
            let (space_sender, _) = space_sender.await;
            space_sender.leave(space.clone(), agent).await?;
            Ok(())
        }
        .boxed()
        .into())
    }

    fn handle_rpc_single(
        &mut self,
        space: Arc<KitsuneSpace>,
        to_agent: Arc<KitsuneAgent>,
        payload: Vec<u8>,
        timeout_ms: Option<u64>,
    ) -> KitsuneP2pHandlerResult<Vec<u8>> {
        let space_sender = match self.spaces.get_mut(&space) {
            None => return Err(KitsuneP2pError::RoutingSpaceError(space)),
            Some(space) => space.get(),
        };
        Ok(async move {
            let (space_sender, _) = space_sender.await;
            space_sender
                .rpc_single(space, to_agent, payload, timeout_ms)
                .await
        }
        .boxed()
        .into())
    }

    #[cfg_attr(feature = "instrument", tracing::instrument(skip(self, input)))]
    fn handle_rpc_multi(
        &mut self,
        input: actor::RpcMulti,
    ) -> KitsuneP2pHandlerResult<Vec<actor::RpcMultiResponse>> {
        let space_sender = match self.spaces.get_mut(&input.space) {
            None => return Err(KitsuneP2pError::RoutingSpaceError(input.space)),
            Some(space) => space.get(),
        };
        Ok(async move {
            let (space_sender, _) = space_sender.await;
            space_sender.rpc_multi(input).await
        }
        .boxed()
        .into())
    }

    fn handle_broadcast(
        &mut self,
        space: Arc<KitsuneSpace>,
        basis: Arc<KitsuneBasis>,
        timeout: KitsuneTimeout,
        data: BroadcastData,
    ) -> KitsuneP2pHandlerResult<()> {
        let space_sender = match self.spaces.get_mut(&space) {
            None => return Err(KitsuneP2pError::RoutingSpaceError(space)),
            Some(space) => space.get(),
        };
        Ok(async move {
            let (space_sender, _) = space_sender.await;
            space_sender.broadcast(space, basis, timeout, data).await
        }
        .boxed()
        .into())
    }

    fn handle_targeted_broadcast(
        &mut self,
        space: Arc<KitsuneSpace>,
        agents: Vec<Arc<KitsuneAgent>>,
        timeout: KitsuneTimeout,
        payload: Vec<u8>,
        drop_at_limit: bool,
    ) -> KitsuneP2pHandlerResult<()> {
        let space_sender = match self.spaces.get_mut(&space) {
            None => return Err(KitsuneP2pError::RoutingSpaceError(space)),
            Some(space) => space.get(),
        };
        Ok(async move {
            let (space_sender, _) = space_sender.await;
            space_sender
                .targeted_broadcast(space, agents, timeout, payload, drop_at_limit)
                .await
        }
        .boxed()
        .into())
    }

    fn handle_new_integrated_data(
        &mut self,
        space: Arc<KitsuneSpace>,
    ) -> KitsuneP2pHandlerResult<()> {
        let space_sender = match self.spaces.get_mut(&space) {
            None => return unit_ok_fut(),
            Some(space) => space.get(),
        };
        Ok(async move {
            let (space_sender, _) = space_sender.await;
            space_sender.new_integrated_data(space).await?;
            Ok(())
        }
        .boxed()
        .into())
    }

    fn handle_authority_for_hash(
        &mut self,
        space: Arc<KitsuneSpace>,
        basis: Arc<KitsuneBasis>,
    ) -> KitsuneP2pHandlerResult<bool> {
        let space_sender = match self.spaces.get_mut(&space) {
            None => return Err(KitsuneP2pError::RoutingSpaceError(space)),
            Some(space) => space.get(),
        };
        Ok(async move {
            let (space_sender, _) = space_sender.await;
            space_sender.authority_for_hash(space, basis).await
        }
        .boxed()
        .into())
    }

    fn handle_dump_network_metrics(
        &mut self,
        space: Option<Arc<KitsuneSpace>>,
    ) -> KitsuneP2pHandlerResult<serde_json::Value> {
        let spaces = self
            .spaces
            .iter()
            .filter_map(|(h, s)| {
                if let Some(space) = &space {
                    if h != space {
                        return None;
                    }
                }
                let h = h.clone();
                Some((h, s.get()))
            })
            .collect::<Vec<_>>();
        let results = async move {
            let mut all: Vec<KitsuneP2pFuture<serde_json::Value>> = Vec::new();
            for (h, (space, _)) in futures::future::join_all(
                spaces.into_iter().map(|(h, s)| async move { (h, s.await) }),
            )
            .await
            {
                all.push(space.dump_network_metrics(Some(h)));
            }
            Ok(futures::future::try_join_all(all).await?.into())
        }
        .boxed()
        .into();
        Ok(results)
    }

    fn handle_dump_network_stats(&mut self) -> KitsuneP2pHandlerResult<serde_json::Value> {
        let peer_fut_list = self
            .spaces
            .keys()
            .map(|space| {
                self.host_api
                    .legacy
                    .query_agents(QueryAgentsEvt::new(space.clone()))
            })
            .collect::<Vec<_>>();
        let stat_fut = self.ep_hnd.dump_network_stats();
        Ok(async move {
            let mut stats = stat_fut.await?;

            let this_id: String = stats
                .as_object()
                .and_then(|obj| obj.get("thisId"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(String::new);

            let all_peers = futures::future::join_all(peer_fut_list).await;

            #[derive(serde::Serialize)]
            #[serde(rename_all = "camelCase")]
            struct Agent {
                pub expires_at_millis: u64,
            }

            for peer in all_peers {
                for peer in peer? {
                    if let Some(net_key) = peer
                        .url_list
                        .first()
                        .map(|u| {
                            KitsuneResult::Ok(
                                kitsune_p2p_types::tx_utils::ProxyUrl::from(u.as_url2())
                                    .digest()?
                                    .to_string(),
                            )
                        })
                        .transpose()?
                    {
                        if net_key == this_id {
                            continue;
                        }

                        let r = stats
                            .as_object_mut()
                            .ok_or(KitsuneP2pError::from("InvalidStats"))?
                            .entry(net_key)
                            .or_insert_with(|| serde_json::json!({}));

                        let r = r
                            .as_object_mut()
                            .ok_or(KitsuneP2pError::from("InvalidStats"))?
                            .entry("hcDnaHashesToAgents".to_string())
                            .or_insert_with(|| serde_json::json!({}));

                        use base64::Engine;

                        let dna_hash = format!(
                            "uhC0k{}",
                            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&**peer.space)
                        );

                        let r = r
                            .as_object_mut()
                            .ok_or(KitsuneP2pError::from("InvalidStats"))?
                            .entry(dna_hash)
                            .or_insert_with(|| serde_json::json!({}));

                        let agent_pub_key = format!(
                            "uhCAk{}",
                            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&**peer.agent)
                        );

                        let agent = Agent {
                            expires_at_millis: peer.expires_at_ms,
                        };

                        r.as_object_mut()
                            .ok_or(KitsuneP2pError::from("InvalidStats"))?
                            .insert(agent_pub_key, serde_json::json!(agent));
                    }
                }
            }

            Ok(stats)
        }
        .boxed()
        .into())
    }

    fn handle_get_diagnostics(
        &mut self,
        space: KSpace,
        // gossip_type: GossipModuleType,
    ) -> KitsuneP2pHandlerResult<KitsuneDiagnostics> {
        let space_sender = match self.spaces.get_mut(&space) {
            None => return Err(KitsuneP2pError::RoutingSpaceError(space)),
            Some(space) => space.get(),
        };
        Ok(async move {
            let (space_sender, _) = space_sender.await;
            space_sender.get_diagnostics(space).await
        }
        .boxed()
        .into())
    }
}

#[cfg(any(test, feature = "test_utils"))]
mockall::mock! {

    pub KitsuneP2pEventHandler {}

    impl KitsuneP2pEventHandler for KitsuneP2pEventHandler {

        fn handle_put_agent_info_signed(
            &mut self,
            input: crate::event::PutAgentInfoSignedEvt,
        ) -> KitsuneP2pEventHandlerResult<Vec<AgentInfoPut>>;

        fn handle_query_agents(
            &mut self,
            input: crate::event::QueryAgentsEvt,
        ) -> KitsuneP2pEventHandlerResult<Vec<crate::types::agent_store::AgentInfoSigned>>;

        fn handle_query_peer_density(
            &mut self,
            space: Arc<KitsuneSpace>,
            arq: kitsune_p2p_types::dht_arc::DhtArc,
        ) -> KitsuneP2pEventHandlerResult<kitsune_p2p_types::dht::PeerView>;

        fn handle_call(
            &mut self,
            space: Arc<KitsuneSpace>,
            to_agent: Arc<KitsuneAgent>,
            payload: Vec<u8>,
        ) -> KitsuneP2pEventHandlerResult<Vec<u8>>;

        fn handle_notify(
            &mut self,
            space: Arc<KitsuneSpace>,
            to_agent: Arc<KitsuneAgent>,
            payload: Vec<u8>,
        ) -> KitsuneP2pEventHandlerResult<()> ;

        fn handle_receive_ops(
            &mut self,
            space: Arc<KitsuneSpace>,
            ops: Vec<KOp>,
            context: Option<FetchContext>,
        ) -> KitsuneP2pEventHandlerResult<()>;

        fn handle_query_op_hashes(
            &mut self,
            input: QueryOpHashesEvt,
        ) -> KitsuneP2pEventHandlerResult<Option<(Vec<Arc<KitsuneOpHash>>, TimeWindowInclusive)>>;

        fn handle_fetch_op_data(
            &mut self,
            input: FetchOpDataEvt,
        ) -> KitsuneP2pEventHandlerResult<Vec<(Arc<KitsuneOpHash>, KOp)>> ;

        fn handle_sign_network_data(
            &mut self,
            input: SignNetworkDataEvt,
        ) -> KitsuneP2pEventHandlerResult<KitsuneSignature> ;

    }
}

#[cfg(any(test, feature = "test_utils"))]
impl ghost_actor::GhostHandler<KitsuneP2pEvent> for MockKitsuneP2pEventHandler {}
#[cfg(any(test, feature = "test_utils"))]
impl ghost_actor::GhostControlHandler for MockKitsuneP2pEventHandler {}

#[cfg(test)]
mod tests {
    use crate::meta_net::PreflightUserData;
    use crate::spawn::actor::create_meta_net;
    use crate::spawn::actor::test_util::InternalStub;
    use crate::spawn::actor::MetaNet;
    use crate::spawn::actor::MetaNetEvtRecv;
    use crate::spawn::Internal;
    use crate::test_util::start_signal_srv;
    use crate::HostStub;
    use crate::KitsuneP2pResult;
    use ghost_actor::actor_builder::GhostActorBuilder;
    use kitsune_p2p_bootstrap_client::BootstrapNet;
    use kitsune_p2p_types::config::KitsuneP2pConfig;
    use kitsune_p2p_types::tls::TlsConfig;
    use url2::url2;

    #[tokio::test(flavor = "multi_thread")]
    async fn create_tx5_with_mdns_meta_net() {
        let (signal_addr, _sig_hnd) = start_signal_srv().await;

        let config = KitsuneP2pConfig::from_signal_addr(signal_addr);

        let (meta_net, _, bootstrap_net) = test_create_meta_net(config).await.unwrap();

        // Not the most interesting check but we mostly care that the above function produces a result given a valid config.
        assert_eq!(BootstrapNet::Tx5, bootstrap_net);

        meta_net.close(0, "test").await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_tx5_with_bootstrap_meta_net() {
        let (signal_addr, _sig_hnd) = start_signal_srv().await;

        let mut config = KitsuneP2pConfig::from_signal_addr(signal_addr);
        config.bootstrap_service = Some(url2!("ws://not-a-bootstrap.test"));

        let (meta_net, _, bootstrap_net) = test_create_meta_net(config).await.unwrap();

        // Not the most interesting check but we mostly care that the above function produces a result given a valid config.
        assert_eq!(BootstrapNet::Tx5, bootstrap_net);

        meta_net.close(0, "test").await;
    }

    async fn test_create_meta_net(
        config: KitsuneP2pConfig,
    ) -> KitsuneP2pResult<(MetaNet, MetaNetEvtRecv, BootstrapNet)> {
        let builder = GhostActorBuilder::new();

        let internal_sender = builder
            .channel_factory()
            .create_channel::<Internal>()
            .await
            .unwrap();

        tokio::spawn(builder.spawn(InternalStub::new()));

        let (sender, _) = futures::channel::mpsc::channel(10);

        create_meta_net(
            &config,
            TlsConfig::new_ephemeral().await.unwrap(),
            internal_sender,
            HostStub::new().legacy(sender),
            PreflightUserData::default(),
        )
        .await
        .map(|(n, r, b, _)| (n, r, b))
    }
}
