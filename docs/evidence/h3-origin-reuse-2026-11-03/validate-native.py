"""Recompute the 32 native suites from their retained raw outputs."""
import json
from pathlib import Path
import re
import sys

root, output = map(Path, sys.argv[1:3])
source = sys.argv[3]
metadata = json.loads((root/'metadata.json').read_text())
summary = json.loads((root/'summary.json').read_text())
assert metadata['commit'] == source
assert metadata['network_namespace'] != metadata['parent_network_namespace']
assert metadata['mount_namespace'] != metadata['parent_mount_namespace']
assert summary['passed'] and summary['binaries_unchanged']
assert summary['binaries'] == metadata['binaries']
assert 'HTTP2' in (root/'curl.log').read_text()
labels = ['full-1-worker', 'bridge-1-worker', 'bridge-1-readable', 'full-1-readable',
          'full-2-readable', 'bridge-2-readable', 'bridge-2-worker', 'full-2-worker']
labels = [payload+'-'+idle+'-'+label for payload in ['copied','owned'] for idle in ['eager', 'cooperative'] for label in labels]
assert [case['label'] for case in summary['cases']] == labels
reports = {}
h2_count = quic_count = raw_corrupt_count = rejected_count = control_replies = 0
for case in summary['cases']:
    label, mode, stats_mode = case['label'], case['wake'], case['rx_stats']
    assert case['passed'] and case['exit_code'] == 0 and case['network_cleanup_ok']
    assert {link['ifname'] for link in case['remaining_links']} == {'lo'}
    assert not case['remaining_namespaces']
    assert json.loads((root/(label+'.json')).read_text()) == case
    log = (root/(label+'.log')).read_text()
    objects, cursor, decoder = [], 0, json.JSONDecoder()
    while (cursor := log.find('{', cursor)) >= 0:
        try:
            data, end = decoder.raw_decode(log[cursor:])
        except json.JSONDecodeError:
            cursor += 1
            continue
        cursor += end
        if isinstance(data, dict):
            objects.append(data)
    app = next(data for data in objects if 'app' in data)
    reload = next(data for data in objects if 'managerReplaced' in data)
    assert app['xskWakeMode'] == reload['xskWakeMode'] == mode
    assert app['xskRxStatsMode'] == reload['xskRxStatsMode'] == stats_mode
    assert app['xskIdleYieldMode'] == reload['xskIdleYieldMode'] == case['idle_yield']
    assert app['xskUdpPayloadMode'] == reload['xskUdpPayloadMode'] == case['udp_payload']
    assert label.startswith(case['udp_payload']+'-'+case['idle_yield']+'-')
    assert app['proxyReady'] and app['proxyRedirectEnabled'] and app['xskDrops'] == 0
    assert all(value == 0 for value in app['xskQueueStats'].values())
    assert app['app'] == {'httpRequests': 2, 'httpsRequests': 4, 'tcpConnections': 2,
                          'udpDatagrams': 5, 'h3Requests': 2, 'sniConnections': 2, 'quicRequests': 2}
    assert reload['managerReplaced'] and reload['bridgeSupervisorAlive']
    for stage in ['beforeReload', 'afterReload']:
        assert reload[stage]['proxyReady'] and reload[stage]['proxyRedirectEnabled']
    raw = [data for data in objects if 'pollStats' in data]
    assert len(raw) == 2
    valid_raw = next(data for data in raw if data['frames'] > 0)
    bypass_raw = next(data for data in raw if data['frames'] == 0)
    assert valid_raw['pollStats']['parseErrors'] == valid_raw['unparseable'] == 4
    assert valid_raw['pollStats']['parsed'] == valid_raw['udp']+valid_raw['tcp'] >= 4
    assert valid_raw['pollStats']['packets'] == valid_raw['frames']
    assert valid_raw['frames'] == valid_raw['pollStats']['parsed']+4
    assert bypass_raw['redirect'] == bypass_raw['pollStats']['packets'] == 0
    assert bypass_raw['pollStats']['parsed'] == bypass_raw['pollStats']['parseErrors'] == 0
    probes = [data['udp_checksum_probe'] for data in objects if 'udp_checksum_probe' in data]
    assert len(probes) == 2 and all(probe['passed'] for probe in probes)
    raw_probe = next(probe for probe in probes if probe['raw_errors_only'])
    app_probe = next(probe for probe in probes if not probe['raw_errors_only'])
    assert raw_probe['invalid_sent'] == 4 and raw_probe['valid_replies'] == 0
    assert app_probe['valid_replies'] == 3
    assert app_probe['invalid_sent'] == app_probe['invalid_reply_timeouts'] == 4
    h2_lines = re.findall(r'^h2\([^)]+\): 2$', log, re.M)
    quic_lines = re.findall(r'^requests=1 ok=1 failed=0 .*$', log, re.M)
    assert len(h2_lines) == 2 and len(quic_lines) == 4 and 'skipping HTTP/2' not in log
    h2_count += len(h2_lines)
    quic_count += len(quic_lines)
    raw_corrupt_count += valid_raw['pollStats']['parseErrors']
    rejected_count += app_probe['invalid_reply_timeouts']
    control_replies += app_probe['valid_replies']
    reports[label] = {'app': app, 'reload': reload, 'raw': raw, 'wire_controls': probes,
                      'h2_lines': h2_lines, 'h3_quic_probe_lines': quic_lines}
result = {'passed': True, 'source_commit': source, 'cases': labels,
          'h2_checks': h2_count, 'h3_quic_probes': quic_count,
          'raw_corrupt_frames_counted': raw_corrupt_count,
          'invalid_application_reply_timeouts': rejected_count,
          'valid_wire_control_replies': control_replies,
          'binary_sha256': metadata['binaries'],
          'scope': 'functional SKB/copy veth suites, not throughput or hardware zero-copy; negative reply timeout requires the separate positive controls and raw arrival counters'}
output.write_text(json.dumps(result, indent=2)+'\n')
output.with_name('native-reports.json').write_text(json.dumps(reports, indent=2)+'\n')
print(json.dumps(result))
