use super::pbft_error::PbftError;
use alloy_rlp::{Decodable, Encodable};
use reth_eth_wire::{
    ClayerBlock, ClayerConsensusMessage, ClayerConsensusMessageHeader, PbftMessage,
    PbftMessageInfo, PbftMessageType, PbftNewValidator, PbftNewView, PbftSeal, PbftSignedVote,
};
use reth_primitives::{Bytes, Signature, B256};
use reth_rpc_types::PeerId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PbftMessageWrapper {
    Message(PbftMessage),
    NewView(PbftNewView),
    Seal(PbftSeal),
    BlockNew(ClayerBlock),
    NewValidator(PbftNewValidator),
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct ParsedMessage {
    /// Serialized ConsensusPeerMessageHeader. Inserted into a signed vote.
    pub header_bytes: Bytes,
    /// Signature for `header_bytes`. Inserted into a signed vote.
    pub header_signature: Signature,
    /// The parsed PBFT message.
    pub message: PbftMessageWrapper,
    /// Whether or not this message was self-constructed.
    pub from_self: bool,
}

impl std::hash::Hash for ParsedMessage {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        match &self.message {
            PbftMessageWrapper::Message(m) => m.hash(state),
            PbftMessageWrapper::NewView(m) => m.hash(state),
            PbftMessageWrapper::Seal(m) => m.hash(state),
            PbftMessageWrapper::BlockNew(m) => m.hash(state),
            PbftMessageWrapper::NewValidator(m) => m.hash(state),
        }
    }
}

impl ParsedMessage {
    pub fn from_peer_message(
        message: ClayerConsensusMessage,
        own_id: &[u8],
    ) -> std::result::Result<Self, PbftError> {
        let header = Self::parse_header(&message.header_bytes)?;
        let deserialized_message = match PbftMessageType::from(header.message_type) {
            PbftMessageType::Seal => {
                let seal: PbftSeal =
                    match PbftSeal::decode(&mut message.message_bytes.to_vec().as_slice()) {
                        Ok(seal) => seal,
                        Err(err) => {
                            return Err(PbftError::SerializationError(
                                "parsing PbftSeal".into(),
                                err.to_string(),
                            ));
                        }
                    };
                PbftMessageWrapper::Seal(seal)
            }
            PbftMessageType::NewView => {
                let new_view: PbftNewView =
                    match PbftNewView::decode(&mut message.message_bytes.to_vec().as_slice()) {
                        Ok(new_view) => new_view,
                        Err(err) => {
                            return Err(PbftError::SerializationError(
                                "parsing PbftNewView".into(),
                                err.to_string(),
                            ));
                        }
                    };
                PbftMessageWrapper::NewView(new_view)
            }
            PbftMessageType::BlockNew => {
                let block =
                    match ClayerBlock::decode(&mut message.message_bytes.to_vec().as_slice()) {
                        Ok(block) => block,
                        Err(err) => {
                            return Err(PbftError::SerializationError(
                                "parsing ClayerExecutionPayload".into(),
                                err.to_string(),
                            ));
                        }
                    };
                PbftMessageWrapper::BlockNew(block)
            }
            PbftMessageType::NewValidator => {
                let new_validator = match PbftNewValidator::decode(
                    &mut message.message_bytes.to_vec().as_slice(),
                ) {
                    Ok(new_validator) => new_validator,
                    Err(err) => {
                        return Err(PbftError::SerializationError(
                            "parsing PbftNewView".into(),
                            err.to_string(),
                        ));
                    }
                };
                PbftMessageWrapper::NewValidator(new_validator)
            }
            _ => {
                let message: PbftMessage =
                    match PbftMessage::decode(&mut message.message_bytes.to_vec().as_slice()) {
                        Ok(msg) => msg,
                        Err(err) => {
                            return Err(PbftError::SerializationError(
                                "parsing PbftMessage".into(),
                                err.to_string(),
                            ));
                        }
                    };
                PbftMessageWrapper::Message(message)
            }
        };

        let mut parsed_message = Self {
            from_self: false,
            header_bytes: message.header_bytes,
            header_signature: message.header_signature.0,
            message: deserialized_message,
        };

        if parsed_message.info().ptype != header.message_type {
            return Err(PbftError::InvalidMessage(format!(
                "Message type mismatch: {:?} != {:?}",
                parsed_message.info().ptype,
                header.message_type
            )));
        }

        parsed_message.from_self = parsed_message.info().signer_id.to_vec().as_slice() == own_id;

        Ok(parsed_message)
    }

    pub fn parse_header(
        header_bytes: &Bytes,
    ) -> std::result::Result<ClayerConsensusMessageHeader, PbftError> {
        let header: ClayerConsensusMessageHeader =
            match ClayerConsensusMessageHeader::decode(&mut header_bytes.to_vec().as_slice()) {
                Ok(header) => header,
                Err(err) => {
                    return Err(PbftError::SerializationError(
                        "parsing ClayerConsensusMessageHeader".into(),
                        err.to_string(),
                    ));
                }
            };
        Ok(header)
    }

    pub fn from_pbft_message(message: PbftMessage) -> Result<Self, PbftError> {
        Ok(Self {
            from_self: true,
            header_bytes: Bytes::new(),
            header_signature: Signature::default(),
            message: PbftMessageWrapper::Message(message),
        })
    }

    pub fn from_block_new_message(message: ClayerBlock) -> Self {
        Self {
            from_self: true,
            header_bytes: Bytes::new(),
            header_signature: Signature::default(),
            message: PbftMessageWrapper::BlockNew(message),
        }
    }

    pub fn from_new_view_message(message: PbftNewView) -> Self {
        Self {
            from_self: true,
            header_bytes: Bytes::new(),
            header_signature: Signature::default(),
            message: PbftMessageWrapper::NewView(message),
        }
    }

    pub fn from_new_validator_message(message: PbftNewValidator) -> Self {
        Self {
            from_self: true,
            header_bytes: Bytes::new(),
            header_signature: Signature::default(),
            message: PbftMessageWrapper::NewValidator(message),
        }
    }

    pub fn from_signed_vote(vote: &PbftSignedVote) -> Result<Self, PbftError> {
        let message = match PbftMessage::decode(&mut vote.message_bytes.to_vec().as_slice()) {
            Ok(msg) => msg,
            Err(err) => {
                return Err(PbftError::SerializationError(
                    "parsing PbftSignedVote".into(),
                    err.to_string(),
                ));
            }
        };

        Ok(Self {
            from_self: false,
            header_bytes: vote.header_bytes.clone(),
            header_signature: vote.header_signature.0.clone(),
            message: PbftMessageWrapper::Message(message),
        })
    }

    pub fn info(&self) -> &PbftMessageInfo {
        match &self.message {
            PbftMessageWrapper::Message(message) => &message.info,
            PbftMessageWrapper::NewView(message) => &message.info,
            PbftMessageWrapper::Seal(message) => &message.info,
            PbftMessageWrapper::BlockNew(message) => &message.info,
            PbftMessageWrapper::NewValidator(message) => &message.info,
        }
    }

    pub fn get_block_id(&self) -> B256 {
        match &self.message {
            PbftMessageWrapper::Message(m) => m.block_id,
            PbftMessageWrapper::NewView(_) => {
                panic!("ParsedPeerMessage.get_block_id found a new view message!")
            }
            PbftMessageWrapper::Seal(_) => {
                panic!("ParsedPeerMessage.get_block_id found a seal message!")
            }
            PbftMessageWrapper::BlockNew(_) => {
                panic!("ParsedPeerMessage.get_block_id found a blocknew message!")
            }
            PbftMessageWrapper::NewValidator(_) => {
                panic!("ParsedPeerMessage.get_block_id found a newvalidator message!")
            }
        }
    }

    pub fn get_pbft(&self) -> &PbftMessage {
        match &self.message {
            PbftMessageWrapper::Message(m) => m,
            PbftMessageWrapper::NewView(_) => {
                panic!("ParsedPeerMessage.get_view_change_message found a new view message!")
            }
            PbftMessageWrapper::Seal(_) => {
                panic!("ParsedPeerMessage.get_view_change_message found a seal message!")
            }
            PbftMessageWrapper::BlockNew(_) => {
                panic!("ParsedPeerMessage.get_view_change_message found a blocknew message!")
            }
            PbftMessageWrapper::NewValidator(_) => {
                panic!("ParsedPeerMessage.get_view_change_message found a newvalidator message!")
            }
        }
    }

    pub fn get_new_view(&self) -> &PbftNewView {
        match &self.message {
            PbftMessageWrapper::Message(_) => {
                panic!("ParsedPeerMessage.get_view_change_message found a pbft message!")
            }
            PbftMessageWrapper::NewView(m) => m,
            PbftMessageWrapper::Seal(_) => {
                panic!("ParsedPeerMessage.get_view_change_message found a seal message!")
            }
            PbftMessageWrapper::BlockNew(_) => {
                panic!("ParsedPeerMessage.get_view_change_message found a blocknew message!")
            }
            PbftMessageWrapper::NewValidator(_) => {
                panic!("ParsedPeerMessage.get_view_change_message found a newvalidator message!")
            }
        }
    }

    pub fn get_seal(&self) -> &PbftSeal {
        match &self.message {
            PbftMessageWrapper::Message(_) => {
                panic!("ParsedPeerMessage.get_seal found a pbft message!")
            }
            PbftMessageWrapper::NewView(_) => {
                panic!("ParsedPeerMessage.get_seal found a new view message!")
            }
            PbftMessageWrapper::BlockNew(_) => {
                panic!("ParsedPeerMessage.get_seal found a block new message!")
            }
            PbftMessageWrapper::Seal(s) => s,
            PbftMessageWrapper::NewValidator(_) => {
                panic!("ParsedPeerMessage.get_view_change_message found a newvalidator message!")
            }
        }
    }

    pub fn get_block_new(&self) -> &ClayerBlock {
        match &self.message {
            PbftMessageWrapper::Message(_) => {
                panic!("ParsedPeerMessage.get_seal found a pbft message!")
            }
            PbftMessageWrapper::NewView(_) => {
                panic!("ParsedPeerMessage.get_seal found a new view message!")
            }
            PbftMessageWrapper::Seal(_) => {
                panic!("ParsedPeerMessage.get_seal found a seal message!")
            }
            PbftMessageWrapper::BlockNew(b) => b,
            PbftMessageWrapper::NewValidator(_) => {
                panic!("ParsedPeerMessage.get_view_change_message found a newvalidator message!")
            }
        }
    }

    pub fn get_new_validator(&self) -> PeerId {
        match &self.message {
            PbftMessageWrapper::Message(_) => {
                panic!("ParsedPeerMessage.new_validator found a pbft message!")
            }
            PbftMessageWrapper::NewView(_) => {
                panic!("ParsedPeerMessage.new_validator found a new view message!")
            }
            PbftMessageWrapper::Seal(_) => {
                panic!("ParsedPeerMessage.new_validator found a seal message!")
            }
            PbftMessageWrapper::BlockNew(_) => {
                panic!("ParsedPeerMessage.new_validator found a blocknew message!")
            }
            PbftMessageWrapper::NewValidator(p) => p.peerid.clone(),
        }
    }

    pub fn get_message_bytes(&self) -> Bytes {
        let mut msg_out = vec![];
        match &self.message {
            PbftMessageWrapper::Message(m) => m.encode(&mut msg_out),
            PbftMessageWrapper::NewView(m) => m.encode(&mut msg_out),
            PbftMessageWrapper::Seal(m) => m.encode(&mut msg_out),
            PbftMessageWrapper::BlockNew(m) => m.encode(&mut msg_out),
            PbftMessageWrapper::NewValidator(m) => m.encode(&mut msg_out),
        }

        Bytes::copy_from_slice(msg_out.as_slice())
    }
}
