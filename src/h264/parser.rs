//! H.264 Annex-B bytestream parser.
//!
//! Splits raw H.264 Annex-B data into individual NAL units by
//! locating start code patterns (0x00000001 and 0x000001).

/// Represents a single H.264 NAL Unit.
#[derive(Debug, Clone, PartialEq)]
pub struct Nalu {
    /// NAL unit type (first byte & 0x1F).
    pub nalu_type: u8,
    /// Raw NALU data (without start code).
    pub data: Vec<u8>,
    /// True if type == 5 (IDR slice).
    pub is_idr: bool,
    /// True if type == 7 (SPS).
    pub is_sps: bool,
    /// True if type == 8 (PPS).
    pub is_pps: bool,
    /// True if type == 9 (AUD — Access Unit Delimiter).
    pub is_aud: bool,
}

/// H.264 Annex-B parser.
pub struct Parser;

impl Parser {
    /// Returns indices of all start code positions in `data`.
    ///
    /// Matches both 4-byte (0x00000001) and 3-byte (0x000001) start codes.
    /// For 4-byte codes the position points to the first zero byte (the
    /// extra `0x00` prefix); for 3-byte codes it points to the first `0x00`.
    pub fn find_start_codes(data: &[u8]) -> Vec<usize> {
        if data.len() < 3 {
            return Vec::new();
        }

        let mut positions = Vec::new();
        let mut i = 0;

        while i < data.len() - 2 {
            // Look for 0x000001 pattern (core of both 3-byte and 4-byte codes).
            if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
                // Check if preceded by 0x00 → 4-byte start code at i - 1.
                if i > 0 && data[i - 1] == 0 {
                    positions.push(i - 1);
                } else {
                    positions.push(i);
                }
                i += 3;
                continue;
            }
            i += 1;
        }

        positions
    }

    /// Splits Annex-B data into individual NAL units.
    ///
    /// Returns an empty vec if no NALUs are found.
    pub fn parse(data: &[u8]) -> Vec<Nalu> {
        if data.is_empty() {
            return Vec::new();
        }

        let positions = Self::find_start_codes(data);
        if positions.is_empty() {
            return Vec::new();
        }

        let mut nalus = Vec::with_capacity(positions.len());

        for i in 0..positions.len() {
            let pos = positions[i];

            // Determine NALU data start: skip the start code bytes.
            let nalu_start = if pos + 4 <= data.len()
                && data[pos] == 0
                && data[pos + 1] == 0
                && data[pos + 2] == 0
                && data[pos + 3] == 1
            {
                pos + 4
            } else {
                pos + 3
            };

            if nalu_start >= data.len() {
                break;
            }

            // End of NALU: next start code or end of data.
            let nalu_end = if i + 1 < positions.len() {
                positions[i + 1]
            } else {
                data.len()
            };

            let nalu_data = &data[nalu_start..nalu_end];
            if nalu_data.is_empty() {
                continue;
            }

            let nalu_type = nalu_data[0] & 0x1F;

            nalus.push(Nalu {
                nalu_type,
                data: nalu_data.to_vec(),
                is_idr: nalu_type == 5,
                is_sps: nalu_type == 7,
                is_pps: nalu_type == 8,
                is_aud: nalu_type == 9,
            });
        }

        nalus
    }

    /// Extracts the first SPS and PPS from a slice of NALUs.
    ///
    /// Returns `(Some(sps_data), Some(pps_data))` if both are found,
    /// or `None` for any that are missing.
    pub fn extract_sps_pps(nalus: &[Nalu]) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
        let mut sps = None;
        let mut pps = None;

        for nalu in nalus {
            if nalu.is_sps && sps.is_none() {
                sps = Some(nalu.data.clone());
            }
            if nalu.is_pps && pps.is_none() {
                pps = Some(nalu.data.clone());
            }
        }

        (sps, pps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // find_start_codes
    // -----------------------------------------------------------------------

    #[test]
    fn test_find_start_codes_short_data() {
        // Data shorter than 3 bytes → no start codes.
        assert!(Parser::find_start_codes(&[]).is_empty());
        assert!(Parser::find_start_codes(&[0x00]).is_empty());
        assert!(Parser::find_start_codes(&[0x00, 0x00]).is_empty());
    }

    #[test]
    fn test_find_start_codes_no_match() {
        let data = &[0x01, 0x02, 0x03, 0x04, 0x05];
        assert!(Parser::find_start_codes(data).is_empty());
    }

    #[test]
    fn test_find_start_codes_3byte() {
        // 00 00 01 at position 2
        let data = &[0xFF, 0xFF, 0x00, 0x00, 0x01, 0xAA];
        let pos = Parser::find_start_codes(data);
        assert_eq!(pos, vec![2]);
    }

    #[test]
    fn test_find_start_codes_4byte() {
        // 00 00 00 01 at position 0
        let data = &[0x00, 0x00, 0x00, 0x01, 0x67];
        let pos = Parser::find_start_codes(data);
        assert_eq!(pos, vec![0]);
    }

    #[test]
    fn test_find_start_codes_mixed() {
        // 4-byte at 0: 00 00 00 01
        // 3-byte at 6:             0x00, 0x00, 0x01
        let data = &[0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x00, 0x01, 0x68];
        let pos = Parser::find_start_codes(data);
        assert_eq!(pos, vec![0, 6]);
    }

    #[test]
    fn test_find_start_codes_consecutive() {
        // Two 4-byte codes back-to-back
        let data = &[0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x65];
        let pos = Parser::find_start_codes(data);
        assert_eq!(pos, vec![0, 4]);
    }

    // -----------------------------------------------------------------------
    // parse
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_empty() {
        let nalus = Parser::parse(&[]);
        assert!(nalus.is_empty());
    }

    #[test]
    fn test_parse_no_start_codes() {
        let nalus = Parser::parse(&[0x01, 0x02, 0x03]);
        assert!(nalus.is_empty());
    }

    #[test]
    fn test_parse_single_nalu_4byte() {
        // 00 00 00 01 65 88 84 → IDR (type 5)
        let data = vec![0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84];
        let nalus = Parser::parse(&data);
        assert_eq!(nalus.len(), 1);
        assert_eq!(nalus[0].nalu_type, 5);
        assert!(nalus[0].is_idr);
        assert!(!nalus[0].is_sps);
        assert!(!nalus[0].is_pps);
        assert!(!nalus[0].is_aud);
        assert_eq!(nalus[0].data, vec![0x65, 0x88, 0x84]);
    }

    #[test]
    fn test_parse_single_nalu_3byte() {
        // 00 00 01 67 42 00 → SPS (type 7)
        let data = vec![0x00, 0x00, 0x01, 0x67, 0x42, 0x00];
        let nalus = Parser::parse(&data);
        assert_eq!(nalus.len(), 1);
        assert_eq!(nalus[0].nalu_type, 7);
        assert!(nalus[0].is_sps);
        assert!(!nalus[0].is_idr);
        assert!(!nalus[0].is_pps);
        assert_eq!(nalus[0].data, vec![0x67, 0x42, 0x00]);
    }

    #[test]
    fn test_parse_aud() {
        // AUD (type 9)
        let data = vec![0x00, 0x00, 0x00, 0x01, 0x69, 0xF0];
        let nalus = Parser::parse(&data);
        assert_eq!(nalus.len(), 1);
        assert_eq!(nalus[0].nalu_type, 9);
        assert!(nalus[0].is_aud);
    }

    #[test]
    fn test_parse_non_idr() {
        // non-IDR slice (type 1)
        let data = vec![0x00, 0x00, 0x00, 0x01, 0x61, 0x88];
        let nalus = Parser::parse(&data);
        assert_eq!(nalus.len(), 1);
        assert_eq!(nalus[0].nalu_type, 1);
        assert!(!nalus[0].is_idr);
        assert!(!nalus[0].is_sps);
        assert!(!nalus[0].is_pps);
        assert!(!nalus[0].is_aud);
    }

    #[test]
    fn test_parse_multiple_nalus() {
        // SPS (7) + PPS (8) + IDR (5)
        let data = vec![
            0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x1E, // SPS
            0x00, 0x00, 0x00, 0x01, 0x68, 0xCE, 0x3C, 0x80, // PPS
            0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84, 0x00, // IDR
        ];
        let nalus = Parser::parse(&data);
        assert_eq!(nalus.len(), 3);
        assert_eq!(nalus[0].nalu_type, 7);
        assert!(nalus[0].is_sps);
        assert_eq!(nalus[1].nalu_type, 8);
        assert!(nalus[1].is_pps);
        assert_eq!(nalus[2].nalu_type, 5);
        assert!(nalus[2].is_idr);
    }

    #[test]
    fn test_parse_interleaved_start_codes() {
        // SPS with 4-byte code, PPS with 3-byte code
        let data = vec![
            0x00, 0x00, 0x00, 0x01, 0x67, 0x42, // SPS (4-byte)
            0x00, 0x00, 0x01, 0x68, 0xCE, 0x3C, // PPS (3-byte)
        ];
        let nalus = Parser::parse(&data);
        assert_eq!(nalus.len(), 2);
        assert!(nalus[0].is_sps);
        assert!(nalus[1].is_pps);
        assert_eq!(nalus[0].data, vec![0x67, 0x42]);
        assert_eq!(nalus[1].data, vec![0x68, 0xCE, 0x3C]);
    }

    #[test]
    fn test_parse_only_start_codes() {
        // Two 4-byte codes with no NALU data between them.
        let data = vec![0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01];
        let nalus = Parser::parse(&data);
        assert!(nalus.is_empty());
    }

    #[test]
    fn test_parse_start_code_right_at_end() {
        // 4-byte start code followed by nothing → no NALU
        let data = vec![0x00, 0x00, 0x00, 0x01];
        let nalus = Parser::parse(&data);
        assert!(nalus.is_empty());
    }

    #[test]
    fn test_parse_type_masks_properly() {
        // NALU type is always byte & 0x1F, so 0xE5 (NRI=3, type=5) → type 5
        let data = vec![0x00, 0x00, 0x00, 0x01, 0xE5, 0x88];
        let nalus = Parser::parse(&data);
        assert_eq!(nalus.len(), 1);
        assert_eq!(nalus[0].nalu_type, 5);
        assert!(nalus[0].is_idr);
    }

    // -----------------------------------------------------------------------
    // extract_sps_pps
    // -----------------------------------------------------------------------

    #[test]
    fn test_extract_sps_pps_both_found() {
        let nalus = vec![
            Nalu {
                nalu_type: 7,
                data: vec![0x67, 0x42, 0x00],
                is_idr: false,
                is_sps: true,
                is_pps: false,
                is_aud: false,
            },
            Nalu {
                nalu_type: 8,
                data: vec![0x68, 0xCE, 0x3C],
                is_idr: false,
                is_sps: false,
                is_pps: true,
                is_aud: false,
            },
        ];
        let (sps, pps) = Parser::extract_sps_pps(&nalus);
        assert_eq!(sps, Some(vec![0x67, 0x42, 0x00]));
        assert_eq!(pps, Some(vec![0x68, 0xCE, 0x3C]));
    }

    #[test]
    fn test_extract_sps_pps_only_sps() {
        let nalus = vec![Nalu {
            nalu_type: 7,
            data: vec![0x67, 0x42],
            is_idr: false,
            is_sps: true,
            is_pps: false,
            is_aud: false,
        }];
        let (sps, pps) = Parser::extract_sps_pps(&nalus);
        assert_eq!(sps, Some(vec![0x67, 0x42]));
        assert!(pps.is_none());
    }

    #[test]
    fn test_extract_sps_pps_only_pps() {
        let nalus = vec![Nalu {
            nalu_type: 8,
            data: vec![0x68, 0xCE],
            is_idr: false,
            is_sps: false,
            is_pps: true,
            is_aud: false,
        }];
        let (sps, pps) = Parser::extract_sps_pps(&nalus);
        assert!(sps.is_none());
        assert_eq!(pps, Some(vec![0x68, 0xCE]));
    }

    #[test]
    fn test_extract_sps_pps_none() {
        let nalus = vec![Nalu {
            nalu_type: 5,
            data: vec![0x65, 0x88],
            is_idr: true,
            is_sps: false,
            is_pps: false,
            is_aud: false,
        }];
        let (sps, pps) = Parser::extract_sps_pps(&nalus);
        assert!(sps.is_none());
        assert!(pps.is_none());
    }

    #[test]
    fn test_extract_sps_pps_empty() {
        let (sps, pps) = Parser::extract_sps_pps(&[]);
        assert!(sps.is_none());
        assert!(pps.is_none());
    }

    #[test]
    fn test_extract_sps_pps_first_only() {
        // Should return first SPS and first PPS, ignore subsequent ones.
        let nalus = vec![
            Nalu {
                nalu_type: 7,
                data: vec![0x67, 0x42],
                is_idr: false,
                is_sps: true,
                is_pps: false,
                is_aud: false,
            },
            Nalu {
                nalu_type: 7,
                data: vec![0x67, 0xFF],
                is_idr: false,
                is_sps: true,
                is_pps: false,
                is_aud: false,
            },
        ];
        let (sps, pps) = Parser::extract_sps_pps(&nalus);
        // Should return the first SPS
        assert_eq!(sps, Some(vec![0x67, 0x42]));
        assert!(pps.is_none());
    }
}
