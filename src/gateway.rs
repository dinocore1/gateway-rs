use crate::{dispatcher, Error, Packet, Result, Settings};
use futures::TryFutureExt;
use semtech_udp::{
    server_runtime::{Error as SemtechError, Event, UdpRuntime, RxPk},
    tx_ack, MacAddress, push_data::RxPkV3, push_data_sig,
};
use slog::{debug, info, o, warn, Logger};
use std::{convert::TryFrom, time::Duration};
use tokio::sync::mpsc;

pub const DOWNLINK_TIMEOUT_SECS: u64 = 5;
pub const UPLINK_TIMEOUT_SECS: u64 = 6;

#[derive(Debug)]
pub enum Message {
    Downlink(Packet),
}

#[derive(Clone, Debug)]
pub struct MessageSender(mpsc::Sender<Message>);
pub type MessageReceiver = mpsc::Receiver<Message>;

pub fn message_channel(size: usize) -> (MessageSender, MessageReceiver) {
    let (tx, rx) = mpsc::channel(size);
    (MessageSender(tx), rx)
}

impl MessageSender {
    pub async fn downlink(&self, packet: Packet) -> Result {
        self.0
            .send(Message::Downlink(packet))
            .map_err(|_| Error::channel())
            .await
    }
}

pub struct Gateway {
    uplinks: dispatcher::MessageSender,
    messages: MessageReceiver,
    downlink_mac: MacAddress,
    udp_runtime: UdpRuntime,
    listen_address: String,
    signed_pkt_queue: Vec<RxPkV3>,
}

impl Gateway {
    pub async fn new(
        uplinks: dispatcher::MessageSender,
        messages: MessageReceiver,
        settings: &Settings,
    ) -> Result<Self> {
        let gateway = Gateway {
            uplinks,
            downlink_mac: Default::default(),
            messages,
            listen_address: settings.listen.clone(),
            udp_runtime: UdpRuntime::new(&settings.listen).await?,
            signed_pkt_queue: Vec::new(),
        };
        Ok(gateway)
    }

    pub async fn run(&mut self, shutdown: triggered::Listener, logger: &Logger) -> Result {
        let logger = logger.new(o!("module" => "gateway"));
        info!(logger, "starting"; "listen" => &self.listen_address);
        loop {
            tokio::select! {
                _ = shutdown.clone() => {
                    info!(logger, "shutting down");
                    return Ok(())
                },
                event = self.udp_runtime.recv() =>
                    self.handle_udp_event(&logger, event).await?,
                message = self.messages.recv() => match message {
                    Some(message) => self.handle_message(&logger, message).await,
                    None => {
                        warn!(logger, "ignoring closed downlinks channel");
                        continue;
                    }
                }
            }
        }
    }

    async fn handle_udp_event(&mut self, logger: &Logger, event: Event) -> Result {
        match event {
            Event::UnableToParseUdpFrame(e, buf) => {
                warn!(
                    logger,
                    "ignoring semtech udp parsing error {e}, raw bytes {buf:?}"
                );
            }
            Event::NewClient((mac, addr)) => {
                info!(logger, "new packet forwarder client: {mac}, {addr}");
                self.downlink_mac = mac;
            }
            Event::UpdateClient((mac, addr)) => {
                info!(logger, "mac existed, but IP updated: {mac}, {addr}")
            }
            Event::ClientDisconnected((mac, addr)) => {
                info!(logger, "disconnected packet forwarder: {mac}, {addr}")
            }
            Event::PacketReceived(rxpk, _gateway_mac) => {

                let v3pkt = match rxpk.clone() {
                    RxPk::V3(v3pkt) => Some(v3pkt),
                    _ => None,
                };

                match Packet::try_from(rxpk) {
                    Ok(mut packet) => {
                        if packet.poc_payload().is_some() {
                            self.handle_poc_packet(logger, packet).await;

                            if let Some(v3pkt) = v3pkt {
                                self.queue_signed_poc_packet(v3pkt).await;
                            }
                            
                        } else {
                            self.handle_uplink(logger, packet).await;
                        }
                    }
                    Err(err) => {
                        warn!(logger, "ignoring push_data: {err:?}");
                    }
                }

            }
            
            Event::PacketSigReceived(sigpkt, gateway_mac) => {
                self.handle_pkt_sig(sigpkt).await;
            }
            Event::NoClientWithMac(_packet, mac) => {
                info!(logger, "ignoring send to client with unknown MAC: {mac}")
            }
            Event::StatReceived(stat, mac) => {
                debug!(logger, "mac: {mac}, stat: {stat:?}")
            }
        };
        Ok(())
    }

    async fn handle_uplink(&mut self, logger: &Logger, packet: Packet) {
        info!(logger, "uplink {} from {}", packet, self.downlink_mac);
        match self.uplinks.uplink(packet).await {
            Ok(()) => (),
            Err(err) => warn!(logger, "ignoring uplink error {:?}", err),
        }
    }

    async fn handle_poc_packet(&mut self, logger: &Logger, packet: Packet) {
        match self.uplinks.poc_packet(packet).await {
            Ok(()) => (),
            Err(err) => warn!(logger, "ignoring uplink error {:?}", err),
        }
    }

    async fn queue_signed_poc_packet(&mut self, packet: RxPkV3) {
        if self.signed_pkt_queue.len() > 5 {
            self.signed_pkt_queue.remove(0);
        }
        self.signed_pkt_queue.push(packet);
        
    }

    async fn handle_pkt_sig(&mut self, sig_pkt: push_data_sig::Packet) {
        if let Some(idx) = self.signed_pkt_queue.iter().position(|pkt| pkt.key == sig_pkt.data.key ) {
            let original_pkt = self.signed_pkt_queue.remove(idx);

        }

    }

    async fn handle_message(&mut self, logger: &Logger, message: Message) {
        match message {
            Message::Downlink(packet) => self.handle_downlink(logger, packet).await,
        }
    }

    async fn handle_downlink(&mut self, logger: &Logger, downlink: Packet) {
        let (mut downlink_rx1, mut downlink_rx2) = (
            // first downlink
            self.udp_runtime.prepare_empty_downlink(self.downlink_mac),
            // 2nd downlink window if requested by the router response
            self.udp_runtime.prepare_empty_downlink(self.downlink_mac),
        );
        let logger = logger.clone();
        tokio::spawn(async move {
            match downlink.to_pull_resp(false).unwrap() {
                None => (),
                Some(txpk) => {
                    info!(
                        logger,
                        "rx1 downlink {} via {}",
                        txpk,
                        downlink_rx1.get_destination_mac()
                    );
                    downlink_rx1.set_packet(txpk);
                    match downlink_rx1
                        .dispatch(Some(Duration::from_secs(DOWNLINK_TIMEOUT_SECS)))
                        .await
                    {
                        // On a too early or too late error retry on the rx2 slot if available.
                        Err(SemtechError::Ack(tx_ack::Error::TooEarly))
                        | Err(SemtechError::Ack(tx_ack::Error::TooLate)) => {
                            if let Some(txpk) = downlink.to_pull_resp(true).unwrap() {
                                info!(
                                    logger,
                                    "rx2 downlink {} via {}",
                                    txpk,
                                    downlink_rx2.get_destination_mac()
                                );
                                downlink_rx2.set_packet(txpk);
                                if let Err(err) = downlink_rx2
                                    .dispatch(Some(Duration::from_secs(DOWNLINK_TIMEOUT_SECS)))
                                    .await
                                {
                                    warn!(logger, "ignoring rx2 downlink error: {:?}", err);
                                }
                            }
                        }
                        Err(err) => {
                            warn!(logger, "ignoring rx1 downlink error: {:?}", err);
                        }
                        Ok(()) => (),
                    }
                }
            }
        });
    }
}
