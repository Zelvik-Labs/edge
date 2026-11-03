import unittest
from node_control import mode_arguments, validate_attachment, validate_report

MODE = ('worker', 'bridge', 'eager', 'copied')


def kernel_report():
    return {'dataplane': 'kernel', 'proxyReady': True, 'ports': {'udp': 443},
            'app': {'udpDatagrams': 0}}


def af_report():
    value = kernel_report()
    del value['dataplane']
    value.update(xskWakeMode='worker', xskRxStatsMode='bridge', xskIdleYieldMode='eager',
                 xskUdpPayloadMode='copied', proxyRedirectEnabled=True, redirect=200,
                 xskDrops=0, xskQueueStats={'queue0': 0})
    counters = {name: 100 for name in ['received', 'upstreamTx', 'upstreamRx', 'sockIngressDelivered', 'downstreamEnq', 'txOk']}
    counters.update(createOk=16)
    for name in ['blocked', 'createErr', 'createNone', 'downstreamShed', 'droppedOnFail', 'noRoute',
                 'pendingQueueFull', 'pendingSessionsFull', 'sessionClosed', 'sessionFull',
                 'sockIngressShed', 'txDeferred', 'txNoRoute', 'upstreamTxErr']:
        counters[name] = 0
    value['udpProxyDiag'] = counters
    value.update(rateLimited=0, ratelimitMapFull=0, udpFwdMapFull=0, snatAllocFail=0)
    return value


class DataplaneTests(unittest.TestCase):
    def test_kernel_contract_has_no_xsk_modes_or_zero_substitutes(self):
        self.assertEqual(mode_arguments(None), ['--kernel'])
        report = kernel_report()
        validate_report(report, None, 100, 16)
        self.assertEqual(report, kernel_report())
        with self.assertRaisesRegex(AssertionError, 'not applicable'):
            validate_report(dict(report, xskDrops=0), None, 100, 16)

    def test_af_contract_checks_all_directions_and_failures(self):
        self.assertNotIn('--kernel', mode_arguments(MODE))
        validate_report(af_report(), MODE, 100, 16)
        for field in af_report()['udpProxyDiag']:
            report = af_report()
            report['udpProxyDiag'][field] += 1
            with self.subTest(field=field), self.assertRaises(AssertionError):
                validate_report(report, MODE, 100, 16)
        for field, value in [('redirect', 100), ('xskDrops', 1), ('proxyReady', False),
                             ('xskUdpPayloadMode', 'owned'), ('xskQueueStats', {'q': 1})]:
            with self.subTest(field=field), self.assertRaises(AssertionError):
                validate_report(dict(af_report(), **{field: value}), MODE, 100, 16)

    def test_wrong_report_class_and_internal_origin_fail(self):
        with self.assertRaises((KeyError, AssertionError)):
            validate_report(kernel_report(), MODE, 100, 16)
        with self.assertRaises((KeyError, AssertionError)):
            validate_report(af_report(), None, 100, 16)
        for report, mode in [(kernel_report(), None), (af_report(), MODE)]:
            report['app']['udpDatagrams'] = 100
            with self.assertRaisesRegex(AssertionError, 'external'):
                validate_report(report, mode, 100, 16)

    def test_attachment_and_cleanup_do_not_accept_stale_xdp(self):
        none = [{'ifname': 'eup-node'}]
        skb = [{'ifname': 'eup-node', 'xdp': {'mode': 2}}]
        validate_attachment(none, False)
        validate_attachment(skb, True)
        with self.assertRaisesRegex(AssertionError, 'stale'):
            validate_attachment(skb, False)
        with self.assertRaisesRegex(AssertionError, 'expected SKB'):
            validate_attachment(none, True)


if __name__ == '__main__':
    unittest.main()
