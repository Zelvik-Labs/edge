"""Separate dataplane contracts; absent kernel XSK counters are never zero-filled."""


def mode_arguments(mode):
    if mode is None:
        return ['--kernel']
    wake, statistics, idle, payload = mode
    return ['--wake', wake, '--rx-stats', statistics, '--idle-yield', idle,
            '--udp-payload', payload]


def validate_attachment(links, afxdp):
    assert len(links) == 1 and links[0]['ifname'] == 'eup-node', 'unexpected interface'
    xdp = links[0].get('xdp')
    if afxdp:
        assert xdp is not None and xdp['mode'] == 2, 'expected SKB XDP attachment'
    else:
        assert xdp is None or xdp['mode'] == 0, 'stale XDP on kernel dataplane'


def validate_report(report, mode, sent, concurrency):
    assert report['proxyReady'] and report['ports']['udp'] == 443
    assert report['app']['udpDatagrams'] == 0, 'origin must be external'
    if mode is None:
        assert report['dataplane'] == 'kernel'
        assert not {'redirect', 'xskDrops', 'xskQueueStats'} & report.keys(), 'kernel XSK counters are not applicable'
        return
    assert report.get('dataplane') != 'kernel'
    for field, value in zip(['xskWakeMode', 'xskRxStatsMode', 'xskIdleYieldMode', 'xskUdpPayloadMode'], mode):
        assert report[field] == value, field
    assert report['proxyRedirectEnabled']
    assert report['redirect'] == 2*sent, 'both traffic directions must use XSK'
    assert report['xskDrops'] == 0 and report['xskQueueStats']
    assert all(value == 0 for value in report['xskQueueStats'].values())
    counters = report['udpProxyDiag']
    for name in ['received', 'upstreamTx', 'upstreamRx', 'sockIngressDelivered', 'downstreamEnq', 'txOk']:
        assert counters[name] == sent, (name, counters[name], sent)
    assert counters['createOk'] == concurrency
    for name in ['blocked', 'createErr', 'createNone', 'downstreamShed', 'droppedOnFail', 'noRoute',
                 'pendingQueueFull', 'pendingSessionsFull', 'sessionClosed', 'sessionFull',
                 'sockIngressShed', 'txDeferred', 'txNoRoute', 'upstreamTxErr']:
        assert counters[name] == 0, (name, counters[name])
    for name in ['rateLimited', 'ratelimitMapFull', 'udpFwdMapFull', 'snatAllocFail']:
        assert report[name] == 0, name
