#![allow(clippy::too_many_arguments)]
//! Module containing incoming events from the HolochainP2p actor.

use crate::*;
use holochain_zome_types::signature::Signature;
use kitsune_p2p::{agent_store::AgentInfoSigned, dht::region::RegionBounds, event::*};

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
/// The data required for a get request.
#[derive(Default)]
pub enum GetRequest {
    /// Get all the integrated data.
    #[default]
    All,
    /// Get only the integrated content.
    Content,
    /// Get only the metadata.
    /// If you already have the content this is all you need.
    Metadata,
    /// Get the content even if it's still pending.
    Pending,
}

/// Get options help control how the get is processed at various levels.
#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct GetOptions {
    /// Whether the remote-end should follow redirects or just return the
    /// requested entry.
    pub follow_redirects: bool,
    /// Return all live actions even if there is deletes.
    /// Useful for metadata calls.
    pub all_live_actions_with_metadata: bool,
    /// The type of data this get request requires.
    pub request_type: GetRequest,
}

impl From<&actor::GetOptions> for GetOptions {
    fn from(a: &actor::GetOptions) -> Self {
        Self {
            follow_redirects: a.follow_redirects,
            all_live_actions_with_metadata: a.all_live_actions_with_metadata,
            request_type: a.request_type.clone(),
        }
    }
}

/// GetMeta options help control how the get is processed at various levels.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct GetMetaOptions {}

impl From<&actor::GetMetaOptions> for GetMetaOptions {
    fn from(_a: &actor::GetMetaOptions) -> Self {
        Self {}
    }
}

/// GetLinks options help control how the get is processed at various levels.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct GetLinksOptions {}

impl From<&actor::GetLinksOptions> for GetLinksOptions {
    fn from(_a: &actor::GetLinksOptions) -> Self {
        Self {}
    }
}

/// Get agent activity options help control how the get is processed at various levels.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GetActivityOptions {
    /// Include the activity actions in the response
    pub include_valid_activity: bool,
    /// Include any rejected actions in the response.
    pub include_rejected_activity: bool,
    /// Include warrants in the response.
    pub include_warrants: bool,
    /// Include the full records, instead of just the hashes.
    pub include_full_records: bool,
}

impl Default for GetActivityOptions {
    fn default() -> Self {
        Self {
            include_valid_activity: true,
            include_warrants: true,
            include_rejected_activity: false,
            include_full_records: false,
        }
    }
}

impl From<&actor::GetActivityOptions> for GetActivityOptions {
    fn from(a: &actor::GetActivityOptions) -> Self {
        Self {
            include_valid_activity: a.include_valid_activity,
            include_warrants: a.include_warrants,
            include_rejected_activity: a.include_rejected_activity,
            include_full_records: a.include_full_records,
        }
    }
}

/// Message between agents actively driving/negotiating a countersigning session.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub enum CountersigningSessionNegotiationMessage {
    /// An authority has a complete set of signed actions and is responding with
    /// them back to the counterparties.
    AuthorityResponse(Vec<SignedAction>),
    /// Counterparties are sending their signed action to an enzyme instead of
    /// authorities as part of an enzymatic session.
    EnzymePush(Box<ChainOp>),
}

/// Multiple ways to fetch op data
#[derive(Debug, derive_more::From)]
pub enum FetchOpDataQuery {
    /// Fetch all ops with the hashes specified
    Hashes {
        /// list of ops to fetch
        op_hash_list: Vec<holo_hash::DhtOpHash>,

        /// should we include limbo ops
        include_limbo: bool,
    },

    /// Fetch all ops within the time and space bounds specified
    Regions(Vec<RegionBounds>),
}

impl FetchOpDataQuery {
    /// Convert from the kitsune form of this query
    pub fn from_kitsune(kit: FetchOpDataEvtQuery) -> Self {
        match kit {
            FetchOpDataEvtQuery::Hashes {
                op_hash_list,
                include_limbo,
            } => Self::Hashes {
                op_hash_list: op_hash_list
                    .into_iter()
                    .map(|h| DhtOpHash::from_kitsune(&h))
                    .collect::<Vec<_>>(),
                include_limbo,
            },
            FetchOpDataEvtQuery::Regions(coords) => Self::Regions(coords),
        }
    }
}

ghost_actor::ghost_chan! {
    /// The HolochainP2pEvent stream allows handling events generated from
    /// the HolochainP2p actor.
    pub chan HolochainP2pEvent<super::HolochainP2pError> {

        /// We need to store signed agent info.
        fn put_agent_info_signed(dna_hash: DnaHash, peer_data: Vec<AgentInfoSigned>) -> Vec<kitsune_p2p_types::bootstrap::AgentInfoPut>;

        /// We need to get previously stored agent info.
        /// The optional `agents` parameter is an include filter. This can be thought of as a way to filter a held list of agents against the current state of the store.
        fn query_agent_info_signed(dna_hash: DnaHash, agents: Option<std::collections::HashSet<Arc<kitsune_p2p::KitsuneAgent>>>, kitsune_space: Arc<kitsune_p2p::KitsuneSpace>) -> Vec<AgentInfoSigned>;

        /// We need to get agents that fit into an arc set for gossip.
        fn query_gossip_agents(
            dna_hash: DnaHash,
            agents: Option<Vec<AgentPubKey>>,
            kitsune_space: Arc<kitsune_p2p::KitsuneSpace>,
            since_ms: u64,
            until_ms: u64,
            arc_set: Arc<kitsune_p2p_types::dht_arc::DhtArcSet>,
        ) -> Vec<AgentInfoSigned>;

        /// query agent info in order of closeness to a basis location.
        fn query_agent_info_signed_near_basis(dna_hash: DnaHash, kitsune_space: Arc<kitsune_p2p::KitsuneSpace>, basis_loc: u32, limit: u32) -> Vec<AgentInfoSigned>;

        /// Query the peer density of a space for a given [`DhtArc`].
        fn query_peer_density(dna_hash: DnaHash, kitsune_space: Arc<kitsune_p2p::KitsuneSpace>, dht_arc: kitsune_p2p_types::dht_arc::DhtArc) -> kitsune_p2p_types::dht::PeerView;

        /// A remote node is attempting to make a remote call on us.
        fn call_remote(
            dna_hash: DnaHash,
            to_agent: AgentPubKey,
            zome_call_params_serialized: ExternIO,
            signature: Signature,
        ) -> SerializedBytes;

        /// A remote node is publishing data in a range we claim to be holding.
        fn publish(
            dna_hash: DnaHash,
            request_validation_receipt: bool,
            countersigning_session: bool,
            ops: Vec<holochain_types::dht_op::DhtOp>,
        ) -> ();

        /// A remote node is requesting entry data from us.
        fn get(
            dna_hash: DnaHash,
            to_agent: AgentPubKey,
            dht_hash: holo_hash::AnyDhtHash,
            options: GetOptions,
        ) -> WireOps;

        /// A remote node is requesting metadata from us.
        fn get_meta(
            dna_hash: DnaHash,
            to_agent: AgentPubKey,
            dht_hash: holo_hash::AnyDhtHash,
            options: GetMetaOptions,
        ) -> MetadataSet;

        /// A remote node is requesting link data from us.
        fn get_links(
            dna_hash: DnaHash,
            to_agent: AgentPubKey,
            link_key: WireLinkKey,
            options: GetLinksOptions,
        ) -> WireLinkOps;

        /// A remote node is requesting a link count from us.
        fn count_links(
            dna_hash: DnaHash,
            to_agent: AgentPubKey,
            query: WireLinkQuery,
        ) -> CountLinksResponse;

        /// A remote node is requesting agent activity from us.
        fn get_agent_activity(
            dna_hash: DnaHash,
            to_agent: AgentPubKey,
            agent: AgentPubKey,
            query: ChainQueryFilter,
            options: GetActivityOptions,
        ) -> AgentActivityResponse;

        /// A remote node is requesting agent activity from us.
        fn must_get_agent_activity(
            dna_hash: DnaHash,
            to_agent: AgentPubKey,
            author: AgentPubKey,
            filter: holochain_zome_types::chain::ChainFilter,
        ) -> MustGetAgentActivityResponse;

        /// A remote node has sent us a validation receipt.
        fn validation_receipts_received(
            dna_hash: DnaHash,
            to_agent: AgentPubKey,
            receipts: ValidationReceiptBundle,
        ) -> ();

        /// The p2p module wishes to query our DhtOpHash store.
        /// Gets all ops from a set of agents within a time window
        /// and max number of ops.
        /// Returns the actual time window of returned ops as well.
        fn query_op_hashes(
            dna_hash: DnaHash,
            arc_set: kitsune_p2p::dht_arc::DhtArcSet,
            window: TimeWindow,
            max_ops: usize,
            include_limbo: bool,
        ) -> Option<(Vec<holo_hash::DhtOpHash>, TimeWindowInclusive)>;

        /// The p2p module needs access to the content for a given set of DhtOpHashes.
        fn fetch_op_data(
            dna_hash: DnaHash,
            query: FetchOpDataQuery,
        ) -> Vec<(holo_hash::DhtOpHash, holochain_types::dht_op::DhtOp)>;

        /// P2p operations require cryptographic signatures and validation.
        fn sign_network_data(
            // The dna_hash / space_hash context.
            dna_hash: DnaHash,
            // The agent_id / agent_pub_key context.
            to_agent: AgentPubKey,
            // The data to sign.
            data: Vec<u8>,
        ) -> Signature;

        /// Messages between agents that drive a countersigning session.
        fn countersigning_session_negotiation(
            dna_hash: DnaHash,
            to_agent: AgentPubKey,
            message: CountersigningSessionNegotiationMessage,
        ) -> ();
    }
}

/// utility macro to make it more ergonomic to access the enum variants
macro_rules! match_p2p_evt {
    ($h:ident => |$i:ident| { $($t:tt)* }, { $($t2:tt)* }) => {
        match $h {
            HolochainP2pEvent::CallRemote { $i, .. } => { $($t)* }
            HolochainP2pEvent::Get { $i, .. } => { $($t)* }
            HolochainP2pEvent::GetMeta { $i, .. } => { $($t)* }
            HolochainP2pEvent::GetLinks { $i, .. } => { $($t)* }
            HolochainP2pEvent::CountLinks { $i, .. } => { $($t)* }
            HolochainP2pEvent::GetAgentActivity { $i, .. } => { $($t)* }
            HolochainP2pEvent::MustGetAgentActivity { $i, .. } => { $($t)* }
            HolochainP2pEvent::ValidationReceiptsReceived { $i, .. } => { $($t)* }
            HolochainP2pEvent::SignNetworkData { $i, .. } => { $($t)* }
            HolochainP2pEvent::CountersigningSessionNegotiation { $i, .. } => { $($t)* }
            $($t2)*
        }
    };
}

impl HolochainP2pEvent {
    /// The dna_hash associated with this network p2p event.
    pub fn dna_hash(&self) -> &DnaHash {
        match_p2p_evt!(self => |dna_hash| { dna_hash }, {
            HolochainP2pEvent::Publish { dna_hash, .. } => { dna_hash }
            HolochainP2pEvent::FetchOpData { dna_hash, .. } => { dna_hash }
            HolochainP2pEvent::QueryOpHashes { dna_hash, .. } => { dna_hash }
            HolochainP2pEvent::QueryAgentInfoSigned { dna_hash, .. } => { dna_hash }
            HolochainP2pEvent::QueryAgentInfoSignedNearBasis { dna_hash, .. } => { dna_hash }
            HolochainP2pEvent::QueryGossipAgents { dna_hash, .. } => { dna_hash }
            HolochainP2pEvent::PutAgentInfoSigned { dna_hash, .. } => { dna_hash }
            HolochainP2pEvent::QueryPeerDensity { dna_hash, .. } => { dna_hash }
        })
    }

    /// The agent_pub_key associated with this network p2p event.
    pub fn target_agents(&self) -> &AgentPubKey {
        match_p2p_evt!(self => |to_agent| { to_agent }, {
            HolochainP2pEvent::Publish { .. } => { unimplemented!("There is no single agent target for Publish") }
            HolochainP2pEvent::FetchOpData { .. } => { unimplemented!("There is no single agent target for FetchOpData") }
            HolochainP2pEvent::QueryOpHashes { .. } => { unimplemented!("There is no single agent target for QueryOpHashes") }
            HolochainP2pEvent::QueryAgentInfoSigned { .. } => { unimplemented!("There is no single agent target for QueryAgentInfoSigned") },
            HolochainP2pEvent::QueryAgentInfoSignedNearBasis { .. } => { unimplemented!("There is no single agent target for QueryAgentInfoSignedNearBasis") },
            HolochainP2pEvent::QueryGossipAgents { .. } => { unimplemented!("There is no single agent target for QueryGossipAgents") },
            HolochainP2pEvent::PutAgentInfoSigned { .. } => { unimplemented!("There is no single agent target for PutAgentInfoSigned") },
            HolochainP2pEvent::QueryPeerDensity { .. } => { unimplemented!() },
        })
    }
}

/// Receiver type for incoming holochain p2p events.
pub type HolochainP2pEventReceiver = futures::channel::mpsc::Receiver<HolochainP2pEvent>;
