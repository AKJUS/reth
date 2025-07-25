//! Implements Ethereum wire protocol for versions 66, 67, and 68.
//! Defines structs/enums for messages, request-response pairs, and broadcasts.
//! Handles compatibility with [`EthVersion`].
//!
//! Examples include creating, encoding, and decoding protocol messages.
//!
//! Reference: [Ethereum Wire Protocol](https://github.com/ethereum/devp2p/blob/master/caps/eth.md).

use super::{
    broadcast::NewBlockHashes, BlockBodies, BlockHeaders, GetBlockBodies, GetBlockHeaders,
    GetNodeData, GetPooledTransactions, GetReceipts, NewPooledTransactionHashes66,
    NewPooledTransactionHashes68, NodeData, PooledTransactions, Receipts, Status, StatusEth69,
    Transactions,
};
use crate::{
    status::StatusMessage, BlockRangeUpdate, EthNetworkPrimitives, EthVersion, NetworkPrimitives,
    RawCapabilityMessage, Receipts69, SharedTransactions,
};
use alloc::{boxed::Box, string::String, sync::Arc};
use alloy_primitives::{
    bytes::{Buf, BufMut},
    Bytes,
};
use alloy_rlp::{length_of_length, Decodable, Encodable, Header};
use core::fmt::Debug;

/// [`MAX_MESSAGE_SIZE`] is the maximum cap on the size of a protocol message.
// https://github.com/ethereum/go-ethereum/blob/30602163d5d8321fbc68afdcbbaf2362b2641bde/eth/protocols/eth/protocol.go#L50
pub const MAX_MESSAGE_SIZE: usize = 10 * 1024 * 1024;

/// Error when sending/receiving a message
#[derive(thiserror::Error, Debug)]
pub enum MessageError {
    /// Flags an unrecognized message ID for a given protocol version.
    #[error("message id {1:?} is invalid for version {0:?}")]
    Invalid(EthVersion, EthMessageID),
    /// Thrown when rlp decoding a message failed.
    #[error("RLP error: {0}")]
    RlpError(#[from] alloy_rlp::Error),
    /// Other message error with custom message
    #[error("{0}")]
    Other(String),
}

/// An `eth` protocol message, containing a message ID and payload.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ProtocolMessage<N: NetworkPrimitives = EthNetworkPrimitives> {
    /// The unique identifier representing the type of the Ethereum message.
    pub message_type: EthMessageID,
    /// The content of the message, including specific data based on the message type.
    #[cfg_attr(
        feature = "serde",
        serde(bound = "EthMessage<N>: serde::Serialize + serde::de::DeserializeOwned")
    )]
    pub message: EthMessage<N>,
}

impl<N: NetworkPrimitives> ProtocolMessage<N> {
    /// Create a new `ProtocolMessage` from a message type and message rlp bytes.
    ///
    /// This will enforce decoding according to the given [`EthVersion`] of the connection.
    pub fn decode_message(version: EthVersion, buf: &mut &[u8]) -> Result<Self, MessageError> {
        let message_type = EthMessageID::decode(buf)?;

        // For EIP-7642 (https://github.com/ethereum/EIPs/blob/master/EIPS/eip-7642.md):
        // pre-merge (legacy) status messages include total difficulty, whereas eth/69 omits it.
        let message = match message_type {
            EthMessageID::Status => EthMessage::Status(if version < EthVersion::Eth69 {
                StatusMessage::Legacy(Status::decode(buf)?)
            } else {
                StatusMessage::Eth69(StatusEth69::decode(buf)?)
            }),
            EthMessageID::NewBlockHashes => {
                EthMessage::NewBlockHashes(NewBlockHashes::decode(buf)?)
            }
            EthMessageID::NewBlock => {
                EthMessage::NewBlock(Box::new(N::NewBlockPayload::decode(buf)?))
            }
            EthMessageID::Transactions => EthMessage::Transactions(Transactions::decode(buf)?),
            EthMessageID::NewPooledTransactionHashes => {
                if version >= EthVersion::Eth68 {
                    EthMessage::NewPooledTransactionHashes68(NewPooledTransactionHashes68::decode(
                        buf,
                    )?)
                } else {
                    EthMessage::NewPooledTransactionHashes66(NewPooledTransactionHashes66::decode(
                        buf,
                    )?)
                }
            }
            EthMessageID::GetBlockHeaders => EthMessage::GetBlockHeaders(RequestPair::decode(buf)?),
            EthMessageID::BlockHeaders => EthMessage::BlockHeaders(RequestPair::decode(buf)?),
            EthMessageID::GetBlockBodies => EthMessage::GetBlockBodies(RequestPair::decode(buf)?),
            EthMessageID::BlockBodies => EthMessage::BlockBodies(RequestPair::decode(buf)?),
            EthMessageID::GetPooledTransactions => {
                EthMessage::GetPooledTransactions(RequestPair::decode(buf)?)
            }
            EthMessageID::PooledTransactions => {
                EthMessage::PooledTransactions(RequestPair::decode(buf)?)
            }
            EthMessageID::GetNodeData => {
                if version >= EthVersion::Eth67 {
                    return Err(MessageError::Invalid(version, EthMessageID::GetNodeData))
                }
                EthMessage::GetNodeData(RequestPair::decode(buf)?)
            }
            EthMessageID::NodeData => {
                if version >= EthVersion::Eth67 {
                    return Err(MessageError::Invalid(version, EthMessageID::GetNodeData))
                }
                EthMessage::NodeData(RequestPair::decode(buf)?)
            }
            EthMessageID::GetReceipts => EthMessage::GetReceipts(RequestPair::decode(buf)?),
            EthMessageID::Receipts => {
                if version < EthVersion::Eth69 {
                    EthMessage::Receipts(RequestPair::decode(buf)?)
                } else {
                    // with eth69, receipts no longer include the bloom
                    EthMessage::Receipts69(RequestPair::decode(buf)?)
                }
            }
            EthMessageID::BlockRangeUpdate => {
                if version < EthVersion::Eth69 {
                    return Err(MessageError::Invalid(version, EthMessageID::BlockRangeUpdate))
                }
                EthMessage::BlockRangeUpdate(BlockRangeUpdate::decode(buf)?)
            }
            EthMessageID::Other(_) => {
                let raw_payload = Bytes::copy_from_slice(buf);
                buf.advance(raw_payload.len());
                EthMessage::Other(RawCapabilityMessage::new(
                    message_type.to_u8() as usize,
                    raw_payload.into(),
                ))
            }
        };
        Ok(Self { message_type, message })
    }
}

impl<N: NetworkPrimitives> Encodable for ProtocolMessage<N> {
    /// Encodes the protocol message into bytes. The message type is encoded as a single byte and
    /// prepended to the message.
    fn encode(&self, out: &mut dyn BufMut) {
        self.message_type.encode(out);
        self.message.encode(out);
    }
    fn length(&self) -> usize {
        self.message_type.length() + self.message.length()
    }
}

impl<N: NetworkPrimitives> From<EthMessage<N>> for ProtocolMessage<N> {
    fn from(message: EthMessage<N>) -> Self {
        Self { message_type: message.message_id(), message }
    }
}

/// Represents messages that can be sent to multiple peers.
#[derive(Clone, Debug)]
pub struct ProtocolBroadcastMessage<N: NetworkPrimitives = EthNetworkPrimitives> {
    /// The unique identifier representing the type of the Ethereum message.
    pub message_type: EthMessageID,
    /// The content of the message to be broadcasted, including specific data based on the message
    /// type.
    pub message: EthBroadcastMessage<N>,
}

impl<N: NetworkPrimitives> Encodable for ProtocolBroadcastMessage<N> {
    /// Encodes the protocol message into bytes. The message type is encoded as a single byte and
    /// prepended to the message.
    fn encode(&self, out: &mut dyn BufMut) {
        self.message_type.encode(out);
        self.message.encode(out);
    }
    fn length(&self) -> usize {
        self.message_type.length() + self.message.length()
    }
}

impl<N: NetworkPrimitives> From<EthBroadcastMessage<N>> for ProtocolBroadcastMessage<N> {
    fn from(message: EthBroadcastMessage<N>) -> Self {
        Self { message_type: message.message_id(), message }
    }
}

/// Represents a message in the eth wire protocol, versions 66, 67, 68 and 69.
///
/// The ethereum wire protocol is a set of messages that are broadcast to the network in two
/// styles:
///  * A request message sent by a peer (such as [`GetPooledTransactions`]), and an associated
///    response message (such as [`PooledTransactions`]).
///  * A message that is broadcast to the network, without a corresponding request.
///
/// The newer `eth/66` is an efficiency upgrade on top of `eth/65`, introducing a request id to
/// correlate request-response message pairs. This allows for request multiplexing.
///
/// The `eth/67` is based on `eth/66` but only removes two messages, [`GetNodeData`] and
/// [`NodeData`].
///
/// The `eth/68` changes only `NewPooledTransactionHashes` to include `types` and `sized`. For
/// it, `NewPooledTransactionHashes` is renamed as [`NewPooledTransactionHashes66`] and
/// [`NewPooledTransactionHashes68`] is defined.
///
/// The `eth/69` announces the historical block range served by the node. Removes total difficulty
/// information. And removes the Bloom field from receipts transferred over the protocol.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum EthMessage<N: NetworkPrimitives = EthNetworkPrimitives> {
    /// Represents a Status message required for the protocol handshake.
    Status(StatusMessage),
    /// Represents a `NewBlockHashes` message broadcast to the network.
    NewBlockHashes(NewBlockHashes),
    /// Represents a `NewBlock` message broadcast to the network.
    #[cfg_attr(
        feature = "serde",
        serde(bound = "N::NewBlockPayload: serde::Serialize + serde::de::DeserializeOwned")
    )]
    NewBlock(Box<N::NewBlockPayload>),
    /// Represents a Transactions message broadcast to the network.
    #[cfg_attr(
        feature = "serde",
        serde(bound = "N::BroadcastedTransaction: serde::Serialize + serde::de::DeserializeOwned")
    )]
    Transactions(Transactions<N::BroadcastedTransaction>),
    /// Represents a `NewPooledTransactionHashes` message for eth/66 version.
    NewPooledTransactionHashes66(NewPooledTransactionHashes66),
    /// Represents a `NewPooledTransactionHashes` message for eth/68 version.
    NewPooledTransactionHashes68(NewPooledTransactionHashes68),
    // The following messages are request-response message pairs
    /// Represents a `GetBlockHeaders` request-response pair.
    GetBlockHeaders(RequestPair<GetBlockHeaders>),
    /// Represents a `BlockHeaders` request-response pair.
    #[cfg_attr(
        feature = "serde",
        serde(bound = "N::BlockHeader: serde::Serialize + serde::de::DeserializeOwned")
    )]
    BlockHeaders(RequestPair<BlockHeaders<N::BlockHeader>>),
    /// Represents a `GetBlockBodies` request-response pair.
    GetBlockBodies(RequestPair<GetBlockBodies>),
    /// Represents a `BlockBodies` request-response pair.
    #[cfg_attr(
        feature = "serde",
        serde(bound = "N::BlockBody: serde::Serialize + serde::de::DeserializeOwned")
    )]
    BlockBodies(RequestPair<BlockBodies<N::BlockBody>>),
    /// Represents a `GetPooledTransactions` request-response pair.
    GetPooledTransactions(RequestPair<GetPooledTransactions>),
    /// Represents a `PooledTransactions` request-response pair.
    #[cfg_attr(
        feature = "serde",
        serde(bound = "N::PooledTransaction: serde::Serialize + serde::de::DeserializeOwned")
    )]
    PooledTransactions(RequestPair<PooledTransactions<N::PooledTransaction>>),
    /// Represents a `GetNodeData` request-response pair.
    GetNodeData(RequestPair<GetNodeData>),
    /// Represents a `NodeData` request-response pair.
    NodeData(RequestPair<NodeData>),
    /// Represents a `GetReceipts` request-response pair.
    GetReceipts(RequestPair<GetReceipts>),
    /// Represents a Receipts request-response pair.
    #[cfg_attr(
        feature = "serde",
        serde(bound = "N::Receipt: serde::Serialize + serde::de::DeserializeOwned")
    )]
    Receipts(RequestPair<Receipts<N::Receipt>>),
    /// Represents a Receipts request-response pair for eth/69.
    #[cfg_attr(
        feature = "serde",
        serde(bound = "N::Receipt: serde::Serialize + serde::de::DeserializeOwned")
    )]
    Receipts69(RequestPair<Receipts69<N::Receipt>>),
    /// Represents a `BlockRangeUpdate` message broadcast to the network.
    #[cfg_attr(
        feature = "serde",
        serde(bound = "N::BroadcastedTransaction: serde::Serialize + serde::de::DeserializeOwned")
    )]
    BlockRangeUpdate(BlockRangeUpdate),
    /// Represents an encoded message that doesn't match any other variant
    Other(RawCapabilityMessage),
}

impl<N: NetworkPrimitives> EthMessage<N> {
    /// Returns the message's ID.
    pub const fn message_id(&self) -> EthMessageID {
        match self {
            Self::Status(_) => EthMessageID::Status,
            Self::NewBlockHashes(_) => EthMessageID::NewBlockHashes,
            Self::NewBlock(_) => EthMessageID::NewBlock,
            Self::Transactions(_) => EthMessageID::Transactions,
            Self::NewPooledTransactionHashes66(_) | Self::NewPooledTransactionHashes68(_) => {
                EthMessageID::NewPooledTransactionHashes
            }
            Self::GetBlockHeaders(_) => EthMessageID::GetBlockHeaders,
            Self::BlockHeaders(_) => EthMessageID::BlockHeaders,
            Self::GetBlockBodies(_) => EthMessageID::GetBlockBodies,
            Self::BlockBodies(_) => EthMessageID::BlockBodies,
            Self::GetPooledTransactions(_) => EthMessageID::GetPooledTransactions,
            Self::PooledTransactions(_) => EthMessageID::PooledTransactions,
            Self::GetNodeData(_) => EthMessageID::GetNodeData,
            Self::NodeData(_) => EthMessageID::NodeData,
            Self::GetReceipts(_) => EthMessageID::GetReceipts,
            Self::Receipts(_) | Self::Receipts69(_) => EthMessageID::Receipts,
            Self::BlockRangeUpdate(_) => EthMessageID::BlockRangeUpdate,
            Self::Other(msg) => EthMessageID::Other(msg.id as u8),
        }
    }

    /// Returns true if the message variant is a request.
    pub const fn is_request(&self) -> bool {
        matches!(
            self,
            Self::GetBlockBodies(_) |
                Self::GetBlockHeaders(_) |
                Self::GetReceipts(_) |
                Self::GetPooledTransactions(_) |
                Self::GetNodeData(_)
        )
    }

    /// Returns true if the message variant is a response to a request.
    pub const fn is_response(&self) -> bool {
        matches!(
            self,
            Self::PooledTransactions(_) |
                Self::Receipts(_) |
                Self::Receipts69(_) |
                Self::BlockHeaders(_) |
                Self::BlockBodies(_) |
                Self::NodeData(_)
        )
    }
}

impl<N: NetworkPrimitives> Encodable for EthMessage<N> {
    fn encode(&self, out: &mut dyn BufMut) {
        match self {
            Self::Status(status) => status.encode(out),
            Self::NewBlockHashes(new_block_hashes) => new_block_hashes.encode(out),
            Self::NewBlock(new_block) => new_block.encode(out),
            Self::Transactions(transactions) => transactions.encode(out),
            Self::NewPooledTransactionHashes66(hashes) => hashes.encode(out),
            Self::NewPooledTransactionHashes68(hashes) => hashes.encode(out),
            Self::GetBlockHeaders(request) => request.encode(out),
            Self::BlockHeaders(headers) => headers.encode(out),
            Self::GetBlockBodies(request) => request.encode(out),
            Self::BlockBodies(bodies) => bodies.encode(out),
            Self::GetPooledTransactions(request) => request.encode(out),
            Self::PooledTransactions(transactions) => transactions.encode(out),
            Self::GetNodeData(request) => request.encode(out),
            Self::NodeData(data) => data.encode(out),
            Self::GetReceipts(request) => request.encode(out),
            Self::Receipts(receipts) => receipts.encode(out),
            Self::Receipts69(receipt69) => receipt69.encode(out),
            Self::BlockRangeUpdate(block_range_update) => block_range_update.encode(out),
            Self::Other(unknown) => out.put_slice(&unknown.payload),
        }
    }
    fn length(&self) -> usize {
        match self {
            Self::Status(status) => status.length(),
            Self::NewBlockHashes(new_block_hashes) => new_block_hashes.length(),
            Self::NewBlock(new_block) => new_block.length(),
            Self::Transactions(transactions) => transactions.length(),
            Self::NewPooledTransactionHashes66(hashes) => hashes.length(),
            Self::NewPooledTransactionHashes68(hashes) => hashes.length(),
            Self::GetBlockHeaders(request) => request.length(),
            Self::BlockHeaders(headers) => headers.length(),
            Self::GetBlockBodies(request) => request.length(),
            Self::BlockBodies(bodies) => bodies.length(),
            Self::GetPooledTransactions(request) => request.length(),
            Self::PooledTransactions(transactions) => transactions.length(),
            Self::GetNodeData(request) => request.length(),
            Self::NodeData(data) => data.length(),
            Self::GetReceipts(request) => request.length(),
            Self::Receipts(receipts) => receipts.length(),
            Self::Receipts69(receipt69) => receipt69.length(),
            Self::BlockRangeUpdate(block_range_update) => block_range_update.length(),
            Self::Other(unknown) => unknown.length(),
        }
    }
}

/// Represents broadcast messages of [`EthMessage`] with the same object that can be sent to
/// multiple peers.
///
/// Messages that contain a list of hashes depend on the peer the message is sent to. A peer should
/// never receive a hash of an object (block, transaction) it has already seen.
///
/// Note: This is only useful for outgoing messages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EthBroadcastMessage<N: NetworkPrimitives = EthNetworkPrimitives> {
    /// Represents a new block broadcast message.
    NewBlock(Arc<N::NewBlockPayload>),
    /// Represents a transactions broadcast message.
    Transactions(SharedTransactions<N::BroadcastedTransaction>),
}

// === impl EthBroadcastMessage ===

impl<N: NetworkPrimitives> EthBroadcastMessage<N> {
    /// Returns the message's ID.
    pub const fn message_id(&self) -> EthMessageID {
        match self {
            Self::NewBlock(_) => EthMessageID::NewBlock,
            Self::Transactions(_) => EthMessageID::Transactions,
        }
    }
}

impl<N: NetworkPrimitives> Encodable for EthBroadcastMessage<N> {
    fn encode(&self, out: &mut dyn BufMut) {
        match self {
            Self::NewBlock(new_block) => new_block.encode(out),
            Self::Transactions(transactions) => transactions.encode(out),
        }
    }

    fn length(&self) -> usize {
        match self {
            Self::NewBlock(new_block) => new_block.length(),
            Self::Transactions(transactions) => transactions.length(),
        }
    }
}

/// Represents message IDs for eth protocol messages.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum EthMessageID {
    /// Status message.
    Status = 0x00,
    /// New block hashes message.
    NewBlockHashes = 0x01,
    /// Transactions message.
    Transactions = 0x02,
    /// Get block headers message.
    GetBlockHeaders = 0x03,
    /// Block headers message.
    BlockHeaders = 0x04,
    /// Get block bodies message.
    GetBlockBodies = 0x05,
    /// Block bodies message.
    BlockBodies = 0x06,
    /// New block message.
    NewBlock = 0x07,
    /// New pooled transaction hashes message.
    NewPooledTransactionHashes = 0x08,
    /// Requests pooled transactions.
    GetPooledTransactions = 0x09,
    /// Represents pooled transactions.
    PooledTransactions = 0x0a,
    /// Requests node data.
    GetNodeData = 0x0d,
    /// Represents node data.
    NodeData = 0x0e,
    /// Requests receipts.
    GetReceipts = 0x0f,
    /// Represents receipts.
    Receipts = 0x10,
    /// Block range update.
    ///
    /// Introduced in Eth69
    BlockRangeUpdate = 0x11,
    /// Represents unknown message types.
    Other(u8),
}

impl EthMessageID {
    /// Returns the corresponding `u8` value for an `EthMessageID`.
    pub const fn to_u8(&self) -> u8 {
        match self {
            Self::Status => 0x00,
            Self::NewBlockHashes => 0x01,
            Self::Transactions => 0x02,
            Self::GetBlockHeaders => 0x03,
            Self::BlockHeaders => 0x04,
            Self::GetBlockBodies => 0x05,
            Self::BlockBodies => 0x06,
            Self::NewBlock => 0x07,
            Self::NewPooledTransactionHashes => 0x08,
            Self::GetPooledTransactions => 0x09,
            Self::PooledTransactions => 0x0a,
            Self::GetNodeData => 0x0d,
            Self::NodeData => 0x0e,
            Self::GetReceipts => 0x0f,
            Self::Receipts => 0x10,
            Self::BlockRangeUpdate => 0x11,
            Self::Other(value) => *value, // Return the stored `u8`
        }
    }

    /// Returns the max value for the given version.
    pub const fn max(version: EthVersion) -> u8 {
        if version.is_eth69() {
            Self::BlockRangeUpdate.to_u8()
        } else {
            Self::Receipts.to_u8()
        }
    }
}

impl Encodable for EthMessageID {
    fn encode(&self, out: &mut dyn BufMut) {
        out.put_u8(self.to_u8());
    }
    fn length(&self) -> usize {
        1
    }
}

impl Decodable for EthMessageID {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let id = match buf.first().ok_or(alloy_rlp::Error::InputTooShort)? {
            0x00 => Self::Status,
            0x01 => Self::NewBlockHashes,
            0x02 => Self::Transactions,
            0x03 => Self::GetBlockHeaders,
            0x04 => Self::BlockHeaders,
            0x05 => Self::GetBlockBodies,
            0x06 => Self::BlockBodies,
            0x07 => Self::NewBlock,
            0x08 => Self::NewPooledTransactionHashes,
            0x09 => Self::GetPooledTransactions,
            0x0a => Self::PooledTransactions,
            0x0d => Self::GetNodeData,
            0x0e => Self::NodeData,
            0x0f => Self::GetReceipts,
            0x10 => Self::Receipts,
            0x11 => Self::BlockRangeUpdate,
            unknown => Self::Other(*unknown),
        };
        buf.advance(1);
        Ok(id)
    }
}

impl TryFrom<usize> for EthMessageID {
    type Error = &'static str;

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        match value {
            0x00 => Ok(Self::Status),
            0x01 => Ok(Self::NewBlockHashes),
            0x02 => Ok(Self::Transactions),
            0x03 => Ok(Self::GetBlockHeaders),
            0x04 => Ok(Self::BlockHeaders),
            0x05 => Ok(Self::GetBlockBodies),
            0x06 => Ok(Self::BlockBodies),
            0x07 => Ok(Self::NewBlock),
            0x08 => Ok(Self::NewPooledTransactionHashes),
            0x09 => Ok(Self::GetPooledTransactions),
            0x0a => Ok(Self::PooledTransactions),
            0x0d => Ok(Self::GetNodeData),
            0x0e => Ok(Self::NodeData),
            0x0f => Ok(Self::GetReceipts),
            0x10 => Ok(Self::Receipts),
            0x11 => Ok(Self::BlockRangeUpdate),
            _ => Err("Invalid message ID"),
        }
    }
}

/// This is used for all request-response style `eth` protocol messages.
/// This can represent either a request or a response, since both include a message payload and
/// request id.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(any(test, feature = "arbitrary"), derive(arbitrary::Arbitrary))]
pub struct RequestPair<T> {
    /// id for the contained request or response message
    pub request_id: u64,

    /// the request or response message payload
    pub message: T,
}

impl<T> RequestPair<T> {
    /// Converts the message type with the given closure.
    pub fn map<F, R>(self, f: F) -> RequestPair<R>
    where
        F: FnOnce(T) -> R,
    {
        let Self { request_id, message } = self;
        RequestPair { request_id, message: f(message) }
    }
}

/// Allows messages with request ids to be serialized into RLP bytes.
impl<T> Encodable for RequestPair<T>
where
    T: Encodable,
{
    fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
        let header =
            Header { list: true, payload_length: self.request_id.length() + self.message.length() };

        header.encode(out);
        self.request_id.encode(out);
        self.message.encode(out);
    }

    fn length(&self) -> usize {
        let mut length = 0;
        length += self.request_id.length();
        length += self.message.length();
        length += length_of_length(length);
        length
    }
}

/// Allows messages with request ids to be deserialized into RLP bytes.
impl<T> Decodable for RequestPair<T>
where
    T: Decodable,
{
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let header = Header::decode(buf)?;

        let initial_length = buf.len();
        let request_id = u64::decode(buf)?;
        let message = T::decode(buf)?;

        // Check that the buffer consumed exactly payload_length bytes after decoding the
        // RequestPair
        let consumed_len = initial_length - buf.len();
        if consumed_len != header.payload_length {
            return Err(alloy_rlp::Error::UnexpectedLength)
        }

        Ok(Self { request_id, message })
    }
}

#[cfg(test)]
mod tests {
    use super::MessageError;
    use crate::{
        message::RequestPair, EthMessage, EthMessageID, EthNetworkPrimitives, EthVersion,
        GetNodeData, NodeData, ProtocolMessage, RawCapabilityMessage,
    };
    use alloy_primitives::hex;
    use alloy_rlp::{Decodable, Encodable, Error};
    use reth_ethereum_primitives::BlockBody;

    fn encode<T: Encodable>(value: T) -> Vec<u8> {
        let mut buf = vec![];
        value.encode(&mut buf);
        buf
    }

    #[test]
    fn test_removed_message_at_eth67() {
        let get_node_data = EthMessage::<EthNetworkPrimitives>::GetNodeData(RequestPair {
            request_id: 1337,
            message: GetNodeData(vec![]),
        });
        let buf = encode(ProtocolMessage {
            message_type: EthMessageID::GetNodeData,
            message: get_node_data,
        });
        let msg = ProtocolMessage::<EthNetworkPrimitives>::decode_message(
            crate::EthVersion::Eth67,
            &mut &buf[..],
        );
        assert!(matches!(msg, Err(MessageError::Invalid(..))));

        let node_data = EthMessage::<EthNetworkPrimitives>::NodeData(RequestPair {
            request_id: 1337,
            message: NodeData(vec![]),
        });
        let buf =
            encode(ProtocolMessage { message_type: EthMessageID::NodeData, message: node_data });
        let msg = ProtocolMessage::<EthNetworkPrimitives>::decode_message(
            crate::EthVersion::Eth67,
            &mut &buf[..],
        );
        assert!(matches!(msg, Err(MessageError::Invalid(..))));
    }

    #[test]
    fn request_pair_encode() {
        let request_pair = RequestPair { request_id: 1337, message: vec![5u8] };

        // c5: start of list (c0) + len(full_list) (length is <55 bytes)
        // 82: 0x80 + len(1337)
        // 05 39: 1337 (request_id)
        // === full_list ===
        // c1: start of list (c0) + len(list) (length is <55 bytes)
        // 05: 5 (message)
        let expected = hex!("c5820539c105");
        let got = encode(request_pair);
        assert_eq!(expected[..], got, "expected: {expected:X?}, got: {got:X?}",);
    }

    #[test]
    fn request_pair_decode() {
        let raw_pair = &hex!("c5820539c105")[..];

        let expected = RequestPair { request_id: 1337, message: vec![5u8] };

        let got = RequestPair::<Vec<u8>>::decode(&mut &*raw_pair).unwrap();
        assert_eq!(expected.length(), raw_pair.len());
        assert_eq!(expected, got);
    }

    #[test]
    fn malicious_request_pair_decode() {
        // A maliciously encoded request pair, where the len(full_list) is 5, but it
        // actually consumes 6 bytes when decoding
        //
        // c5: start of list (c0) + len(full_list) (length is <55 bytes)
        // 82: 0x80 + len(1337)
        // 05 39: 1337 (request_id)
        // === full_list ===
        // c2: start of list (c0) + len(list) (length is <55 bytes)
        // 05 05: 5 5(message)
        let raw_pair = &hex!("c5820539c20505")[..];

        let result = RequestPair::<Vec<u8>>::decode(&mut &*raw_pair);
        assert!(matches!(result, Err(Error::UnexpectedLength)));
    }

    #[test]
    fn empty_block_bodies_protocol() {
        let empty_block_bodies =
            ProtocolMessage::from(EthMessage::<EthNetworkPrimitives>::BlockBodies(RequestPair {
                request_id: 0,
                message: Default::default(),
            }));
        let mut buf = Vec::new();
        empty_block_bodies.encode(&mut buf);
        let decoded =
            ProtocolMessage::decode_message(EthVersion::Eth68, &mut buf.as_slice()).unwrap();
        assert_eq!(empty_block_bodies, decoded);
    }

    #[test]
    fn empty_block_body_protocol() {
        let empty_block_bodies =
            ProtocolMessage::from(EthMessage::<EthNetworkPrimitives>::BlockBodies(RequestPair {
                request_id: 0,
                message: vec![BlockBody {
                    transactions: vec![],
                    ommers: vec![],
                    withdrawals: Some(Default::default()),
                }]
                .into(),
            }));
        let mut buf = Vec::new();
        empty_block_bodies.encode(&mut buf);
        let decoded =
            ProtocolMessage::decode_message(EthVersion::Eth68, &mut buf.as_slice()).unwrap();
        assert_eq!(empty_block_bodies, decoded);
    }

    #[test]
    fn decode_block_bodies_message() {
        let buf = hex!("06c48199c1c0");
        let msg = ProtocolMessage::<EthNetworkPrimitives>::decode_message(
            EthVersion::Eth68,
            &mut &buf[..],
        )
        .unwrap_err();
        assert!(matches!(msg, MessageError::RlpError(alloy_rlp::Error::InputTooShort)));
    }

    #[test]
    fn custom_message_roundtrip() {
        let custom_payload = vec![1, 2, 3, 4, 5];
        let custom_message = RawCapabilityMessage::new(0x20, custom_payload.into());
        let protocol_message = ProtocolMessage::<EthNetworkPrimitives> {
            message_type: EthMessageID::Other(0x20),
            message: EthMessage::Other(custom_message),
        };

        let encoded = encode(protocol_message.clone());
        let decoded = ProtocolMessage::<EthNetworkPrimitives>::decode_message(
            EthVersion::Eth68,
            &mut &encoded[..],
        )
        .unwrap();

        assert_eq!(protocol_message, decoded);
    }

    #[test]
    fn custom_message_empty_payload_roundtrip() {
        let custom_message = RawCapabilityMessage::new(0x30, vec![].into());
        let protocol_message = ProtocolMessage::<EthNetworkPrimitives> {
            message_type: EthMessageID::Other(0x30),
            message: EthMessage::Other(custom_message),
        };

        let encoded = encode(protocol_message.clone());
        let decoded = ProtocolMessage::<EthNetworkPrimitives>::decode_message(
            EthVersion::Eth68,
            &mut &encoded[..],
        )
        .unwrap();

        assert_eq!(protocol_message, decoded);
    }
}
