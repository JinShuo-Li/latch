import unittest

from config_layers import resolve_config


DEFAULTS = {
    "service": {"endpoint": "https://default.example", "timeout": 30, "retries": 2},
    "output": {"format": "text", "color": True},
}


class ConfigLayersTest(unittest.TestCase):
    def test_environment_overrides_file(self):
        actual = resolve_config(DEFAULTS, {"service": {"timeout": 10}},
                                {"APP_TIMEOUT": "5"}, {})
        self.assertEqual(actual["service"]["timeout"], 5)

    def test_project_keeps_unrelated_nested_defaults(self):
        actual = resolve_config(DEFAULTS, {"service": {"timeout": 10}}, {}, {})
        self.assertEqual(actual["service"]["retries"], 2)
        self.assertEqual(actual["service"]["endpoint"], "https://default.example")

    def test_explicit_empty_flag_wins(self):
        actual = resolve_config(DEFAULTS, {}, {"APP_ENDPOINT": "https://env.example"},
                                {"endpoint": ""})
        self.assertEqual(actual["service"]["endpoint"], "")


if __name__ == "__main__":
    unittest.main()
