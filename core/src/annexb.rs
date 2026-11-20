//! HEVC Annex-B helpers: split NAL units, find parameter sets, and convert to
//! 4-byte length-prefixed ("hvcC"/AVCC style) samples for VideoToolbox.
//! All functions are zero-copy / allocation-free.

pub const NAL_VPS: u8 = 32;
pub const NAL_SPS: u8 = 33;
pub const NAL_PPS: u8 = 34;
pub const NAL_AUD: u8 = 35;

/// HEVC NAL unit type from the first header byte.
pub fn nal_type(nal: &[u8]) -> u8 {
    nal.first().map_or(0xff, |b| (b >> 1) & 0x3f)
}

/// IRAP pictures (BLA/IDR/CRA, types 16..=23) are random-access points.
pub fn is_irap(nal: &[u8]) -> bool {
    (16..=23).contains(&nal_type(nal))
}

/// VCL (slice) NAL units are types 0..=31.
pub fn is_vcl(nal: &[u8]) -> bool {
    nal_type(nal) < 32
}

/// Splits a raw Annex-B elementary stream into access units (frames). Each
/// item is a slice of `data` including its start codes. A new AU begins at an
/// AUD, VPS/SPS/PPS or prefix SEI, or at a slice with
/// first_slice_segment_in_pic_flag set, once the current AU has a slice.
pub fn access_units(data: &[u8]) -> AccessUnits<'_> {
    // Start at 0 so the first AU keeps a 4-byte start code's leading zero.
    AccessUnits { data, pos: 0 }
}

pub struct AccessUnits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Iterator for AccessUnits<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        let d = self.data;
        if self.pos >= d.len() {
            return None;
        }
        let start = self.pos;
        let mut seen_vcl = false;
        let mut sc = find_start_code(d, start);
        while let Some((sc_start, nal_start)) = sc {
            let nal = &d[nal_start..];
            let t = nal_type(nal);
            let begins_au = matches!(t, NAL_AUD | NAL_VPS | NAL_SPS | NAL_PPS | 39)
                || (is_vcl(nal) && nal.get(2).is_some_and(|b| b & 0x80 != 0));
            if seen_vcl && begins_au {
                self.pos = sc_start;
                return Some(&d[start..sc_start]);
            }
            seen_vcl |= is_vcl(nal);
            sc = find_start_code(d, nal_start);
        }
        self.pos = d.len();
        Some(&d[start..])
    }
}

/// Iterator over the NAL units in an Annex-B byte stream (start codes removed,
/// trailing zero bytes stripped).
pub struct NalUnits<'a> {
    rest: &'a [u8],
}

pub fn nal_units(data: &[u8]) -> NalUnits<'_> {
    let start = find_start_code(data, 0).map_or(data.len(), |(_, after)| after);
    NalUnits {
        rest: &data[start..],
    }
}

/// Finds the next 00 00 01 at or after `from`. Returns (index of the first
/// zero, index just after the 01). Emulation prevention (00 00 03) guarantees
/// this sequence never occurs inside a NAL.
fn find_start_code(data: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut i = from;
    while i + 3 <= data.len() {
        if data[i + 2] > 1 {
            i += 3;
        } else if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            return Some((i, i + 3));
        } else {
            i += 1;
        }
    }
    None
}

impl<'a> Iterator for NalUnits<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        loop {
            if self.rest.is_empty() {
                return None;
            }
            let (end, next) = match find_start_code(self.rest, 0) {
                Some((s, after)) => (s, after),
                None => (self.rest.len(), self.rest.len()),
            };
            let mut nal = &self.rest[..end];
            self.rest = &self.rest[next..];
            while let [head @ .., 0] = nal {
                nal = head; // trailing_zero_8bits / leading zero of a 4-byte start code
            }
            if !nal.is_empty() {
                return Some(nal);
            }
        }
    }
}

/// VPS/SPS/PPS found in an access unit (first of each).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ParamSets<'a> {
    pub vps: Option<&'a [u8]>,
    pub sps: Option<&'a [u8]>,
    pub pps: Option<&'a [u8]>,
}

impl<'a> ParamSets<'a> {
    pub fn complete(&self) -> bool {
        self.vps.is_some() && self.sps.is_some() && self.pps.is_some()
    }
}

pub fn param_sets(data: &[u8]) -> ParamSets<'_> {
    let mut ps = ParamSets::default();
    for nal in nal_units(data) {
        let slot = match nal_type(nal) {
            NAL_VPS => &mut ps.vps,
            NAL_SPS => &mut ps.sps,
            NAL_PPS => &mut ps.pps,
            _ => continue,
        };
        slot.get_or_insert(nal);
    }
    ps
}

fn skip_in_sample(nal: &[u8]) -> bool {
    matches!(nal_type(nal), NAL_VPS | NAL_SPS | NAL_PPS | NAL_AUD)
}

/// Bytes needed by [`to_length_prefixed`] for this access unit.
pub fn length_prefixed_len(data: &[u8]) -> usize {
    nal_units(data)
        .filter(|n| !skip_in_sample(n))
        .map(|n| 4 + n.len())
        .sum()
}

/// Writes every non-parameter-set, non-AUD NAL as `[u32 BE len][nal]` into
/// `out`. Returns bytes written, or `None` if `out` is too small.
pub fn to_length_prefixed(data: &[u8], out: &mut [u8]) -> Option<usize> {
    let mut o = 0;
    for nal in nal_units(data).filter(|n| !skip_in_sample(n)) {
        let end = o + 4 + nal.len();
        if end > out.len() {
            return None;
        }
        out[o..o + 4].copy_from_slice(&(nal.len() as u32).to_be_bytes());
        out[o + 4..end].copy_from_slice(nal);
        o = end;
    }
    Some(o)
}

/// Builds an Annex-B access unit from encoder output: each of `prefix` (raw
/// NALs, e.g. VPS/SPS/PPS on a keyframe) and then every 4-byte
/// length-prefixed NAL in `sample` (VideoToolbox's format), each behind a
/// 4-byte start code. Returns bytes written, or `None` if `sample` is
/// malformed or `out` is too small.
pub fn from_length_prefixed(prefix: &[&[u8]], sample: &[u8], out: &mut [u8]) -> Option<usize> {
    const START: [u8; 4] = [0, 0, 0, 1];
    let mut o = 0;
    let mut put = |nal: &[u8], o: &mut usize| -> Option<()> {
        let end = *o + 4 + nal.len();
        out.get_mut(*o..end)?;
        out[*o..*o + 4].copy_from_slice(&START);
        out[*o + 4..end].copy_from_slice(nal);
        *o = end;
        Some(())
    };
    for nal in prefix {
        put(nal, &mut o)?;
    }
    let mut i = 0;
    while i < sample.len() {
        let len = u32::from_be_bytes(sample.get(i..i + 4)?.try_into().ok()?) as usize;
        let nal = sample.get(i + 4..i + 4 + len)?;
        if nal.is_empty() {
            return None;
        }
        put(nal, &mut o)?;
        i += 4 + len;
    }
    Some(o)
}

#[cfg(test)]
mod tests {
    use super::*;

    // NAL headers: type << 1. VPS=0x40, SPS=0x42, PPS=0x44, AUD=0x46, IDR_W_RADL=0x26, TRAIL_R=0x02.
    const AU: &[u8] = &[
        0, 0, 0, 1, 0x46, 0x01, 0x50, // AUD (4-byte start code)
        0, 0, 0, 1, 0x40, 0x01, 0xAA, // VPS
        0, 0, 1, 0x42, 0x01, 0xBB, 0xBB, // SPS (3-byte start code)
        0, 0, 1, 0x44, 0x01, 0xCC, // PPS
        0, 0, 0, 1, 0x26, 0x01, 0x00, 0x00, 0x03, 0x01, 0xDD, // IDR with emulation prevention
        0, 0, // trailing zeros
    ];

    #[test]
    fn splits_nals_with_mixed_start_codes() {
        let nals: Vec<_> = nal_units(AU).collect();
        let types: Vec<_> = nals.iter().map(|n| nal_type(n)).collect();
        assert_eq!(types, [NAL_AUD, NAL_VPS, NAL_SPS, NAL_PPS, 19]);
        assert_eq!(nals[2], &[0x42, 0x01, 0xBB, 0xBB]);
        // 00 00 03 01 is not a start code; trailing zeros are stripped.
        assert_eq!(nals[4], &[0x26, 0x01, 0x00, 0x00, 0x03, 0x01, 0xDD]);
        assert!(is_irap(nals[4]));
    }

    #[test]
    fn ignores_leading_garbage_and_empty_input() {
        assert_eq!(nal_units(&[]).count(), 0);
        assert_eq!(nal_units(&[0, 0, 0, 0]).count(), 0);
        let v: Vec<_> = nal_units(&[9, 9, 0, 0, 1, 0x02, 0x01]).collect();
        assert_eq!(v, [&[0x02, 0x01][..]]);
    }

    #[test]
    fn splits_access_units() {
        // IDR AU (param sets + first slice + second slice), then two P frames.
        let stream: &[u8] = &[
            0, 0, 0, 1, 0x40, 0x01, 0xAA, // VPS
            0, 0, 1, 0x42, 0x01, 0xBB, // SPS
            0, 0, 1, 0x44, 0x01, 0xCC, // PPS
            0, 0, 1, 0x26, 0x01, 0x80, 0x11, // IDR slice, first_slice=1
            0, 0, 1, 0x26, 0x01, 0x00, 0x12, // IDR slice, first_slice=0 (same AU)
            0, 0, 0, 1, 0x02, 0x01, 0x80, 0x21, // TRAIL_R, first_slice=1
            0, 0, 1, 0x02, 0x01, 0x80, 0x31, // TRAIL_R, first_slice=1
        ];
        let aus: Vec<_> = access_units(stream).collect();
        assert_eq!(aus.len(), 3);
        assert_eq!(nal_units(aus[0]).count(), 5);
        assert!(param_sets(aus[0]).complete());
        assert_eq!(
            nal_units(aus[1]).collect::<Vec<_>>(),
            [&[0x02, 0x01, 0x80, 0x21][..]]
        );
        assert_eq!(
            nal_units(aus[2]).collect::<Vec<_>>(),
            [&[0x02, 0x01, 0x80, 0x31][..]]
        );
        assert_eq!(aus.concat(), stream);
        assert_eq!(access_units(&[]).count(), 0);
    }

    #[test]
    fn finds_param_sets() {
        let ps = param_sets(AU);
        assert!(ps.complete());
        assert_eq!(ps.vps, Some(&[0x40, 0x01, 0xAA][..]));
        assert_eq!(ps.pps, Some(&[0x44, 0x01, 0xCC][..]));
        assert!(!param_sets(&[0, 0, 1, 0x02, 0x01]).complete());
    }

    #[test]
    fn length_prefixed_drops_param_sets_and_aud() {
        let need = length_prefixed_len(AU);
        assert_eq!(need, 4 + 7);
        let mut out = [0u8; 64];
        let n = to_length_prefixed(AU, &mut out).unwrap();
        assert_eq!(
            &out[..n],
            &[0, 0, 0, 7, 0x26, 0x01, 0x00, 0x00, 0x03, 0x01, 0xDD]
        );
        assert_eq!(to_length_prefixed(AU, &mut out[..need - 1]), None);
    }

    #[test]
    fn length_prefixed_round_trip() {
        let mut lp = [0u8; 64];
        let n = to_length_prefixed(AU, &mut lp).unwrap();
        let ps = param_sets(AU);
        let prefix = [ps.vps.unwrap(), ps.sps.unwrap(), ps.pps.unwrap()];
        let mut out = [0u8; 128];
        let m = from_length_prefixed(&prefix, &lp[..n], &mut out).unwrap();
        let types: Vec<_> = nal_units(&out[..m]).map(nal_type).collect();
        assert_eq!(types, [NAL_VPS, NAL_SPS, NAL_PPS, 19]);
        assert!(param_sets(&out[..m]).complete());
        // Same NAL bytes as the original (AUD dropped).
        let a: Vec<_> = nal_units(AU).filter(|n| nal_type(n) != NAL_AUD).collect();
        let b: Vec<_> = nal_units(&out[..m]).collect();
        assert_eq!(a, b);
        // Truncated length, short output, zero-length NAL.
        assert_eq!(from_length_prefixed(&[], &lp[..n - 1], &mut out), None);
        assert_eq!(
            from_length_prefixed(&prefix, &lp[..n], &mut out[..10]),
            None
        );
        assert_eq!(from_length_prefixed(&[], &[0, 0, 0, 0], &mut out), None);
    }
}
