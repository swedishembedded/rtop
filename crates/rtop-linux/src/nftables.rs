// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Read-only nftables rule counters over `NETLINK_NETFILTER`.
//!
//! rtop only *reads* the ruleset (`NFT_MSG_GETRULE` dump). It never adds
//! counters, enables `nftrace`, resets counters (`GETRULE_RESET` is never
//! sent) or changes rules. Rules without a `counter` expression are reported
//! as uninstrumented, never as zero hits. Dumping the ruleset requires
//! `CAP_NET_ADMIN` in the network namespace; without it the provider reports
//! permission denied.

use std::time::Duration;

use crate::fs::KernelError;
use crate::netlink::{self, attrs, string, u32_be, u64_be};

const NFNL_SUBSYS_NFTABLES: u16 = 10;
const NFT_MSG_GETRULE: u16 = 7;
const NFTA_RULE_TABLE: u16 = 1;
const NFTA_RULE_CHAIN: u16 = 2;
const NFTA_RULE_HANDLE: u16 = 3;
const NFTA_RULE_EXPRESSIONS: u16 = 4;
const NFTA_RULE_USERDATA: u16 = 7;
const NFTA_LIST_ELEM: u16 = 1;
const NFTA_EXPR_NAME: u16 = 1;
const NFTA_EXPR_DATA: u16 = 2;
const NFTA_COUNTER_BYTES: u16 = 1;
const NFTA_COUNTER_PACKETS: u16 = 2;
const NFTA_IMMEDIATE_DATA: u16 = 2;
const NFTA_DATA_VERDICT: u16 = 2;
const NFTA_VERDICT_CODE: u16 = 1;
const NFTA_VERDICT_CHAIN: u16 = 2;
/// libnftnl user-data TLV type of the rule comment.
const NFTNL_UDATA_RULE_COMMENT: u8 = 0;

/// The `nf_tables` message type of a rule dump.
pub const RULE_MSG_TYPE: u16 = (NFNL_SUBSYS_NFTABLES << 8) | NFT_MSG_GETRULE;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    /// Address family name (`inet`, `ip`, `ip6`, `arp`, `bridge`, `netdev`).
    pub family: &'static str,
    pub table: String,
    pub chain: String,
    pub handle: u64,
    /// `Some((packets, bytes))` when the rule has a counter expression.
    pub counter: Option<(u64, u64)>,
    /// Final verdict (`accept`, `drop`, `jump <chain>`, ...), when explicit.
    pub verdict: Option<String>,
    pub comment: Option<String>,
}

pub fn family_name(f: u8) -> &'static str {
    match f {
        1 => "inet",
        2 => "ip",
        3 => "arp",
        5 => "netdev",
        7 => "bridge",
        10 => "ip6",
        _ => "unknown",
    }
}

fn verdict_name(code: i32, chain: Option<String>) -> String {
    match (code, chain) {
        (0, _) => "drop".into(),
        (1, _) => "accept".into(),
        (2, _) => "stolen".into(),
        (3, _) => "queue".into(),
        (4, _) => "repeat".into(),
        (-1, _) => "continue".into(),
        (-2, _) => "break".into(),
        (-3, Some(c)) => format!("jump {c}"),
        (-4, Some(c)) => format!("goto {c}"),
        (-5, _) => "return".into(),
        (c, _) => format!("verdict {c}"),
    }
}

/// Decodes one `NFT_MSG_NEWRULE` payload (`nfgenmsg` + attributes).
/// Public for fuzzing; never panics on arbitrary input.
pub fn decode_rule(m: &[u8]) -> Option<Rule> {
    let family = *m.first()?;
    let mut r = Rule {
        family: family_name(family),
        table: String::new(),
        chain: String::new(),
        handle: 0,
        counter: None,
        verdict: None,
        comment: None,
    };
    let mut have_handle = false;
    for (ty, p) in attrs(m.get(4..)?) {
        match ty {
            NFTA_RULE_TABLE => r.table = string(p),
            NFTA_RULE_CHAIN => r.chain = string(p),
            NFTA_RULE_HANDLE => {
                r.handle = u64_be(p)?;
                have_handle = true;
            }
            NFTA_RULE_EXPRESSIONS => {
                for (et, elem) in attrs(p) {
                    if et != NFTA_LIST_ELEM {
                        continue;
                    }
                    let mut name = String::new();
                    let mut data: &[u8] = &[];
                    for (k, v) in attrs(elem) {
                        match k {
                            NFTA_EXPR_NAME => name = string(v),
                            NFTA_EXPR_DATA => data = v,
                            _ => {}
                        }
                    }
                    match name.as_str() {
                        "counter" => {
                            let (mut bytes, mut packets) = (None, None);
                            for (k, v) in attrs(data) {
                                match k {
                                    NFTA_COUNTER_BYTES => bytes = u64_be(v),
                                    NFTA_COUNTER_PACKETS => packets = u64_be(v),
                                    _ => {}
                                }
                            }
                            if let (Some(p), Some(b)) = (packets, bytes) {
                                r.counter = Some((p, b));
                            }
                        }
                        "immediate" => {
                            for (k, v) in attrs(data) {
                                if k != NFTA_IMMEDIATE_DATA {
                                    continue;
                                }
                                for (dk, dv) in attrs(v) {
                                    if dk != NFTA_DATA_VERDICT {
                                        continue;
                                    }
                                    let (mut code, mut chain) = (None, None);
                                    for (vk, vv) in attrs(dv) {
                                        match vk {
                                            NFTA_VERDICT_CODE => {
                                                code = u32_be(vv).map(|c| c as i32)
                                            }
                                            NFTA_VERDICT_CHAIN => chain = Some(string(vv)),
                                            _ => {}
                                        }
                                    }
                                    if let Some(c) = code {
                                        r.verdict = Some(verdict_name(c, chain));
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            NFTA_RULE_USERDATA => {
                // TLVs: type u8, length u8, value.
                let mut u = p;
                while u.len() >= 2 {
                    let (t, l) = (u[0], u[1] as usize);
                    let Some(v) = u.get(2..2 + l) else { break };
                    if t == NFTNL_UDATA_RULE_COMMENT {
                        r.comment = Some(crate::netlink::string(v));
                    }
                    u = &u[2 + l..];
                }
            }
            _ => {}
        }
    }
    (have_handle && !r.table.is_empty()).then_some(r)
}

/// Source of firewall rules.
pub trait FirewallSource: Send + Sync + std::fmt::Debug {
    fn rules(&self) -> Result<Vec<Rule>, KernelError>;
}

/// The live kernel's nftables ruleset.
#[derive(Debug)]
pub struct Nftables {
    timeout: Duration,
}

impl Default for Nftables {
    fn default() -> Self {
        Nftables {
            timeout: Duration::from_secs(2),
        }
    }
}

impl FirewallSource for Nftables {
    fn rules(&self) -> Result<Vec<Rule>, KernelError> {
        let sock = netlink::Socket::open(libc::NETLINK_NETFILTER, self.timeout)?;
        // nfgenmsg: AF_UNSPEC (all families), NFNETLINK_V0, res_id 0.
        let req = [0u8, 0, 0, 0];
        let mut out = Vec::new();
        sock.dump(RULE_MSG_TYPE, &req, |m| out.extend(decode_rule(m)))?;
        Ok(out)
    }
}

/// No firewall information (simulated systems without one).
#[derive(Debug)]
pub struct NoFirewall;

impl FirewallSource for NoFirewall {
    fn rules(&self) -> Result<Vec<Rule>, KernelError> {
        Err(KernelError::Unsupported)
    }
}

/// Encodes a rule the way the kernel does; used by tests and the demo
/// machine to produce realistic messages.
pub fn encode_rule(r: &Rule) -> Vec<u8> {
    fn attr(ty: u16, payload: &[u8], nested: bool) -> Vec<u8> {
        let len = 4 + payload.len();
        let mut v = Vec::with_capacity((len + 3) & !3);
        v.extend_from_slice(&(len as u16).to_ne_bytes());
        v.extend_from_slice(&(ty | if nested { 0x8000 } else { 0 }).to_ne_bytes());
        v.extend_from_slice(payload);
        v.resize((len + 3) & !3, 0);
        v
    }
    let cstr = |s: &str| {
        let mut b = s.as_bytes().to_vec();
        b.push(0);
        b
    };
    let family = match r.family {
        "inet" => 1,
        "ip" => 2,
        "arp" => 3,
        "netdev" => 5,
        "bridge" => 7,
        "ip6" => 10,
        _ => 0,
    };
    let mut m = vec![family, 0, 0, 0];
    m.extend(attr(NFTA_RULE_TABLE, &cstr(&r.table), false));
    m.extend(attr(NFTA_RULE_CHAIN, &cstr(&r.chain), false));
    m.extend(attr(NFTA_RULE_HANDLE, &r.handle.to_be_bytes(), false));
    let mut exprs = Vec::new();
    if let Some((p, b)) = r.counter {
        let mut data = attr(NFTA_COUNTER_BYTES, &b.to_be_bytes(), false);
        data.extend(attr(NFTA_COUNTER_PACKETS, &p.to_be_bytes(), false));
        let mut e = attr(NFTA_EXPR_NAME, &cstr("counter"), false);
        e.extend(attr(NFTA_EXPR_DATA, &data, true));
        exprs.extend(attr(NFTA_LIST_ELEM, &e, true));
    }
    if let Some(v) = &r.verdict {
        let code: i32 = match v.as_str() {
            "drop" => 0,
            "accept" => 1,
            "return" => -5,
            _ => -1,
        };
        let verdict = attr(NFTA_VERDICT_CODE, &(code as u32).to_be_bytes(), false);
        let data = attr(NFTA_DATA_VERDICT, &verdict, true);
        let imm = attr(NFTA_IMMEDIATE_DATA, &data, true);
        let mut e = attr(NFTA_EXPR_NAME, &cstr("immediate"), false);
        e.extend(attr(NFTA_EXPR_DATA, &imm, true));
        exprs.extend(attr(NFTA_LIST_ELEM, &e, true));
    }
    m.extend(attr(NFTA_RULE_EXPRESSIONS, &exprs, true));
    if let Some(c) = &r.comment {
        let mut u = vec![NFTNL_UDATA_RULE_COMMENT, (c.len() + 1) as u8];
        u.extend(cstr(c));
        m.extend(attr(NFTA_RULE_USERDATA, &u, false));
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn rule(counter: Option<(u64, u64)>, verdict: Option<&str>, comment: Option<&str>) -> Rule {
        Rule {
            family: "inet",
            table: "filter".into(),
            chain: "input".into(),
            handle: 42,
            counter,
            verdict: verdict.map(Into::into),
            comment: comment.map(Into::into),
        }
    }

    #[test]
    fn rules_round_trip_through_kernel_encoding() {
        for r in [
            rule(
                Some((284_302, 18_293_760)),
                Some("accept"),
                Some("established"),
            ),
            rule(None, Some("drop"), None),
            rule(Some((0, 0)), None, None),
        ] {
            assert_eq!(decode_rule(&encode_rule(&r)), Some(r));
        }
    }

    #[test]
    fn rule_without_counter_has_no_hit_count() {
        assert_eq!(
            decode_rule(&encode_rule(&rule(None, Some("drop"), None)))
                .unwrap()
                .counter,
            None
        );
    }

    #[test]
    fn live_dump_is_permission_checked_not_fatal() {
        // Whatever the privileges here, the call returns rules or an error;
        // it never panics and never modifies anything.
        let _ = Nftables::default().rules();
    }

    proptest! {
        #[test]
        fn decoder_never_panics(b in proptest::collection::vec(any::<u8>(), 0..400)) {
            let _ = decode_rule(&b);
        }
    }
}
