
use std::time::Duration;

use loragw_hw::lib::{TxPkt, nlighten::types::FullRxPkt, CardId};
use semtech_udp::{server_runtime::{Event}, MacAddress, pull_resp::{self, TxPk}};
use serde::{Serialize, Deserialize};
use zbus::{dbus_proxy, zvariant::Type};
use tokio::sync::mpsc;
use futures::prelude::*;


#[dbus_proxy(
    interface = "com.nlighten.LoraCard1",
    default_service = "com.nlighten.LoraCard",
    default_path = "/com/nlighten/LoraCard"
)]
trait LoraCard {
    fn send(&self, pkt: DBusTxPkt) -> zbus::fdo::Result<()>;

    fn sign(&self, data: Vec<u8>) -> zbus::fdo::Result<Vec<u8>>;

    #[dbus_proxy(signal)]
    fn rx(&self, rx: DBusFullRxPkt) -> zbus::Result<()>;

    #[dbus_proxy(signal)]
    fn rx_sig(&self, rx: loragw_hw::lib::RxSig) -> zbus::Result<()>;

    #[dbus_proxy(property)]
    fn eui(&self) -> zbus::fdo::Result<CardId>;

    #[dbus_proxy(property)]
    fn pub_key(&self) -> zbus::fdo::Result<Vec<u8>>;

    #[dbus_proxy(property)]
    fn firmware(&self) -> zbus::fdo::Result<u32>;

    #[dbus_proxy(property)]
    fn serial_num(&self) -> zbus::fdo::Result<u32>;

}

#[derive(Debug, Serialize, Deserialize, Type)]
pub struct DBusTxPkt(Vec<u8>);

impl From<TxPkt> for DBusTxPkt {
    fn from(value: TxPkt) -> Self {
        let mut buf = Vec::new();
        borsh_serde::to_writer(&value, &mut buf).expect("no io error");
        DBusTxPkt(buf)
    }
}

impl DBusTxPkt {
    pub fn to_pkt(self) -> Result<TxPkt, borsh_serde::Error> {
        borsh_serde::de::from_bytes(&self.0)
    }
}

#[derive(Debug, Serialize, Deserialize, Type)]
pub struct DBusFullRxPkt(Vec<u8>);

impl From<FullRxPkt> for DBusFullRxPkt {
    fn from(value: FullRxPkt) -> Self {
        let mut buf = Vec::new();
        borsh_serde::to_writer(&value, &mut buf).expect("no io error");
        DBusFullRxPkt(buf)
    }
}

impl DBusFullRxPkt {
    pub fn to_pkt(self) -> Result<FullRxPkt, borsh_serde::Error> {
        borsh_serde::de::from_bytes(&self.0)
    }
}

struct DBusRuntimeInner {
    dbus_connection: zbus::Connection,
    proxy: LoraCardProxy<'static>,
    rx_stream: Option<RxStream<'static>>,
}

impl DBusRuntimeInner {

    async fn new() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let dbus_connection = zbus::ConnectionBuilder::system()?
        .build()
        .await?;

        let proxy = LoraCardProxy::new(&dbus_connection).await?;
        let rx_stream = Some(proxy.receive_rx().await?);

        Ok(Self {
            dbus_connection,
            proxy,
            rx_stream,
        })
    }
}

pub struct DBusRuntime {
    inner: DBusRuntimeInner,
}

impl DBusRuntime {
    pub async fn new() -> crate::Result<Self> {

        Ok(Self {
            inner: DBusRuntimeInner::new().await.map_err(|e| std::io::Error::new(std::io::ErrorKind::NotConnected, e))?,
        })
    }

    pub async fn recv(&mut self) -> Event {
        loop {
            let rx_stream = self.get_rx_stream();
            if let Some(msg) = rx_stream.next().await {
                
            }
        }
        //let msg = self.rx_stream.next().await;
        todo!()
    }

    pub fn prepare_downlink(&self, txpk: pull_resp::TxPk, mac: MacAddress) -> Downlink {
        // let packet = txpk.map(|txpk| pull_resp::Packet {
        //     random_token: rand::thread_rng().gen(),
        //     data: pull_resp::Data::from_txpk(txpk),
        // });

        Downlink {
            // mac,
            // packet,
            // sender: todo!(),
        }
    }

    pub fn prepare_empty_downlink(&self, downlink_mac: MacAddress) -> Downlink {
        todo!()
    }

    fn get_rx_stream(&mut self) -> &mut RxStream<'static> {
        todo!()
    }

    
}

pub struct Downlink {

}

impl Downlink {
    pub fn set_packet(&mut self, txpk: TxPk) {
        todo!()
    }

    pub async fn dispatch(self, timeout_duration: Option<Duration>) -> semtech_udp::server_runtime::Result<Option<u32>> {
        todo!()
    }
}