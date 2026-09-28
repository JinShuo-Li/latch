"""Evaluator-only checks. This file is never copied into the agent workspace."""

import json
import subprocess
import sys
from pathlib import Path


CHECKS = {
    "handler_failure_retries_same_job": '''
from pathlib import Path
import tempfile
from spool_recovery import process_spool
with tempfile.TemporaryDirectory() as d:
    p, c = Path(d)/'spool', Path(d)/'offset'
    p.write_bytes(b'{"id":1}\\n{"id":2}\\n')
    seen=[]
    def handler(job):
        if job['id']==2: raise RuntimeError('failed')
        seen.append(job['id'])
    try: process_spool(p,c,handler)
    except RuntimeError: pass
    retry=[]
    process_spool(p,c,retry.append)
    print(seen == [1] and retry == [{'id':2}])
''',
    "partial_line_survives_restart": '''
from pathlib import Path
import tempfile
from spool_recovery import process_spool
with tempfile.TemporaryDirectory() as d:
    p, c = Path(d)/'spool', Path(d)/'offset'
    p.write_bytes(b'{"id":1}\\n{"id":')
    seen=[]
    first=process_spool(p,c,seen.append)
    with p.open('ab') as f: f.write(b'2}\\n')
    second=process_spool(p,c,seen.append)
    print(first == 1 and second == 1 and seen == [{'id':1},{'id':2}])
''',
    "multibyte_offsets_and_append": '''
from pathlib import Path
import tempfile
from spool_recovery import process_spool
with tempfile.TemporaryDirectory() as d:
    p, c = Path(d)/'spool', Path(d)/'offset'
    p.write_bytes('{"name":"雪"}\\n'.encode())
    seen=[]
    process_spool(p,c,seen.append)
    with p.open('ab') as f: f.write(b'{"id":2}\\n')
    process_spool(p,c,seen.append)
    print(seen == [{'name':'雪'},{'id':2}] and int(c.read_text()) == p.stat().st_size)
''',
    "stale_checkpoint_fails_closed": '''
from pathlib import Path
import tempfile
from spool_recovery import process_spool
with tempfile.TemporaryDirectory() as d:
    p, c = Path(d)/'spool', Path(d)/'offset'
    p.write_bytes(b'{"id":1}\\n')
    c.write_text('999')
    seen=[]
    try: process_spool(p,c,seen.append)
    except (ValueError, OSError): print(seen == [] and c.read_text() == '999')
    else: print(False)
''',
}


root = Path(sys.argv[1]).resolve()
checks = []
for name, code in CHECKS.items():
    result = subprocess.run([sys.executable, "-c", code], cwd=root, text=True,
                            capture_output=True, timeout=10)
    checks.append({"name": name, "passed": result.returncode == 0 and result.stdout.strip() == "True",
                   "detail": result.stderr[-500:] or result.stdout.strip()})
print(json.dumps({"checks": checks}))
