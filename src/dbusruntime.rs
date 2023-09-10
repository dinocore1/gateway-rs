
use std::time::Duration;

use loragw_hw::lib::{TxPkt, nlighten::types::FullRxPkt, CardId, CodingRate};
use semtech_udp::{server_runtime::{Event, RxPk}, MacAddress, pull_resp::{self, TxPk}, push_data::RxPkV1};
use serde::{Serialize, Deserialize};
use zbus::{dbus_proxy, zvariant::Type};
use tokio::sync::mpsc;
use futures::prelude::*;
use tracing::{debug, info, warn};


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
    dbus_connection: zbus::Connection,
    proxy: LoraCardProxy<'static>,
    rx_stream: RxStream<'static>,
}

impl DBusRuntime {
    pub async fn new() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {

        let dbus_connection = zbus::ConnectionBuilder::system()?
            .build()
            .await?;

        let proxy = LoraCardProxy::new(&dbus_connection).await?;
        let rx_stream = proxy.receive_rx().await?;

        Ok(Self {
            dbus_connection,
            proxy,
            rx_stream,
        })
    }

    pub async fn recv(&mut self) -> Event {
        let msg = self.rx_stream.next().await.unwrap();
        let args = msg.args().unwrap();
        let rx = args.rx.to_pkt().unwrap();
        Event::PacketReceived(RxPk::V1(RxPkV1 {
            chan: 0, // dont care
            codr: convert_coding_rate(rx.pkt.codingrate),
            data: Vec::from(rx.pkt.payload.as_slice()),
            datr: convert_datarate(rx.pkt.datarate),
            freq: to_mhz(rx.pkt.freq),
            lsnr: rx.pkt.snr as f32 * 10_f32,
            modu: semtech_udp::Modulation::LORA,
            rfch: 0, // dont care,
            rssi: rx.rssic as i32,
            rssis: Some(rx.pkt.rssi as i32 / 10),
            size: rx.pkt.payload.len() as u64,
            stat: convert_crc(&rx),
            tmst: rx.pkt.tmst,
            time: convert_time(&rx),

        }), MacAddress::nil())
        
    }

    pub fn prepare_downlink(&self, txpk: TxPk, mac: MacAddress) -> Downlink {
        Downlink {
            packet: Some(txpk),
            proxy: self.proxy.clone(),
        }
    }

    pub fn prepare_empty_downlink(&self, downlink_mac: MacAddress) -> Downlink {
        Downlink { 
            packet: None,
            proxy: self.proxy.clone(),
        }
    }

    
}


pub struct Downlink {
    packet: Option<TxPk>,
    proxy: LoraCardProxy<'static>,
}

impl Downlink {
    pub fn set_packet(&mut self, txpk: TxPk) {
        self.packet = Some(txpk);
    }

    pub async fn dispatch(self, timeout_duration: Option<Duration>) -> semtech_udp::server_runtime::Result<Option<u32>> {
        if let Some(pkt) = self.packet {
            let pkt = TxPkt {
                freq_hz: (pkt.freq * 1_000_000_f64).round() as u32,
                rf_chain: pkt.rfch as u8,
                rf_power: pkt.powe as i8,
                datarate: pkt.datr,
                coderate: pkt.codr,
                invert_pol: pkt.ipol,
                preamble: pkt.prea,
                no_crc: pkt.ncrc,
                no_header: false,
                payload: pkt.data,
                tx_mode: convert_txmode(&pkt),
                tx_tmst: pkt.time.tmms().unwrap_or(0),
            };
            self.proxy.send(pkt.into()).await;
        }

        todo!()
        
    }
}

fn convert_coding_rate(dr: loragw_hw::lib::CodingRate) -> semtech_udp::CodingRate {
    match dr {
        CodingRate::CR_4_5 => semtech_udp::CodingRate::_4_5,
        CodingRate::CR_4_6 => semtech_udp::CodingRate::_4_6,
        CodingRate::CR_4_7 => semtech_udp::CodingRate::_4_7,
        CodingRate::CR_4_8 => semtech_udp::CodingRate::_4_8,
        CodingRate::OFF => semtech_udp::CodingRate::OFF,
    }

}

fn convert_datarate(datarate: loragw_hw::lib::Datarate) -> semtech_udp::DataRate {
    todo!()
}

fn to_mhz(hz: u32) -> f64 {
    hz as f64 / 1_000_000_f64
}

fn convert_crc(rx: &FullRxPkt) -> semtech_udp::push_data::CRC {
    todo!()
}

fn convert_time(rx: &FullRxPkt) -> Option<String> {
    todo!()
}