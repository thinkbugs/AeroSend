//! Annex-B H.264 bitstream parsing.
//!
//! ffmpeg's `h264` raw muxer emits start-code delimited NAL units. Encoder
//! stdout is a byte stream, so access units arrive split across reads; the
//! splitter below is incremental and only yields complete NAL units.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NalType {
    Unspecified,
    SliceNonIdr,
    SliceIdr,
    Sei,
    Sps,
    Pps,
    AccessUnitDelimiter,
}

impl NalType {
    pub fn from_header_byte(byte: u8) -> Self {
        // forbidden_zero(1) | nal_ref_idc(2) | nal_unit_type(5)
        match byte & 0x1F {
            1 => NalType::SliceNonIdr,
            5 => NalType::SliceIdr,
            6 => NalType::Sei,
            7 => NalType::Sps,
            8 => NalType::Pps,
            9 => NalType::AccessUnitDelimiter,
            _ => NalType::Unspecified,
        }
    }

    /// Parameter sets and AUDs belong to the access unit that follows them, so
    /// they must not be flushed as a standalone access unit.
    pub fn is_slice(&self) -> bool {
        matches!(self, NalType::SliceNonIdr | NalType::SliceIdr)
    }
}

/// A single NAL unit with its start code removed.
#[derive(Debug, Clone)]
pub struct Nal {
    pub data: Vec<u8>,
}

impl Nal {
    pub fn nal_type(&self) -> NalType {
        self.data
            .first()
            .map(|b| NalType::from_header_byte(*b))
            .unwrap_or(NalType::Unspecified)
    }

    pub fn is_idr(&self) -> bool {
        self.nal_type() == NalType::SliceIdr
    }

    /// True when this NAL begins a new coded picture.
    ///
    /// `zerolatency` turns on x264's sliced threads, so one input frame leaves
    /// as several slice NALs. They must share one RTP timestamp, and the only
    /// in-band signal for a picture boundary is the slice header's
    /// `first_mb_in_slice` field: it is 0 on a picture's first slice.
    pub fn starts_new_picture(&self) -> bool {
        match self.nal_type() {
            NalType::SliceNonIdr | NalType::SliceIdr => {
                match read_ue(self.data.get(1..).unwrap_or(&[])) {
                    Some(first_mb_in_slice) => first_mb_in_slice == 0,
                    None => true,
                }
            }
            _ => true,
        }
    }
}

/// Reads one unsigned Exp-Golomb coded value from the start of `bits`.
/// Returns `None` when the data runs out before the value completes.
fn read_ue(bits: &[u8]) -> Option<u64> {
    let mut pos = 0usize;
    let total = bits.len() * 8;

    let mut leading_zeros = 0u32;
    let mut found_terminator = false;
    while pos < total {
        let bit = (bits[pos / 8] >> (7 - (pos % 8))) & 1;
        pos += 1;
        if bit == 1 {
            found_terminator = true;
            break;
        }
        leading_zeros += 1;
        if leading_zeros >= 32 {
            return None;
        }
    }
    if !found_terminator {
        return None;
    }

    if leading_zeros == 0 {
        return Some(0);
    }
    if pos + leading_zeros as usize > total {
        return None;
    }

    let mut suffix = 0u64;
    for _ in 0..leading_zeros {
        let bit = (bits[pos / 8] >> (7 - (pos % 8))) & 1;
        pos += 1;
        suffix = (suffix << 1) | bit as u64;
    }
    Some((1u64 << leading_zeros) + suffix - 1)
}

/// Groups NAL units into access units.
///
/// Parameter sets and SEI that precede a slice belong to that slice's picture,
/// so a group is only complete once the *next* picture's first slice appears.
pub fn group_access_unit(nals: &mut Vec<Nal>) -> Option<Vec<Nal>> {
    if nals.len() < 2 {
        return None;
    }
    let boundary = nals.len() - 1;
    if !nals[boundary].starts_new_picture() {
        return None;
    }
    // Only a boundary that follows at least one slice closes a picture.
    if !nals[..boundary].iter().any(|n| n.nal_type().is_slice()) {
        return None;
    }
    let tail = nals.remove(boundary);
    let group = std::mem::take(nals);
    nals.push(tail);
    Some(group)
}

/// Pops the front picture once the stream has ended.
///
/// [`group_access_unit`] deliberately holds the newest picture back until the
/// next slice proves it is complete. At end of stream no further slice can
/// arrive, so every NAL left in the buffer belongs to pictures that are as
/// finished as they will ever be — splitting them at slice-start boundaries
/// releases them one picture at a time instead of merging them into one AU.
pub fn finish_access_unit(nals: &mut Vec<Nal>) -> Option<Vec<Nal>> {
    if nals.is_empty() {
        return None;
    }

    // A picture ends where the next one's first slice begins, but only once a
    // slice has actually been seen: leading parameter sets have no picture of
    // their own and trailing non-VCL NALs belong to the picture after them.
    let mut seen_slice = false;
    let mut boundary = None;
    for (index, nal) in nals.iter().enumerate() {
        let is_slice = nal.nal_type().is_slice();
        if index > 0 && seen_slice && is_slice && nal.starts_new_picture() {
            boundary = Some(index);
            break;
        }
        if is_slice {
            seen_slice = true;
        }
    }

    let Some(boundary) = boundary else {
        // No second picture: whatever is left is the last one, parameters and
        // trailing filler included.
        return Some(std::mem::take(nals));
    };
    // Walk back over the parameter sets that introduce the next picture.
    let mut end = boundary;
    while !nals[end - 1].nal_type().is_slice() {
        end -= 1;
    }
    Some(nals.splice(..end, []).collect())
}

/// Turns the Annex-B bytes an encoder writes into whole access units.
///
/// Owns both the start-code scanner and the picture-boundary grouping so the
/// capture session and anything testing it run exactly the same logic.
#[derive(Default)]
pub struct AccessUnitReader {
    parser: AnnexBParser,
    pending: Vec<Nal>,
}

impl AccessUnitReader {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds encoded bytes and returns every access unit they complete.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Vec<Nal>> {
        self.parser.push(bytes);
        let mut units = Vec::new();
        while let Some(nal) = self.parser.next_nal() {
            self.pending.push(nal);
            if let Some(au) = group_access_unit(&mut self.pending) {
                units.push(au);
            }
        }
        units
    }

    /// Ends the stream and releases the pictures still held back.
    pub fn finish(&mut self) -> Vec<Vec<Nal>> {
        if let Some(nal) = self.parser.finish() {
            self.pending.push(nal);
        }
        let mut units = Vec::new();
        while let Some(au) = finish_access_unit(&mut self.pending) {
            units.push(au);
        }
        units
    }
}

/// Locates the next start code (3- or 4-byte) at or after `from`.
/// Returns `(start_of_the_zero_run, first_byte_after_the_start_code)`.
fn find_start_code(buf: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut i = from;
    while i + 2 < buf.len() {
        if buf[i] == 0 && buf[i + 1] == 0 {
            if buf[i + 2] == 1 {
                return Some((i, i + 3));
            }
            if buf[i + 2] == 0 && i + 3 < buf.len() && buf[i + 3] == 1 {
                return Some((i, i + 4));
            }
        }
        i += 1;
    }
    None
}

/// Incremental Annex-B demuxer.
#[derive(Default)]
pub struct AnnexBParser {
    buffer: Vec<u8>,
    /// True once the first start code has been consumed, so `buffer` currently
    /// holds an in-flight NAL payload.
    started: bool,
}

impl AnnexBParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    /// Extracts the next complete NAL, or `None` when more bytes are needed.
    ///
    /// The final NAL in the buffer can never be known to be complete (the next
    /// start code may still be in flight), so callers that need the tail must
    /// call [`AnnexBParser::finish`].
    pub fn next_nal(&mut self) -> Option<Nal> {
        loop {
            if !self.started {
                match find_start_code(&self.buffer, 0) {
                    Some((_, after_code)) => {
                        self.buffer.drain(..after_code);
                        self.started = true;
                    }
                    None => {
                        // Keep the last 3 bytes: they could be a split start code.
                        let keep = self.buffer.len().saturating_sub(3);
                        self.buffer.drain(..keep);
                        return None;
                    }
                }
            }

            if let Some((code_begin, after_code)) = find_start_code(&self.buffer, 0) {
                let mut nal = self.buffer[..code_begin].to_vec();
                self.buffer.drain(..after_code);
                trim_trailing_zeros(&mut nal);
                if !nal.is_empty() {
                    return Some(Nal { data: nal });
                }
                continue;
            }

            return None;
        }
    }

    /// Flushes whatever remains as a final NAL (used at end-of-stream).
    pub fn finish(&mut self) -> Option<Nal> {
        if !self.started {
            self.buffer.clear();
            return None;
        }
        let mut nal = std::mem::take(&mut self.buffer);
        self.started = false;
        trim_trailing_zeros(&mut nal);
        if nal.is_empty() {
            None
        } else {
            Some(Nal { data: nal })
        }
    }
}

/// Emulation-prevention bytes are not present in Annex-B raw output from
/// ffmpeg's h264 muxer beyond what the encoder already inserted, but trailing
/// zero padding does appear between the last NAL and the next start code.
fn trim_trailing_zeros(nal: &mut Vec<u8>) {
    while nal.last() == Some(&0) {
        nal.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sps() -> Vec<u8> {
        vec![0x67, 0x42, 0x00, 0x1E, 0xAB, 0xCD]
    }
    fn pps() -> Vec<u8> {
        vec![0x68, 0xCE, 0x38, 0x80]
    }
    fn idr(len: usize) -> Vec<u8> {
        let mut v = vec![0x65, 0x88, 0x84];
        v.extend((0..len).map(|i| (i % 251) as u8 + 1));
        v
    }

    #[test]
    fn splits_four_and_three_byte_start_codes() {
        let mut p = AnnexBParser::new();
        let mut stream = Vec::new();
        stream.extend([0, 0, 0, 1]);
        stream.extend(sps());
        stream.extend([0, 0, 1]);
        stream.extend(pps());
        stream.extend([0, 0, 0, 1]);
        stream.extend(idr(10));
        stream.extend([0, 0, 0, 1]);
        stream.extend(idr(4));
        p.push(&stream);

        let mut nals = vec![];
        while let Some(n) = p.next_nal() {
            nals.push(n);
        }
        // last NAL is not provably complete until finish()
        let tail = p.finish();
        if let Some(t) = tail {
            nals.push(t);
        }

        assert_eq!(nals.len(), 4);
        assert_eq!(nals[0].nal_type(), NalType::Sps);
        assert_eq!(nals[1].nal_type(), NalType::Pps);
        assert_eq!(nals[2].nal_type(), NalType::SliceIdr);
        assert_eq!(nals[3].nal_type(), NalType::SliceIdr);
        assert_eq!(nals[0].data, sps());
        assert_eq!(nals[1].data, pps());
    }

    #[test]
    fn handles_nal_split_across_reads() {
        let mut stream = Vec::new();
        stream.extend([0, 0, 0, 1]);
        stream.extend(idr(50));
        stream.extend([0, 0, 0, 1]);
        stream.extend(idr(30));
        let first_len = idr(50).len();

        let mut p = AnnexBParser::new();
        let mut got = Vec::new();
        for chunk in stream.chunks(7) {
            p.push(chunk);
            while let Some(n) = p.next_nal() {
                got.push(n);
            }
        }
        // A NAL is only provably complete once the following start code lands,
        // so the trailing one needs finish().
        assert_eq!(got.len(), 1, "emitted a NAL before its terminator arrived");
        assert_eq!(got[0].data.len(), first_len);
        got.push(p.finish().expect("tail nal"));
        assert_eq!(got.len(), 2);
        assert!(got.iter().all(|n| n.is_idr()));
        assert_eq!(got[1].data, idr(30));
    }

    #[test]
    fn strips_zero_run_between_nal_and_next_start_code() {
        let mut p = AnnexBParser::new();
        let mut stream = Vec::new();
        stream.extend([0, 0, 0, 1]);
        stream.extend(idr(6));
        stream.extend([0, 0, 0, 0, 0, 0, 1]); // padding zeros then start code
        stream.extend(idr(6));
        p.push(&stream);

        let first = p.next_nal().expect("first nal");
        assert_eq!(first.data.len(), 9);
        assert!(!first.data.ends_with(&[0]));
        let rest = p.finish().expect("tail");
        assert_eq!(rest.data.len(), 9);
    }

    /// A slice of the *same* picture: first_mb_in_slice is 1, encoded as the
    /// Exp-Golomb bits `010`.
    fn slice_continuation(len: usize) -> Vec<u8> {
        let mut v = vec![0x65, 0x40, 0x84];
        v.extend((0..len).map(|i| (i % 251) as u8 + 1));
        v
    }

    #[test]
    fn reads_exp_golomb_first_mb_in_slice() {
        // 0b1....... -> 0
        assert_eq!(read_ue(&[0x80]), Some(0));
        // 0b010..... -> 1
        assert_eq!(read_ue(&[0x40]), Some(1));
        // 0b011..... -> 2
        assert_eq!(read_ue(&[0x60]), Some(2));
        // 0b00100... -> 3
        assert_eq!(read_ue(&[0x24]), Some(3));
        // 0b00111... -> 6
        assert_eq!(read_ue(&[0x38]), Some(6));
        // the lone 1 bit is the terminator of a 7-zero run, and the suffix it
        // demands does not fit in the byte
        assert_eq!(read_ue(&[0x01]), None);
        // truncated run of zeros cannot complete a value
        assert_eq!(read_ue(&[0x00, 0x00]), None);
        assert_eq!(read_ue(&[]), None);
    }

    #[test]
    fn slice_start_flags_come_from_the_slice_header() {
        assert!(Nal { data: idr(4) }.starts_new_picture());
        assert!(
            Nal {
                data: slice_continuation(4)
            }
            .starts_new_picture()
                == false,
            "first_mb_in_slice > 0 is a continuation of the current picture"
        );
        assert!(Nal { data: sps() }.starts_new_picture());
    }

    #[test]
    fn groups_parameters_with_the_slice_they_precede() {
        let mut pending: Vec<Nal> = Vec::new();
        for bytes in [sps(), pps(), idr(8)] {
            pending.push(Nal { data: bytes });
            assert!(
                group_access_unit(&mut pending).is_none(),
                "a picture is not known complete until the next one starts"
            );
        }
        assert_eq!(pending.len(), 3);

        // The next picture's first slice closes the previous access unit.
        pending.push(Nal {
            data: idr(8),
        });
        let au = group_access_unit(&mut pending).expect("closed access unit");
        assert_eq!(au.len(), 3);
        assert_eq!(au[0].nal_type(), NalType::Sps);
        assert_eq!(au[2].nal_type(), NalType::SliceIdr);
        assert_eq!(pending.len(), 1, "the new slice starts the next unit");
    }

    #[test]
    fn multi_slice_picture_stays_one_access_unit() {
        let mut pending: Vec<Nal> = Vec::new();
        let mut units = Vec::new();

        // picture 0: SPS + PPS + an IDR slice and two continuation slices
        for bytes in [sps(), pps(), idr(6), slice_continuation(6), slice_continuation(6)] {
            pending.push(Nal { data: bytes });
            if let Some(au) = group_access_unit(&mut pending) {
                units.push(au);
            }
        }
        assert!(units.is_empty(), "picture 0 is not yet known complete");
        assert_eq!(pending.len(), 5);

        // picture 1 begins with its own first slice, which closes picture 0
        pending.push(Nal { data: idr(6) });
        if let Some(au) = group_access_unit(&mut pending) {
            units.push(au);
        }

        assert_eq!(units.len(), 1, "the first picture is exactly one unit");
        assert_eq!(units[0].len(), 5, "SPS+PPS+3 slices");
        assert_eq!(pending.len(), 1, "picture 1 has only its first slice");
        assert_eq!(pending[0].nal_type(), NalType::SliceIdr);
    }

    #[test]
    fn end_of_stream_flush_releases_held_pictures_one_at_a_time() {
        let mut pending: Vec<Nal> = Vec::new();
        pending.push(Nal { data: idr(6) });
        pending.push(Nal {
            data: slice_continuation(6),
        });
        pending.push(Nal { data: sps() });
        pending.push(Nal { data: pps() });
        pending.push(Nal { data: idr(6) });

        let first = finish_access_unit(&mut pending).expect("picture 0");
        assert_eq!(first.len(), 2, "the held-back picture keeps its slices");
        let second = finish_access_unit(&mut pending).expect("picture 1");
        assert_eq!(second.len(), 3, "parameters belong to the picture after them");
        assert!(
            finish_access_unit(&mut pending).is_none(),
            "the buffer is drained"
        );
    }

    /// Some encoders repeat SPS/PPS in front of every IDR; the grouper must
    /// keep those with the picture they introduce, never with the one before.
    #[test]
    fn reader_keeps_repeated_parameters_with_their_own_picture() {
        let mut stream = Vec::new();
        for bytes in [&sps(), &pps(), &idr(20), &sps(), &pps(), &idr(20)] {
            stream.extend([0u8, 0u8, 0u8, 0u8, 1]);
            stream.extend(bytes);
        }

        let mut reader = AccessUnitReader::new();
        // Feed one byte at a time so no picture can be completed by luck of
        // the chunk boundary.
        let mut units = Vec::new();
        for i in 0..stream.len() {
            units.extend(reader.push(&stream[i..i + 1]));
        }
        assert_eq!(units.len(), 1, "only the first picture is provably done");
        units.extend(reader.finish());

        assert_eq!(units.len(), 2);
        for au in &units {
            assert_eq!(au.len(), 3, "SPS+PPS+IDR");
            assert_eq!(au[0].nal_type(), NalType::Sps);
            assert_eq!(au[1].nal_type(), NalType::Pps);
            assert!(au[2].is_idr());
        }
    }
}
