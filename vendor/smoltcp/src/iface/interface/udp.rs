use super::*;

#[cfg(feature = "socket-dns")]
use crate::socket::dns::Socket as DnsSocket;

#[cfg(feature = "socket-udp")]
use crate::socket::udp::Socket as UdpSocket;

impl InterfaceInner {
    pub(super) fn process_udp<'frame>(
        &mut self,
        sockets: &mut SocketSet,
        meta: PacketMeta,
        handled_by_raw_socket: bool,
        ip_repr: IpRepr,
        ip_payload: &'frame [u8],
    ) -> Option<Packet<'frame>> {
        let (src_addr, dst_addr) = (ip_repr.src_addr(), ip_repr.dst_addr());
        let udp_packet = check!(UdpPacket::new_checked(ip_payload));
        let udp_repr = check!(UdpRepr::parse(
            &udp_packet,
            &src_addr,
            &dst_addr,
            &self.caps.checksum
        ));

        #[cfg(feature = "socket-udp")]
        {
            // Find the best matching socket based on priority score
            // We need to do this in two passes because we can't hold mutable references
            // to multiple sockets at once.

            // First pass: find the best match
            let mut best_match: Option<(usize, u8)> = None;

            for (idx, item) in sockets.items().enumerate() {
                if let Some(udp_socket) = UdpSocket::downcast(&item.socket) {
                    let score = udp_socket.accepts(self, &ip_repr, &udp_repr);
                    if score > 0 {
                        match best_match {
                            Some((_, best_score)) if score > best_score => {
                                best_match = Some((idx, score));
                            }
                            None => {
                                best_match = Some((idx, score));
                            }
                            _ => {}
                        }
                    }
                }
            }

            // Second pass: process the packet with the best matching socket
            if let Some((best_idx, _)) = best_match {
                for (idx, item) in sockets.items_mut().enumerate() {
                    if idx == best_idx {
                        if let Some(udp_socket) = UdpSocket::downcast_mut(&mut item.socket) {
                            udp_socket.process(self, meta, &ip_repr, &udp_repr, udp_packet.payload());
                            return None;
                        }
                    }
                }
            }
        }

        #[cfg(feature = "socket-dns")]
        for dns_socket in sockets
            .items_mut()
            .filter_map(|i| DnsSocket::downcast_mut(&mut i.socket))
        {
            if dns_socket.accepts(&ip_repr, &udp_repr) {
                dns_socket.process(self, &ip_repr, &udp_repr, udp_packet.payload());
                return None;
            }
        }

        // The packet wasn't handled by a socket, send an ICMP port unreachable packet.
        match ip_repr {
            #[cfg(feature = "proto-ipv4")]
            IpRepr::Ipv4(_) if handled_by_raw_socket => None,
            #[cfg(feature = "proto-ipv6")]
            IpRepr::Ipv6(_) if handled_by_raw_socket => None,
            #[cfg(feature = "proto-ipv4")]
            IpRepr::Ipv4(ipv4_repr) => {
                let payload_len =
                    icmp_reply_payload_len(ip_payload.len(), IPV4_MIN_MTU, ipv4_repr.buffer_len());
                let icmpv4_reply_repr = Icmpv4Repr::DstUnreachable {
                    reason: Icmpv4DstUnreachable::PortUnreachable,
                    header: ipv4_repr,
                    data: &ip_payload[0..payload_len],
                };
                self.icmpv4_reply(ipv4_repr, icmpv4_reply_repr)
            }
            #[cfg(feature = "proto-ipv6")]
            IpRepr::Ipv6(ipv6_repr) => {
                let payload_len =
                    icmp_reply_payload_len(ip_payload.len(), IPV6_MIN_MTU, ipv6_repr.buffer_len());
                let icmpv6_reply_repr = Icmpv6Repr::DstUnreachable {
                    reason: Icmpv6DstUnreachable::PortUnreachable,
                    header: ipv6_repr,
                    data: &ip_payload[0..payload_len],
                };
                self.icmpv6_reply(ipv6_repr, icmpv6_reply_repr)
            }
        }
    }
}
