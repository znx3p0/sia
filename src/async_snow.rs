use std::sync::Arc;

use futures::stream::{SplitSink, SplitStream};
use serde::{de::DeserializeOwned, Serialize};
use snow::{params::*, Builder, StatelessTransportState};
use tungstenite::Message;

use crate::channel::bipartite::unformatted::UnformattedRawBidirectionalChannel;
use crate::channel::unified::UnformattedRawUnifiedChannel;
use crate::err;
use crate::io::{Read, ReadExt, Write, WriteExt};
use crate::serialization::formats::{Bincode, ReadFormat, SendFormat};
use crate::serialization::{rx, tx, wss_rx, wss_tx, zc};
use crate::{io::Wss, Result};

#[repr(transparent)]
#[derive(Clone)]
pub struct Snow {
    /// contains the stream
    transport: Arc<StatelessTransportState>,
}

const PACKET_LEN: u64 = 65519;

impl Snow {
    pub(crate) fn encrypt_packets(&self, buf: Vec<u8>) -> Result<Vec<u8>> {
        let mut total = Vec::with_capacity(buf.len() + 16);

        for buf in buf.chunks(PACKET_LEN as _) {
            let mut buf = self.encrypt_packet(buf)?;
            total.append(&mut buf);
        }
        Ok(total)
    }

    // returns an error if length of buf is greater than the packet length
    fn encrypt_packet(&self, buf: &[u8]) -> Result<Vec<u8>> {
        // create message buffer
        let mut msg = vec![0u8; buf.len() + 16];
        // encrypt into message buffer
        self.encrypt_packet_raw(buf, &mut msg)?;
        Ok(msg)
    }
    fn encrypt_packet_raw(&self, buf: &[u8], mut msg: &mut [u8]) -> Result {
        // encrypt into message buffer
        self.transport
            .write_message(0, buf, &mut msg)
            .map_err(err!(@invalid_data))?;
        Ok(())
    }

    pub fn decrypt(&self, buf: &[u8]) -> Result<Vec<u8>> {
        let mut bytes = vec![];
        for buf in buf.chunks(PACKET_LEN as usize + 16) {
            let mut message = vec![0u8; buf.len()]; // move message outside the loop
            self.transport
                .read_message(0, &buf, &mut message)
                .map_err(err!(@other))?;
            bytes.append(&mut message);
        }
        Ok(bytes)
    }

    ///////////////////////

    /// Starts a new snow stream using the default noise parameters
    pub async fn new(stream: &mut UnformattedRawUnifiedChannel) -> Result<Self> {
        let noise_params = NoiseParams::new(
            "".into(),
            BaseChoice::Noise,
            HandshakeChoice {
                pattern: HandshakePattern::NN,
                modifiers: HandshakeModifierList { list: vec![] },
            },
            DHChoice::Curve25519,
            CipherChoice::ChaChaPoly,
            HashChoice::Blake2s,
        );
        Self::new_with_params(stream, noise_params).await
    }

    /// starts a new snow stream using the provided parameters.
    pub async fn new_with_params(
        chan: &mut UnformattedRawUnifiedChannel,
        noise_params: NoiseParams,
    ) -> Result<Self> {
        let should_init = loop {
            let local_num = rand::random::<u64>();
            chan.send(local_num, &Bincode).await?;
            let peer_num: u64 = chan.receive(&Bincode).await?;
            if local_num == peer_num {
                continue;
            } else {
                break local_num > peer_num;
            }
        };
        if should_init {
            Self::initialize_initiator(chan, noise_params).await
        } else {
            Self::initialize_responder(chan, noise_params).await
        }
    }

    /// starts a new snow stream using the provided parameters.
    pub(crate) async fn initialize_initiator(
        chan: &mut UnformattedRawUnifiedChannel,
        noise_params: NoiseParams,
    ) -> Result<Self> {
        let builder = snow::Builder::new(noise_params);
        let keypair = builder.generate_keypair().map_err(err!(@other))?;
        let builder = builder.local_private_key(&keypair.private);
        // send public key to peer
        chan.send(keypair.public, &Bincode).await?;
        // receive peer's public key
        let peer_public_key: Vec<u8> = chan.receive(&Bincode).await?;
        // set peer's public key
        let builder = builder.remote_public_key(&peer_public_key);
        let mut buf = vec![0u8; 256];
        // initialize the encrypted stream
        let mut handshake = builder.build_initiator().map_err(err!(@other))?;
        let len = handshake
            .write_message(&[], &mut buf)
            .map_err(err!(@other))?;
        chan.send(&buf[..len], &Bincode).await?;
        let message: Vec<u8> = chan.receive(&Bincode).await?;
        // <- e, ee, s, es
        handshake
            .read_message(&message, &mut buf)
            .map_err(err!(@other))?;
        let transport = Arc::new(
            handshake
                .into_stateless_transport_mode()
                .map_err(err!(@other))?,
        );
        // let stream = chan.to_unformatted();
        Ok(Snow { transport })
    }

    /// starts a new snow stream using the provided parameters.
    pub(crate) async fn initialize_responder(
        chan: &mut UnformattedRawUnifiedChannel,
        noise_params: NoiseParams,
    ) -> Result<Self> {
        let builder = snow::Builder::new(noise_params);
        let keypair = builder.generate_keypair().map_err(err!(@other))?;
        let builder = builder.local_private_key(&keypair.private);
        let message: Vec<u8> = chan.receive(&Bincode).await?;
        let mut handshake = builder.build_responder().map_err(err!(@other))?;
        let mut buf = vec![0u8; 256];
        // <- e
        handshake
            .read_message(&message, &mut buf)
            .map_err(err!(@other))?;
        // -> e, ee, s, es
        let len = handshake
            .write_message(&[0u8; 0], &mut buf)
            .map_err(err!(@other))?;
        chan.send(&buf[..len], &Bincode).await?;
        // Transition the state machine into transport mode now that the handshake is complete.
        let transport = Arc::new(
            handshake
                .into_stateless_transport_mode()
                .map_err(err!(@other))?,
        );
        Ok(Snow { transport })
    }
}
