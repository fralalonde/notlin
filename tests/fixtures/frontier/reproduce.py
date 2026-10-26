#!/usr/bin/env python3
"""Characterize frontier fixtures. Candidate failures are evidence, not test passes."""
import argparse
import json
import os
import re
from pathlib import Path
import shutil
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--notlin', required=True, type=Path)
    parser.add_argument('--java-home', required=True, type=Path)
    parser.add_argument('--kotlinc', required=True, type=Path)
    parser.add_argument('--stdlib', required=True, type=Path)
    parser.add_argument('--full-helper', type=Path)
    args = parser.parse_args()
    args.notlin = args.notlin.resolve(strict=True)
    root = Path(__file__).resolve().parent
    work = Path(tempfile.mkdtemp(prefix='notlin-frontier-', dir=os.environ['TMPDIR']))
    if work.is_relative_to(root.parents[2]):
        raise RuntimeError('Probe outputs must not be inside the repository/indexing root')
    print('Owned evidence directory:', work, flush=True)
    env = {k: v for k, v in os.environ.items() if not k.startswith('NOTLIN_')}
    env['JAVA_HOME'] = str(args.java_home)
    java = args.java_home / 'bin/java'
    javac = args.java_home / 'bin/javac'
    javap = args.java_home / 'bin/javap'
    steps = {}

    def run(name, command, *, cwd=None, required=True, extra_env=None):
        child_env = dict(env)
        child_env.update(extra_env or {})
        result = subprocess.run(list(map(str, command)), cwd=cwd, env=child_env,
                                capture_output=True, text=True, timeout=120)
        (work / (name + '.stdout')).write_text(result.stdout)
        (work / (name + '.stderr')).write_text(result.stderr)
        steps[name] = {'exit_code': result.returncode}
        if required and result.returncode:
            raise RuntimeError(f'{name} failed; see {work}/{name}.stderr')
        return result

    def cli(name, source, *, helper=False, classpath=''):
        extra = {}
        if helper:
            extra = dict(NOTLIN_JAVA=str(java),
                         NOTLIN_JVM_FACTS_JAR=str(args.full_helper.resolve(strict=True)),
                         NOTLIN_KOTLIN_SOURCES=str(source), NOTLIN_CLASSPATH=classpath)
        expected_files = (len(list(source.rglob('*.kt'))), len(list(source.rglob('*.java'))))
        result = run(name, [args.notlin, '--jvm-facts=' + ('on' if helper else 'off'),
                           '--annotations', 'none', '--in-place', '--coverage-report',
                           work / (name + '.json'), '.'], cwd=source, extra_env=extra)
        indexed = re.search(r'index\s+(\d+) declarations? \((\d+) Kotlin files?\) · (\d+) Java files?',
                            result.stderr)
        assert indexed is not None, 'Require explicit indexed scope, not only selected-input totals'
        assert tuple(map(int, indexed.groups()[1:])) == expected_files, 'Contaminated indexing scope'
        steps[name]['indexed_files'] = dict(kotlin=expected_files[0], java=expected_files[1])
        report = json.loads((work / (name + '.json')).read_text())
        steps[name].update({k: report[k] for k in ['original', 'translated', 'retained']})
        steps[name]['reasons'] = [row['reasons'] for row in report['declarations']]

    fixture = root / 'generic_owner_fixed_return'
    source = work / 'generic-input'
    source.mkdir()
    for path in fixture.glob('*.kt'):
        shutil.copy2(path, source / path.name)
    cli('generic-cli', source)
    for mode in ['disable', 'enable', 'no-compatibility']:
        original = work / ('original-' + mode)
        original.mkdir()
        run('kotlinc-' + mode, [args.kotlinc, *sorted(fixture.glob('*.kt')),
                               '-jvm-default=' + mode, '-d', original])
        cp = f'{original}:{args.stdlib}'
        run('driver-compile-' + mode, [javac, '-cp', cp, '-d', original, fixture / 'Driver.java'])
        result = run('original-' + mode, [java, '-cp', cp, 'probe.Driver'])
        assert result.stdout.splitlines()[:4] == ['true', 'true', 'true', 'NPE']
        run('original-api-' + mode, [javap, '-p', '-s', '-v', '-classpath', cp, 'probe.Subject'])
        if not (source / 'Subject.java').is_file():
            steps['candidate-' + mode] = {'retained': True}
            continue
        candidate = work / ('candidate-' + mode)
        candidate.mkdir()
        generated = [source / 'Subject.java']
        if (source / 'Wide.java').is_file():
            generated.append(source / 'Wide.java')
        result = run('candidate-compile-' + mode,
                     [javac, '-cp', cp, '-d', candidate, *generated], required=False)
        if result.returncode == 0:
            assert (candidate / 'probe/Subject.class').is_file(), 'Do not fall back to original Subject'
            result = run('candidate-' + mode, [java, '-cp', f'{candidate}:{cp}', 'probe.Driver'],
                         required=False)
            steps['candidate-' + mode]['stdout_equals_original'] = (
                result.stdout == (work / ('original-' + mode + '.stdout')).read_text())

    fixture = root / 'secondary_constructor'
    java_out = work / 'elements'
    original = work / 'bag-original'
    driver = work / 'bag-driver'
    for out in [java_out, original, driver]:
        out.mkdir()
    java_sources = [fixture / 'src/probe/Element.java', fixture / 'src/probe/Marker.java']
    run('elements', [javac, '-d', java_out, *java_sources])
    run('bag-original-compile', [args.kotlinc, fixture / 'src/Bag.kt', '-cp', java_out, '-d', original])
    cp = f'{java_out}:{original}:{args.stdlib}'
    run('bag-driver-compile', [javac, '-cp', cp, '-d', driver, fixture / 'src/probe/JavaDriver.java'])
    result = run('bag-original', [java, '-cp', f'{driver}:{cp}', 'probe.JavaDriver'])
    assert 'ALL_CHECKS_PASSED' in result.stdout
    steps['bag-original']['checks_passed'] = sum(line.endswith(':true') for line in result.stdout.splitlines())
    run('bag-original-api', [javap, '-p', '-s', '-v', '-classpath', cp, 'probe.Bag'])
    manual = work / 'bag-manual'
    manual.mkdir()
    run('manual-compile', [javac, '-cp', str(args.stdlib), '-d', manual, *java_sources,
                           fixture / 'manual/Bag.java', fixture / 'src/probe/JavaDriver.java'])
    result = run('manual-behavior', [java, '-cp', f'{manual}:{args.stdlib}', 'probe.JavaDriver'])
    assert 'ALL_CHECKS_PASSED' in result.stdout
    run('manual-api', [javap, '-p', '-s', '-v', '-classpath', manual, 'probe.Bag'])
    caller = work / 'caller'
    caller.mkdir()
    run('caller-compile-original', [javac, '-cp', cp, '-d', caller, fixture / 'caller/MappedCaller.java'])
    result = run('caller-original', [java, '-cp', f'{caller}:{cp}', 'probe.MappedCaller'])
    assert result.stdout.strip() == '1'
    run('caller-manual', [java, '-cp', f'{caller}:{manual}:{args.stdlib}', 'probe.MappedCaller'], required=False)
    for label, helper in [('constructor-off', False), ('constructor-full', True)]:
        if helper and args.full_helper is None:
            continue
        source = work / label
        shutil.copytree(fixture / 'src', source)
        cli(label, source, helper=helper, classpath=f'{java_out}:{args.stdlib}')
    nobody_original = work / 'nobody-original'
    nobody_original.mkdir()
    run('nobody-original-compile', [args.kotlinc, fixture / 'nobody/Bag.kt', '-cp', java_out, '-d', nobody_original])
    source = work / 'constructor-nobody'
    source.mkdir()
    shutil.copy2(fixture / 'nobody/Bag.kt', source / 'Bag.kt')
    for item in java_sources:
        shutil.copy2(item, source / item.name)
    cli('constructor-nobody', source)
    if (source / 'Bag.java').is_file():
        out = work / 'nobody-emitted'
        out.mkdir()
        run('nobody-emitted-compile', [javac, '-cp', java_out, '-d', out, source / 'Bag.java'], required=False)
    (work / 'summary.json').write_text(json.dumps(steps, indent=2) + '\n')
    print(json.dumps(steps, indent=2))
    print('Characterization complete. Candidate failures/differences above are NOT passing correctness gates.')


if __name__ == '__main__':
    main()
