#!/usr/bin/env python3
"""Publish fixed original archives without changing any release/tag/source bytes."""
import base64
import hashlib
import json
import os
from pathlib import Path
import subprocess
import urllib.parse
from urllib.request import Request, urlopen

ROOT = Path.cwd()
REPO = 'Zelvik-Labs/edge'
API = 'https://api.github.com/repos/' + REPO
INDEX_PATH = ROOT / '.github/release-evidence-v26.11.1.1.json'
INDEX = json.loads(INDEX_PATH.read_text())
TAG = INDEX['release_tag']
assert TAG == 'v26.11.1.1'
assert INDEX['release_source'] == '21e01923ea5daf31152dfee18d72bf5697ed6029'
OUT = ROOT / '.local/release-evidence'
OUT.mkdir(parents=True, exist_ok=True)


def sha(path):
    digest = hashlib.sha256()
    with path.open('rb') as source:
        for block in iter(lambda: source.read(1024 * 1024), b''):
            digest.update(block)
    return digest.hexdigest()


def api(path):
    request = Request(API + path, headers={'Accept':'application/vnd.github+json'})
    request.add_unredirected_header('Authorization', 'Bearer ' + os.environ['GH_TOKEN'])
    with urlopen(request, timeout=60) as source:
        return json.load(source)


def upload_exact(path, release):
    existing = [a for a in release['assets'] if a['name'] == path.name]
    if existing:
        assert len(existing) == 1
        assert existing[0]['state'] == 'uploaded'
        assert existing[0]['size'] == path.stat().st_size
        assert existing[0]['digest'] == 'sha256:' + sha(path)
        return
    subprocess.run(['gh','release','upload',TAG,str(path),'--repo',REPO], check=True)


release = api('/releases/tags/' + TAG)
assert not release['draft'] and not release['prerelease']
ref = api('/git/ref/tags/' + TAG)['object']
if ref['type'] == 'tag':
    ref = api('/git/tags/' + ref['sha'])['object']
assert ref['type'] == 'commit' and ref['sha'] == INDEX['release_source']
files = []
for row in INDEX['archives']:
    artifact = api('/actions/artifacts/' + str(row['original_artifact_id']))
    assert not artifact['expired'] and artifact['workflow_run']['id'] == row['workflow_run']['id']
    assert artifact['workflow_run']['head_sha'] == row['workflow_run']['head_sha']
    assert artifact['size_in_bytes'] == row['sizeBytes']
    assert artifact['digest'] == 'sha256:' + row['sha256']
    path = OUT / row['name']
    request = Request(API + '/actions/artifacts/' + str(artifact['id']) + '/zip')
    # The repository token never follows the Azure archive redirect.
    request.add_unredirected_header('Authorization', 'Bearer ' + os.environ['GH_TOKEN'])
    with urlopen(request, timeout=120) as source, path.open('wb') as dest:
        for block in iter(lambda: source.read(1024 * 1024), b''):
            dest.write(block)
    assert path.stat().st_size == row['sizeBytes'] and sha(path) == row['sha256']
    files.append(path)
    print(json.dumps({'original_archive_verified':row['name'],'sha256':row['sha256']}), flush=True)
index_file = OUT / 'evidence-index.json'
index_file.write_bytes(INDEX_PATH.read_bytes())
for path in [*files,index_file]:
    upload_exact(path, release)
current = api('/releases/tags/' + TAG)
by_name = {a['name']:a for a in current['assets']}
for path in [*files,index_file]:
    assert by_name[path.name]['state'] == 'uploaded'
    assert by_name[path.name]['digest'] == 'sha256:' + sha(path)
    assert by_name[path.name]['size'] == path.stat().st_size

manifest_row = by_name['release-manifest.json']
with urlopen(manifest_row['browser_download_url'], timeout=60) as source:
    raw_manifest = source.read()
assert 'sha256:' + hashlib.sha256(raw_manifest).hexdigest() == manifest_row['digest']
manifest = json.loads(raw_manifest)
assert manifest['sourceCommit'] == INDEX['release_source'] and manifest['tag'] == TAG
assert manifest['version'] == '26.11.1.1' and len(manifest['assets']) == 5
for row in manifest['assets']:
    assert by_name[row['name']]['digest'] == 'sha256:' + row['sha256']
    assert by_name[row['name']]['size'] == row['sizeBytes']

image = manifest['images'][0]
assert image['image'] == 'ghcr.io/zelvik-labs/zelvik-edge:26.11.1.1'
repository = 'zelvik-labs/zelvik-edge'
query = urllib.parse.urlencode({'service':'ghcr.io','scope':'repository:'+repository+':pull'})
request = Request('https://ghcr.io/token?' + query)
credentials = os.environ['GITHUB_ACTOR'] + ':' + os.environ['GH_TOKEN']
request.add_unredirected_header('Authorization','Basic '+base64.b64encode(credentials.encode()).decode())
with urlopen(request, timeout=60) as source:
    registry_token = json.load(source)['token']
accept = ', '.join(['application/vnd.oci.image.index.v1+json',
    'application/vnd.docker.distribution.manifest.list.v2+json',
    'application/vnd.oci.image.manifest.v1+json',
    'application/vnd.docker.distribution.manifest.v2+json'])


def registry(kind, reference, expected):
    request = Request('https://ghcr.io/v2/'+repository+'/'+kind+'/'+reference, headers={'Accept':accept})
    request.add_unredirected_header('Authorization','Bearer '+registry_token)
    with urlopen(request, timeout=60) as source:
        raw = source.read()
        assert 'sha256:'+hashlib.sha256(raw).hexdigest() == expected
        return json.loads(raw)


index = registry('manifests',manifest['version'],image['digest'])
assert registry('manifests',image['digest'],image['digest']) == index
children = [r for r in index['manifests'] if r.get('platform',{}).get('os') == 'linux'
            and r['platform'].get('architecture') in {'amd64','arm64'}]
assert len(children) == 2 and {r['platform']['architecture'] for r in children} == {'amd64','arm64'}
platforms = []
for child in children:
    body = registry('manifests',child['digest'],child['digest'])
    config = registry('blobs',body['config']['digest'],body['config']['digest'])
    assert config['architecture'] == child['platform']['architecture'] and config['os'] == 'linux'
    labels = config['config']['Labels']
    assert labels['org.opencontainers.image.version'] == manifest['version']
    assert labels['org.opencontainers.image.revision'] == manifest['sourceCommit']
    platforms.append({'architecture':config['architecture'],'digest':child['digest']})
report = {'release_tag':TAG,'release_source':manifest['sourceCommit'],
    'original_zip_bytes_verified_and_published':True,'evidence_archives':INDEX['archives'],
    'primary_manifest_assets_match_server_digests':True,'image':image,
    'authenticated_registry_read_verified':True,'platforms':platforms}
report_path = OUT / 'publication-verification.json'
report_path.write_text(json.dumps(report,indent=2)+'\n')
upload_exact(report_path,api('/releases/tags/'+TAG))
print(json.dumps(report),flush=True)
