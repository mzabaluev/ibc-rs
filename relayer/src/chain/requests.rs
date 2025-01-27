use core::fmt::Display;

use crate::error::Error;

use ibc::core::ics04_channel::packet::Sequence;
use ibc::core::ics24_host::identifier::{ChannelId, ClientId, ConnectionId, PortId};
use ibc::Height;
use ibc_proto::cosmos::base::query::v1beta1::PageRequest as RawPageRequest;
use ibc_proto::ibc::core::channel::v1::{
    QueryChannelClientStateRequest as RawQueryChannelClientStateRequest,
    QueryChannelsRequest as RawQueryChannelsRequest,
    QueryConnectionChannelsRequest as RawQueryConnectionChannelsRequest,
    QueryNextSequenceReceiveRequest as RawQueryNextSequenceReceiveRequest,
    QueryPacketAcknowledgementsRequest as RawQueryPacketAcknowledgementsRequest,
    QueryPacketCommitmentsRequest as RawQueryPacketCommitmentsRequest,
    QueryUnreceivedAcksRequest as RawQueryUnreceivedAcksRequest,
    QueryUnreceivedPacketsRequest as RawQueryUnreceivedPacketsRequest,
};
use ibc_proto::ibc::core::client::v1::{
    QueryClientStatesRequest as RawQueryClientStatesRequest,
    QueryConsensusStatesRequest as RawQueryConsensusStatesRequest,
};
use ibc_proto::ibc::core::connection::v1::{
    QueryClientConnectionsRequest as RawQueryClientConnectionsRequest,
    QueryConnectionsRequest as RawQueryConnectionsRequest,
};

use serde::{Deserialize, Serialize};
use tendermint::block::Height as TMBlockHeight;
use tonic::metadata::AsciiMetadataValue;

/// Type to specify a height in a query. Specifically, this caters to the use
/// case where the user wants to query at whatever the latest height is, as
/// opposed to specifying a specific height.
#[derive(Copy, Clone, Debug, Serialize, Deserialize)]
pub enum HeightQuery {
    Latest,
    Specific(Height),
}

impl TryFrom<HeightQuery> for TMBlockHeight {
    type Error = Error;

    fn try_from(height_query: HeightQuery) -> Result<Self, Self::Error> {
        let height = match height_query {
            HeightQuery::Latest => 0u64,
            HeightQuery::Specific(height) => height.revision_height,
        };

        Self::try_from(height).map_err(Error::invalid_height)
    }
}

impl TryFrom<HeightQuery> for AsciiMetadataValue {
    type Error = Error;

    fn try_from(height_query: HeightQuery) -> Result<Self, Self::Error> {
        let height = match height_query {
            HeightQuery::Latest => 0u64,
            HeightQuery::Specific(height) => height.revision_height,
        };

        str::parse(&height.to_string()).map_err(Error::invalid_metadata)
    }
}

impl Display for HeightQuery {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            HeightQuery::Latest => write!(f, "latest height"),
            HeightQuery::Specific(height) => write!(f, "{}", height),
        }
    }
}

/// Defines a type to be used in select requests to specify whether or not a proof should be
/// returned along with the response.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum IncludeProof {
    Yes,
    No,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PageRequest {
    /// key is a value returned in PageResponse.next_key to begin
    /// querying the next page most efficiently. Only one of offset or key
    /// should be set.
    pub key: ::prost::alloc::vec::Vec<u8>,
    /// offset is a numeric offset that can be used when key is unavailable.
    /// It is less efficient than using key. Only one of offset or key should
    /// be set.
    pub offset: u64,
    /// limit is the total number of results to be returned in the result page.
    /// If left empty it will default to a value to be set by each app.
    pub limit: u64,
    /// count_total is set to true  to indicate that the result set should include
    /// a count of the total number of items available for pagination in UIs.
    /// count_total is only respected when offset is used. It is ignored when key
    /// is set.
    pub count_total: bool,
    /// reverse is set to true if results are to be returned in the descending order.
    pub reverse: bool,
}

impl PageRequest {
    pub fn all() -> PageRequest {
        PageRequest {
            limit: u64::MAX,
            ..Default::default()
        }
    }
}

impl From<PageRequest> for RawPageRequest {
    fn from(request: PageRequest) -> Self {
        RawPageRequest {
            key: request.key,
            offset: request.offset,
            limit: request.limit,
            count_total: request.count_total,
            reverse: request.reverse,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueryClientStateRequest {
    pub client_id: ClientId,
    pub height: HeightQuery,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueryClientStatesRequest {
    pub pagination: Option<PageRequest>,
}

impl From<QueryClientStatesRequest> for RawQueryClientStatesRequest {
    fn from(request: QueryClientStatesRequest) -> Self {
        RawQueryClientStatesRequest {
            pagination: request.pagination.map(|pagination| pagination.into()),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueryConsensusStateRequest {
    pub client_id: ClientId,
    pub consensus_height: Height,
    pub query_height: HeightQuery,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueryUpgradedClientStateRequest {
    /// Height at which the chain is scheduled to halt for upgrade
    pub upgrade_height: Height,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueryUpgradedConsensusStateRequest {
    /// Height at which the chain is scheduled to halt for upgrade
    pub upgrade_height: Height,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueryConsensusStatesRequest {
    pub client_id: ClientId,
    pub pagination: Option<PageRequest>,
}

impl From<QueryConsensusStatesRequest> for RawQueryConsensusStatesRequest {
    fn from(request: QueryConsensusStatesRequest) -> Self {
        RawQueryConsensusStatesRequest {
            client_id: request.client_id.to_string(),
            pagination: request.pagination.map(|pagination| pagination.into()),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueryConnectionsRequest {
    pub pagination: Option<PageRequest>,
}

impl From<QueryConnectionsRequest> for RawQueryConnectionsRequest {
    fn from(request: QueryConnectionsRequest) -> Self {
        RawQueryConnectionsRequest {
            pagination: request.pagination.map(|pagination| pagination.into()),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueryClientConnectionsRequest {
    pub client_id: ClientId,
}

impl From<QueryClientConnectionsRequest> for RawQueryClientConnectionsRequest {
    fn from(request: QueryClientConnectionsRequest) -> Self {
        RawQueryClientConnectionsRequest {
            client_id: request.client_id.to_string(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueryConnectionRequest {
    pub connection_id: ConnectionId,
    pub height: HeightQuery,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueryConnectionChannelsRequest {
    pub connection_id: ConnectionId,
    pub pagination: Option<PageRequest>,
}

impl From<QueryConnectionChannelsRequest> for RawQueryConnectionChannelsRequest {
    fn from(request: QueryConnectionChannelsRequest) -> Self {
        RawQueryConnectionChannelsRequest {
            connection: request.connection_id.to_string(),
            pagination: request.pagination.map(|pagination| pagination.into()),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueryChannelsRequest {
    pub pagination: Option<PageRequest>,
}

impl From<QueryChannelsRequest> for RawQueryChannelsRequest {
    fn from(request: QueryChannelsRequest) -> Self {
        RawQueryChannelsRequest {
            pagination: request.pagination.map(|pagination| pagination.into()),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueryChannelRequest {
    pub port_id: PortId,
    pub channel_id: ChannelId,
    pub height: HeightQuery,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueryChannelClientStateRequest {
    pub port_id: PortId,
    pub channel_id: ChannelId,
}

impl From<QueryChannelClientStateRequest> for RawQueryChannelClientStateRequest {
    fn from(request: QueryChannelClientStateRequest) -> Self {
        RawQueryChannelClientStateRequest {
            port_id: request.port_id.to_string(),
            channel_id: request.channel_id.to_string(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueryPacketCommitmentRequest {
    pub port_id: PortId,
    pub channel_id: ChannelId,
    pub sequence: Sequence,
    pub height: HeightQuery,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueryPacketCommitmentsRequest {
    pub port_id: PortId,
    pub channel_id: ChannelId,
    pub pagination: Option<PageRequest>,
}

impl From<QueryPacketCommitmentsRequest> for RawQueryPacketCommitmentsRequest {
    fn from(request: QueryPacketCommitmentsRequest) -> Self {
        RawQueryPacketCommitmentsRequest {
            port_id: request.port_id.to_string(),
            channel_id: request.channel_id.to_string(),
            pagination: request.pagination.map(|pagination| pagination.into()),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueryPacketReceiptRequest {
    pub port_id: PortId,
    pub channel_id: ChannelId,
    pub sequence: Sequence,
    pub height: HeightQuery,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueryUnreceivedPacketsRequest {
    pub port_id: PortId,
    pub channel_id: ChannelId,
    pub packet_commitment_sequences: Vec<Sequence>,
}

impl From<QueryUnreceivedPacketsRequest> for RawQueryUnreceivedPacketsRequest {
    fn from(request: QueryUnreceivedPacketsRequest) -> Self {
        RawQueryUnreceivedPacketsRequest {
            port_id: request.port_id.to_string(),
            channel_id: request.channel_id.to_string(),
            packet_commitment_sequences: request
                .packet_commitment_sequences
                .into_iter()
                .map(|seq| seq.into())
                .collect(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueryPacketAcknowledgementRequest {
    pub port_id: PortId,
    pub channel_id: ChannelId,
    pub sequence: Sequence,
    pub height: HeightQuery,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueryPacketAcknowledgementsRequest {
    pub port_id: PortId,
    pub channel_id: ChannelId,
    pub pagination: Option<PageRequest>,
    pub packet_commitment_sequences: Vec<Sequence>,
}

impl From<QueryPacketAcknowledgementsRequest> for RawQueryPacketAcknowledgementsRequest {
    fn from(request: QueryPacketAcknowledgementsRequest) -> Self {
        RawQueryPacketAcknowledgementsRequest {
            port_id: request.port_id.to_string(),
            channel_id: request.channel_id.to_string(),
            pagination: request.pagination.map(|pagination| pagination.into()),
            packet_commitment_sequences: request
                .packet_commitment_sequences
                .into_iter()
                .map(|seq| seq.into())
                .collect(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueryUnreceivedAcksRequest {
    pub port_id: PortId,
    pub channel_id: ChannelId,
    pub packet_ack_sequences: Vec<Sequence>,
}

impl From<QueryUnreceivedAcksRequest> for RawQueryUnreceivedAcksRequest {
    fn from(request: QueryUnreceivedAcksRequest) -> Self {
        RawQueryUnreceivedAcksRequest {
            port_id: request.port_id.to_string(),
            channel_id: request.channel_id.to_string(),
            packet_ack_sequences: request
                .packet_ack_sequences
                .into_iter()
                .map(|seq| seq.into())
                .collect(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueryNextSequenceReceiveRequest {
    pub port_id: PortId,
    pub channel_id: ChannelId,
    pub height: HeightQuery,
}

impl From<QueryNextSequenceReceiveRequest> for RawQueryNextSequenceReceiveRequest {
    fn from(request: QueryNextSequenceReceiveRequest) -> Self {
        RawQueryNextSequenceReceiveRequest {
            port_id: request.port_id.to_string(),
            channel_id: request.channel_id.to_string(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueryHostConsensusStateRequest {
    pub height: HeightQuery,
}
