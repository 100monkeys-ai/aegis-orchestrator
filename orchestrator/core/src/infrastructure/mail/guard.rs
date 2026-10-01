// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! What the mailbox check may connect to (AEGIS ADR-125 D1, CORRECTION C3).
//!
//! `POST /v1/credentials/mailboxes` makes the orchestrator connect to a host
//! and port a user names and report the server's first reply. Without a
//! rule that is a server-side request forgery primitive into the pods of the
//! orchestrator's own network, the box's loopback and the cloud metadata
//! address. The production connector therefore admits:
//!
//! - **ports:** IMAP 993 or 143, SMTP 465 or 587, nothing else;
//! - **addresses:** public unicast only. A host is resolved once and refused
//!   when *any* address it resolves to is in a class
//!   [`forbidden_address`] names; the connection then goes to the addresses
//!   that were checked, never to a second resolution (no DNS rebinding
//!   window). An IP literal is held to the same rule without a lookup.
//!
//! The rule lives here rather than in `domain/git_repo.rs`, whose ADR-081
//! guard refuses every IP literal through a private `is_ip_address` and
//! never resolves a name: it has no function for address classes to call,
//! and sharing one would mean changing that file.

use super::MailProtocol;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// The IMAP ports the check connects to: 993 (TLS) and 143 (STARTTLS).
pub const IMAP_PORTS: [u16; 2] = [993, 143];
/// The SMTP ports the check connects to: 465 (TLS) and 587 (submission,
/// STARTTLS). Port 25 is relay, not submission, and is refused.
pub const SMTP_PORTS: [u16; 2] = [465, 587];

/// The class of `ip` when the mailbox check must not connect to it: every
/// address that is not public unicast. IPv4-mapped (`::ffff:a.b.c.d`) and
/// NAT64 (`64:ff9b::a.b.c.d`) IPv6 forms are judged by the IPv4 address they
/// carry.
pub fn forbidden_address(ip: IpAddr) -> Option<&'static str> {
    match ip {
        IpAddr::V4(v4) => forbidden_v4(v4),
        IpAddr::V6(v6) => forbidden_v6(v6),
    }
}

fn forbidden_v4(ip: Ipv4Addr) -> Option<&'static str> {
    let o = ip.octets();
    if ip.is_loopback() {
        Some("a loopback address")
    } else if ip.is_private() {
        Some("a private address")
    } else if ip.is_link_local() {
        Some("a link-local address (169.254/16, the cloud metadata range)")
    } else if o[0] == 100 && (o[1] & 0xc0) == 64 {
        Some("a carrier-grade NAT address (100.64/10)")
    } else if o[0] == 0 {
        Some("an unspecified address")
    } else if ip.is_broadcast() {
        Some("the broadcast address")
    } else if ip.is_multicast() {
        Some("a multicast address")
    } else if o[0] >= 240 {
        Some("a reserved address (240/4)")
    } else {
        None
    }
}

fn forbidden_v6(ip: Ipv6Addr) -> Option<&'static str> {
    let s = ip.segments();
    if let Some(v4) = ip.to_ipv4_mapped() {
        return forbidden_v4(v4);
    }
    if s[0] == 0x0064 && s[1] == 0xff9b && s[2..6] == [0, 0, 0, 0] {
        let v4 = Ipv4Addr::new((s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8);
        return forbidden_v4(v4);
    }
    if ip.is_loopback() {
        Some("a loopback address")
    } else if ip.is_unspecified() {
        Some("an unspecified address")
    } else if s[..6] == [0, 0, 0, 0, 0, 0] {
        // ::/96, the deprecated IPv4-compatible form.
        Some("an IPv4-compatible address")
    } else if ip.is_multicast() {
        Some("a multicast address")
    } else if (s[0] & 0xffc0) == 0xfe80 {
        Some("a link-local address (fe80::/10)")
    } else if (s[0] & 0xffc0) == 0xfec0 {
        Some("a site-local address (fec0::/10)")
    } else if (s[0] & 0xfe00) == 0xfc00 {
        Some("a unique-local address (fc00::/7)")
    } else {
        None
    }
}

/// The field a refusal names: `imap_host`, `smtp_host`, `imap_port` or
/// `smtp_port`.
pub(super) fn field(protocol: MailProtocol, host: bool) -> &'static str {
    match (protocol, host) {
        (MailProtocol::Imap, true) => "imap_host",
        (MailProtocol::Imap, false) => "imap_port",
        (MailProtocol::Smtp, true) => "smtp_host",
        (MailProtocol::Smtp, false) => "smtp_port",
    }
}

/// Refuse a port the protocol's rule does not list.
pub(super) fn check_port(protocol: MailProtocol, port: u16) -> Result<(), String> {
    let (allowed, names) = match protocol {
        MailProtocol::Imap => (&IMAP_PORTS, "993 or 143"),
        MailProtocol::Smtp => (&SMTP_PORTS, "465 or 587"),
    };
    if allowed.contains(&port) {
        Ok(())
    } else {
        Err(format!(
            "{} must be {names}; the mailbox check does not connect to port {port}",
            field(protocol, false)
        ))
    }
}

/// The address a host names when it is an IP literal, bracketed or not.
pub(super) fn ip_literal(host: &str) -> Option<IpAddr> {
    host.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse()
        .ok()
}

/// Refuse when any address is forbidden, naming the first such.
pub(super) fn check_addresses(
    protocol: MailProtocol,
    host: &str,
    addrs: &[IpAddr],
) -> Result<(), String> {
    for ip in addrs {
        if let Some(class) = forbidden_address(*ip) {
            return Err(format!(
                "{} {host} resolves to {ip}, {class}; the mailbox check connects only to public mail servers",
                field(protocol, true)
            ));
        }
    }
    Ok(())
}
