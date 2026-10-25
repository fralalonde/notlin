#!/usr/bin/env python3
"""Rank explicit default-provider blockers; never print corpus identifiers."""
import argparse
import collections
import hashlib
import json
import re
from pathlib import Path


FAMILIES = {
    'jackson_defaults': 'Jackson observes Kotlin default constructor parameters',
    'covariant_provider': 'inherited covariant getter needs a concrete Java bridge',
    'unknown_default_getter': 'cannot prove inherited default getter',
    'secondary_constructor': 'secondary constructor is not a direct',
}


def identity(symbol):
    return json.dumps(symbol, sort_keys=True, separators=(',', ':'))


def analyze(report):
    rows = report['declarations']
    keys = [identity(row['symbol']) for row in rows]
    assert len(keys) == len(set(keys)), 'duplicate complete declaration identity'
    translated = sum(bool(row['translated']) for row in rows)
    assert len(rows) == report['original']
    assert translated == report['translated']
    assert len(rows) - translated == report['retained']
    owners = collections.defaultdict(list)
    for row in rows:
        symbol = row['symbol']
        fq = '.'.join(filter(None, [symbol['package'], *symbol['owner_path'], symbol['name']]))
        owners[fq].append(row)
    family_counts = collections.Counter()
    reason_counts = collections.Counter()
    blocked = collections.defaultdict(set)
    private = {}
    unresolved = set()
    for row in rows:
        if row['translated']:
            continue
        messages = []
        seen_reasons = set()
        for reason in row['reasons']:
            for kind, value in reason.items():
                seen_reasons.add(kind)
                if isinstance(value, dict):
                    messages.append(value.get('message', ''))
        reason_counts.update(seen_reasons)
        text = '\n'.join(messages)
        family_counts.update(key for key, needle in FAMILIES.items() if needle in text)
        for message in messages:
            if FAMILIES['covariant_provider'] not in message:
                continue
            for owner, _method in re.findall(r'`([^`#]+)#([^`]+)`', message):
                matches = owners.get(owner, [])
                if len(matches) != 1 or matches[0]['symbol']['kind'].startswith('ambiguous:'):
                    unresolved.add((identity(row['symbol']), owner))
                    continue
                target = matches[0]
                key = identity(target['symbol'])
                token = hashlib.sha256(key.encode()).hexdigest()[:16]
                blocked[token].add(identity(row['symbol']))
                private[token] = target['symbol']
    providers = [dict(root_id=token, direct_blocked=len(children))
                 for token, children in blocked.items()]
    providers.sort(key=lambda item: (-item['direct_blocked'], item['root_id']))
    result = dict(original=len(rows), translated=translated,
                  retained=len(rows)-translated,
                  families=dict(sorted(family_counts.items())),
                  reason_declarations=dict(sorted(reason_counts.items())),
                  providers=providers,
                  unresolved_provider_mentions=len(unresolved))
    return result, private


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('report', type=Path)
    parser.add_argument('--private-map', type=Path)
    args = parser.parse_args()
    result, private = analyze(json.loads(args.report.read_text()))
    if args.private_map:
        args.private_map.parent.mkdir(parents=True, exist_ok=True)
        args.private_map.write_text(json.dumps(private, indent=2)+'\n')
    print(json.dumps(result, indent=2))


if __name__ == '__main__':
    main()
