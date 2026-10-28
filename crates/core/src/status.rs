//! Parser for the ZPL `~HS` (host status) reply.
//!
//! The printer answers with three STX..ETX framed, comma separated strings:
//!   1: aaa,b,c,dddd,eee,f,g,h,iii,j,k,l
//!   2: mmm,n,o,p,q,r,s,t,uuuuuuuu,v,www
//!   3: xxxx,0

use serde::Serialize;

use crate::{Error, Result};

const STX: u8 = 0x02;
const ETX: u8 = 0x03;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HostStatus {
    pub ready: bool,
    pub paper_out: bool,
    pub paused: bool,
    pub head_open: bool,
    pub ribbon_out: bool,
    pub thermal_transfer: bool,
    pub buffer_full: bool,
    pub corrupt_ram: bool,
    pub under_temperature: bool,
    pub over_temperature: bool,
    pub formats_in_buffer: u32,
    pub labels_remaining: u32,
}

pub fn parse_host_status(raw: &[u8]) -> Result<HostStatus> {
    let frames = frames(raw);
    if frames.len() < 2 {
        return Err(Error::BadResponse(format!(
            "expected 3 ~HS strings, got {}",
            frames.len()
        )));
    }
    let s1: Vec<&str> = frames[0].split(',').map(str::trim).collect();
    let s2: Vec<&str> = frames[1].split(',').map(str::trim).collect();
    if s1.len() < 12 || s2.len() < 11 {
        return Err(Error::BadResponse("~HS strings are too short".into()));
    }

    let paper_out = flag(&s1, 1);
    let paused = flag(&s1, 2);
    let buffer_full = flag(&s1, 5);
    let head_open = flag(&s2, 2);
    let ribbon_out = flag(&s2, 3);
    let thermal_transfer = flag(&s2, 4);

    Ok(HostStatus {
        // A missing ribbon only matters when the printer is in thermal transfer mode.
        ready: !(paper_out || paused || head_open || buffer_full || (ribbon_out && thermal_transfer)),
        paper_out,
        paused,
        head_open,
        ribbon_out,
        thermal_transfer,
        buffer_full,
        corrupt_ram: flag(&s1, 9),
        under_temperature: flag(&s1, 10),
        over_temperature: flag(&s1, 11),
        formats_in_buffer: num(&s1, 4),
        labels_remaining: num(&s2, 8),
    })
}

fn frames(raw: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut current: Option<Vec<u8>> = None;
    for &b in raw {
        match b {
            STX => current = Some(Vec::new()),
            ETX => {
                if let Some(frame) = current.take() {
                    out.push(String::from_utf8_lossy(&frame).into_owned());
                }
            }
            _ => {
                if let Some(frame) = current.as_mut() {
                    frame.push(b);
                }
            }
        }
    }
    out
}

fn flag(fields: &[&str], i: usize) -> bool {
    fields.get(i).is_some_and(|f| *f == "1")
}

fn num(fields: &[&str], i: usize) -> u32 {
    fields.get(i).and_then(|f| f.parse().ok()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const READY: &[u8] = b"\x02030,0,0,1245,000,0,0,0,000,0,0,0\x03\r\n\
                           \x02001,0,0,0,1,2,6,0,00000000,1,000\x03\r\n\
                           \x021234,0\x03\r\n";

    #[test]
    fn parses_ready_printer() {
        let s = parse_host_status(READY).unwrap();
        assert!(s.ready);
        assert!(s.thermal_transfer);
        assert_eq!(s.labels_remaining, 0);
    }

    #[test]
    fn detects_paper_out_and_head_open() {
        let raw = b"\x02030,1,0,1245,002,0,0,0,000,0,0,0\x03\r\n\
                    \x02001,0,1,0,0,2,6,0,00000005,1,000\x03\r\n\
                    \x021234,0\x03\r\n";
        let s = parse_host_status(raw).unwrap();
        assert!(!s.ready);
        assert!(s.paper_out);
        assert!(s.head_open);
        assert_eq!(s.formats_in_buffer, 2);
        assert_eq!(s.labels_remaining, 5);
    }

    #[test]
    fn ribbon_out_ignored_in_direct_thermal() {
        let raw = b"\x02030,0,0,1245,000,0,0,0,000,0,0,0\x03\
                    \x02001,0,0,1,0,2,6,0,00000000,1,000\x03\
                    \x021234,0\x03";
        assert!(parse_host_status(raw).unwrap().ready);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_host_status(b"hello").is_err());
    }
}
