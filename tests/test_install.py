"""Run the installer offline against compatible and incompatible downloads."""

import io
import os
from pathlib import Path
import shlex
import subprocess
import tarfile
import tempfile
import unittest


INSTALLER = Path(__file__).resolve().parents[1] / "install.sh"


class InstallerTests(unittest.TestCase):
    def run_installer(self, binary, previous=None):
        fixture = tempfile.TemporaryDirectory(prefix="undo-installer-")
        self.addCleanup(fixture.cleanup)
        root = Path(fixture.name)
        destination = root / "bin with spaces"
        destination.mkdir()
        installed = destination / "undo"
        if previous is not None:
            installed.write_bytes(previous)
            installed.chmod(0o755)

        archive = root / "release.tar.gz"
        with tarfile.open(archive, "w:gz") as tar:
            entry = tarfile.TarInfo("undo")
            entry.size = len(binary)
            entry.mode = 0o755
            tar.addfile(entry, io.BytesIO(binary))

        # Only redirect the hardcoded installation destination. All validation,
        # filesystem operations, cleanup, and shell error handling run unchanged.
        script = INSTALLER.read_text()
        self.assertEqual(script.count('INSTALL_DIR="/usr/local/bin"'), 1)
        script = script.replace(
            'INSTALL_DIR="/usr/local/bin"',
            f"INSTALL_DIR={shlex.quote(str(destination))}",
        )
        staged_installer = root / "install.sh"
        staged_installer.write_text(script)
        fake_tools = root / "tools"
        fake_tools.mkdir()
        curl = fake_tools / "curl"
        curl.write_text(
            "#!/bin/sh\n"
            'case "$2" in\n'
            '  */releases/latest) printf \'{"tag_name":"v0.2.11"}\\n\' ;;\n'
            f'  *.tar.gz) cp {shlex.quote(str(archive))} "$4" ;;\n'
            '  *) exit 99 ;;\n'
            "esac\n"
        )
        curl.chmod(0o755)
        environment = os.environ.copy()
        environment["PATH"] = str(fake_tools) + os.pathsep + environment["PATH"]
        result = subprocess.run(
            ["sh", str(staged_installer)],
            env=environment,
            capture_output=True,
            text=True,
            timeout=15,
        )
        return result, installed

    def test_incompatible_download_fails_without_installing(self):
        result, installed = self.run_installer(
            b"#!/bin/sh\necho 'GLIBC_2.39 not found' >&2\nexit 69\n"
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(installed.exists())
        self.assertNotIn("Installed undo", result.stdout)
        self.assertIn("GLIBC_2.39 not found", result.stderr)

    def test_incompatible_update_preserves_working_installation(self):
        previous = b"#!/bin/sh\necho 'undo previous'\n"
        result, installed = self.run_installer(
            b"#!/bin/sh\nexit 69\n", previous=previous
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(installed.read_bytes(), previous)
        self.assertNotIn("Installed undo", result.stdout)

    def test_compatible_download_installs_and_reports_version(self):
        binary = b"#!/bin/sh\necho 'undo 0.2.11'\n"
        result, installed = self.run_installer(binary)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(installed.read_bytes(), binary)
        self.assertTrue(os.access(installed, os.X_OK))
        self.assertIn("Installed undo v0.2.11", result.stdout)
        self.assertIn("undo 0.2.11", result.stdout)


if __name__ == "__main__":
    unittest.main()
