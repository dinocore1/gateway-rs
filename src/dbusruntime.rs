
use std::time::Duration;

use loragw_hw::lib::{nlighten::{types::{FullRxPkt, TxPkt}, dbus::{LoraCardProxy, RxStream, DBusTxPkt, SendResult}, lora::Frequency}, CardId, CodingRate};
use semtech_udp::{server_runtime::{Event, RxPk, Error}, MacAddress, pull_resp::{self, TxPk}, push_data::RxPkV1};
use serde::{Serialize, Deserialize};
use zbus::{dbus_proxy, zvariant::Type};
use tokio::sync::mpsc;
use futures::prelude::*;
use tracing::{debug, info, warn};

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
        let rx = args.rx.unwrap().unwrap();
        Event::PacketReceived(RxPk::V1(RxPkV1 {
            chan: 0, // dont care
            codr: nl_to_st::convert_coding_rate(&rx.pkt.rxmeta.coding_rate),
            data: Vec::from(rx.pkt.payload.as_slice()),
            datr: nl_to_st::convert_datarate(&rx.pkt.rxmeta.datarate),
            freq: to_mhz(rx.pkt.rxmeta.freq),
            lsnr: rx.pkt.rxmeta.snr.to_db(),
            modu: semtech_udp::Modulation::LORA,
            rfch: 0, // dont care,
            rssi: rx.rssic as i32,
            rssis: Some(rx.pkt.rxmeta.rssis.to_db().round() as i32),
            size: rx.pkt.payload.len() as u64,
            stat: nl_to_st::convert_crc(&rx),
            tmst: rx.pkt.rxmeta.tmst,
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
                payload: loragw_hw::lib::nlighten::lora::Payload::from_slice(pkt.data.as_ref()).map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "packet"))?,
                tx_mode: st_to_nl::convert_txmode(&pkt),
            };

            return match self.proxy.send(DBusTxPkt::wrap(&pkt)).await {
                Ok(SendResult::Ok) => Ok(None),
                Ok(SendResult::ErrPacketCollision | SendResult::ErrTooEarly | SendResult::ErrTooLate | SendResult::ErrQueueFull) => Err(semtech_udp::server_runtime::Error::Ack(semtech_udp::tx_ack::Error::TooLate)),
                Ok(SendResult::ErrIO) => Err(semtech_udp::server_runtime::Error::Ack(semtech_udp::tx_ack::Error::SendFail)),
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

    pub(crate) fn convert_txmode(pkt: &TxPk) -> loragw_hw::lib::nlighten::types::TxMode {
        match pkt.is_immediate() {
            true => loragw_hw::lib::nlighten::types::TxMode::Immediate,
            false => match pkt.get_tmst() {
                Some(tmst) => loragw_hw::lib::nlighten::types::TxMode::Timestamped(tmst),
                None => loragw_hw::lib::nlighten::types::TxMode::OnGPS,
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

fn to_mhz(hz: Frequency) -> f64 {
    hz.to_mhz() as f64
}



fn convert_time(rx: &FullRxPkt) -> Option<String> {
    match rx.pkt.rxmeta.gps_time {
        Some(gps_time) => Some(gps_time.as_utc().to_rfc3339()),
        None => None,
    }
}