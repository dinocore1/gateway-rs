// This module provides time-on-air regulatory compliance for the
// LoraWAN ISM bands.
//
// This module does not interface with hardware or provide any
// transmission capabilities itself. Instead, the API provides its
// core functionality through `track_sent', `can_send', and
// `time_on_air'.

use crate::Region;
use std::{cmp::max, ops::Div, str::FromStr};

pub const MAX_TIME_ON_AIR: f32 = 400.0;

#[derive(Debug)]
pub struct LoraThrottle {
    model: Option<LoraRegulatoryModel>,
    sent_packets: Vec<SentPacket>,
}
#[derive(PartialEq, Debug)]
enum LoraRegulatoryModel {
    Dwell { limit: f32, period: u32 },
    Duty { limit: f32, period: u32 },
}

#[derive(Debug)]
struct SentPacket {
    frequency: f32,
    sent_at: i64,
    time_on_air: f32,
}

impl PartialEq for SentPacket {
    fn eq(&self, other: &Self) -> bool {
        self.sent_at == other.sent_at
    }
}

impl Eq for SentPacket {}

impl PartialOrd for SentPacket {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.sent_at.partial_cmp(&other.sent_at)
    }
}

impl Ord for SentPacket {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.sent_at.cmp(&other.sent_at)
    }
}

impl LoraRegulatoryModel {
    pub fn common_duty() -> Self {
        Self::Duty {
            limit: 0.01,
            period: 3600000,
        }
    }

    pub fn us_dwell_time() -> Self {
        Self::Dwell {
            limit: 400.0,
            period: 20000,
        }
    }

    pub fn period(&self) -> u32 {
        match self {
            Self::Duty { period, .. } => *period,
            Self::Dwell { period, .. } => *period,
        }
    }

    pub fn can_send(
        &self,
        sent_packets: &[SentPacket],
        at_time: i64,
        frequency: f32,
        time_on_air: f32,
    ) -> bool {
        match self {
            Self::Dwell { period, limit } => {
                let cutoff_time = (at_time - *period as i64) as f32 + time_on_air;
                eprintln!("CUTOFF {}", cutoff_time);
                let projected_dwell_time =
                    dwell_time(sent_packets, cutoff_time, Some(frequency)) + time_on_air;
                eprintln!(
                    "PROJECTED {} limit {} x {}",
                    projected_dwell_time,
                    limit,
                    projected_dwell_time <= *limit
                );
                projected_dwell_time <= *limit
            }
            Self::Duty { period, limit } => {
                let cutoff_time = (at_time - *period as i64) as f32;
                let current_dwell = dwell_time(sent_packets, cutoff_time, None);
                (current_dwell + time_on_air) / (*period as f32) < *limit
            }
        }
    }
}

impl From<LoraRegulatoryModel> for LoraThrottle {
    fn from(v: LoraRegulatoryModel) -> Self {
        Self {
            sent_packets: vec![],
            model: Some(v),
        }
    }
}

impl LoraThrottle {
    pub fn track_sent(&mut self, sent_at: i64, frequency: f32, time_on_air: f32) {
        let model = if let Some(model) = &self.model {
            model
        } else {
            return;
        };
        let sent_packet = SentPacket {
            frequency,
            sent_at,
            time_on_air,
        };
        let sort = self
            .sent_packets
            .last()
            .map(|last_packet| &sent_packet < last_packet)
            .unwrap_or(false);
        self.sent_packets.push(sent_packet);
        if sort {
            self.sent_packets.sort_unstable();
        }
        if let Some(last_packet) = self.sent_packets.last() {
            let cutoff_time = last_packet.sent_at - model.period() as i64;
            self.sent_packets
                .retain(|sent_packet| sent_packet.sent_at > cutoff_time)
        }
    }

    // Based on previously sent packets, returns a boolean value if
    // it is legal to send on Frequency at time Now.
    pub fn can_send(&self, at_time: i64, frequency: f32, time_on_air: f32) -> bool {
        if let Some(model) = &self.model {
            if time_on_air > MAX_TIME_ON_AIR {
                return false;
            }
            model.can_send(&self.sent_packets, at_time, frequency, time_on_air)
        } else {
            false
        }
    }
}

// Returns total time on air for packet sent with given parameters.
//
// See Semtech Appnote AN1200.13, "LoRa Modem Designer's Guide"
fn time_on_air(
    bandwidth: f32,
    spreading_factor: u32,
    code_rate: u32,
    preamble_symbols: u32,
    explicit_header: bool,
    payload_len: usize,
) -> f32 {
    let symbol_duration = symbol_duration(bandwidth, spreading_factor);
    let payload_symbols = payload_symbols(
        spreading_factor,
        code_rate,
        explicit_header,
        payload_len,
        (bandwidth <= 125000_f32) && (spreading_factor >= 11),
    );
    symbol_duration * (4.25 + preamble_symbols as f32 + payload_symbols as f32)
}

// Returns the number of payload symbols required to send payload.
fn payload_symbols(
    spreading_factor: u32,
    code_rate: u32,
    explicit_header: bool,
    payload_len: usize,
    low_datarate_optimized: bool,
) -> u32 {
    let eh = u32::from(explicit_header);
    let ldo = u32::from(low_datarate_optimized);
    8 + (max(
        ((8 * (payload_len as u32) - 4 * spreading_factor + 28 + 16 - 20 * (1 - eh)) as f32
            / (4 * (spreading_factor - 2 * ldo)) as f32)
            .ceil() as u32
            * code_rate,
        0,
    ))
}

fn symbol_duration(bandwidth: f32, spreading_factor: u32) -> f32 {
    2u32.pow(spreading_factor) as f32 / bandwidth
}

// Computes the total time on air for packets sent on Frequency
// and no older than a given cutoff time.
fn dwell_time(sent_packets: &[SentPacket], cutoff_time: f32, frequency: Option<f32>) -> f32 {
    let mut dwell_time: f32 = 0.0;
    for sent_packet in sent_packets {
        let sent_at = sent_packet.sent_at as f32;
        // Scenario 1: entire packet sent before cutoff_time
        if (sent_at + sent_packet.time_on_air) < cutoff_time {
            continue;
        }
        // Scenario 2: packet sent on non-relevant frequency.
        if let Some(frequency) = frequency {
            if sent_packet.frequency != frequency {
                continue;
            }
        }
        // Scenario 3: Packet started before cutoff_time but finished after cutoff_time.
        if sent_at <= cutoff_time {
            let relevant_time_on_air = sent_packet.time_on_air - (cutoff_time - sent_at);
            assert!(relevant_time_on_air >= 0.0);
            dwell_time += relevant_time_on_air;
        } else {
            // Scenario 4: 100 % of packet transmission after CutoffTime.
            dwell_time += sent_packet.time_on_air;
        }
    }
    return dwell_time;
}

#[cfg(test)]
mod test {
    use super::*;

    // Converts floating point seconds to integer seconds to remove
    // floating point ambiguity from test cases.
    fn ms(seconds: f32) -> u32 {
        (seconds * 1000.0) as u32
    }
    // Test cases generated with https://www.loratools.nl/#/airtime and
    // truncated to milliseconds.
    #[test]
    fn test_us_time_on_air() {
        assert_eq!(991, ms(time_on_air(125.0e3, 12, 5, 8, true, 7)));
        assert_eq!(2465, ms(time_on_air(125.0e3, 12, 5, 8, true, 51)));

        assert_eq!(495, ms(time_on_air(125.0e3, 11, 5, 8, true, 7)));
        assert_eq!(1314, ms(time_on_air(125.0e3, 11, 5, 8, true, 51)));

        assert_eq!(247, ms(time_on_air(125.0e3, 10, 5, 8, true, 7)));
        assert_eq!(616, ms(time_on_air(125.0e3, 10, 5, 8, true, 51)));

        assert_eq!(123, ms(time_on_air(125.0e3, 9, 5, 8, true, 7)));
        assert_eq!(328, ms(time_on_air(125.0e3, 9, 5, 8, true, 51)));

        assert_eq!(72, ms(time_on_air(125.0e3, 8, 5, 8, true, 7)));
        assert_eq!(184, ms(time_on_air(125.0e3, 8, 5, 8, true, 51)));

        assert_eq!(36, ms(time_on_air(125.0e3, 7, 5, 8, true, 7)));
        assert_eq!(102, ms(time_on_air(125.0e3, 7, 5, 8, true, 51)));
    }

    #[test]
    fn us915_dwell_time_test() {
        let max_dwell: f32 = 400.0;
        let period: i64 = 20000;
        let half_max = max_dwell.div(2.0);
        let quarter_max = max_dwell.div(4.0);
        // There are no special frequencies in region US915, so the
        // lorareg API doesn't care what values you use for Frequency
        // arguments as long as they are distinct and comparable. We can
        // use channel number instead like so.
        let ch0: f32 = 0.0;
        let ch1: f32 = 1.0;
        // Time naught. Times can be negative as the only requirement lorareg places
        // is on time is that it is monotonically increasing and expressed as
        // milliseconds.
        let t0: i64 = -123456789;

        let mut throttle = LoraThrottle::from(LoraRegulatoryModel::us_dwell_time());
        throttle.track_sent(t0, ch0, max_dwell);
        throttle.track_sent(t0, ch1, half_max);
        eprintln!("{:?}", throttle);

        assert_eq!(false, throttle.can_send(t0 + 100, ch0, max_dwell));
        assert_eq!(true, throttle.can_send(t0, ch1, half_max));
        assert_eq!(false, throttle.can_send(t0 + 1, ch0, max_dwell));
        assert_eq!(true, throttle.can_send(t0 + 1, ch1, half_max));

        assert_eq!(false, throttle.can_send(t0 + period - 1, ch0, max_dwell));
        assert_eq!(true, throttle.can_send(t0 + period, ch0, max_dwell));
        assert_eq!(true, throttle.can_send(t0 + period + 1, ch0, max_dwell));

        // The following cases are all allowed because no matter how you vary
        // the start time this transmission, (half_max + half_max) ratifies the
        // constrain of `<= max_dwell'.
        assert_eq!(
            true,
            throttle.can_send(t0 + period - half_max as i64 - 1, ch1, half_max)
        );
        assert_eq!(
            true,
            throttle.can_send(t0 + period - half_max as i64, ch1, half_max)
        );
        assert_eq!(
            true,
            throttle.can_send(t0 + period - half_max as i64 + 1, ch1, half_max)
        );

        // None of the following cases are allowed because they all exceed maximum
        // dwell time by 1.
        assert_eq!(
            false,
            throttle.can_send(t0 + period - half_max as i64 - 1, ch1, half_max + 1.0)
        );
        assert_eq!(
            false,
            throttle.can_send(t0 + period - half_max as i64 - 2, ch1, half_max + 1.0)
        );
        assert_eq!(
            false,
            throttle.can_send(t0 + period - half_max as i64 - 3, ch1, half_max + 1.0)
        );

        // The following cases are all allowed because they all begin a full period
        // of concern after the currently tracked transmissions.
        assert_eq!(
            true,
            throttle.can_send(t0 + period + max_dwell as i64, ch0, max_dwell)
        );
        assert_eq!(
            true,
            throttle.can_send(t0 + period + max_dwell as i64, ch1, max_dwell)
        );

        // Let's finish of by tracking two more small packets of 1/4 maximum dwell
        // in length and asserting that there is no more time left in the [T0, T0 +
        // Period) for even a packet of 1ms in duration.
        assert_eq!(
            true,
            throttle.can_send(t0 + period.div(4), ch1, quarter_max)
        );
        throttle.track_sent(t0 + period.div(4), ch1, quarter_max);
        assert_eq!(
            true,
            throttle.can_send(t0 + (period as f32 * 0.75) as i64, ch1, quarter_max)
        );
        throttle.track_sent(t0 + (period * 3).div(4), ch1, quarter_max);
        assert_eq!(false, throttle.can_send(t0 + period - 1, ch1, 1.0));

        // ... but one ms later, we're all clear to send a packet. Note that if had
        // sent that first packet on channel 1 even a ms later this would fail too.
        assert_eq!(true, throttle.can_send(t0 + period, ch1, 1.0));
    }
}

// eu868_duty_cycle_test() ->
//     MaxTimeOnAir = 400,
//     Ten_ms = 10,
//     Ch0 = 0,
//     Ch1 = 1,

//     S0 = new('EU868'),

//     assert_eq!(true, can_send(S0, 0, Ch0, MaxTimeOnAir)),
//     assert_eq!(false, can_send(S0, 0, Ch0, MaxTimeOnAir + 1)),
//     %% Send 3599 packets of duration 10ms on a single channel over the
//     %% course of one hour. All should be accepted because 3599 * 10ms
//     %% = 35.99s, or approx 0.9997 % duty-cycle.
//     {S1, Now} = lists:foldl(
//         fun (N, {State, _T}) ->
//             Now = (N - 1) * 1000,
//             assert_eq!(true, can_send(State, Now, Ch0, Ten_ms)),
//             {track_sent(State, Now, Ch0, Ten_ms), Now + 1000}
//         end,
//         {new('EU868'), 0},
//         lists:seq(1, 3599)
//     ),

//     %% Let's try sending on a different channel. This will fail
//     %% because, unlike FCC, ETSI rules limit overall duty-cycle and
//     %% not per-channel dwell. So despite being a different channel, if
//     %% this transmission were allowed, it raise our overall duty cycle
//     %% to exactly 1 %.
//     assert_eq!(false, can_send(S1, Now, Ch1, Ten_ms)),

//     ok.

// %% Converts floating point seconds to integer seconds to remove
// %% floating point ambiguity from test cases.
// ms(Seconds) ->
//     erlang:trunc(Seconds * 1000.0).
