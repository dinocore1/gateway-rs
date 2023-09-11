
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
            codr: nl_to_st::convert_coding_rate(&rx.pkt.codingrate),
            data: Vec::from(rx.pkt.payload.as_slice()),
            datr: nl_to_st::convert_datarate(&rx.pkt.datarate),
            freq: to_mhz(rx.pkt.freq),
            lsnr: rx.pkt.snr as f32 * 10_f32,
            modu: semtech_udp::Modulation::LORA,
            rfch: 0, // dont care,
            rssi: rx.rssic as i32,
            rssis: Some(rx.pkt.rssi as i32 / 10),
            size: rx.pkt.payload.len() as u64,
            stat: nl_to_st::convert_crc(&rx),
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
                datarate: st_to_nl::convert_datarate(&pkt.datr),
                coderate: st_to_nl::convert_coderate(&pkt.codr),
                invert_pol: pkt.ipol,
                preamble: pkt.prea.map(|x| x as u16),
                no_crc: pkt.ncrc.unwrap_or(false),
                no_header: false,
                payload: pkt.data.as_ref().into(),
                tx_mode: st_to_nl::convert_txmode(&pkt),
                tx_tmst: pkt.get_tmst().unwrap_or(0), // TODO: properly handle send on timestamp and gps
            };

            // TODO: implement SemtechError::Ack(TxAckErr::TooEarly | TxAckErr::TooLate)
            return match self.proxy.send(pkt.into()).await {
                Ok(()) => Ok(None),
                Err(e) => Err(semtech_udp::server_runtime::Error::SendTimeout),
            };
        }

        panic!("send null packet");
        
    }
}

mod nl_to_st {
    use super::*;

    pub(crate) fn convert_coding_rate(dr: &loragw_hw::lib::CodingRate) -> semtech_udp::CodingRate {
        match dr {
            CodingRate::CR_4_5 => semtech_udp::CodingRate::_4_5,
            CodingRate::CR_4_6 => semtech_udp::CodingRate::_4_6,
            CodingRate::CR_4_7 => semtech_udp::CodingRate::_4_7,
            CodingRate::CR_4_8 => semtech_udp::CodingRate::_4_8,
            CodingRate::OFF => semtech_udp::CodingRate::OFF,
        }
    
    }

    pub(crate) fn convert_crc(rx: &FullRxPkt) -> semtech_udp::push_data::CRC {
        match (rx.crc_en, rx.crc_err) {
            (true, false) => semtech_udp::push_data::CRC::OK,
            (true, true) => semtech_udp::push_data::CRC::Fail,
            (false, _) => semtech_udp::push_data::CRC::Disabled,
        }
    }

    pub(crate) fn convert_datarate(datarate: &loragw_hw::lib::Datarate) -> semtech_udp::DataRate {
        semtech_udp::DataRate::new(convert_sf(&datarate.0), convert_bandwidth(&datarate.1))
    }

    pub(crate) fn convert_sf(sf: &loragw_hw::lib::SpreadingFactor) -> semtech_udp::SpreadingFactor {
        match sf {
            loragw_hw::lib::SpreadingFactor::SF5 => semtech_udp::SpreadingFactor::SF5,
            loragw_hw::lib::SpreadingFactor::SF6 => semtech_udp::SpreadingFactor::SF6,
            loragw_hw::lib::SpreadingFactor::SF7 => semtech_udp::SpreadingFactor::SF7,
            loragw_hw::lib::SpreadingFactor::SF8 => semtech_udp::SpreadingFactor::SF8,
            loragw_hw::lib::SpreadingFactor::SF9 => semtech_udp::SpreadingFactor::SF9,
            loragw_hw::lib::SpreadingFactor::SF10 => semtech_udp::SpreadingFactor::SF10,
            loragw_hw::lib::SpreadingFactor::SF11 => semtech_udp::SpreadingFactor::SF11,
            loragw_hw::lib::SpreadingFactor::SF12 => semtech_udp::SpreadingFactor::SF12,
        }
    }

    pub(crate) fn convert_bandwidth(bw: &loragw_hw::lib::Bandwidth) -> semtech_udp::Bandwidth {
        match bw {
            loragw_hw::lib::Bandwidth::BW125 => semtech_udp::Bandwidth::BW125,
            loragw_hw::lib::Bandwidth::BW250 => semtech_udp::Bandwidth::BW250,
            loragw_hw::lib::Bandwidth::BW500 => semtech_udp::Bandwidth::BW500,
        }
    }
}

mod st_to_nl {
    use super::*;

    pub(crate) fn convert_datarate(datr: &semtech_udp::DataRate) -> loragw_hw::lib::Datarate {
        loragw_hw::lib::Datarate(convert_sf(datr.spreading_factor()), convert_bandwidth(datr.bandwidth()))
    }

    pub(crate) fn convert_coderate(codr: &semtech_udp::CodingRate) -> loragw_hw::lib::CodingRate {
        match codr {
            semtech_udp::CodingRate::_4_5 => loragw_hw::lib::CodingRate::CR_4_5,
            semtech_udp::CodingRate::_4_6 => loragw_hw::lib::CodingRate::CR_4_6,
            semtech_udp::CodingRate::_4_7 => loragw_hw::lib::CodingRate::CR_4_7,
            semtech_udp::CodingRate::_4_8 => loragw_hw::lib::CodingRate::CR_4_8,
            semtech_udp::CodingRate::OFF => loragw_hw::lib::CodingRate::OFF,
        }
    }

    pub(crate) fn convert_txmode(pkt: &TxPk) -> loragw_hw::lib::TxMode {
        match pkt.is_immediate() {
            true => loragw_hw::lib::TxMode::Immediate,
            false => match pkt.get_tmst() {
                Some(tmst) => loragw_hw::lib::TxMode::Timestamped,
                None => loragw_hw::lib::TxMode::OnGPS,
            }
        }
    }

    pub(crate) fn convert_sf(sf: &semtech_udp::SpreadingFactor) -> loragw_hw::lib::SpreadingFactor {
        match sf {
            semtech_udp::SpreadingFactor::SF5 => loragw_hw::lib::SpreadingFactor::SF5,
            semtech_udp::SpreadingFactor::SF6 => loragw_hw::lib::SpreadingFactor::SF6,
            semtech_udp::SpreadingFactor::SF7 => loragw_hw::lib::SpreadingFactor::SF7,
            semtech_udp::SpreadingFactor::SF8 => loragw_hw::lib::SpreadingFactor::SF8,
            semtech_udp::SpreadingFactor::SF9 => loragw_hw::lib::SpreadingFactor::SF9,
            semtech_udp::SpreadingFactor::SF10 => loragw_hw::lib::SpreadingFactor::SF10,
            semtech_udp::SpreadingFactor::SF11 => loragw_hw::lib::SpreadingFactor::SF11,
            semtech_udp::SpreadingFactor::SF12 => loragw_hw::lib::SpreadingFactor::SF12,
        }
    }

    pub(crate) fn convert_bandwidth(bw: &semtech_udp::Bandwidth) -> loragw_hw::lib::Bandwidth {
        match bw {
            semtech_udp::Bandwidth::BW125 => loragw_hw::lib::Bandwidth::BW125,
            semtech_udp::Bandwidth::BW250 => loragw_hw::lib::Bandwidth::BW250,
            semtech_udp::Bandwidth::BW500 => loragw_hw::lib::Bandwidth::BW500,
        }
    }
}

fn to_mhz(hz: u32) -> f64 {
    hz as f64 / 1_000_000_f64
}



fn convert_time(rx: &FullRxPkt) -> Option<String> {
    match rx.pkt.gps_time {
        Some(gps_time) => Some(gps_time.as_utc().to_rfc3339()),
        None => None,
    }
}