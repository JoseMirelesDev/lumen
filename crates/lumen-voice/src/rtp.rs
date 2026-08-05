//! RTP packetization for OPUS audio (RFC 7587): one frame per packet, no
//! payload header. Sequence numbers and 48 kHz timestamps advance per frame.

use crate::audio::FRAME_SAMPLES;
use rtc::rtp::{Header, Packet};

pub struct AudioPacketizer {
    pub ssrc: u32,
    pub payload_type: u8,
    seq: u16,
    ts: u32,
}

impl AudioPacketizer {
    pub fn new(ssrc: u32, payload_type: u8) -> Self {
        Self { ssrc, payload_type, seq: rand::random(), ts: rand::random() }
    }

    /// Wrap one encoded OPUS frame in an RTP packet and advance the clock.
    pub fn packet(&mut self, frame: &[u8]) -> Packet {
        let pkt = Packet {
            header: Header {
                version: 2,
                padding: false,
                extension: false,
                marker: false,
                payload_type: self.payload_type,
                sequence_number: self.seq,
                timestamp: self.ts,
                ssrc: self.ssrc,
                csrc: vec![],
                extension_profile: 0x0BED,
                extensions: vec![],
                extensions_padding: 0,
            },
            payload: bytes::Bytes::copy_from_slice(frame),
        };
        self.seq = self.seq.wrapping_add(1);
        self.ts = self.ts.wrapping_add(FRAME_SAMPLES as u32);
        pkt
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_advances_seq_and_ts() {
        let mut p = AudioPacketizer::new(0xdead_beef, 111);
        let a = p.packet(&[1, 2, 3]);
        let b = p.packet(&[4, 5]);
        assert_eq!(a.header.ssrc, 0xdead_beef);
        assert_eq!(a.header.payload_type, 111);
        assert_eq!(a.header.version, 2);
        assert_eq!(b.header.sequence_number, a.header.sequence_number.wrapping_add(1));
        assert_eq!(b.header.timestamp, a.header.timestamp.wrapping_add(FRAME_SAMPLES as u32));
        assert_eq!(a.payload.as_ref(), &[1, 2, 3]);
    }
}
