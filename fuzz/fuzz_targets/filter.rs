// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Search and filter expressions typed by the user.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let s = String::from_utf8_lossy(data);
    let _ = rtop_app::filter::Filter::parse(&s);
    let _ = rtop_core::units::parse_size(&s);
    let _ = rtop_core::units::parse_duration_secs(&s);
});
