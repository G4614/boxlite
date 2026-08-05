"""Unit coverage for the privileged advanced option.

Lives outside test_options.py because that module is marked `integration` and
so is excluded from the unit job (`pytest -m "not integration"`). These cases
need no runtime, so they belong where CI actually runs them.
"""

from __future__ import annotations

import boxlite


class TestPrivilegedOptions:
    def test_privileged_defaults_to_disabled(self):
        advanced = boxlite.AdvancedBoxOptions()

        assert advanced.privileged is False

    def test_privileged_survives_the_python_binding(self):
        advanced = boxlite.AdvancedBoxOptions(privileged=True)
        opts = boxlite.BoxOptions(image="docker:dind", advanced=advanced)

        assert opts.advanced.privileged is True

    def test_python_options_carry_the_request_verbatim(self):
        # The Python objects are carriers: the ["ALL"] expansion and the
        # conflict check both run in the Rust core when the options are
        # converted at create time, not here. Asserting the expansion at
        # construction would assert something the binding never does.
        advanced = boxlite.AdvancedBoxOptions(
            capabilities=boxlite.ContainerCapabilities(drop=["NET_RAW"]),
            privileged=True,
        )
        opts = boxlite.BoxOptions(image="docker:dind", advanced=advanced)

        assert opts.advanced.privileged is True
        assert opts.advanced.capabilities.add == []
        assert opts.advanced.capabilities.drop == ["NET_RAW"]
