#!/usr/bin/env python3
"""Build identical clients against same-version Debian libcurl TLS variants."""
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess

ROOT = Path.cwd()
OUT = ROOT / '.local/quinn-interop-private'


def main():
    versions = {name: subprocess.check_output(['dpkg-query', '-W', '-f=${Version}', name], text=True)
        for name in ['libcurl4t64', 'libcurl3t64-gnutls']}
    assert len(set(versions.values())) == 1, versions
    (OUT / 'clients').mkdir(parents=True)
    source = ROOT / 'benches/support/protocol_state_client.c'
    flags = subprocess.check_output(['pkg-config', '--cflags', 'libcurl'], text=True).split()
    library_flags = subprocess.check_output(['pkg-config', '--libs', 'libcurl'], text=True).split()
    metadata = {'package_version': versions, 'source_sha256': hashlib.sha256(source.read_bytes()).hexdigest(),
                'instrumentation': 'EDGE_QUIC_DIAGNOSTICS absent in both builds', 'clients': {}}
    for variant, libs in [('openssl', library_flags), ('gnutls', ['-Wl,-l:libcurl-gnutls.so.4'])]:
        binary = OUT / 'clients' / variant
        command = ['cc', '-O3', '-march=x86-64-v2', '-Wall', '-Wextra', '-Werror', str(source), *flags, *libs, '-o', str(binary)]
        subprocess.run(command, check=True)
        linkage = subprocess.check_output(['ldd', str(binary)], text=True)
        (OUT / 'clients' / (variant + '.ldd')).write_text(linkage)
        libraries = {}
        for name in re.findall(r'=> (/\S+)', linkage):
            path = Path(name).resolve()
            digest = hashlib.sha256(path.read_bytes()).hexdigest()
            libraries[str(path)] = {'sha256': digest, 'bytes': path.stat().st_size}
            saved = OUT / 'client-libraries' / path.name
            saved.parent.mkdir(exist_ok=True)
            if saved.exists():
                assert hashlib.sha256(saved.read_bytes()).hexdigest() == digest
            else:
                shutil.copy2(path, saved)
        metadata['clients'][variant] = {'command': command, 'sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
            'version': subprocess.check_output([str(binary), '--version'], text=True).strip(), 'libraries': libraries}
    (OUT / 'client-build.json').write_text(json.dumps(metadata, indent=2) + '\n')


if __name__ == '__main__':
    main()
