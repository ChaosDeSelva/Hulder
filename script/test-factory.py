import json
from pathlib import Path
import subprocess
import tempfile
import unittest

FACTORY = Path(__file__).with_name('factory').resolve()


class FactoryTasks(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        self.config = self.root / 'config.json'
        for command in ('open', 'curl'):
            script = self.root / command
            script.write_text('#!/bin/bash\nprintf "%s\\n" "${!#}" >> "$CALLS"\n')
            script.chmod(0o755)
        self.calls = self.root / 'calls'
        self.env = {
            'HOME': str(self.root), 'VIKING_HOME': str(self.root), 'CALLS': str(self.calls),
            'PATH': f'{self.root}:/usr/bin:/bin',
        }

    def task(self, name, **env):
        return subprocess.run([str(FACTORY), name], env={**self.env, **env},
                              capture_output=True, text=True)

    def test_tasks_use_the_managed_ports(self):
        self.config.write_text(json.dumps({'mimir_port': 47000, 'sindri_port': 44310, 'heimdall_port': 44320}))
        self.assertEqual(self.task('fleet').returncode, 0)
        self.assertEqual(self.task('status').returncode, 0)
        self.assertEqual(self.calls.read_text().splitlines(), [
            'http://127.0.0.1:44320/?view=fleet', 'http://127.0.0.1:47000/v1/health',
            'http://127.0.0.1:44310/v1/health', 'http://127.0.0.1:44320/',
        ])

    def test_source_defaults_and_overrides(self):
        self.config.write_text('{"heimdall_port":44320}')
        self.assertEqual(self.task('open', FACTORY_LAUNCHER='source/factory.mjs').returncode, 0)
        self.assertEqual(self.task('open', HEIMDALL_PORT='4321').returncode, 0)
        self.assertEqual(self.calls.read_text().splitlines(), ['http://127.0.0.1:4320/', 'http://127.0.0.1:4321/'])

    def test_invalid_port_never_opens_a_different_dashboard(self):
        for value in ('0', '65536', '123/foo', 'localhost'):
            self.assertNotEqual(self.task('open', HEIMDALL_PORT=value).returncode, 0)
        self.assertFalse(self.calls.exists())


if __name__ == '__main__':
    unittest.main()
