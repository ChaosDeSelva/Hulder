#!/usr/bin/env python3
"""Fail release validation when supported Hulder dependencies have security advisories."""
import argparse
import json
import subprocess
import sys
import urllib.request
from pathlib import Path

# These are maintenance notices, not suppressions of vulnerability or unsoundness reports.
REVIEWED_MAINTENANCE = {
    'RUSTSEC-2024-0370', 'RUSTSEC-2024-0384', 'RUSTSEC-2024-0436',
    'RUSTSEC-2024-0449', 'RUSTSEC-2025-0052', 'RUSTSEC-2025-0134',
    'RUSTSEC-2025-0141', 'RUSTSEC-2026-0058', 'RUSTSEC-2026-0105',
    'RUSTSEC-2026-0192', 'RUSTSEC-2026-0206',
}

def dependency_closure(metadata, package_name):
    roots = [package['id'] for package in metadata['packages']
             if package['name'] == package_name and package.get('source') is None]
    if len(roots) != 1:
        raise ValueError('Expected exactly one local release root')
    nodes = {node['id']: node for node in metadata['resolve']['nodes']}
    pending = roots.copy()
    visited = set()
    while pending:
        identifier = pending.pop()
        if identifier in visited:
            continue
        visited.add(identifier)
        for dependency in nodes[identifier]['deps']:
            if any(kind['kind'] != 'dev' for kind in dependency['dep_kinds']):
                pending.append(dependency['pkg'])
    return [package for package in metadata['packages'] if package['id'] in visited]

def api(path, payload=None):
    data = None if payload is None else json.dumps(payload).encode()
    request = urllib.request.Request('https://api.osv.dev/v1/' + path, data=data,
                                    headers={'Content-Type': 'application/json'})
    with urllib.request.urlopen(request, timeout=60) as response:
        return json.load(response)

def maintenance_notice(advisory):
    return advisory['id'] in REVIEWED_MAINTENANCE and all(
        affected.get('database_specific', {}).get('informational') == 'unmaintained'
        for affected in advisory.get('affected', [])
    ) and bool(advisory.get('affected'))

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--metadata', type=Path)
    parser.add_argument('--output', type=Path, required=True)
    arguments = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    if arguments.metadata:
        metadata = json.loads(arguments.metadata.read_text())
    else:
        metadata = json.loads(subprocess.check_output([
            'cargo', 'metadata', '--locked', '--format-version', '1',
            '--filter-platform', 'aarch64-apple-darwin', '--features', 'util/debug-embed',
        ], cwd=root))
    packages = dependency_closure(metadata, 'zed')
    registry = [package for package in packages if (package.get('source') or '').startswith('registry+')]
    findings = []
    details = {}
    for offset in range(0, len(registry), 100):
        batch = registry[offset:offset + 100]
        result = api('querybatch', {'queries': [
            {'package': {'name': package['name'], 'ecosystem': 'crates.io'}, 'version': package['version']}
            for package in batch
        ]})
        if len(result['results']) != len(batch):
            raise ValueError('Incomplete advisory response')
        for package, entry in zip(batch, result['results']):
            for match in entry.get('vulns', []):
                identifier = match['id']
                if identifier not in details:
                    details[identifier] = api('vulns/' + identifier)
                advisory = details[identifier]
                if advisory.get('withdrawn'):
                    continue
                findings.append({'package': package['name'], 'version': package['version'],
                                 'advisory': identifier, 'summary': advisory.get('summary'),
                                 'disposition': 'maintenance' if maintenance_notice(advisory) else 'block'})
    report = {
        'target': 'aarch64-apple-darwin', 'root': 'zed',
        'registryPackagesChecked': len(registry), 'findings': findings,
        'gitDependencies': [{'name': package['name'], 'source': package['source']}
                            for package in packages if (package.get('source') or '').startswith('git+')],
        'limitations': 'Registry advisory audit only. Git dependencies require source review. Other targets are not cleared.',
        'advisories': details,
    }
    arguments.output.write_text(json.dumps(report, indent=2) + '\n')
    blockers = [finding for finding in findings if finding['disposition'] == 'block']
    print(f"Checked {len(registry)} registry packages; {len(blockers)} blockers; {len(findings) - len(blockers)} maintenance notices")
    return bool(blockers)

if __name__ == '__main__':
    try:
        sys.exit(main())
    except Exception as error:
        print(f'Release audit failed: {error}', file=sys.stderr)
        sys.exit(2)
