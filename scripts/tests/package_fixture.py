"""Compile native packaging input without executing the resulting program."""

from pathlib import Path
import shutil
import subprocess


def compile_fixture(destination):
    compiler = shutil.which("cc") or shutil.which("gcc")
    if compiler is None:
        raise RuntimeError("Packaging fixture checks require a native C compiler (cc or gcc).")
    destination = Path(destination)
    destination.parent.mkdir(parents=True, exist_ok=True)
    # Some minimal distros implement /bin/true as a multicall wrapper script.
    # Compile our own ELF so native dependency discovery tests are independent
    # of that distro-specific layout. Never run the resulting fixture.
    subprocess.run([compiler, "-x", "c", "-o", str(destination), "-"],
                   input="int main(void) { return 0; }\n", check=True, text=True)
    return destination
