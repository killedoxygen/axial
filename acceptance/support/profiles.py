"""Independent temporary profiles and canaries; never discover installed user data."""

from contextlib import contextmanager
from dataclasses import dataclass
import hashlib
import os
from pathlib import Path
import shutil
import stat
import tempfile


PROFILE_FIXTURE = Path(__file__).resolve().parents[1] / "fixtures" / "profiles" / "offline-vanilla"


def tree_fingerprint(root):
    """Fingerprint paths, types, permissions, content, symlink targets and file IDs.

    Does not follow symlinks. File IDs detect same-content replacement/hardlinking;
    atime/mtime are deliberately excluded because reads can change access metadata.
    """
    root = Path(root)
    result = {}

    def visit(path, relative):
        info = path.lstat()
        kind = stat.S_IFMT(info.st_mode)
        common = (kind, stat.S_IMODE(info.st_mode), info.st_dev, info.st_ino, info.st_nlink)
        if stat.S_ISLNK(info.st_mode):
            result[relative] = common + (os.readlink(path),)
        elif stat.S_ISREG(info.st_mode):
            result[relative] = common + (hashlib.sha256(path.read_bytes()).hexdigest(),)
        elif stat.S_ISDIR(info.st_mode):
            result[relative] = common
            for child in sorted(path.iterdir()):
                visit(child, str(Path(relative) / child.name))
        else:
            raise ValueError("canary trees only admit directories, regular files and symlinks")

    visit(root, ".")
    return result


def assert_tree_unchanged(root, before):
    try:
        after = tree_fingerprint(root)
    except (FileNotFoundError, NotADirectoryError) as error:
        raise AssertionError("protected fixture tree was removed or replaced") from error
    changed = sorted(key for key in before.keys() | after.keys() if before.get(key) != after.get(key))
    if changed:
        raise AssertionError("protected fixture tree changed: " + ", ".join(changed[:10]))


@dataclass(frozen=True)
class IsolatedProfiles:
    root: Path
    baseline: Path
    replacement: Path
    unrelated: Path

    def baseline_environment(self):
        return {"AXIAL_APP_ROOT_MODE": "portable", "AXIAL_APP_ROOT": str(self.baseline)}


@contextmanager
def isolated_profiles():
    """Yield separate old/new roots; replacement starts empty for its own schema.

    All payload copying uses new inodes. The unrelated sibling and source fixture
    are checked on exit even after a failing scenario. No keyring is configured.
    """
    source_before = tree_fingerprint(PROFILE_FIXTURE)
    with tempfile.TemporaryDirectory(prefix="axial-scenario-") as directory:
        root = Path(directory)
        baseline = root / "baseline"
        replacement = root / "replacement"
        unrelated = root / "unrelated"
        shutil.copytree(PROFILE_FIXTURE, baseline, symlinks=True)
        replacement.mkdir()
        unrelated.mkdir()
        (unrelated / "keep.txt").write_text("Synthetic unrelated sibling canary.\n", encoding="utf-8")
        (unrelated / "nested").mkdir()
        (unrelated / "nested" / "user-data.txt").write_text("Never mutate this synthetic user file.\n", encoding="utf-8")
        before = tree_fingerprint(unrelated)
        try:
            yield IsolatedProfiles(root, baseline, replacement, unrelated)
        finally:
            assert_tree_unchanged(unrelated, before)
            assert_tree_unchanged(PROFILE_FIXTURE, source_before)
