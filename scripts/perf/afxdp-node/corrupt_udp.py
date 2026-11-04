#!/usr/bin/env python3
"""Owned-netns wire controls: valid replies, bad checksums and IPv4 zero UDP."""
import argparse
import fcntl
import ipaddress
import json
import socket
import struct


def checksum(data):
    if len(data) % 2:
        data += b'\0'
    total = sum(struct.unpack('!'+str(len(data)//2)+'H', data))
    while total >> 16:
        total = (total & 65535) + (total >> 16)
    return (~total) & 65535


def packet(family, source, destination, source_mac, destination_mac, port, mode, payload):
    source = socket.inet_pton(family, source)
    destination = socket.inet_pton(family, destination)
    udp = struct.pack('!HHHH', port, 443, len(payload)+8, 0) + payload
    if family == socket.AF_INET:
        pseudo = source+destination+struct.pack('!BBH', 0, 17, len(udp))
        ip = struct.pack('!BBHHHBBH4s4s', 0x45, 0, 20+len(udp), 0, 0, 64, 17, 0, source, destination)
        ip = ip[:10]+struct.pack('!H', checksum(ip))+ip[12:]
        ethertype = 0x0800
    else:
        assert family == socket.AF_INET6
        pseudo = source+destination+struct.pack('!I3xB', len(udp), 17)
        ip = struct.pack('!IHBB16s16s', 6 << 28, len(udp), 17, 64, source, destination)
        ethertype = 0x86dd
    value = checksum(pseudo+udp) or 65535
    udp = udp[:6]+struct.pack('!H', value)+udp[8:]
    if mode == 'bad-udp':
        assert payload
        udp = udp[:-1]+bytes([udp[-1] ^ 1])
    elif mode == 'zero-udp':
        udp = udp[:6]+b'\0\0'+udp[8:]
    elif mode == 'bad-ip':
        assert family == socket.AF_INET
        ip = ip[:10]+bytes([ip[10] ^ 1])+ip[11:]
    else:
        assert mode == 'valid'
    assert len(source_mac) == len(destination_mac) == 6
    return destination_mac+source_mac+struct.pack('!H', ethertype)+ip+udp


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ['interface', 'destination-mac', 'source4', 'destination4']:
        parser.add_argument('--'+name, required=True)
    parser.add_argument('--source6')
    parser.add_argument('--destination6')
    parser.add_argument('--raw-errors-only', action='store_true')
    args = parser.parse_args()
    assert bool(args.source6) == bool(args.destination6)
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as control:
        source_mac = fcntl.ioctl(control, 0x8927, struct.pack('256s', args.interface.encode()))[18:24]
    destination_mac = bytes.fromhex(args.destination_mac.replace(':', ''))
    families = [(socket.AF_INET, args.source4, args.destination4)]
    if args.source6:
        families.append((socket.AF_INET6, args.source6, args.destination6))
    counts = {'valid_replies': 0, 'invalid_sent': 0, 'invalid_reply_timeouts': 0}
    with socket.socket(socket.AF_PACKET, socket.SOCK_RAW, socket.htons(3)) as wire:
        wire.bind((args.interface, 0))
        for family, source, destination in families:
            with socket.socket(family, socket.SOCK_DGRAM) as receiver:
                receiver.bind((source, 0))
                modes = ['bad-ip', 'bad-udp'] if family == socket.AF_INET else ['bad-udp', 'zero-udp']
                positives = ['valid', 'zero-udp'] if family == socket.AF_INET else ['valid']
                for valid, mode in [(True, mode) for mode in positives if not args.raw_errors_only] + [(False, mode) for mode in modes]:
                    payload = f'checksum-control-{family}-{mode}'.encode()
                    frame = packet(family, source, destination, source_mac, destination_mac,
                                   receiver.getsockname()[1], mode, payload)
                    assert wire.send(frame) == len(frame), 'short raw-frame send'
                    if valid:
                        receiver.settimeout(3)
                        reply, peer = receiver.recvfrom(4096)
                        assert reply == b'xdp-udp-smoke:'+payload, (mode, reply)
                        assert ipaddress.ip_address(peer[0]) == ipaddress.ip_address(destination) and peer[1] == 443
                        counts['valid_replies'] += 1
                    else:
                        counts['invalid_sent'] += 1
                        if args.raw_errors_only:
                            continue  # Full raw-poll statistics prove these arrived.
                        receiver.settimeout(1)
                        try:
                            reply, peer = receiver.recvfrom(4096)
                        except socket.timeout:
                            counts['invalid_reply_timeouts'] += 1
                        else:
                            raise AssertionError(f'{mode} unexpectedly received {reply!r} from {peer!r}')
    print(json.dumps({'udp_checksum_probe': {'passed': True, 'raw_errors_only': args.raw_errors_only, **counts}}))


if __name__ == '__main__':
    main()
