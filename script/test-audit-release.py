import importlib.util
from pathlib import Path
import unittest

specification = importlib.util.spec_from_file_location('audit', Path(__file__).with_name('audit-release.py'))
audit = importlib.util.module_from_spec(specification)
specification.loader.exec_module(audit)

class ReleaseAuditTests(unittest.TestCase):
    def test_build_dependencies_are_checked_and_dev_only_edges_are_excluded(self):
        metadata = {
            'packages': [{'id': name, 'name': name, 'source': None if name == 'zed' else 'registry+test'}
                         for name in ['zed', 'runtime', 'build', 'dev']],
            'resolve': {'nodes': [
                {'id': 'zed', 'deps': [
                    {'pkg': 'runtime', 'dep_kinds': [{'kind': None}]},
                    {'pkg': 'build', 'dep_kinds': [{'kind': 'build'}]},
                    {'pkg': 'dev', 'dep_kinds': [{'kind': 'dev'}]},
                ]},
                *[{'id': name, 'deps': []} for name in ['runtime', 'build', 'dev']],
            ]},
        }
        self.assertEqual({package['id'] for package in audit.dependency_closure(metadata, 'zed')},
                         {'zed', 'runtime', 'build'})

    def test_release_graph_excludes_inactive_workspace_features(self):
        metadata = {'packages': [
            {'name': 'runtime', 'version': '1.0.0'},
            {'name': 'inactive', 'version': '2.0.0'},
        ]}
        self.assertEqual(audit.release_packages(metadata, 'runtime v1.0.0\n'),
                         [metadata['packages'][0]])
        with self.assertRaises(ValueError):
            audit.release_packages(metadata, 'unknown v1.0.0\n')
        with self.assertRaises(ValueError):
            audit.release_packages(metadata, '')

    def test_a_reclassified_notice_becomes_a_blocker(self):
        advisory = {'id': 'RUSTSEC-2024-0370',
                    'affected': [{'database_specific': {'informational': 'unmaintained'}}]}
        self.assertTrue(audit.maintenance_notice(advisory))
        advisory['affected'][0]['database_specific']['informational'] = 'unsound'
        self.assertFalse(audit.maintenance_notice(advisory))
        advisory['affected'] = []
        self.assertFalse(audit.maintenance_notice(advisory))

    def test_unknown_maintenance_notices_require_review(self):
        self.assertFalse(audit.maintenance_notice({'id': 'UNREVIEWED',
                         'affected': [{'database_specific': {'informational': 'unmaintained'}}]}))

if __name__ == '__main__':
    unittest.main()
