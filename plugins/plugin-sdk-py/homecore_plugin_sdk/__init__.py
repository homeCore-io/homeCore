"""homecore_plugin_sdk — Python SDK for HomeCore device plugins.

Provides :class:`PluginBase`, a base class that handles broker connection,
device registration, and state publishing.  Subclass it to implement a plugin:

.. code-block:: python

    from homecore_plugin_sdk import PluginBase

    class MyLightPlugin(PluginBase):
        PLUGIN_ID = "plugin.my_light"

        def on_connect(self):
            caps = {"on": {"type": "boolean"}, "brightness": {"type": "integer", "minimum": 0, "maximum": 255}}
            self.register_device("light.01", "My Light", caps)
            self.publish_availability("light.01", True)
            self.publish_plugin_status("active")

        def on_command(self, device_id: str, payload: dict) -> None:
            print(f"Command for {device_id}: {payload}")
            self.publish_state(device_id, {"on": payload.get("on", False)})

    if __name__ == "__main__":
        MyLightPlugin().run()

Configuration
-------------
Constructor parameters override environment variables, which override defaults:

+---------------------+----------------------+------------+
| Parameter           | Env var              | Default    |
+=====================+======================+============+
| ``broker_host``     | ``HC_BROKER_HOST``   | 127.0.0.1  |
+---------------------+----------------------+------------+
| ``broker_port``     | ``HC_BROKER_PORT``   | 1883       |
+---------------------+----------------------+------------+
| ``password``        | ``HC_PLUGIN_PASSWORD``| (empty)   |
+---------------------+----------------------+------------+
"""

from __future__ import annotations

import json
import logging
import os
from abc import ABC, abstractmethod
from typing import Any

logger = logging.getLogger(__name__)


class PluginBase(ABC):
    """Base class for HomeCore plugins written in Python.

    Subclasses must set :attr:`PLUGIN_ID` and implement :meth:`on_command`.
    Call :meth:`run` to connect and enter the event loop.
    """

    #: Unique plugin identifier — override in subclass.
    PLUGIN_ID: str = "plugin.unnamed"

    def __init__(
        self,
        broker_host: str | None = None,
        broker_port: int | None = None,
        password: str | None = None,
    ) -> None:
        self.broker_host = broker_host or os.getenv("HC_BROKER_HOST", "127.0.0.1")
        self.broker_port = broker_port or int(os.getenv("HC_BROKER_PORT", "1883"))
        self.password = password or os.getenv("HC_PLUGIN_PASSWORD", "")
        self._client: Any = None  # paho.mqtt.client.Client

    # ------------------------------------------------------------------
    # Public API
    # ------------------------------------------------------------------

    def publish_state(self, device_id: str, state: dict) -> None:
        """Publish a full device state update (retained, QoS 1).

        :param device_id: The canonical HomeCore device identifier.
        :param state: Dict of attribute names → values.
        """
        topic = f"homecore/devices/{device_id}/state"
        self._publish(topic, json.dumps(state), qos=1, retain=True)

    def publish_state_partial(self, device_id: str, patch: dict) -> None:
        """Publish a partial state update (JSON merge-patch, QoS 1, not retained).

        Use this for high-frequency sensors that send diffs rather than full state blobs.

        :param device_id: The canonical HomeCore device identifier.
        :param patch: Dict of attributes to merge into the current state.
        """
        topic = f"homecore/devices/{device_id}/state/partial"
        self._publish(topic, json.dumps(patch), qos=1, retain=False)

    def register_device(
        self,
        device_id: str,
        name: str,
        capabilities: dict,
        area: str | None = None,
    ) -> None:
        """Publish a device registration message.

        :param device_id: Stable unique identifier for the device.
        :param name: Human-readable label.
        :param capabilities: JSON Schema object describing device attributes.
        :param area: Optional room/zone assignment.
        """
        topic = f"homecore/plugins/{self.PLUGIN_ID}/register"
        payload = json.dumps(
            {
                "device_id": device_id,
                "plugin_id": self.PLUGIN_ID,
                "name": name,
                "area": area,
                "capabilities": capabilities,
            }
        )
        self._publish(topic, payload, qos=1)

    def register_device_typed(
        self,
        device_id: str,
        name: str,
        device_type: str,
        area: str | None = None,
    ) -> None:
        """Register a device by type name.

        Instead of supplying a full capability schema, provide a ``device_type``
        string that HomeCore resolves against its built-in device-type catalog.
        This is the recommended path for well-known device categories.

        Example device types: ``"light"``, ``"light_color"``, ``"switch"``,
        ``"temperature_sensor"``, ``"power_monitor"``, ``"cover"``, ``"lock"``,
        ``"climate"``, …

        :param device_id: Stable unique identifier for the device.
        :param name: Human-readable label.
        :param device_type: Type name from the device-type catalog.
        :param area: Optional room/zone assignment.
        """
        topic = f"homecore/plugins/{self.PLUGIN_ID}/register"
        payload = json.dumps(
            {
                "device_id": device_id,
                "plugin_id": self.PLUGIN_ID,
                "name": name,
                "area": area,
                "device_type": device_type,
            }
        )
        self._publish(topic, payload, qos=1)

    def publish_availability(self, device_id: str, available: bool) -> None:
        """Publish an availability heartbeat (retained, QoS 1).

        :param device_id: The target device.
        :param available: ``True`` for ``"online"``, ``False`` for ``"offline"``.
        """
        topic = f"homecore/devices/{device_id}/availability"
        self._publish(topic, "online" if available else "offline", qos=1, retain=True)

    def publish_plugin_status(self, status: str) -> None:
        """Publish plugin status to ``homecore/plugins/{plugin_id}/status`` (retained).

        :param status: One of ``"active"``, ``"degraded"``, ``"offline"``.
        """
        topic = f"homecore/plugins/{self.PLUGIN_ID}/status"
        self._publish(topic, status, qos=1, retain=True)

    # ------------------------------------------------------------------
    # Subclass hooks
    # ------------------------------------------------------------------

    @abstractmethod
    def on_command(self, device_id: str, payload: dict) -> None:
        """Called when a command message arrives for one of this plugin's devices.

        :param device_id: The target device.
        :param payload: Decoded JSON command payload.
        """

    def on_connect(self) -> None:
        """Called after the broker connection is established.  Override to
        register devices and perform startup subscriptions."""

    # ------------------------------------------------------------------
    # Lifecycle
    # ------------------------------------------------------------------

    def run(self) -> None:
        """Connect to the broker and block until interrupted."""
        try:
            import paho.mqtt.client as mqtt
        except ImportError as exc:
            raise ImportError("paho-mqtt is required: pip install paho-mqtt") from exc

        client = mqtt.Client(client_id=self.PLUGIN_ID, protocol=mqtt.MQTTv5)
        self._client = client

        if self.password:
            client.username_pw_set(self.PLUGIN_ID, self.password)

        def _on_connect(c, userdata, flags, reason_code, properties):
            if reason_code == 0:
                logger.info("Connected to broker at %s:%s", self.broker_host, self.broker_port)
                client.subscribe("homecore/devices/+/cmd", qos=1)
                self.on_connect()
            else:
                logger.error("Broker connection refused: reason_code=%s", reason_code)

        def _on_message(c, userdata, msg):
            self._on_message_handler(msg)

        def _on_disconnect(c, userdata, flags, reason_code, properties):
            if reason_code != 0:
                logger.warning("Disconnected from broker (reason_code=%s); will reconnect", reason_code)

        client.on_connect = _on_connect
        client.on_message = _on_message
        client.on_disconnect = _on_disconnect

        client.connect(self.broker_host, self.broker_port, keepalive=60)
        logger.info("Plugin %s entering event loop", self.PLUGIN_ID)
        client.loop_forever()

    # ------------------------------------------------------------------
    # Internal helpers
    # ------------------------------------------------------------------

    def _on_message_handler(self, msg: Any) -> None:
        """Route an incoming MQTT message.  Extracted for unit-testability."""
        parts = msg.topic.split("/")
        if (
            len(parts) == 4
            and parts[0] == "homecore"
            and parts[1] == "devices"
            and parts[3] == "cmd"
        ):
            device_id = parts[2]
            try:
                payload = json.loads(msg.payload)
            except (json.JSONDecodeError, ValueError):
                payload = {"raw": msg.payload.decode(errors="replace")}
            self.on_command(device_id, payload)

    def _publish(self, topic: str, payload: str, qos: int = 0, retain: bool = False) -> None:
        if self._client is None:
            logger.warning("publish called before run(): topic=%s", topic)
            return
        logger.debug("publish topic=%s retain=%s", topic, retain)
        self._client.publish(topic, payload, qos=qos, retain=retain)
