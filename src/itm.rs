/*
 * Copyright 2020 Oxide Computer Company
 */

use crate::debug::Register;
use bitfield::bitfield;
use crate::register;
use crate::tpiu::*;
use std::error::Error;

/*
 * ITM Trace Enable Register
 */
register!(ITM_TER, 0xe000_0e00,
    #[derive(Copy, Clone)]
    #[allow(non_camel_case_types)]
    pub struct ITM_TER(u32);
    impl Debug;
    pub enabled, set_enabled: 31, 0;
);

/*
 * ITM Trace Configuration Register
 */
register!(ITM_TCR, 0xe000_0e80,
    #[derive(Copy, Clone)]
    #[allow(non_camel_case_types)]
    pub struct ITM_TCR(u32);
    impl Debug;
    pub itm_busy, _: 23;
    pub traceid, set_traceid: 22, 16;
    pub timestamp_prescaler, _: 9, 8;
    pub swo_enable, _: 4;
    pub dwt_enable, set_dwt_enable: 3;
    pub sync_enable, set_sync_enable: 2;
    pub timestamp_enable, set_timestamp_enable: 1;
    pub itm_enable, set_itm_enable: 0;
);

/*
 * ITM Lock Access Register
 */
register!(ITM_LAR, 0xe000_0fb0,
    #[derive(Copy, Clone)]
    #[allow(non_camel_case_types)]
    pub struct ITM_LAR(u32);
    impl Debug;
    pub key, _: 1;
);

/*
 * ITM Lock Status Register
 */
register!(ITM_LSR, 0xe000_0fb4,
    #[derive(Copy, Clone)]
    #[allow(non_camel_case_types)]
    pub struct ITM_LSR(u32);
    impl Debug;
    pub locked, _: 1;
    pub unlock_required, _: 0;
);

impl ITM_LAR {
    pub fn unlock(core: &probe_rs::Core) -> Result<(), probe_rs::Error> {
        /*
         * To unlock, we write "CoreSight Access" in l33t
         */
        let val: u32 = 0xc5_acce55;
        core.write_word_32(ITM_LAR::ADDRESS, val)?;
        Ok(())
    }

    pub fn lock(core: &probe_rs::Core) -> Result<(), probe_rs::Error> {
        let val: u32 = 0x1de_c0de;
        core.write_word_32(ITM_LAR::ADDRESS, val)?;
        Ok(())
    }
}

#[derive(Debug)]
pub enum ITMPayload {
    None,
    LocalTimestamp { timestamp: u32, delayed: bool, early: bool },
    Extension { payload: u32, sh: bool },
    GlobalTimestamp { timestamp: u64 },
    Instrumentation { port: u32, payload: Vec<u8> },
    Hardware { source: u32, payload: [u8; 4], len: usize },
}

#[derive(Copy, Clone, Debug)]
pub enum ITMHeader {
    Sync,
    Overflow,
    LocalTimestamp1 { tc: u8 },
    LocalTimestamp2 { ts: u8 },
    GlobalTimestamp1,
    GlobalTimestamp2,
    Extension { c: bool, d: u8, s: bool },
    Instrumentation { a: u8, ss: u8 },
    Hardware { a: u8, ss: u8 }
}

#[derive(Copy, Clone, Debug)]
enum ITMPacketState {
    AwaitingHeader,
    AwaitingPayload,
    Complete
}

#[derive(Debug)]
pub struct ITMPacket {
    pub header: ITMHeader,
    pub payload: ITMPayload,
    pub offset: usize,
    pub time: f64,
}

fn encode(hdr: ITMHeader) -> u8 {
    match hdr {
        ITMHeader::Sync => 0,
        ITMHeader::Overflow => 0b0111_0000,
        ITMHeader::LocalTimestamp1 { tc } => {
            assert!(tc <= 0b11);
            0b1100_0000 | (tc << 4)
        }

        ITMHeader::LocalTimestamp2 { ts } => {
            assert!(ts != 0);
            assert!(ts < 0b111);
            0b0000_0000 | (ts << 4)
        }

        ITMHeader::GlobalTimestamp1 => 0b1001_0100,
        ITMHeader::GlobalTimestamp2 => 0b1011_0100,
        ITMHeader::Extension { c, d, s } => {
            let sh = if s { 1 << 2 } else { 0 };
            0b0000_1000 | (d & 0b111) << 4 | if c { 1 << 7 } else { 0 } | sh
        }

        ITMHeader::Instrumentation { a, ss } => {
            assert!(ss != 0);
            assert!((a >> 5) == 0);
            0b0000_0000 | (a << 3) | ss
        }

        ITMHeader::Hardware { a, ss } => {
            assert!(ss != 0);
            assert!((a >> 5) == 0);
            0b0000_0100 | (a << 3) | ss
        }
    }
}

fn set(table: &mut Vec<Option<ITMHeader>>, hdr: ITMHeader)
{
    let val = encode(hdr) as usize;

    match table[val] {
        None => { table[val] = Some(hdr); }
        Some(h) => {
            panic!("two values for ITM header 0x{:x} (0b{:b}): {:?} and {:?}",
                val, val, h, hdr);
        }
    }
}

fn itm_hdrs() -> Vec<Option<ITMHeader>>
{
    let mut hdr: Vec<Option<ITMHeader>> = vec![None; 256];
    let bools = [ false, true ];

    set(&mut hdr, ITMHeader::Sync);
    set(&mut hdr, ITMHeader::Overflow);

    for a in 0..32 {
        for ss in 1..=0b11 {
            set(&mut hdr, ITMHeader::Instrumentation { a: a, ss: ss });
            set(&mut hdr, ITMHeader::Hardware { a: a, ss: ss });
        }
    }

    for i in 0..=0b11 {
        set(&mut hdr, ITMHeader::LocalTimestamp1 { tc: i });
    }

    for i in 1..0b111 {
        set(&mut hdr, ITMHeader::LocalTimestamp2 { ts: i });
    }

    for i in 0..=0b111 {
        for s in bools.iter() {
            for c in bools.iter() {
                set(&mut hdr, ITMHeader::Extension { c: *c, d: i, s: *s });
            }
        }
    }

    set(&mut hdr, ITMHeader::GlobalTimestamp1);
    set(&mut hdr, ITMHeader::GlobalTimestamp2);

    hdr
}

fn itm_packet_state(
    hdr: ITMHeader,
    payload: &Vec<u8>,
) -> ITMPacketState
{
    let expect = |size: u8| {
        if payload.len() < size as usize {
            ITMPacketState::AwaitingPayload
        } else {
            ITMPacketState::Complete
        }
    };

    let compressed = |max: u8| {
        let mut ndx: u8 = 0;

        while ndx < payload.len() as u8 {
            if ndx == max - 1 || (payload[ndx as usize] & 0b1000_0000) != 0 {
                break;
            }

            ndx += 1;
        }

        ndx + 1
    };

    match hdr {
        ITMHeader::Sync => { expect(5) }
        ITMHeader::Overflow => { ITMPacketState::Complete }
        ITMHeader::LocalTimestamp1 { .. } => { expect(compressed(4)) }
        ITMHeader::LocalTimestamp2 { .. } => { ITMPacketState::Complete }
        ITMHeader::GlobalTimestamp1 => { expect(compressed(4)) }
        ITMHeader::GlobalTimestamp2 => { expect(4) }
        ITMHeader::Extension { .. } => { expect(compressed(4)) }
        ITMHeader::Instrumentation { a: _, ss } |
        ITMHeader::Hardware { a: _, ss } => {
            expect(match ss {
                0b01 => 1,
                0b10 => 2,
                0b11 => 4,
                _ => panic!("invalid ss")
            })
        }
    }
}

fn itm_payload_decode(
    hdr: ITMHeader,
    payload: &Vec<u8>,
) -> ITMPayload {

    match hdr {
        ITMHeader::Instrumentation { a, .. } => {
//            let mut p: [u8; 4] = [0; 4];
//            payload.iter().enumerate().for_each(|v| { p[v.0] = *v.1; });

            ITMPayload::Instrumentation {
                port: a as u32,
                // len: payload.len(),
                payload: payload.clone()
            }
        }
        _ => { ITMPayload::None }
    }
}

pub fn itm_ingest(
    traceid: u8,
    mut readnext: impl FnMut() -> Result<Option<(u8, f64)>, Box<dyn Error>>,
    mut callback: impl FnMut(&ITMPacket) -> Result<(), Box<dyn Error>>,
) -> Result<(), Box<dyn Error>> {

    #[derive(Copy, Clone, Debug)]
    enum IngestState { SyncSearching, Ingesting };

    let mut state: IngestState = IngestState::SyncSearching;
    let mut pstate: ITMPacketState = ITMPacketState::AwaitingHeader;
    let mut vec = Vec::with_capacity(16);

    let mut valid = vec![false; 256];
    valid[traceid as usize] = true;

    let hdrs = &itm_hdrs();
    let mut hdr = ITMHeader::Sync;
    let mut runlen = 0;

    tpiu_ingest(&valid, &mut readnext, |packet| {
        let payload = &mut vec;

        match state {
            IngestState::SyncSearching => {
                match packet.datum {
                    0 => { runlen += 1 }
                    0x80 => {
                        if runlen >= 5 {
                            info!(concat!("Synchronization ",
                                "packet found at line {}"), packet.offset);
                            state = IngestState::Ingesting;
                        }
                    }
                    _ => { runlen = 0; }
                }

                return Ok(());
            }
            _ => {}
        }

        match pstate {
            ITMPacketState::AwaitingHeader => {
                hdr = match hdrs[packet.datum as usize] {
                    Some(hdr) => { hdr }
                    None => {
                        panic!("unrecognized ITM header 0x{:x} at line {}",
                            packet.datum, packet.offset);
                    }
                };

                payload.truncate(0);
            }

            ITMPacketState::AwaitingPayload => {
                payload.push(packet.datum);
            }

            ITMPacketState::Complete => {
                panic!("unexpected packet state");
            }
        }

        pstate = itm_packet_state(hdr, &payload);

        match pstate {
            ITMPacketState::AwaitingHeader | 
            ITMPacketState::AwaitingPayload => {
                return Ok(());
            }
            ITMPacketState::Complete => {}
        }

        match state {
            IngestState::Ingesting => {
                callback(&ITMPacket {
                    header: hdr,
                    payload: itm_payload_decode(hdr, payload),
                    offset: packet.offset,
                    time: packet.time
                })?;
            }
            _ => {
                unreachable!();
            }
        }

        pstate = ITMPacketState::AwaitingHeader;

        Ok(())
    })
}