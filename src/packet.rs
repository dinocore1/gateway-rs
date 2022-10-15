use crate::{error::DecodeError, Error, Result};
use helium_proto::{
    packet::PacketType, routing_information::Data as RoutingData, services::poc_lora,
    BlockchainStateChannelResponseV1, DataRate as ProtoDataRate, Eui, RoutingInformation,
    Window,
};
use lorawan::{Direction, PHYPayloadFrame, MHDR};
use nlighten::gps::{WGS84Position, GPSTime};
use semtech_udp::{
    pull_resp::{self, PhyData},
    push_data::{self, CRC},
    CodingRate, DataRate, Modulation, StringOrNum, MacAddress,
};
use sha2::{Digest, Sha256};
use std::{convert::TryFrom, fmt::{self, Display}, str::FromStr};

#[derive(Debug, Clone)]
pub struct Packet {
    /// Internal timestamp of "RX finished" event (32b unsigned)
    tmst: u32,

    datr: DataRate,

    payload: Vec<u8>,

    /// Received Signal Strength Indicator
    rssi: Rssi,

    /// Signal to Noise radio
    snr: Snr,

    /// Frequency
    freq: Frequency,

    /// Gateway
    gateway: MacAddress,

    routing: Option<RoutingInformation>,

    rx2_window: Option<Window>,

    oui: u32,

    packet_type: PacketType,

    // Metadata from Secure Concentrator
    key: Option<u32>,
    pos: Option<WGS84Position>,
    gps_time: Option<GPSTime>,
    concentrator_sig: Option<Vec<u8>>,
}

impl fmt::Display for Packet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!(
            "@{} us, {}, {:?}, snr: {}, rssi: {}, len: {}",
            self.tmst,
            self.freq,
            self.datr,
            self.snr,
            self.rssi,
            self.payload.len()
        ))
    }
}

impl TryFrom<push_data::RxPk> for Packet {
    type Error = Error;

    fn try_from(rxpk: push_data::RxPk) -> Result<Self> {
        if rxpk.get_crc_status() == &CRC::OK {
            let rssi = rxpk
                .get_signal_rssi()
                .unwrap_or_else(|| rxpk.get_channel_rssi());

            let (key, pos, gps_time) = match rxpk {
                push_data::RxPk::V3(ref p) => (Some(p.key), p.pos, p.gps_time),
                _ => (None, None, None),
            };

            let packet = Self {
                tmst: *rxpk.get_timestamp(),
                datr: rxpk.get_datarate(),
                rssi: Rssi::from_dbm(rssi),
                snr: Snr::from_db(rxpk.get_snr()),
                freq: Frequency::from_mhz(rxpk.get_frequency()),
                gateway: MacAddress::nil(),
                routing: Self::routing_information(&Self::parse_frame(
                    lorawan::Direction::Uplink,
                    rxpk.get_data(),
                )?)?,
                payload: rxpk.get_data().to_vec(),
                rx2_window: None,
                oui: 0,
                packet_type: PacketType::Lorawan.into(),
                key: key,
                pos: pos,
                gps_time: gps_time,
                concentrator_sig: None,
            };
            Ok(packet)
        } else {
            Err(DecodeError::invalid_crc())
        }
    }
}

impl From<helium_proto::Packet> for Packet {
    fn from(v: helium_proto::Packet) -> Self {
        Self {
            tmst: v.timestamp as u32,
            datr: DataRate::from_str(&v.datarate).unwrap(),
            payload: v.payload.to_vec(),
            rssi: Rssi::from_dbm(v.signal_strength as i32),
            snr: Snr::from_db(v.snr),
            freq: Frequency::from_mhz(v.frequency.into()),
            gateway: MacAddress::nil(),
            oui: v.oui,
            packet_type: v.r#type(),
            rx2_window: v.rx2_window,
            routing: v.routing,
            key: None,
            pos: None,
            gps_time: None,
            concentrator_sig: None,
        }
    }
}

impl Packet {
    pub fn routing(&self) -> &Option<RoutingInformation> {
        &self.routing
    }

    pub fn to_packet(self) -> helium_proto::Packet {
        helium_proto::Packet {
            oui: self.oui,
            r#type: self.packet_type.into(),
            payload: self.payload.to_vec(),
            timestamp: self.tmst.into(),
            signal_strength: self.rssi.dbm() as f32,
            frequency: self.freq.to_mhz(),
            datarate: self.datr.to_string(),
            snr: self.snr.db(),
            routing: self.routing,
            rx2_window: self.rx2_window,
        }
    }

    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    pub fn routing_information(frame: &PHYPayloadFrame) -> Result<Option<RoutingInformation>> {
        let routing_data = match frame {
            PHYPayloadFrame::JoinRequest(request) => Some(RoutingData::Eui(Eui {
                deveui: request.dev_eui,
                appeui: request.app_eui,
            })),
            PHYPayloadFrame::MACPayload(mac_payload) => {
                Some(RoutingData::Devaddr(mac_payload.dev_addr()))
            }
            _ => return Ok(None),
        };
        Ok(routing_data.map(|r| RoutingInformation { data: Some(r) }))
    }

    pub fn parse_frame(direction: lorawan::Direction, payload: &[u8]) -> Result<PHYPayloadFrame> {
        use std::io::Cursor;
        lorawan::PHYPayload::read(direction, &mut Cursor::new(payload))
            .map(|p| p.payload)
            .map_err(Error::from)
    }

    pub fn parse_header(payload: &[u8]) -> Result<MHDR> {
        use std::io::Cursor;
        lorawan::MHDR::read(&mut Cursor::new(payload)).map_err(Error::from)
    }

    pub fn is_potential_beacon(&self) -> bool {
        Self::parse_header(self.payload())
            .map(|header| header.mtype() == lorawan::MType::Proprietary)
            .unwrap_or(false)
    }

    pub fn to_pull_resp(&self, use_rx2: bool, tx_power: u32) -> Result<Option<pull_resp::TxPk>> {
        let (timestamp, frequency, datarate) = if use_rx2 {
            if let Some(rx2) = &self.rx2_window {
                (Some(rx2.timestamp), rx2.frequency, rx2.datarate.parse()?)
            } else {
                return Ok(None);
            }
        } else {
            (
                Some(self.tmst as u64),
                self.freq.to_mhz(),
                self.datr.clone(),
            )
        };
        Ok(Some(pull_resp::TxPk {
            imme: timestamp.is_none(),
            ipol: true,
            modu: Modulation::LORA,
            codr: CodingRate::_4_5,
            datr: datarate,
            // for normal lorawan packets we're not selecting different frequencies
            // like we are for PoC
            freq: frequency as f64,
            data: PhyData::new(self.payload.clone()),
            powe: tx_power as u64,
            rfch: 0,
            tmst: match timestamp {
                Some(t) => Some(StringOrNum::N(t as u32)),
                None => Some(StringOrNum::S("immediate".to_string())),
            },
            tmms: None,
            fdev: None,
            prea: None,
            ncrc: None,
        }))
    }

    pub fn from_state_channel_response(response: BlockchainStateChannelResponseV1) -> Option<Self> {
        response.downlink.map(Self::from)
    }

    pub fn hash(&self) -> Vec<u8> {
        Sha256::digest(&self.payload).to_vec()
    }

    pub fn dc_payload(&self) -> u64 {
        const DC_PAYLOAD_SIZE: usize = 24;
        let payload_size = self.payload().len();
        if payload_size <= DC_PAYLOAD_SIZE {
            1
        } else {
            // integer div/ceil from: https://stackoverflow.com/a/2745086
            ((payload_size + DC_PAYLOAD_SIZE - 1) / DC_PAYLOAD_SIZE) as u64
        }
    }

    pub fn secure_packet(&self) -> Result<poc_lora::SecurePacketV1> {
        let dr = match ProtoDataRate::from_str(&self.datr.to_string()) {
            Ok(value)
                if [
                    ProtoDataRate::Sf7bw125,
                    ProtoDataRate::Sf8bw125,
                    ProtoDataRate::Sf9bw125,
                    ProtoDataRate::Sf10bw125,
                    ProtoDataRate::Sf12bw125,
                ]
                .contains(&value) =>
            {
                value
            }
            _ => {
                return Err(Error::custom(format!(
                    "invalid beacon witness datarate: {}",
                    self.datr.to_string()
                )));
            }
        };
        Ok(poc_lora::SecurePacketV1 {
            freq: self.freq.hz().into(),
            datarate: dr as i32,
            snr: self.snr.centi_db(),
            rssi: self.rssi.dbm(),
            tmst: self.tmst,
            card_id: self.gateway.clone().into_array().to_vec(),
            pos: self.pos.map(convert_wgs84pos),
            time: self.gps_time.map(convert_gps_time),
            signature: self.concentrator_sig.as_ref().ok_or_else(|| Error::custom("missing concentrator_sig"))?.to_vec(),
        })
    }

    pub fn to_witness_report(self) -> Result<poc_lora::LoraWitnessReportReqV1> {
        let payload = match Self::parse_frame(Direction::Uplink, self.payload()) {
            Ok(PHYPayloadFrame::Proprietary(payload)) => payload,
            _ => return Err(Error::custom("not a beacon")),
        };
        let dr = match ProtoDataRate::from_str(&self.datr.to_string()) {
            Ok(value)
                if [
                    ProtoDataRate::Sf7bw125,
                    ProtoDataRate::Sf8bw125,
                    ProtoDataRate::Sf9bw125,
                    ProtoDataRate::Sf10bw125,
                    ProtoDataRate::Sf12bw125,
                ]
                .contains(&value) =>
            {
                value
            }
            _ => {
                return Err(Error::custom(format!(
                    "invalid beacon witness datarate: {}",
                    self.datr.to_string()
                )));
            }
        };
        let report = poc_lora::LoraWitnessReportReqV1 {
            pub_key: vec![],
            data: payload,
            timestamp: self.tmst.into(),
            ts_res: 0,
            signal: self.rssi.centi_dbm(),
            snr: self.snr.centi_db(),
            frequency: self.freq.hz().into(),
            datarate: dr as i32,
            signature: vec![],
            secure_pkt: self.secure_packet().ok()
        };
        Ok(report)
    }

    pub fn packet_key(&self) -> Option<u32> {
        self.key
    }

    pub fn set_secure_sig(&mut self, sig: Vec<u8>) {
        self.concentrator_sig = Some(sig);
    }

    /// did this packet come from a Secure Concentrator?
    pub fn is_secure_packet(&self) -> bool {
        // Secure Packets always have the populated key
        self.key.is_some()
    }
}

fn convert_wgs84pos(input: WGS84Position) -> poc_lora::Wgs84Position {
    poc_lora::Wgs84Position {
        latitude: input.lat,
        longitude: input.lon,
        height: input.height,
        hacc: input.hacc,
        vacc: input.vacc,
    }
}

fn convert_gps_time(input: GPSTime) -> poc_lora::GpsTime {
    let duration = input.as_duration();
    poc_lora::GpsTime {
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
