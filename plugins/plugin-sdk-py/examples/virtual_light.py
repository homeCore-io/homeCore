#!/usr/bin/env python3
"""virtual_light.py — A software-only dimmable light plugin for HomeCore.

Simulates a dimmable light bulb.  On startup it registers with the broker,
publishes its initial state, then toggles on/off and steps brightness every
5 seconds.  It also responds to commands received via MQTT.

Usage::

    pip install homecore-plugin-sdk
    python virtual_light.py
    python virtual_light.py --broker 192.168.1.10 --port 1883 --id plugin.mylight
    HC_BROKER_HOST=192.168.1.10 python virtual_light.py
"""

import argparse
import logging
import threading
import time

from homecore_plugin_sdk import PluginBase

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s %(levelname)-8s %(name)s — %(message)s",
)
logger = logging.getLogger("virtual_light")

DEVICE_ID = "light.virtual_py_01"
CAPABILITIES = {
    "on":         {"type": "boolean"},
    "brightness": {"type": "integer", "minimum": 0, "maximum": 255},
}


class VirtualLightPlugin(PluginBase):
    """Simulated dimmable light bulb plugin."""

    PLUGIN_ID = "plugin.virtual_py"

    def __init__(self, **kwargs):
        super().__init__(**kwargs)
        self._state = {"on": False, "brightness": 128}
        self._lock = threading.Lock()

    # ------------------------------------------------------------------
    # PluginBase hooks
    # ------------------------------------------------------------------

    def on_connect(self) -> None:
        self.register_device(DEVICE_ID, "Virtual Light (Python)", CAPABILITIES, area="living_room")
        self.publish_availability(DEVICE_ID, True)

        with self._lock:
            state = dict(self._state)
        self.publish_state(DEVICE_ID, state)
        self.publish_plugin_status("active")

        logger.info("Device registered and initial state published: %s", state)

        # Periodic toggle in a background daemon thread so run() can block.
        t = threading.Thread(target=self._tick_loop, daemon=True, name="virtual-light-tick")
        t.start()

    def on_command(self, device_id: str, payload: dict) -> None:
        logger.info("Command received for %s: %s", device_id, payload)
        with self._lock:
            self._state.update({k: v for k, v in payload.items() if k in CAPABILITIES})
            state = dict(self._state)
        self.publish_state(device_id, state)
        logger.info("State after command: %s", state)

    # ------------------------------------------------------------------
    # Internals
    # ------------------------------------------------------------------

    def _tick_loop(self) -> None:
        """Toggle on/off and step brightness every 5 seconds."""
        while True:
            time.sleep(5)
            with self._lock:
                self._state["on"] = not self._state["on"]
                self._state["brightness"] = (self._state["brightness"] + 16) % 256
                state = dict(self._state)
            logger.info("Periodic tick — publishing state: %s", state)
            self.publish_state(DEVICE_ID, state)


def main() -> None:
    parser = argparse.ArgumentParser(description="HomeCore virtual light plugin (Python)")
    parser.add_argument("--broker",   default=None, help="MQTT broker host (default: HC_BROKER_HOST or 127.0.0.1)")
    parser.add_argument("--port",     type=int, default=None, help="MQTT broker port (default: HC_BROKER_PORT or 1883)")
    parser.add_argument("--id",       default=None, help="Plugin ID override")
    parser.add_argument("--password", default=None, help="MQTT password (default: HC_PLUGIN_PASSWORD)")
    args = parser.parse_args()

    plugin = VirtualLightPlugin(
        broker_host=args.broker,
        broker_port=args.port,
        password=args.password,
    )
    if args.id:
        plugin.PLUGIN_ID = args.id

    logger.info("Virtual light plugin starting")
    logger.info("  Device ID : %s", DEVICE_ID)
    logger.info("  Broker    : %s:%s", plugin.broker_host, plugin.broker_port)
    logger.info("Press Ctrl-C to stop")

    try:
        plugin.run()
    except KeyboardInterrupt:
        logger.info("Shutting down")
        plugin.publish_availability(DEVICE_ID, False)
        plugin.publish_plugin_status("offline")


if __name__ == "__main__":
    main()
