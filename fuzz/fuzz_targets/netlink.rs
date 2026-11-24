// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Netlink message decoders: sock_diag (inet, unix) and nftables rules.
#![no_main]
use libfuzzer_sys::fuzz_target;
use rtop_linux::sockdiag::{InetProto, decode_inet, decode_unix};

fuzz_target!(|data: &[u8]| {
    let _ = decode_inet(InetProto::Tcp, data);
    let _ = decode_inet(InetProto::Udp, data);
    let _ = decode_unix(data);
    if let Some(rule) = rtop_linux::nftables::decode_rule(data) {
        // Anything decodable re-encodes and decodes to the same rule.
        assert_eq!(rtop_linux::nftables::decode_rule(&rtop_linux::nftables::encode_rule(&rule)).as_ref(), Some(&rule));
    }
});
