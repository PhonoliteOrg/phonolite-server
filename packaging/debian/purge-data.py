"""Embedded in Debian postrm; clean package state without traversing music."""

import errno
import os
from pathlib import Path
import re
import stat
import sys


DIRECTORY_FLAGS = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW


class RetainState(Exception):
    """A protection check could not establish that deleting state is safe."""


def within(path, parent):
    return path == parent or parent in path.parents


def mount_paths():
    # st_dev alone cannot detect bind mounts on the same filesystem.
    try:
        lines = Path("/proc/self/mountinfo").read_text().splitlines()
    except OSError as error:
        raise RetainState("mount information is unavailable") from error
    return {
        Path(re.sub(r"\\([0-7]{3})", lambda match: chr(int(match[1], 8)), line.split()[4]))
        for line in lines
    }


def music_paths(directory, root):
    try:
        descriptor = os.open("config.yaml", os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=directory)
    except OSError as error:
        raise RetainState("config.yaml is missing, unreadable or a symbolic link") from error
    with os.fdopen(descriptor, "rb") as source:
        if not stat.S_ISREG(os.fstat(source.fileno()).st_mode):
            raise RetainState("config.yaml is not a regular file")
        content = source.read(1024 * 1024 + 1)
    if len(content) > 1024 * 1024:
        raise RetainState("config.yaml is too large to inspect safely")
    try:
        import yaml
    except ImportError as error:
        raise RetainState("install python3-yaml to inspect configured music folders") from error
    try:
        config = yaml.safe_load(content)
    except (yaml.YAMLError, ValueError) as error:
        raise RetainState("config.yaml could not be parsed") from error
    if config is None:
        config = {}
    if not isinstance(config, dict):
        raise RetainState("config.yaml is not a configuration mapping")
    legacy = config.get("music_root", "")
    roots = config.get("music_roots", [])
    if not isinstance(legacy, str) or not isinstance(roots, list):
        raise RetainState("configured music folders have an invalid format")
    paths = [legacy]
    for item in roots:
        if not isinstance(item, dict) or not isinstance(item.get("path"), str):
            raise RetainState("configured music folders have an invalid format")
        # Disabled roots are still user data.
        paths.append(item["path"])
    protected = set()
    for value in paths:
        value = value.strip()
        if not value:
            continue
        path = Path(os.path.abspath(root / value))
        try:
            resolved = path.resolve()
        except (OSError, RuntimeError, ValueError) as error:
            raise RetainState("a configured music folder cannot be resolved") from error
        protected.update((path, resolved))
    return protected


def protected_path(path, protected):
    return any(within(path, folder) for folder in protected)


def remove_directory_contents(directory, path, device, protected, mounts, keep_config=False):
    # Descriptor-relative operations and O_NOFOLLOW prevent a symlink from
    # redirecting recursive deletion outside the opened state tree.
    for name in os.listdir(directory):
        child = path / name
        if (keep_config and name == "config.yaml") or protected_path(child, protected):
            continue
        try:
            info = os.stat(name, dir_fd=directory, follow_symlinks=False)
        except FileNotFoundError:
            continue
        if stat.S_ISDIR(info.st_mode):
            if child in mounts or info.st_dev != device:
                print(f"Phonolite purge retained mounted directory: {child}", file=sys.stderr)
                continue
            descriptor = os.open(name, DIRECTORY_FLAGS, dir_fd=directory)
            try:
                opened = os.fstat(descriptor)
                if (opened.st_dev, opened.st_ino) != (info.st_dev, info.st_ino):
                    raise OSError("Directory changed during Phonolite purge")
                remove_directory_contents(descriptor, child, device, protected, mounts)
            finally:
                os.close(descriptor)
            try:
                os.rmdir(name, dir_fd=directory)
            except OSError as error:
                if error.errno not in (errno.ENOTEMPTY, errno.EEXIST):
                    raise
        else:
            # Removing a link never removes its target, including linked data
            # and music stored outside /var/lib/phonolite.
            os.unlink(name, dir_fd=directory)


def purge_state(state_path):
    root = Path(os.path.abspath(state_path))
    if root in (Path("/"), Path("/var"), Path("/var/lib")):
        raise ValueError("Refusing an invalid Phonolite state directory")
    try:
        info = root.lstat()
    except FileNotFoundError:
        return
    if not stat.S_ISDIR(info.st_mode):
        raise RetainState("the state directory is not a regular directory")
    # Resolve only the parent after checking the final component for links.
    root = root.parent.resolve() / root.name
    mounts = mount_paths()
    if root in mounts:
        raise RetainState("the state directory is a mount point")
    parent = os.open(root.parent, DIRECTORY_FLAGS)
    try:
        directory = os.open(root.name, DIRECTORY_FLAGS, dir_fd=parent)
        try:
            opened = os.fstat(directory)
            if (opened.st_dev, opened.st_ino) != (info.st_dev, info.st_ino):
                raise OSError("State directory changed during Phonolite purge")
            if os.listdir(directory):
                protected = music_paths(directory, root)
                if protected_path(root, protected):
                    raise RetainState("the state directory is inside a configured music folder")
                remove_directory_contents(directory, root, opened.st_dev, protected, mounts, keep_config=True)
                # Keep the configuration until all other deletions succeed so
                # a retry can still establish music-folder exclusions.
                if not protected_path(root / "config.yaml", protected):
                    os.unlink("config.yaml", dir_fd=directory)
        finally:
            os.close(directory)
        try:
            os.rmdir(root.name, dir_fd=parent)
        except OSError as error:
            if error.errno not in (errno.ENOTEMPTY, errno.EEXIST):
                raise
            print(f"Phonolite purge retained music folders or mounted data in {root}.", file=sys.stderr)
    finally:
        os.close(parent)


if __name__ == "__main__":
    try:
        purge_state(sys.argv[1])
    except RetainState as error:
        # postrm may run after dependencies have been removed. Retain data when
        # safe inspection is unavailable; never fall back to a blanket rm -rf.
        print(f"Phonolite data retained: {error}.", file=sys.stderr)
    except (OSError, ValueError) as error:
        print(f"Phonolite purge failed: {error}", file=sys.stderr)
        sys.exit(1)
