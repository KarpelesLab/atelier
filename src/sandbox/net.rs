//! The guest's network, by host name: `[sandbox] network = ["crates.io"]`.
//!
//! nixvm hands every outside connection of the guest to an `Egress`, by IP
//! address: `connect_tcp(ip, port)`, `open_udp()`. A list of *names* is made
//! exact on top of that by answering the guest's DNS here:
//!
//! - The guest's `/etc/resolv.conf` names one server, [`RESOLVER`] — an
//!   address that is nobody's. UDP never leaves the host: a datagram to that
//!   address, port 53, is a question this module answers; any other datagram
//!   is dropped.
//! - A question for a name on the list is resolved by the host's own
//!   resolver, and the addresses in the answer are **remembered**
//!   ([`Policy::admit`]). A name that is not on the list does not exist
//!   (`NXDOMAIN`) and is never looked up, so a name cannot carry data out
//!   through the host's resolver either.
//! - `connect_tcp` goes through only to a remembered address
//!   ([`Policy::may_connect`]). An address typed into a command, another
//!   resolver, DNS over TCP, DNS over HTTPS to a host that is not on the
//!   list: all are connections to an address no allowed name resolved to.
//! - An allowed name that resolves to a loopback, private, link-local or
//!   otherwise non-public address ([`public`]) is not answered with it: the
//!   name's owner would otherwise choose what of the user's own network the
//!   guest reaches. Such an address is reachable only when the list names
//!   the address itself.
//!
//! What this cannot do, since the filter is by address in the end: tell two
//! names on one address apart. Whatever shares an address with an allowed
//! name — the other sites of its CDN — can be reached by connecting to that
//! address and asking for the other site. `docs/sandbox.md` says so.
//!
//! The memory lasts as long as the guest, which is one command.
//!
//! The matching, the DNS wire format and the bookkeeping are plain code,
//! compiled and tested in every build; only the `Egress` at the end needs
//! nixvm.

use std::collections::{BTreeSet, HashMap};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// The name server the guest is told to use: TEST-NET-1 (RFC 5737), which is
/// routed nowhere. Nothing is ever sent to it; datagrams addressed to it are
/// answered by [`question`] and [`answer`].
pub const RESOLVER: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 53);

/// The guest's `/etc/resolv.conf` while the allowlist is on.
pub fn resolv_conf() -> String {
    format!("nameserver {RESOLVER}\n")
}

/// How many refusals are kept for the report: enough to name what a failed
/// command wanted, not a log.
const REFUSALS: usize = 16;
/// How many addresses one answer carries. Eight `AAAA` records after the
/// longest question still fit the 512 bytes every resolver accepts over UDP.
const ANSWERS: usize = 8;
/// The answers' time to live, in seconds. The guest rarely outlives it.
const TTL: u32 = 60;

/// One entry of the list, as it matches.
#[derive(Debug, Clone, PartialEq)]
enum Pattern {
    /// `crates.io`: that name.
    Exact(String),
    /// `*.crates.io`, kept as `.crates.io`: every name under it, at any
    /// depth — not `crates.io` itself.
    Under(String),
}

/// `[sandbox] network`, parsed.
///
/// An entry is a host name (`crates.io`), a name with a leading `*.` for
/// everything under it (`*.crates.io`; it does not cover `crates.io`), or an
/// IP address, which allows connecting to that very address — the only way
/// to a private one. Names are compared without regard to case. There are no
/// ports: an allowed host is allowed on all of them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Allowlist {
    patterns: Vec<Pattern>,
    addresses: Vec<IpAddr>,
}

impl Allowlist {
    /// The list for `entries`, and what was wrong with those left out.
    pub fn parse<S: AsRef<str>>(entries: &[S]) -> (Self, Vec<String>) {
        let mut list = Self::default();
        let mut rejected = Vec::new();
        for entry in entries {
            let entry = entry.as_ref().trim();
            let lower = entry.to_ascii_lowercase();
            let text = lower.strip_suffix('.').unwrap_or(&lower);
            if let Ok(address) = text.parse::<IpAddr>() {
                let address = address.to_canonical();
                if address.is_loopback() || address.is_unspecified() || address.is_multicast() {
                    rejected.push(format!(
                        "`{entry}`: the guest's own loopback, or not a host's address"
                    ));
                } else if !list.addresses.contains(&address) {
                    list.addresses.push(address);
                }
                continue;
            }
            let (under, name) = match text.strip_prefix("*.") {
                Some(rest) => (true, rest),
                None => (false, text),
            };
            if !valid_name(name) {
                rejected.push(format!(
                    "`{entry}`: not a host name, `*.` and a host name, or an IP address"
                ));
                continue;
            }
            let pattern = match under {
                true => Pattern::Under(format!(".{name}")),
                false => Pattern::Exact(name.to_string()),
            };
            if !list.patterns.contains(&pattern) {
                list.patterns.push(pattern);
            }
        }
        (list, rejected)
    }

    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty() && self.addresses.is_empty()
    }

    /// The entries as they are understood: lower case, each once. What
    /// `atelier sandbox status` shows and the helper process is given.
    pub fn entries(&self) -> Vec<String> {
        let names = self.patterns.iter().map(|p| match p {
            Pattern::Exact(name) => name.clone(),
            Pattern::Under(suffix) => format!("*{suffix}"),
        });
        names
            .chain(self.addresses.iter().map(IpAddr::to_string))
            .collect()
    }

    /// Whether `name` (lower case, no trailing dot) is on the list.
    pub fn allows_name(&self, name: &str) -> bool {
        valid_name(name)
            && self.patterns.iter().any(|p| match p {
                Pattern::Exact(exact) => exact == name,
                Pattern::Under(suffix) => name.len() > suffix.len() && name.ends_with(suffix),
            })
    }

    fn allows_address(&self, address: IpAddr) -> bool {
        self.addresses.contains(&address)
    }
}

/// Whether `name` is a host name this module will hand to the host's
/// resolver: labels of letters, digits, `-` and `_`, in lower case, and a
/// last label that is not a number. The host's `getaddrinfo` reads
/// `10.0.0.1`, `0x7f.1` or `2130706433` as addresses, without asking anyone;
/// a `*.1` on the list must not turn that into "every address".
fn valid_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 253 {
        return false;
    }
    let label = |l: &str| {
        (1..=63).contains(&l.len())
            && l.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
    };
    let last = name.rsplit('.').next().unwrap_or(name);
    name.split('.').all(label)
        && !last.bytes().all(|b| b.is_ascii_digit())
        && !last.starts_with("0x")
}

/// Whether `address` is one a name may lead the guest to: a public unicast
/// address. Not the host itself (loopback), not the networks it sits on
/// (private, carrier-grade NAT, link-local — where a cloud's metadata service
/// lives —, unique-local), not multicast, not the reserved and documentation
/// ranges. An IPv4 address carried in an IPv6 one (mapped, NAT64, 6to4) is
/// judged as the IPv4 address it is.
pub fn public(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(a) => public_v4(a),
        IpAddr::V6(a) => public_v6(a),
    }
}

fn public_v4(a: Ipv4Addr) -> bool {
    let [b0, b1, b2, _] = a.octets();
    !(b0 == 0                                   // "this network"
        || b0 == 10                             // private
        || (b0 == 100 && b1 & 0xc0 == 64)       // 100.64/10, carrier-grade NAT
        || b0 == 127                            // loopback
        || (b0 == 169 && b1 == 254)             // link-local
        || (b0 == 172 && b1 & 0xf0 == 16)       // private
        || (b0 == 192 && b1 == 0 && b2 == 0)    // IETF protocol assignments
        || (b0 == 192 && b1 == 0 && b2 == 2)    // TEST-NET-1 (and RESOLVER)
        || (b0 == 192 && b1 == 168)             // private
        || (b0 == 198 && b1 & 0xfe == 18)       // benchmarking
        || (b0 == 198 && b1 == 51 && b2 == 100) // TEST-NET-2
        || (b0 == 203 && b1 == 0 && b2 == 113)  // TEST-NET-3
        || b0 >= 224) // multicast, reserved, broadcast
}

fn public_v6(a: Ipv6Addr) -> bool {
    let s = a.segments();
    let embedded = |hi: u16, lo: u16| Ipv4Addr::from((u32::from(hi) << 16) | u32::from(lo));
    // ::ffff:a.b.c.d, and the NAT64 prefix a DNS64 resolver answers with on
    // an IPv6-only network.
    if (s[..5] == [0; 5] && s[5] == 0xffff) || s[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        return public_v4(embedded(s[6], s[7]));
    }
    // 6to4 carries its IPv4 gateway.
    if s[0] == 0x2002 {
        return public_v4(embedded(s[1], s[2]));
    }
    // Global unicast (2000::/3) only: that leaves out ::, ::1, the
    // IPv4-compatible ::/96, link-local, unique-local, site-local and
    // multicast. Within it, not Teredo (2001::/32, an IPv4 tunnel) and not
    // the documentation prefix.
    s[0] & 0xe000 == 0x2000 && !(s[0] == 0x2001 && (s[1] == 0 || s[1] == 0x0db8))
}

/// What the guest was allowed, what it has been told, and what it was
/// refused: one per guest.
#[derive(Debug, Default)]
pub struct Policy {
    list: Allowlist,
    /// Every address an answer has carried: where `connect_tcp` may go.
    handed: BTreeSet<IpAddr>,
    /// What the host's resolver said for each allowed name asked about, after
    /// [`Policy::admit`]: asked once per guest.
    known: HashMap<String, Vec<IpAddr>>,
    refused: Vec<String>,
}

impl Policy {
    pub fn new(list: Allowlist) -> Self {
        Self {
            list,
            ..Self::default()
        }
    }

    /// What was refused so far, each thing once, in the words the agent is
    /// told ([`super::diagnose`]).
    pub fn refused(&self) -> &[String] {
        &self.refused
    }

    fn note(&mut self, refusal: String) {
        if self.refused.len() < REFUSALS && !self.refused.contains(&refusal) {
            self.refused.push(refusal);
        }
    }

    /// The addresses of `found` — what the host resolved the allowed `name`
    /// to — that the guest is told, and may from now on connect to. An
    /// address that is not [`public`] is left out, unless the list names it.
    pub fn admit(&mut self, name: &str, found: &[IpAddr]) -> Vec<IpAddr> {
        let mut kept = Vec::new();
        for address in found.iter().map(IpAddr::to_canonical) {
            if kept.contains(&address) {
                continue;
            }
            if public(address) || self.list.allows_address(address) {
                self.handed.insert(address);
                kept.push(address);
            } else {
                self.note(format!(
                    "the name `{name}` resolves to {address}, which is not a public address \
                     (a private or local address is reached only if `[sandbox] network` lists \
                     the address itself)"
                ));
            }
        }
        self.known.insert(name.to_string(), kept.clone());
        kept
    }

    /// Whether the guest may open a TCP connection to `address`: only one an
    /// answer carried, or one the list names.
    pub fn may_connect(&mut self, address: IpAddr, port: u16) -> bool {
        let address = address.to_canonical();
        if self.handed.contains(&address) || self.list.allows_address(address) {
            return true;
        }
        self.note(format!(
            "a connection to {address} port {port}, an address that no allowed name resolved to"
        ));
        false
    }

    /// A datagram that is not a question for [`RESOLVER`]: dropped.
    pub fn refuse_datagram(&mut self, address: IpAddr, port: u16) {
        let address = address.to_canonical();
        self.note(format!(
            "UDP to {address} port {port} (the only UDP is the sandbox's own DNS; other \
             resolvers, QUIC and NTP are not let out)"
        ));
    }
}

// --- DNS ------------------------------------------------------------------

const TYPE_A: u16 = 1;
const TYPE_AAAA: u16 = 28;
const CLASS_IN: u16 = 1;

const NOERROR: u8 = 0;
const FORMERR: u8 = 1;
const NXDOMAIN: u8 = 3;
const NOTIMP: u8 = 4;

/// A question the guest asked, as far as it is needed to answer it.
#[derive(Debug, Clone, PartialEq)]
pub struct Question {
    id: u16,
    /// The asker's "recursion desired", echoed.
    recursion: bool,
    /// The question section as it was sent (the name in the asker's own
    /// spelling: some resolvers vary the case and check the echo).
    section: Vec<u8>,
    /// The name asked about: lower case, no trailing dot.
    pub name: String,
    qtype: u16,
}

/// What to do with a datagram sent to [`RESOLVER`].
#[derive(Debug, PartialEq)]
pub enum Step {
    /// Send this back.
    Reply(Vec<u8>),
    /// The name is on the list and has not been asked about: resolve
    /// [`Question::name`] on the host, then [`answer`].
    Lookup(Question),
}

/// Read `packet` as a DNS query and decide what happens to it. `None` for
/// what is not a query at all, which gets no answer.
///
/// Only a name on the list goes on to the host's resolver. Every other name
/// gets `NXDOMAIN` — not `REFUSED`, which musl's resolver ignores and then
/// waits out its timeout: "does not exist" is what the guest should hear at
/// once. Names on the list have addresses (`A`, `AAAA`) and nothing else: an
/// empty answer for any other type.
pub fn question(policy: &mut Policy, packet: &[u8]) -> Option<Step> {
    if packet.len() < 12 {
        return None;
    }
    let word = |at: usize| u16::from_be_bytes([packet[at], packet[at + 1]]);
    let (id, flags) = (word(0), word(2));
    // A response is not answered, or two of these could talk forever.
    if flags & 0x8000 != 0 {
        return None;
    }
    let recursion = flags & 0x0100 != 0;
    let header_only = |code: u8| Some(Step::Reply(header(id, recursion, code, 0, 0).to_vec()));
    if flags & 0x7800 != 0 {
        return header_only(NOTIMP); // not a standard query
    }
    if word(4) != 1 {
        return header_only(FORMERR);
    }
    // One name, uncompressed (there is nothing before it to point at).
    let mut at = 12;
    let mut name = String::new();
    loop {
        let Some(&len) = packet.get(at) else {
            return header_only(FORMERR);
        };
        let len = usize::from(len);
        at += 1;
        if len == 0 {
            break;
        }
        let Some(label) = packet.get(at..at + len).filter(|_| len <= 63) else {
            return header_only(FORMERR);
        };
        at += len;
        if !name.is_empty() {
            name.push('.');
        }
        // A byte that is no part of a host name, a dot inside a label among
        // them, becomes one `valid_name` refuses.
        name.extend(label.iter().map(|&b| match b {
            b'.' => '\u{fffd}',
            b => char::from(b.to_ascii_lowercase()),
        }));
        if name.len() > 253 {
            return header_only(FORMERR);
        }
    }
    if packet.len() < at + 4 {
        return header_only(FORMERR);
    }
    let (qtype, class) = (word(at), word(at + 2));
    let question = Question {
        id,
        recursion,
        section: packet[12..at + 4].to_vec(),
        name,
        qtype,
    };
    if class != CLASS_IN {
        return Some(Step::Reply(reply(&question, NOTIMP, &[])));
    }
    let addresses = matches!(qtype, TYPE_A | TYPE_AAAA);
    if !policy.list.allows_name(&question.name) {
        // Reverse lookups and the like are refused the same, and not worth
        // a line in the report.
        if addresses {
            policy.note(format!(
                "the name `{}`, which is not on the allowlist",
                printable(&question.name)
            ));
        }
        return Some(Step::Reply(reply(&question, NXDOMAIN, &[])));
    }
    if !addresses {
        return Some(Step::Reply(reply(&question, NOERROR, &[])));
    }
    Some(match policy.known.get(&question.name) {
        Some(known) => Step::Reply(reply(&question, NOERROR, known)),
        None => Step::Lookup(question),
    })
}

/// The answer to `question`, whose name the host resolved to `found` (`None`:
/// it did not resolve). The addresses in it are admitted first
/// ([`Policy::admit`]): the guest is never told an address it may not use.
pub fn answer(policy: &mut Policy, question: &Question, found: Option<&[IpAddr]>) -> Vec<u8> {
    let Some(found) = found else {
        policy.note(format!(
            "the name `{}` is allowed, but did not resolve on the host",
            question.name
        ));
        return reply(question, NXDOMAIN, &[]);
    };
    // Another question for the same name may have been answered meanwhile
    // (resolvers ask for `A` and `AAAA` at once): one answer per name.
    let kept = match policy.known.get(&question.name) {
        Some(known) => known.clone(),
        None => policy.admit(&question.name, found),
    };
    reply(question, NOERROR, &kept)
}

fn printable(name: &str) -> String {
    name.chars()
        .take(80)
        .map(|c| match c {
            c if c.is_ascii_graphic() && c != '`' => c,
            _ => '?',
        })
        .collect()
}

fn header(id: u16, recursion: bool, code: u8, questions: u16, answers: u16) -> [u8; 12] {
    // A response, recursion available, the asker's "recursion desired".
    let flags = 0x8080 | (u16::from(recursion) << 8) | u16::from(code);
    let mut out = [0u8; 12];
    out[0..2].copy_from_slice(&id.to_be_bytes());
    out[2..4].copy_from_slice(&flags.to_be_bytes());
    out[4..6].copy_from_slice(&questions.to_be_bytes());
    out[6..8].copy_from_slice(&answers.to_be_bytes());
    out
}

/// A response to `question` carrying those of `addresses` that are of the
/// type asked for.
fn reply(question: &Question, code: u8, addresses: &[IpAddr]) -> Vec<u8> {
    let records: Vec<Vec<u8>> = addresses
        .iter()
        .filter_map(|address| match (question.qtype, address) {
            (TYPE_A, IpAddr::V4(a)) => Some(a.octets().to_vec()),
            (TYPE_AAAA, IpAddr::V6(a)) => Some(a.octets().to_vec()),
            _ => None,
        })
        .take(ANSWERS)
        .collect();
    let mut out = header(
        question.id,
        question.recursion,
        code,
        1,
        records.len() as u16,
    )
    .to_vec();
    out.extend_from_slice(&question.section);
    for data in records {
        out.extend_from_slice(&[0xc0, 12]); // the name: the question's, at offset 12
        out.extend_from_slice(&question.qtype.to_be_bytes());
        out.extend_from_slice(&CLASS_IN.to_be_bytes());
        out.extend_from_slice(&TTL.to_be_bytes());
        out.extend_from_slice(&(data.len() as u16).to_be_bytes());
        out.extend_from_slice(&data);
    }
    out
}

#[cfg(sandbox)]
pub use egress::Filter;

#[cfg(sandbox)]
mod egress {
    //! The `Egress` nixvm is given: [`Policy`] in front of the host's
    //! network.

    use std::collections::VecDeque;
    use std::io;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs, UdpSocket};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, MutexGuard};

    use nixvm::kernel::egress::{Datagram, Egress, HostConn, HostDgram, HostEgress};

    use super::{Allowlist, Policy, RESOLVER, Step, answer, question};

    /// How many names may be resolving on the host at once. A question past
    /// that is dropped, and asked again by the guest's resolver.
    const LOOKUPS: usize = 16;

    /// The guest's way out: TCP to the addresses its allowed names resolved
    /// to, and DNS answered here. No other UDP, no ICMP (nixvm's default for
    /// `open_icmp` is "unsupported", and it stays).
    #[derive(Debug)]
    pub struct Filter {
        policy: Arc<Mutex<Policy>>,
        lookups: Arc<AtomicUsize>,
    }

    impl Filter {
        pub fn new(list: Allowlist) -> Self {
            Self {
                policy: Arc::new(Mutex::new(Policy::new(list))),
                lookups: Arc::new(AtomicUsize::new(0)),
            }
        }

        /// The policy, to read what was refused once the guest has ended.
        pub fn policy(&self) -> Arc<Mutex<Policy>> {
            self.policy.clone()
        }
    }

    /// A poisoned lock is a thread that panicked while holding it; the
    /// policy it left is still the policy.
    fn lock(policy: &Mutex<Policy>) -> MutexGuard<'_, Policy> {
        policy.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn address(ip: [u8; 16], v6: bool) -> IpAddr {
        match v6 {
            true => IpAddr::V6(Ipv6Addr::from(ip)),
            false => IpAddr::V4(Ipv4Addr::new(ip[0], ip[1], ip[2], ip[3])),
        }
    }

    impl Egress for Filter {
        fn connect_tcp(&self, ip: [u8; 16], v6: bool, port: u16) -> io::Result<Box<dyn HostConn>> {
            if !lock(&self.policy).may_connect(address(ip, v6), port) {
                // The guest sees ENETUNREACH, as it does with no network.
                return Err(io::Error::new(
                    io::ErrorKind::NetworkUnreachable,
                    "not an address of an allowed host",
                ));
            }
            HostEgress.connect_tcp(ip, v6, port)
        }

        fn open_udp(&self) -> io::Result<Box<dyn HostDgram>> {
            Ok(Box::new(Resolver {
                policy: self.policy.clone(),
                lookups: self.lookups.clone(),
                replies: Arc::new(Mutex::new(VecDeque::new())),
            }))
        }
    }

    /// A guest UDP socket's other end. There is no host socket behind it:
    /// what is sent to [`RESOLVER`] is answered from here, the rest goes
    /// nowhere.
    #[derive(Debug)]
    struct Resolver {
        policy: Arc<Mutex<Policy>>,
        lookups: Arc<AtomicUsize>,
        replies: Arc<Mutex<VecDeque<Vec<u8>>>>,
    }

    impl HostDgram for Resolver {
        fn send_to(&mut self, buf: &[u8], ip: [u8; 16], v6: bool, port: u16) -> io::Result<usize> {
            let to = address(ip, v6).to_canonical();
            if to != IpAddr::V4(RESOLVER) || port != 53 {
                lock(&self.policy).refuse_datagram(to, port);
                return Ok(buf.len());
            }
            let push = |replies: &Mutex<VecDeque<Vec<u8>>>, reply| {
                replies
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push_back(reply);
            };
            let step = question(&mut lock(&self.policy), buf);
            match step {
                None => {}
                Some(Step::Reply(reply)) => push(&self.replies, reply),
                // The host's resolver may take seconds, and this is the
                // thread the whole guest runs on: ask from another one.
                Some(Step::Lookup(asked)) => {
                    if self.lookups.fetch_add(1, Ordering::SeqCst) >= LOOKUPS {
                        self.lookups.fetch_sub(1, Ordering::SeqCst);
                        return Ok(buf.len());
                    }
                    let (policy, lookups, replies) = (
                        self.policy.clone(),
                        self.lookups.clone(),
                        self.replies.clone(),
                    );
                    let spawned = std::thread::Builder::new()
                        .name("atelier-sandbox-dns".into())
                        .spawn(move || {
                            let found = resolve(&asked.name);
                            let reply = answer(&mut lock(&policy), &asked, found.as_deref());
                            push(&replies, reply);
                            lookups.fetch_sub(1, Ordering::SeqCst);
                        });
                    if spawned.is_err() {
                        self.lookups.fetch_sub(1, Ordering::SeqCst);
                    }
                }
            }
            Ok(buf.len())
        }

        fn recv_from(&mut self) -> io::Result<Option<Datagram>> {
            let reply = self
                .replies
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pop_front();
            Ok(reply.map(|payload| {
                let mut ip = [0u8; 16];
                ip[..4].copy_from_slice(&RESOLVER.octets());
                (ip, false, 53, payload)
            }))
        }
    }

    /// `name`'s addresses by the host's resolver, or `None` if it has none.
    ///
    /// With a trailing dot: the name as it is, never with one of the host's
    /// search domains appended (`crates.io.corp.example` is not the name
    /// that was allowed). Addresses of a family the host has no route for
    /// are left out, so a guest on an IPv4-only host is not sent to IPv6.
    fn resolve(name: &str) -> Option<Vec<IpAddr>> {
        let found = (format!("{name}.").as_str(), 0).to_socket_addrs().ok()?;
        let mut out: Vec<IpAddr> = Vec::new();
        for address in found.map(|a| a.ip().to_canonical()) {
            if !out.contains(&address) && routed(address) {
                out.push(address);
            }
        }
        Some(out)
    }

    /// Whether the host has a route to `address`: connecting a UDP socket
    /// sends nothing, and fails without one.
    fn routed(address: IpAddr) -> bool {
        let any: IpAddr = match address {
            IpAddr::V4(_) => Ipv4Addr::UNSPECIFIED.into(),
            IpAddr::V6(_) => Ipv6Addr::UNSPECIFIED.into(),
        };
        UdpSocket::bind((any, 0)).is_ok_and(|s| s.connect((address, 53)).is_ok())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn bytes(address: IpAddr) -> ([u8; 16], bool) {
            match address {
                IpAddr::V4(a) => {
                    let mut ip = [0u8; 16];
                    ip[..4].copy_from_slice(&a.octets());
                    (ip, false)
                }
                IpAddr::V6(a) => (a.octets(), true),
            }
        }

        /// Through nixvm's own trait, as its kernel calls it: a connection
        /// to an address nobody handed out is unreachable (and no socket is
        /// opened for it), a datagram to a real resolver goes nowhere, and a
        /// question for a name off the list is answered, at once, with
        /// "no such name".
        #[test]
        fn the_egress_refuses_what_the_policy_does() {
            let (list, _) = Allowlist::parse(&["allowed.example"]);
            let filter = Filter::new(list);
            for address in ["1.1.1.1", "127.0.0.1", "192.168.1.1", "2606:4700::1111"] {
                let (ip, v6) = bytes(address.parse().unwrap());
                let refused = filter.connect_tcp(ip, v6, 443).unwrap_err();
                assert_eq!(refused.kind(), io::ErrorKind::NetworkUnreachable);
            }
            assert!(filter.open_icmp(false).is_err());

            let mut udp = filter.open_udp().unwrap();
            let query = crate::sandbox::net::tests::query(7, "other.example", 1);
            // To a resolver that is not ours: dropped.
            let (ip, v6) = bytes("8.8.8.8".parse().unwrap());
            assert_eq!(udp.send_to(&query, ip, v6, 53).unwrap(), query.len());
            assert!(udp.recv_from().unwrap().is_none());
            // To ours, on another port: dropped.
            let (ip, v6) = bytes(IpAddr::V4(RESOLVER));
            udp.send_to(&query, ip, v6, 5353).unwrap();
            assert!(udp.recv_from().unwrap().is_none());
            // To ours: answered from here.
            udp.send_to(&query, ip, v6, 53).unwrap();
            let (from, from_v6, port, reply) = udp.recv_from().unwrap().unwrap();
            assert_eq!((from, from_v6, port), (ip, false, 53));
            assert_eq!(reply[3] & 0x0f, 3, "NXDOMAIN");
            assert!(udp.recv_from().unwrap().is_none());

            let policy = filter.policy();
            let refused = lock(&policy).refused().join("\n");
            assert!(refused.contains("1.1.1.1 port 443"), "{refused}");
            assert!(refused.contains("UDP to 8.8.8.8 port 53"), "{refused}");
            assert!(refused.contains("`other.example`"), "{refused}");
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn list(entries: &[&str]) -> Allowlist {
        let (list, rejected) = Allowlist::parse(entries);
        assert!(rejected.is_empty(), "{rejected:?}");
        list
    }

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    /// A DNS query for `name`, as a stub resolver sends it.
    pub(crate) fn query(id: u16, name: &str, qtype: u16) -> Vec<u8> {
        let mut out = id.to_be_bytes().to_vec();
        out.extend_from_slice(&[0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]);
        for label in name.split('.').filter(|l| !l.is_empty()) {
            out.push(label.len() as u8);
            out.extend_from_slice(label.as_bytes());
        }
        out.push(0);
        out.extend_from_slice(&qtype.to_be_bytes());
        out.extend_from_slice(&CLASS_IN.to_be_bytes());
        out
    }

    /// A reply's code and the addresses it carries.
    fn read(reply: &[u8]) -> (u8, Vec<IpAddr>) {
        assert!(reply[2] & 0x80 != 0, "not a response");
        let count = u16::from_be_bytes([reply[6], reply[7]]);
        // Skip the question: labels, the root, type and class.
        let mut at = 12;
        while reply[at] != 0 {
            at += usize::from(reply[at]) + 1;
        }
        at += 5;
        let mut out = Vec::new();
        for _ in 0..count {
            assert_eq!(&reply[at..at + 2], &[0xc0, 12]);
            let len = usize::from(u16::from_be_bytes([reply[at + 10], reply[at + 11]]));
            let data = &reply[at + 12..at + 12 + len];
            out.push(match len {
                4 => IpAddr::from(<[u8; 4]>::try_from(data).unwrap()),
                _ => IpAddr::from(<[u8; 16]>::try_from(data).unwrap()),
            });
            at += 12 + len;
        }
        assert_eq!(at, reply.len());
        (reply[3] & 0x0f, out)
    }

    /// Ask `policy` for `name`, the host resolving it to `host`.
    fn ask(
        policy: &mut Policy,
        name: &str,
        qtype: u16,
        host: Option<&[IpAddr]>,
    ) -> (u8, Vec<IpAddr>) {
        match question(policy, &query(1, name, qtype)).expect("a query") {
            Step::Reply(reply) => read(&reply),
            Step::Lookup(asked) => read(&answer(policy, &asked, host)),
        }
    }

    #[test]
    fn an_entry_is_a_name_everything_under_a_name_or_an_address() {
        let list = list(&[
            "Crates.IO.",
            "*.crates.io",
            "crates.io",
            "192.168.0.50",
            "2001:db8::1",
        ]);
        assert_eq!(
            list.entries(),
            ["crates.io", "*.crates.io", "192.168.0.50", "2001:db8::1"]
        );
        for name in ["crates.io", "static.crates.io", "a.b.crates.io"] {
            assert!(list.allows_name(name), "{name}");
        }
        for name in [
            "io",
            "xcrates.io",
            "crates.io.evil.example",
            "evil.example",
            "",
            "crates.io.",
            ".crates.io",
            "a..crates.io",
        ] {
            assert!(!list.allows_name(name), "{name}");
        }
        // `*.` does not cover the name itself.
        let under = self::list(&["*.example.org"]);
        assert!(!under.allows_name("example.org"));
        assert!(under.allows_name("www.example.org"));

        let (list, rejected) = Allowlist::parse(&[
            "https://crates.io",
            "crates.io:443",
            "*",
            "*.*.example",
            "foo.*.example",
            "exa mple.org",
            "127.0.0.1",
            "::1",
            "0.0.0.0",
            "224.0.0.1",
            "*.1",
            "10.1",
            "0x7f.1",
            "",
        ]);
        assert!(list.is_empty(), "{:?}", list.entries());
        assert_eq!(rejected.len(), 14, "{rejected:?}");
    }

    /// What `getaddrinfo` would read as an address is never a name, however
    /// the list is written.
    #[test]
    fn a_name_that_reads_as_an_address_is_never_allowed() {
        let list = Allowlist {
            patterns: vec![
                Pattern::Under(".1".to_string()),
                Pattern::Under(".0x1".to_string()),
            ],
            addresses: Vec::new(),
        };
        for name in ["10.0.0.1", "0x7f.1", "127.1", "a.0x1", "2130706433"] {
            assert!(!list.allows_name(name), "{name}");
        }
    }

    #[test]
    fn only_public_addresses_are_public() {
        for address in [
            "1.1.1.1",
            "151.101.2.132",
            "100.63.255.255",
            "100.128.0.0",
            "172.15.0.1",
            "172.32.0.1",
            "198.17.0.1",
            "2606:4700::1111",
            "2a04:4e42::644",
            "64:ff9b::101:101",
            "2002:0101:0101::1",
        ] {
            assert!(public(ip(address)), "{address}");
        }
        for address in [
            "0.0.0.0",
            "0.1.2.3",
            "10.0.0.1",
            "100.64.0.1",
            "100.127.255.255",
            "127.0.0.1",
            "127.8.8.8",
            "169.254.169.254",
            "172.16.0.1",
            "172.31.255.255",
            "192.0.0.1",
            "192.0.2.53",
            "192.168.0.50",
            "198.18.0.1",
            "198.19.0.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "::7f00:1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "64:ff9b::7f00:1",
            "64:ff9b::a00:1",
            "2002:0a00:0001::1",
            "2002:7f00:0001::1",
            "2001::1",
            "2001:db8::1",
            "fe80::1",
            "fc00::1",
            "fd12:3456::1",
            "fec0::1",
            "ff02::1",
            "4000::1",
        ] {
            assert!(!public(ip(address)), "{address}");
        }
    }

    #[test]
    fn a_name_on_the_list_is_answered_and_its_addresses_remembered() {
        let mut policy = Policy::new(list(&["crates.io", "*.crates.io"]));
        let host = [
            ip("151.101.2.132"),
            ip("2a04:4e42::644"),
            ip("151.101.66.132"),
        ];
        // Nothing may be connected to before anything was asked.
        assert!(!policy.may_connect(host[0], 443));

        let (code, v4) = ask(&mut policy, "crates.io", TYPE_A, Some(&host));
        assert_eq!(code, NOERROR);
        assert_eq!(v4, [host[0], host[2]]);
        // The second question for the name does not go to the host again,
        // and each type gets its own family.
        let again = question(&mut policy, &query(2, "CRATES.io", TYPE_AAAA)).unwrap();
        let Step::Reply(reply) = again else {
            panic!("asked the host twice");
        };
        assert_eq!(read(&reply), (NOERROR, vec![host[1]]));
        // The asker's spelling and id come back.
        assert_eq!(&reply[..2], &[0, 2]);
        assert_eq!(&reply[12..23], b"\x06CRATES\x02io\0");

        // Every address answered may be connected to, on any port; v4-mapped
        // is the same address.
        for address in host {
            assert!(policy.may_connect(address, 443));
            assert!(policy.may_connect(address, 22));
        }
        assert!(policy.may_connect(ip("::ffff:151.101.2.132"), 443));
        // Their neighbours may not.
        assert!(!policy.may_connect(ip("151.101.2.133"), 443));
        assert!(!policy.may_connect(ip("1.1.1.1"), 53));
        assert!(!policy.may_connect(IpAddr::V4(RESOLVER), 53));
        let refused = policy.refused().join("\n");
        assert!(refused.contains("151.101.2.133 port 443"), "{refused}");
        assert!(!refused.contains("crates.io"), "{refused}");
    }

    #[test]
    fn a_name_off_the_list_does_not_exist_and_is_not_looked_up() {
        let mut policy = Policy::new(list(&["crates.io"]));
        for name in [
            "example.com",
            "static.crates.io",
            "crates.io.example.com",
            "10.0.0.1",
        ] {
            for qtype in [TYPE_A, TYPE_AAAA, 16, 65] {
                let step = question(&mut policy, &query(9, name, qtype)).unwrap();
                let Step::Reply(reply) = step else {
                    panic!("{name} went to the host's resolver");
                };
                assert_eq!(read(&reply), (NXDOMAIN, vec![]), "{name}");
            }
        }
        assert!(policy.handed.is_empty());
        let refused = policy.refused().join("\n");
        assert!(
            refused.contains("`example.com`, which is not on the allowlist"),
            "{refused}"
        );
        // Said once, however often it was asked.
        assert_eq!(refused.matches("`example.com`").count(), 1, "{refused}");

        // An allowed name has addresses and nothing else: no TXT, no HTTPS
        // records, and no trip to the host for them.
        let step = question(&mut policy, &query(9, "crates.io", 16)).unwrap();
        let Step::Reply(reply) = step else {
            panic!("a TXT question went to the host's resolver");
        };
        assert_eq!(read(&reply), (NOERROR, vec![]));
        // One that does not resolve on the host: no such name, and said so.
        let (code, none) = ask(&mut policy, "crates.io", TYPE_A, None);
        assert_eq!((code, none), (NXDOMAIN, vec![]));
        assert!(
            policy
                .refused()
                .iter()
                .any(|r| r.contains("did not resolve"))
        );
    }

    /// DNS rebinding: the owner of an allowed name answers with an address
    /// inside the user's network. It is not passed on, and stays closed.
    #[test]
    fn an_allowed_name_does_not_lead_to_a_private_address() {
        let mut policy = Policy::new(list(&["rebind.example", "192.168.0.50"]));
        let host = [
            ip("127.0.0.1"),
            ip("10.0.0.5"),
            ip("169.254.169.254"),
            ip("::1"),
            ip("fe80::1"),
            ip("fd00::5"),
            ip("::ffff:10.0.0.6"),
            ip("93.184.216.34"),
            ip("192.168.0.50"),
        ];
        let (_, v4) = ask(&mut policy, "rebind.example", TYPE_A, Some(&host));
        // The public one, and the private one the list names itself.
        assert_eq!(v4, [ip("93.184.216.34"), ip("192.168.0.50")]);
        let (_, v6) = ask(&mut policy, "rebind.example", TYPE_AAAA, Some(&host));
        assert!(v6.is_empty(), "{v6:?}");
        for address in &host[..7] {
            assert!(!policy.may_connect(*address, 80), "{address}");
        }
        assert!(policy.may_connect(ip("192.168.0.50"), 11400));
        let refused = policy.refused().join("\n");
        assert!(
            refused.contains("`rebind.example` resolves to 169.254.169.254"),
            "{refused}"
        );

        // An address on the list needs no name.
        let mut policy = Policy::new(list(&["192.168.0.50"]));
        assert!(policy.may_connect(ip("192.168.0.50"), 11400));
        assert!(!policy.may_connect(ip("192.168.0.51"), 11400));
    }

    #[test]
    fn what_is_not_a_query_is_not_answered_and_a_bad_one_is_an_error() {
        let mut policy = Policy::new(list(&["crates.io"]));
        assert_eq!(question(&mut policy, b""), None);
        assert_eq!(question(&mut policy, &[0; 11]), None);
        // A response.
        let mut response = query(3, "crates.io", TYPE_A);
        response[2] |= 0x80;
        assert_eq!(question(&mut policy, &response), None);

        let code = |packet: &[u8]| match question(&mut Policy::new(list(&["crates.io"])), packet) {
            Some(Step::Reply(reply)) => {
                assert_eq!(&reply[..2], &packet[..2]);
                reply[3] & 0x0f
            }
            other => panic!("{other:?}"),
        };
        // Not a standard query (a status request).
        let mut status = query(3, "crates.io", TYPE_A);
        status[2] |= 0x10;
        assert_eq!(code(&status), NOTIMP);
        // No question, two questions.
        let mut none = query(3, "crates.io", TYPE_A);
        none[5] = 0;
        assert_eq!(code(&none), FORMERR);
        none[5] = 2;
        assert_eq!(code(&none), FORMERR);
        // Cut short, at every length.
        let whole = query(3, "crates.io", TYPE_A);
        for len in 12..whole.len() {
            assert_eq!(code(&whole[..len]), FORMERR, "{len}");
        }
        // A compression pointer where the name should be; a label too long.
        let mut pointer = whole.clone();
        pointer[12] = 0xc0;
        assert_eq!(code(&pointer), FORMERR);
        // Another class.
        let mut chaos = whole.clone();
        let at = chaos.len() - 1;
        chaos[at] = 3;
        assert_eq!(code(&chaos), NOTIMP);
        // A label with a dot in it is not the name it then spells.
        let mut dotted = 4u16.to_be_bytes().to_vec();
        dotted.extend_from_slice(&[0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]);
        dotted.extend_from_slice(b"\x09crates.io\0\0\x01\0\x01");
        assert_eq!(code(&dotted), NXDOMAIN);
        // A query with an EDNS record after the question is a query.
        let mut edns = whole.clone();
        edns[11] = 1;
        edns.extend_from_slice(&[0, 0, 41, 0x04, 0xd0, 0, 0, 0, 0, 0, 0]);
        assert!(matches!(
            question(&mut policy, &edns),
            Some(Step::Lookup(_))
        ));
    }

    #[test]
    fn an_answer_fits_a_datagram_and_the_report_stays_short() {
        let name = format!("{}.crates.io", ["a".repeat(63).as_str(); 3].join("."));
        let mut policy = Policy::new(list(&["*.crates.io"]));
        let host: Vec<IpAddr> = (0..40u16)
            .map(|n| IpAddr::V6(Ipv6Addr::new(0x2a04, 0x4e42, 0, 0, 0, 0, 0, n)))
            .collect();
        let Some(Step::Lookup(asked)) = question(&mut policy, &query(1, &name, TYPE_AAAA)) else {
            panic!("expected a lookup");
        };
        let reply = answer(&mut policy, &asked, Some(&host));
        assert!(reply.len() <= 512, "{}", reply.len());
        assert_eq!(read(&reply).1.len(), ANSWERS);

        for n in 0..100 {
            policy.may_connect(IpAddr::V4(Ipv4Addr::new(8, 8, 8, n)), 53);
            policy.refuse_datagram(IpAddr::V4(Ipv4Addr::new(8, 8, 4, n)), 53);
        }
        assert_eq!(policy.refused().len(), REFUSALS);
    }
}
