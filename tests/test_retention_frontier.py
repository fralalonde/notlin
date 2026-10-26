import copy
import importlib.util
import unittest
from pathlib import Path
from typing import Any

path = Path(__file__).resolve().parents[1] / 'tools' / 'retention_frontier.py'
spec = importlib.util.spec_from_file_location('retention_frontier', path)
assert spec is not None and spec.loader is not None
mod = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mod)


def symbol(name: str) -> dict[str, Any]:
    return dict(module='', package='probe', file=name+'.kt', owner_path=[],
                kind='interface', name=name, receiver=None, parameters=[])


def row(name: str, message: str = '', translated: bool = False) -> dict[str, Any]:
    reasons = [] if not message else [dict(PreparationBlocker=dict(
        kind='CompatibilityRule', message=message))]
    return dict(symbol=symbol(name), kind='Class', translated=translated,
                reasons=reasons, dependencies=[])


class FrontierTests(unittest.TestCase):
    def fixture(self) -> dict[str, Any]:
        message = ('inherited covariant getter needs a concrete Java bridge, '
                   'but its default provider remains Kotlin: '
                   '`probe.Provider#getValue`, `probe.Provider#getOther`')
        return dict(original=3, translated=1, retained=2, declarations=[
            row('Provider', 'an independent root blocker'),
            row('Child', message), row('Control', translated=True)])

    def test_counts_consumers_not_method_mentions(self):
        result, private = mod.analyze(self.fixture())
        self.assertEqual(result['providers'][0]['direct_blocked'], 1)
        self.assertEqual(result['retained'], 2)
        self.assertEqual(len(private), 1)

    def test_ambiguous_owner_names_are_not_guessed(self):
        report = self.fixture()
        other = row('Provider', 'other root')
        other['symbol']['file'] = 'another/Provider.kt'
        report['declarations'].append(other)
        report['original'] += 1
        report['retained'] += 1
        result, _ = mod.analyze(report)
        self.assertEqual(result['providers'], [])
        self.assertEqual(result['unresolved_provider_mentions'], 1)

    def test_explicitly_ambiguous_symbol_is_not_a_root(self):
        report = self.fixture()
        report['declarations'][0]['symbol']['kind'] = 'ambiguous:interface'
        result, _ = mod.analyze(report)
        self.assertEqual(result['providers'], [])
        self.assertEqual(result['unresolved_provider_mentions'], 1)

    def test_public_result_contains_no_source_names(self):
        import json
        public, _ = mod.analyze(self.fixture())
        text = json.dumps(public)
        for secret in ['probe.Provider', 'Child.kt', 'getValue']:
            self.assertNotIn(secret, text)

    def test_duplicate_identity_and_wrong_totals_reject(self):
        for mutation in ['duplicate', 'totals']:
            report = copy.deepcopy(self.fixture())
            if mutation == 'duplicate':
                report['declarations'][1] = report['declarations'][0]
            else:
                report['retained'] = 999
            with self.assertRaises(AssertionError):
                mod.analyze(report)


if __name__ == '__main__':
    unittest.main()
