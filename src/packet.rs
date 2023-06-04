use crate::{error::DecodeError, Error, Region, Result};
use chrono::{prelude::*};
use helium_proto::services::{
    poc_lora::{self, SecurePacketV1},
    router::{PacketRouterPacketDownV1, PacketRouterPacketUpV1},
};
use lorawan::{Direction, PHYPayloadFrame, MHDR};
use semtech_udp::{
    pull_resp::{self, PhyData, Time},
    push_data::{self, CRC},
    CodingRate, DataRate, Modulation, MacAddress, GPSTime, WGS84Position,
};
use sha2::{Digest, Sha256};
use tracing::warn;
use std::{
    convert::TryFrom,
    fmt::{self, Display},
    ops::Deref,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Clone)]
pub struct PacketUp {
    payload: Vec<u8>,

    /// Internal timestamp of "RX finished" event (32b unsigned) as provided by SX130x
    tmst: u32,

    /// Timestamp of the received packet (nanoseconds since the unix epoch)
    timestamp: u64,

    /// Received Signal Strength Indicator
    rssi: Rssi,

    /// Frequency
    freq: Frequency,

    datr: DataRate,

    /// Signal to Noise radio
    snr: Snr,

    region: Region,

    hold_time: u64,

    /// Gateway
    gateway: MacAddress,

    // packet key. Used for Secure Concentrator
    key: Option<u32>,

    pos: Option<WGS84Position>,

    concentrator_sig: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct PacketDown(PacketRouterPacketDownV1);



impl From<PacketUp> for PacketRouterPacketUpV1 {
    fn from(value: PacketUp) -> Self {
        PacketRouterPacketUpV1 {
            timestamp: value.unix_timestamp(),
            rssi: value.rssi.dbm(),
            frequency: value.freq.hz(),
            datarate: datarate::to_proto(value.datr) as i32,
            snr: value.snr.db(),
            region: value.region.into(),
            hold_time: value.hold_time,
            gateway: Vec::from(value.gateway.into_array()),
            payload: value.payload,
            signature: vec![],
        }
    }
}
impl From<&PacketUp> for PacketRouterPacketUpV1 {
    fn from(value: &PacketUp) -> Self {
        PacketRouterPacketUpV1::from(value.clone())
    }
}

impl From<PacketRouterPacketDownV1> for PacketDown {
    fn from(value: PacketRouterPacketDownV1) -> Self {
        Self(value)
    }
}

impl fmt::Display for PacketUp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!(
            "@{} us, {}, {:?}, snr: {}, rssi: {}, len: {}",
            self.timestamp,
            self.freq,
            self.datr,
            self.snr,
            self.rssi,
            self.payload.len()
        ))
    }
}

impl TryFrom<PacketUp> for poc_lora::LoraWitnessReportReqV1 {
    type Error = Error;
    fn try_from(value: PacketUp) -> Result<Self> {
        let payload = match PacketUp::parse_frame(Direction::Uplink, value.payload()) {
            Ok(PHYPayloadFrame::Proprietary(payload)) => payload,
            _ => return Err(DecodeError::not_beacon()),
        };
        let secure_pkt = if value.is_secure_packet() {
            let time = std::time::Duration::from_nanos(value.timestamp);
            let time = helium_proto::GpsTime {
                sec: time.as_secs(),
                nano: time.subsec_nanos(),
            };
            
            Some(SecurePacketV1 {
                card_id: Vec::from(value.gateway.into_array()),
                pos: value.get_pos(),
                time: Some(time),
                sc_signature: value.concentrator_sig.unwrap_or_else(|| vec![]),
            })
        } else {
            None
        };
        let report = poc_lora::LoraWitnessReportReqV1 {
            data: payload,
            tmst: value.tmst,
            timestamp: value.timestamp,
            signal: value.rssi.centi_dbm(),
            snr: value.snr.centi_db(),
            frequency: value.freq.hz() as u64,
            datarate: datarate::to_proto(value.datr) as i32,
            pub_key: vec![],
            signature: vec![],
            secure_pkt,
        };
        Ok(report)
    }
}

impl PacketUp {
    pub fn from_rxpk(rxpk: push_data::RxPk, region: Region) -> Result<Self> {
        if rxpk.get_crc_status() != &CRC::OK {
            return Err(DecodeError::invalid_crc());
        }
        let rssi = rxpk
            .get_signal_rssi()
            .unwrap_or_else(|| rxpk.get_channel_rssi());

        fn now_timestamp() -> u64 {
            SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(Error::from).unwrap()
            .as_nanos() as u64
        }

        // Get the timestamp (nanoseconds since the unix epoch) of the packet arrival time
        // Use the GPS time if provided as first priority as that will be the most accurate.
        // use the time as provided from the packet forwarder next.
        // as a last resort use the current system time.
        let timestamp = match (rxpk.get_gpstime(), rxpk.get_time()) {
            (Some(gps_time), _) => gps_time.to_datetime().unix_timestamp_nanos() as u64,
            (None, Some(time_str)) => match DateTime::parse_from_rfc3339(time_str.as_ref()) {
                Ok(time) => time.timestamp_nanos() as u64,
                Err(parse_error) => {
                    warn!("unable to parse time: {}: {}", time_str, parse_error);
                    now_timestamp()
                }
            }
            (None, None) => now_timestamp(),
        };

        let packet = Self {
            payload: rxpk.get_data().to_vec(),
            tmst: *rxpk.get_timestamp(),
            timestamp,
            rssi: Rssi::from_dbm(rssi),
            freq: Frequency::from_mhz(rxpk.get_frequency()),
            datr: rxpk.get_datarate(),
            snr: Snr::from_db(rxpk.get_snr()),
            region: region.into(),
            hold_time: 0,
            gateway: MacAddress::nil(),
            key: rxpk.get_id(),
            pos: rxpk.get_pos(),
            concentrator_sig: None,
        };

        Ok(packet)
    }

    pub fn is_potential_beacon(&self) -> bool {
        Self::parse_header(self.payload())
            .map(|header| header.mtype() == lorawan::MType::Proprietary)
            .unwrap_or(false)
    }

    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    pub fn parse_header(payload: &[u8]) -> Result<MHDR> {
        use std::io::Cursor;
        lorawan::MHDR::read(&mut Cursor::new(payload)).map_err(Error::from)
    }

    pub fn parse_frame(direction: lorawan::Direction, payload: &[u8]) -> Result<PHYPayloadFrame> {
        use std::io::Cursor;
        lorawan::PHYPayload::read(direction, &mut Cursor::new(payload))
            .map(|p| p.payload)
            .map_err(Error::from)
    }

    pub fn hash(&self) -> Vec<u8> {
        Sha256::digest(&self.payload).to_vec()
    }

    pub fn is_secure_packet(&self) -> bool {
        // Packets from Secure Concentrators always have the packet id key (because the key is used to match the packet signature)
        self.key.is_some()
    }

    pub fn packet_id(&self) -> Option<u32> {
        self.key
    }

    pub fn set_secure_sig(&mut self, sig: Vec<u8>) {
        self.concentrator_sig = Some(sig);
    }

    /// get the unix timestamp (in nanoseconds) of the packet arrival time
    pub fn unix_timestamp(&self) -> u64 {
        self.timestamp
    }

    pub fn get_pos(&self) -> Option<helium_proto::Wgs84Position> {
        match self.pos {
            Some(pos) => Some(convert_wgs84pos(pos)),
            None => None,
        }
    }
}

impl PacketDown {
    pub fn to_rx1_pull_resp(&self, tx_power: u32) -> Result<pull_resp::TxPk> {
        let rx1 = self.0.rx1.as_ref().ok_or_else(DecodeError::no_rx1_window)?;
        let time = if rx1.immediate {
            Time::immediate()
        } else {
            Time::by_tmst(rx1.timestamp as u32)
        };
        self.inner_to_pull_resp(
            time,
            rx1.frequency,
            datarate::from_proto(rx1.datarate())?,
            tx_power,
        )
    }

    pub fn to_rx2_pull_resp(&self, tx_power: u32) -> Result<Option<pull_resp::TxPk>> {
        let rx2 = match self.0.rx2.as_ref() {
            Some(window) => window,
            None => return Ok(None),
        };

        self.inner_to_pull_resp(
            Time::by_tmst(rx2.timestamp as u32),
            rx2.frequency,
            datarate::from_proto(rx2.datarate())?,
            tx_power,
        )
        .map(Some)
    }

    fn inner_to_pull_resp(
        &self,
        time: Time,
        frequency_hz: u32,
        datarate: DataRate,
        tx_power: u32,
    ) -> Result<pull_resp::TxPk> {
        Ok(pull_resp::TxPk {
            time,
            ipol: true,
            modu: Modulation::LORA,
            codr: CodingRate::_4_5,
            datr: datarate,
            // for normal lorawan packets we're not selecting different frequencies
            // like we are for PoC
            freq: to_mhz(frequency_hz),
            data: PhyData::new(self.0.payload.clone()),
            powe: tx_power as u64,
            rfch: 0,
            fdev: None,
            prea: None,
            ncrc: None,
        })
    }
}

pub(crate) fn to_mhz<H: Into<f64>>(hz: H) -> f64 {
    hz.into() / 1_000_000.0
}

pub(crate) mod datarate {
    use super::{DecodeError, Result};
    use helium_proto::DataRate as ProtoRate;
    use semtech_udp::{Bandwidth, DataRate, SpreadingFactor};

    pub fn from_proto(rate: ProtoRate) -> Result<DataRate> {
        let (spreading_factor, bandwidth) = match rate {
            ProtoRate::Sf12bw125 => (SpreadingFactor::SF12, Bandwidth::BW125),
            ProtoRate::Sf11bw125 => (SpreadingFactor::SF11, Bandwidth::BW125),
            ProtoRate::Sf10bw125 => (SpreadingFactor::SF10, Bandwidth::BW125),
            ProtoRate::Sf9bw125 => (SpreadingFactor::SF9, Bandwidth::BW125),
            ProtoRate::Sf8bw125 => (SpreadingFactor::SF8, Bandwidth::BW125),
            ProtoRate::Sf7bw125 => (SpreadingFactor::SF7, Bandwidth::BW125),

            ProtoRate::Sf12bw250 => (SpreadingFactor::SF12, Bandwidth::BW250),
            ProtoRate::Sf11bw250 => (SpreadingFactor::SF11, Bandwidth::BW250),
            ProtoRate::Sf10bw250 => (SpreadingFactor::SF10, Bandwidth::BW250),
            ProtoRate::Sf9bw250 => (SpreadingFactor::SF9, Bandwidth::BW250),
            ProtoRate::Sf8bw250 => (SpreadingFactor::SF8, Bandwidth::BW250),
            ProtoRate::Sf7bw250 => (SpreadingFactor::SF7, Bandwidth::BW250),

            ProtoRate::Sf12bw500 => (SpreadingFactor::SF12, Bandwidth::BW500),
            ProtoRate::Sf11bw500 => (SpreadingFactor::SF11, Bandwidth::BW500),
            ProtoRate::Sf10bw500 => (SpreadingFactor::SF10, Bandwidth::BW500),
            ProtoRate::Sf9bw500 => (SpreadingFactor::SF9, Bandwidth::BW500),
            ProtoRate::Sf8bw500 => (SpreadingFactor::SF8, Bandwidth::BW500),
            ProtoRate::Sf7bw500 => (SpreadingFactor::SF7, Bandwidth::BW500),

            ProtoRate::Lrfhss2bw137
            | ProtoRate::Lrfhss1bw336
            | ProtoRate::Lrfhss1bw137
            | ProtoRate::Lrfhss2bw336
            | ProtoRate::Lrfhss1bw1523
            | ProtoRate::Lrfhss2bw1523
            | ProtoRate::Fsk50 => {
                return Err(DecodeError::invalid_data_rate("unsupported".to_string()))
            }
        };
        Ok(DataRate::new(spreading_factor, bandwidth))
    }

    pub fn to_proto(rate: DataRate) -> ProtoRate {
        match (rate.spreading_factor(), rate.bandwidth()) {
            (SpreadingFactor::SF12, Bandwidth::BW125) => ProtoRate::Sf12bw125,
            (SpreadingFactor::SF11, Bandwidth::BW125) => ProtoRate::Sf11bw125,
            (SpreadingFactor::SF10, Bandwidth::BW125) => ProtoRate::Sf10bw125,
            (SpreadingFactor::SF9, Bandwidth::BW125) => ProtoRate::Sf9bw125,
            (SpreadingFactor::SF8, Bandwidth::BW125) => ProtoRate::Sf8bw125,
            (SpreadingFactor::SF7, Bandwidth::BW125) => ProtoRate::Sf7bw125,

            (SpreadingFactor::SF12, Bandwidth::BW250) => ProtoRate::Sf12bw250,
            (SpreadingFactor::SF11, Bandwidth::BW250) => ProtoRate::Sf11bw250,
            (SpreadingFactor::SF10, Bandwidth::BW250) => ProtoRate::Sf10bw250,
            (SpreadingFactor::SF9, Bandwidth::BW250) => ProtoRate::Sf9bw250,
            (SpreadingFactor::SF8, Bandwidth::BW250) => ProtoRate::Sf8bw250,
            (SpreadingFactor::SF7, Bandwidth::BW250) => ProtoRate::Sf7bw250,

            (SpreadingFactor::SF12, Bandwidth::BW500) => ProtoRate::Sf12bw500,
            (SpreadingFactor::SF11, Bandwidth::BW500) => ProtoRate::Sf11bw500,
            (SpreadingFactor::SF10, Bandwidth::BW500) => ProtoRate::Sf10bw500,
            (SpreadingFactor::SF9, Bandwidth::BW500) => ProtoRate::Sf9bw500,
            (SpreadingFactor::SF8, Bandwidth::BW500) => ProtoRate::Sf8bw500,
            (SpreadingFactor::SF7, Bandwidth::BW500) => ProtoRate::Sf7bw500,
        }
    }
}

fn convert_wgs84pos(input: WGS84Position) -> helium_proto::Wgs84Position {
    helium_proto::Wgs84Position {
        latitude: input.lat,
        longitude: input.lon,
        height: input.height,
        hacc: input.hacc,
        vacc: input.vacc,
    }
}

fn convert_gps_time(input: GPSTime) -> helium_proto::GpsTime {
    let duration = input.as_duration();
    helium_proto::GpsTime {
        sec: duration.as_secs(),
        nano: duration.subsec_nanos(),
    }
}

/// Frequency (hz)
#[derive(Debug, Clone, Copy)]
struct Frequency(u32);

impl Frequency {
    fn from_mhz(mhz: f64) -> Self {
        Self((mhz * 1_000_000_f64).trunc() as u32)
    }

    fn hz(&self) -> u32 {
        self.0
    }

    fn to_mhz(&self) -> f32 {
        self.0 as f32 / 1_000_000_f32
    }
}

impl Display for Frequency {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:.2} MHz", self.to_mhz())
    }
}

/// Signal to Noise Ratio (0.01dB)
#[derive(Debug, Clone, Copy)]
struct Snr(i32);

impl Snr {
    fn from_db(db: f32) -> Self {
        Self((db * 10_f32).trunc() as i32)
    }

    fn centi_db(&self) -> i32 {
        self.0
    }

    fn db(&self) -> f32 {
        self.0 as f32 / 10_f32
    }
}

impl Display for Snr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:.1} dB", self.db())
    }
}

/// RSSI (dBm)
#[derive(Debug, Clone, Copy)]
struct Rssi(i32);

impl Rssi {
    fn from_dbm(dbm: i32) -> Self {
        Self(dbm)
    }

    fn dbm(&self) -> i32 {
        self.0
    }

    fn centi_dbm(&self) -> i32 {
        self.0 * 10
    }
}

impl Display for Rssi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} dBm", self.dbm())
    }
}
