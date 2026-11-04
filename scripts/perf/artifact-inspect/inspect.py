#!/usr/bin/env python3
"""Read retained diagnostic text after verifying the original ZIP; execute no contents."""
import importlib.util
import json
import os
from pathlib import Path
import tempfile

ROOT = Path.cwd()
spec = importlib.util.spec_from_file_location('archive_review', ROOT / 'scripts/perf/evidence-review/review.py')
review = importlib.util.module_from_spec(spec)
spec.loader.exec_module(review)
review.OUT = ROOT / '.local/artifact-inspect'
run = int(os.environ.get('INSPECT_RUN') or '41818510632')
source = os.environ.get('INSPECT_SOURCE') or '429ccf12b481c548a24bc66f6f3609b3a32513fc'
assert len(source) == 40 and all(value in '0123456789abcdef' for value in source)
prefix = os.environ.get('INSPECT_PREFIX') or 'idle-cpu-'
with tempfile.TemporaryDirectory() as folder:
    extracted, artifact = review.download(run, source, prefix, Path(folder))
    print('ARTIFACT_INSPECT_META ' + json.dumps(dict(run=run, source=source, artifact=artifact['id'], digest=artifact['digest'])))
    seen = set()
    for path in sorted(extracted.rglob('*')):
        if not path.is_file() or not (path.name.endswith('.driver.log') or path.name == 'suite.json'):
            continue
        assert path.stat().st_size <= 1024 * 1024, 'diagnostic text exceeds bounded inspector budget'
        digest = review.sha(path)
        print('ARTIFACT_INSPECT_TEXT ' + json.dumps(dict(path=str(path.relative_to(extracted)), sha256=digest,
              duplicate=digest in seen, content=path.read_text() if digest not in seen else None)))
        seen.add(digest)
