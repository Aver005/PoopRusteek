"""Compare prompt variants from the harness reports.

Only the most recent report per (task, variant) is read: earlier attempts in
this directory ran against a broken image and would otherwise be mixed in.

A run is scored, excluded, or classified by *mechanism*:
  pass      - every expectation met
  announce  - last step was text with no tool call, and the files are missing:
              the model said what it would do instead of doing it
  in-chat   - same, but the text contains a code block: it wrote the code into
              the conversation instead of calling a tool to create the file
  fail      - a substantive failure of some other kind
  excluded  - provider/setup error; says nothing about the prompt
"""
import json, pathlib, re, sys, collections, io
sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding='utf-8', errors='replace')

OUT = pathlib.Path(sys.argv[1])
RATE = re.compile(r'rate_limit|too many requests', re.I)
NAME = re.compile(r'^(greenfield|fixtest|constraints)-([A-D]-\w+)$')

def last_step(path):
    seen = None
    try:
        for line in path.open(encoding='utf-8'):
            r = json.loads(line)
            if r.get('action') == 'agent.step.parsed.payload':
                seen = r['data']
    except (OSError, json.JSONDecodeError):
        return None
    return seen

# newest report per (task, variant)
newest = {}
for report in OUT.rglob('report.json'):
    try:
        data = json.loads(report.read_text(encoding='utf-8'))
    except (OSError, json.JSONDecodeError):
        continue
    m = NAME.match(data.get('name', ''))
    if not m:
        continue
    key = (m.group(1), m.group(2))
    stamp = report.parent.name
    if key not in newest or stamp > newest[key][0]:
        newest[key] = (stamp, report, data)

rows, excluded_why, examples = {}, collections.defaultdict(list), collections.defaultdict(list)
for key, (_stamp, report, data) in newest.items():
    counter = collections.Counter()
    steps = []
    for run in data['runs']:
        err = run['outcome'].get('error') or ''
        status = run['outcome']['status']
        if RATE.search(err):
            counter['excluded'] += 1; excluded_why[key].append('rate limit'); continue
        if status == 'setup_failed':
            counter['excluded'] += 1; excluded_why[key].append(f'setup: {err[:60]}'); continue
        if not run['failures']:
            counter['pass'] += 1
            steps.append((run['metrics']['steps'], run['metrics']['tool_calls']))
            continue
        trace = pathlib.Path(run['trace_path'])
        if not trace.is_file():
            trace = report.parent / trace.name
        info = last_step(trace)
        missing = any('was not created' in f or 'missing or unreadable' in f for f in run['failures'])
        text = (info or {}).get('visible_text', '') or ''
        no_calls = not (info or {}).get('tool_calls')
        if missing and no_calls and text.strip():
            kind = 'in-chat' if '```' in text else 'announce'
            counter[kind] += 1
            examples[(key, kind)].append(text.strip().replace('\n', ' ')[:110])
        else:
            counter['fail'] += 1
            examples[(key, 'fail')].append('; '.join(run['failures'])[:110])
        steps.append((run['metrics']['steps'], run['metrics']['tool_calls']))
    rows[key] = (counter, steps)

hdr = f"{'task':<12}{'variant':<12}{'pass':>5}{'announce':>9}{'in-chat':>8}{'fail':>5}{'excl':>5}{'scored':>8}  steps/calls"
print(hdr); print('-' * len(hdr))
for task in ('greenfield', 'fixtest', 'constraints'):
    for variant in sorted({v for _, v in rows}):
        if (task, variant) not in rows:
            continue
        c, steps = rows[(task, variant)]
        scored = c['pass'] + c['fail'] + c['announce'] + c['in-chat']
        rate = f"{c['pass']}/{scored}" if scored else "n/a"
        sc = (f"{sum(s for s, _ in steps)/len(steps):.1f}/"
              f"{sum(t for _, t in steps)/len(steps):.1f}") if steps else "-"
        print(f"{task:<12}{variant:<12}{c['pass']:>5}{c['announce']:>9}{c['in-chat']:>8}"
              f"{c['fail']:>5}{c['excluded']:>5}{rate:>8}  {sc}")

print("\nWhy runs were excluded")
for key, why in sorted(excluded_why.items()):
    counts = collections.Counter(why)
    print(f"  {key[0]}/{key[1]}: " + ", ".join(f"{n}x {w}" for w, n in counts.most_common()))

for kind, title in (('in-chat', 'Wrote the code into the chat instead of calling a tool'),
                    ('announce', 'Announced the work and stopped'),
                    ('fail', 'Other substantive failures')):
    shown = [(k, v) for (k, kk), v in examples.items() if kk == kind]
    if not shown:
        continue
    print(f"\n{title}")
    for key, texts in sorted(shown):
        print(f"  {key[0]}/{key[1]}: {texts[0]}")
