#!/usr/bin/env python3
"""Keep voice pkg-config probes inside the pinned SDK, with platform inputs separate."""

import os
import sys

from cargo_native_inputs import pkg_config_command

arguments, environment = pkg_config_command(sys.argv[1:], os.environ)
os.execvpe(arguments[0], arguments, environment)
