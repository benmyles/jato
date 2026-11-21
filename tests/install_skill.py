"""Exercise the global installer from a project outside the Jato checkout."""

import os
from pathlib import Path
import subprocess
from tempfile import TemporaryDirectory
import unittest


REPO = Path(__file__).resolve().parents[1]
INSTALLER = REPO / "scripts/install-skill"
SOURCE = REPO / "skills/jato/SKILL.md"


class GlobalSkillInstall(unittest.TestCase):
    def setUp(self):
        scratch = TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.project = Path(scratch.name) / "rust project"
        self.project.mkdir()
        self.home = Path(scratch.name) / "user home"
        self.env = {**os.environ, "HOME": str(self.home)}
        self.env.pop("CLAUDE_CONFIG_DIR", None)
        self.claude = self.home / ".claude/skills/jato/SKILL.md"
        self.codex = self.home / ".agents/skills/jato/SKILL.md"
        self.agy = self.home / ".gemini/antigravity-cli/skills/jato/SKILL.md"

    def install(self, *args, check=True):
        return subprocess.run([INSTALLER, *args], cwd=self.project, env=self.env,
                              capture_output=True, text=True, check=check)

    def test_all_clients_and_repeat_install(self):
        self.install()
        files = [self.claude, self.codex, self.agy]
        for path in files:
            self.assertEqual(path.read_bytes(), SOURCE.read_bytes())
            self.assertFalse(path.samefile(SOURCE))
        before = [p.stat() for p in files]
        self.install()
        for path, previous in zip(files, before):
            self.assertEqual(path.stat().st_ino, previous.st_ino)
            self.assertEqual(path.stat().st_mtime_ns, previous.st_mtime_ns)
        self.assertEqual(list(self.project.iterdir()), [])

    def test_conflicting_copy_and_explicit_update(self):
        self.agy.parent.mkdir(parents=True)
        self.agy.write_text("local instructions")
        notes = self.agy.with_name("notes.md")
        notes.write_text("personal notes")
        failed = self.install(check=False)
        self.assertNotEqual(failed.returncode, 0)
        self.assertIn("--force", failed.stderr)
        self.assertFalse(self.claude.exists())
        self.assertFalse(self.codex.exists())
        self.assertEqual(self.agy.read_text(), "local instructions")
        self.install("--force")
        for path in (self.claude, self.codex, self.agy):
            self.assertEqual(path.read_bytes(), SOURCE.read_bytes())
        self.assertEqual(notes.read_text(), "personal notes")

    def test_custom_claude_root_and_shared_skills_directory(self):
        profile = self.home / "claude profile"
        self.env["CLAUDE_CONFIG_DIR"] = str(profile)
        profile.mkdir(parents=True)
        root = self.codex.parent.parent
        root.mkdir(parents=True)
        (profile / "skills").symlink_to(root, target_is_directory=True)
        self.install()
        self.assertTrue((profile / "skills/jato/SKILL.md").samefile(self.codex))
        self.assertEqual(self.agy.read_bytes(), SOURCE.read_bytes())
        self.assertFalse(self.claude.exists())

    def test_force_preserves_linked_skill_files_and_directories(self):
        original = self.project / "SKILL.md"
        original.write_text("another skill")
        self.agy.parent.mkdir(parents=True)
        self.agy.symlink_to(original)
        self.assertNotEqual(self.install("--force", check=False).returncode, 0)
        self.assertTrue(self.agy.is_symlink())
        self.agy.unlink()
        self.agy.parent.rmdir()
        self.agy.parent.symlink_to(self.project, target_is_directory=True)
        self.assertNotEqual(self.install("--force", check=False).returncode, 0)
        self.assertEqual(original.read_text(), "another skill")
        self.assertFalse(self.claude.exists())


if __name__ == "__main__":
    unittest.main()
