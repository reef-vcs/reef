"""Process settings applied before grpc is imported.

grpc's fork support logs two lines at INFO from every ``subprocess`` call made while a channel is
open, which an agent makes all the time, and a child process cannot use the parent's channels
anyway (create clients after forking). So the SDK turns it off unless the program chose:
``GRPC_ENABLE_FORK_SUPPORT`` set beforehand wins, and grpc imported before reef keeps its own.
grpc reads it when it starts, so it stays set, and the program's child processes inherit it.
"""

import os

os.environ.setdefault("GRPC_ENABLE_FORK_SUPPORT", "false")
