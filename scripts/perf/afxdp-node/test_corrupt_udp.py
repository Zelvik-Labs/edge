"""Verify the wire controls are valid/corrupt independently of server behavior."""
import socket
import struct
import unittest
from corrupt_udp import checksum, packet


def folded_sum(data):
    # Separate byte-pair sum so the builder's result is independently checked.
    total = sum((byte << (8 if index % 2 == 0 else 0)) for index, byte in enumerate(data))
    while total > 65535:
        total = (total & 65535) + (total >> 16)
    return total


class WireControlsTest(unittest.TestCase):
    def test_rfc1071_checksum_vector(self):
        self.assertEqual(checksum(bytes.fromhex('0001f203f4f5f6f7')), 0x220d)

    def test_ipv4_positive_and_negative_controls(self):
        for mode in ['valid', 'bad-ip', 'bad-udp', 'zero-udp']:
            frame = packet(socket.AF_INET, '10.200.0.2', '10.200.0.1', b'\x01'*6, b'\x02'*6, 32123, mode, b'odd-bytes')
            ip, udp = frame[14:34], frame[34:]
            self.assertEqual(frame[:6], b'\x02'*6)
            self.assertEqual(struct.unpack('!HH', udp[:4]), (32123, 443))
            self.assertEqual(folded_sum(ip) == 65535, mode != 'bad-ip')
            pseudo = ip[12:20] + struct.pack('!BBH', 0, 17, len(udp))
            if mode == 'zero-udp':
                self.assertEqual(udp[6:8], b'\0\0')
            else:
                self.assertEqual(folded_sum(pseudo+udp) == 65535, mode != 'bad-udp')

    def test_ipv6_positive_and_negative_controls(self):
        for mode in ['valid', 'bad-udp', 'zero-udp']:
            frame = packet(socket.AF_INET6, 'fd00:200::2', 'fd00:200::1', b'\x01'*6, b'\x02'*6, 32123, mode, b'odd-bytes')
            ip, udp = frame[14:54], frame[54:]
            pseudo = ip[8:40] + struct.pack('!I3xB', len(udp), 17)
            if mode == 'zero-udp':
                self.assertEqual(udp[6:8], b'\0\0')
            else:
                self.assertEqual(folded_sum(pseudo+udp) == 65535, mode == 'valid')


if __name__ == '__main__':
    unittest.main()
