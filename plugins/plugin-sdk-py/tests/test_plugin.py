"""Tests for homecore_plugin_sdk.PluginBase.

All tests use unittest.mock to avoid a real MQTT broker.
"""
import json
import os
import unittest
from unittest.mock import MagicMock, call, patch

from homecore_plugin_sdk import PluginBase


class ConcretePlugin(PluginBase):
    """Minimal concrete subclass for testing."""

    PLUGIN_ID = "plugin.test"

    def __init__(self, **kwargs):
        super().__init__(**kwargs)
        self.commands: list = []
        self.connect_called = False

    def on_connect(self):
        self.connect_called = True

    def on_command(self, device_id: str, payload: dict) -> None:
        self.commands.append((device_id, payload))


def _make_plugin(**kwargs) -> ConcretePlugin:
    return ConcretePlugin(broker_host="127.0.0.1", broker_port=1883, **kwargs)


def _attach_mock_client(plugin: ConcretePlugin) -> MagicMock:
    mock = MagicMock()
    plugin._client = mock
    return mock


def _make_msg(topic: str, payload: bytes) -> MagicMock:
    msg = MagicMock()
    msg.topic = topic
    msg.payload = payload
    return msg


class TestPublishMethods(unittest.TestCase):
    def test_publish_state(self):
        plugin = _make_plugin()
        mc = _attach_mock_client(plugin)
        plugin.publish_state("light.01", {"on": True, "brightness": 200})
        mc.publish.assert_called_once_with(
            "homecore/devices/light.01/state",
            json.dumps({"on": True, "brightness": 200}),
            qos=1,
            retain=True,
        )

    def test_publish_state_attaches_change_metadata(self):
        plugin = _make_plugin()
        mc = _attach_mock_client(plugin)
        plugin.publish_state(
            "light.01",
            {"on": True},
            change={"kind": "external", "source": "wall_switch"},
        )
        mc.publish.assert_called_once_with(
            "homecore/devices/light.01/state",
            json.dumps(
                {
                    "on": True,
                    "_hc": {"change": {"kind": "external", "source": "wall_switch"}},
                }
            ),
            qos=1,
            retain=True,
        )

    def test_publish_state_partial(self):
        plugin = _make_plugin()
        mc = _attach_mock_client(plugin)
        plugin.publish_state_partial("light.01", {"brightness": 128})
        mc.publish.assert_called_once_with(
            "homecore/devices/light.01/state/partial",
            json.dumps({"brightness": 128}),
            qos=1,
            retain=False,
        )

    def test_publish_state_for_command_preserves_command_metadata(self):
        plugin = _make_plugin()
        mc = _attach_mock_client(plugin)
        plugin.publish_state_for_command(
            "light.01",
            {"on": True},
            {
                "on": True,
                "_hc": {
                    "command": {
                        "changed_at": "2026-04-01T12:00:00Z",
                        "kind": "homecore",
                        "source": "api",
                        "correlation_id": "corr-1",
                    }
                },
            },
        )
        mc.publish.assert_called_once_with(
            "homecore/devices/light.01/state",
            json.dumps(
                {
                    "on": True,
                    "_hc": {
                        "change": {
                            "changed_at": "2026-04-01T12:00:00Z",
                            "kind": "homecore",
                            "source": "api",
                            "correlation_id": "corr-1",
                        }
                    },
                }
            ),
            qos=1,
            retain=True,
        )

    def test_register_device(self):
        plugin = _make_plugin()
        mc = _attach_mock_client(plugin)
        caps = {"on": {"type": "boolean"}}
        plugin.register_device("light.01", "Test Light", caps, area="living_room")

        mc.publish.assert_called_once()
        topic, payload_str = mc.publish.call_args[0][:2]
        payload = json.loads(payload_str)

        self.assertEqual(topic, "homecore/plugins/plugin.test/register")
        self.assertEqual(payload["device_id"], "light.01")
        self.assertEqual(payload["plugin_id"], "plugin.test")
        self.assertEqual(payload["name"], "Test Light")
        self.assertEqual(payload["area"], "living_room")
        self.assertEqual(payload["capabilities"], caps)

    def test_register_device_no_area(self):
        plugin = _make_plugin()
        mc = _attach_mock_client(plugin)
        plugin.register_device("light.01", "Test Light", {})
        _, payload_str = mc.publish.call_args[0][:2]
        self.assertIsNone(json.loads(payload_str)["area"])

    def test_register_device_typed(self):
        plugin = _make_plugin()
        mc = _attach_mock_client(plugin)
        plugin.register_device_typed("light.01", "Test Light", "light", area="living_room")

        mc.publish.assert_called_once()
        topic, payload_str = mc.publish.call_args[0][:2]
        payload = json.loads(payload_str)

        self.assertEqual(topic, "homecore/plugins/plugin.test/register")
        self.assertEqual(payload["device_id"], "light.01")
        self.assertEqual(payload["plugin_id"], "plugin.test")
        self.assertEqual(payload["name"], "Test Light")
        self.assertEqual(payload["device_type"], "light")
        self.assertEqual(payload["area"], "living_room")
        self.assertNotIn("capabilities", payload)

    def test_register_device_typed_no_area(self):
        plugin = _make_plugin()
        mc = _attach_mock_client(plugin)
        plugin.register_device_typed("sensor.01", "Temp Sensor", "temperature_sensor")
        _, payload_str = mc.publish.call_args[0][:2]
        payload = json.loads(payload_str)
        self.assertIsNone(payload["area"])
        self.assertEqual(payload["device_type"], "temperature_sensor")

    def test_unregister_device(self):
        plugin = _make_plugin()
        mc = _attach_mock_client(plugin)
        plugin.unregister_device("sensor.01")

        expected = [
            call(
                "homecore/devices/sensor.01/state",
                "",
                qos=1,
                retain=True,
            ),
            call(
                "homecore/devices/sensor.01/availability",
                "",
                qos=1,
                retain=True,
            ),
            call(
                "homecore/devices/sensor.01/schema",
                "",
                qos=1,
                retain=True,
            ),
            call(
                "homecore/plugins/plugin.test/unregister",
                json.dumps({"device_id": "sensor.01"}),
                qos=1,
                retain=False,
            ),
        ]
        self.assertEqual(mc.publish.call_args_list, expected)

    def test_publish_availability_online(self):
        plugin = _make_plugin()
        mc = _attach_mock_client(plugin)
        plugin.publish_availability("light.01", True)
        mc.publish.assert_called_once_with(
            "homecore/devices/light.01/availability",
            "online",
            qos=1,
            retain=True,
        )

    def test_publish_availability_offline(self):
        plugin = _make_plugin()
        mc = _attach_mock_client(plugin)
        plugin.publish_availability("light.01", False)
        mc.publish.assert_called_once_with(
            "homecore/devices/light.01/availability",
            "offline",
            qos=1,
            retain=True,
        )

    def test_publish_plugin_status(self):
        plugin = _make_plugin()
        mc = _attach_mock_client(plugin)
        plugin.publish_plugin_status("active")
        mc.publish.assert_called_once_with(
            "homecore/plugins/plugin.test/status",
            "active",
            qos=1,
            retain=True,
        )

    def test_publish_before_connect_logs_warning(self):
        plugin = _make_plugin()
        # _client is None — should warn, not raise
        with self.assertLogs("homecore_plugin_sdk", level="WARNING") as cm:
            plugin.publish_state("light.01", {"on": True})
        self.assertTrue(any("before run()" in line for line in cm.output))


class TestCommandRouting(unittest.TestCase):
    def test_on_command_routing(self):
        plugin = _make_plugin()
        msg = _make_msg("homecore/devices/light.01/cmd", json.dumps({"on": True}).encode())
        plugin._on_message_handler(msg)
        self.assertEqual(plugin.commands, [("light.01", {"on": True})])

    def test_invalid_json_payload(self):
        plugin = _make_plugin()
        msg = _make_msg("homecore/devices/light.01/cmd", b"not-json")
        plugin._on_message_handler(msg)
        self.assertEqual(len(plugin.commands), 1)
        device_id, payload = plugin.commands[0]
        self.assertEqual(device_id, "light.01")
        self.assertIn("raw", payload)

    def test_non_cmd_topic_ignored(self):
        plugin = _make_plugin()
        msg = _make_msg("homecore/devices/light.01/state", json.dumps({"on": True}).encode())
        plugin._on_message_handler(msg)
        self.assertEqual(plugin.commands, [])

    def test_wrong_prefix_ignored(self):
        plugin = _make_plugin()
        msg = _make_msg("other/devices/light.01/cmd", b"{}")
        plugin._on_message_handler(msg)
        self.assertEqual(plugin.commands, [])


class TestConfig(unittest.TestCase):
    def test_explicit_params(self):
        plugin = _make_plugin(password="secret")
        self.assertEqual(plugin.broker_host, "127.0.0.1")
        self.assertEqual(plugin.broker_port, 1883)
        self.assertEqual(plugin.password, "secret")

    def test_env_var_config(self):
        os.environ["HC_BROKER_HOST"] = "192.168.1.5"
        os.environ["HC_BROKER_PORT"] = "1884"
        os.environ["HC_PLUGIN_PASSWORD"] = "envpass"
        try:
            plugin = ConcretePlugin()
            self.assertEqual(plugin.broker_host, "192.168.1.5")
            self.assertEqual(plugin.broker_port, 1884)
            self.assertEqual(plugin.password, "envpass")
        finally:
            del os.environ["HC_BROKER_HOST"]
            del os.environ["HC_BROKER_PORT"]
            del os.environ["HC_PLUGIN_PASSWORD"]

    def test_explicit_params_override_env(self):
        os.environ["HC_BROKER_HOST"] = "10.0.0.1"
        try:
            plugin = ConcretePlugin(broker_host="192.168.0.1", broker_port=1883)
            self.assertEqual(plugin.broker_host, "192.168.0.1")
        finally:
            del os.environ["HC_BROKER_HOST"]


class TestRunLifecycle(unittest.TestCase):
    """Tests for run()-adjacent logic that don't require paho to be installed."""

    def test_run_raises_if_paho_not_installed(self):
        """run() raises a helpful ImportError when paho-mqtt is absent."""
        plugin = _make_plugin()
        # Ensure paho.mqtt.client is absent from sys.modules so the import fails.
        with patch.dict("sys.modules", {"paho": None, "paho.mqtt": None, "paho.mqtt.client": None}):
            with self.assertRaises(ImportError) as ctx:
                plugin.run()
        self.assertIn("paho-mqtt", str(ctx.exception))

    def test_password_stored_on_instance(self):
        plugin = _make_plugin(password="mysecret")
        self.assertEqual(plugin.password, "mysecret")

    def test_no_password_stored_as_empty_string(self):
        plugin = _make_plugin(password="")
        self.assertEqual(plugin.password, "")

    def test_client_is_none_before_run(self):
        plugin = _make_plugin()
        self.assertIsNone(plugin._client)


if __name__ == "__main__":
    unittest.main()
