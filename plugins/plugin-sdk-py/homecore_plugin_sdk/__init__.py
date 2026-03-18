"""homecore_plugin_sdk — Python SDK for HomeCore device plugins.

Provides :class:`PluginBase`, a base class that handles broker connection,
device registration, and state publishing.  Subclass it to implement a plugin:

.. code-block:: python

    from homecore_plugin_sdk import PluginBase

    class MyLightPlugin(PluginBase):
        PLUGIN_ID = "plugin.my_light"

        def on_command(self, device_id: str, payload: dict) -> None:
            print(f"Command for {device_id}: {payload}")
            self.publish_state(device_id, {"on": payload.get("on", False)})

    if __name__ == "__main__":
        MyLightPlugin().run()
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
        """Publish a device state update to the broker.

        :param device_id: The canonical HomeCore device identifier.
        :param state: Dict of attribute names → values.
        """
        topic = f"homecore/devices/{device_id}/state"
        payload = json.dumps(state)
        logger.debug("publish_state topic=%s payload=%s", topic, payload)
        if self._client is not None:
            self._client.publish(topic, payload, qos=1, retain=True)

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
        logger.debug("register_device topic=%s", topic)
        if self._client is not None:
            self._client.publish(topic, payload, qos=1)

    def publish_availability(self, device_id: str, available: bool) -> None:
        """Publish an availability heartbeat."""
        topic = f"homecore/devices/{device_id}/availability"
        payload = "online" if available else "offline"
        if self._client is not None:
            self._client.publish(topic, payload, qos=1, retain=True)

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
                # Subscribe to commands for all of this plugin's devices
                client.subscribe(f"homecore/devices/+/cmd", qos=1)
                self.on_connect()
            else:
                logger.error("Broker connection refused: reason_code=%s", reason_code)

        def _on_message(c, userdata, msg):
            # Route homecore/devices/{device_id}/cmd → on_command
            parts = msg.topic.split("/")
            if len(parts) == 4 and parts[0] == "homecore" and parts[1] == "devices" and parts[3] == "cmd":
                device_id = parts[2]
                try:
                    payload = json.loads(msg.payload)
                except json.JSONDecodeError:
                    payload = {"raw": msg.payload.decode(errors="replace")}
                self.on_command(device_id, payload)

        client.on_connect = _on_connect
        client.on_message = _on_message

        client.connect(self.broker_host, self.broker_port, keepalive=60)
        logger.info("Plugin %s entering event loop", self.PLUGIN_ID)
        client.loop_forever()
