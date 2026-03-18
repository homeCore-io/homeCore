#!/usr/bin/env node
'use strict';

/**
 * virtual_light.js — A software-only dimmable light plugin for HomeCore.
 *
 * Simulates a dimmable light bulb.  On startup it registers with the broker,
 * publishes its initial state, then toggles on/off and steps brightness every
 * 5 seconds.  It also responds to commands received via MQTT.
 *
 * Usage:
 *   npm install
 *   node examples/virtual_light.js
 *   node examples/virtual_light.js --broker 192.168.1.10 --port 1883
 *   HC_BROKER_HOST=192.168.1.10 node examples/virtual_light.js
 */

const { PluginBase } = require('../index');

const DEVICE_ID = 'light.virtual_js_01';
const CAPABILITIES = {
  on:         { type: 'boolean' },
  brightness: { type: 'integer', minimum: 0, maximum: 255 },
};

class VirtualLightPlugin extends PluginBase {
  constructor(options = {}) {
    super({ pluginId: options.id || 'plugin.virtual_js', ...options });
    this._state = { on: false, brightness: 128 };
  }

  onConnect() {
    this.registerDevice(DEVICE_ID, 'Virtual Light (Node.js)', CAPABILITIES, 'living_room');
    this.publishAvailability(DEVICE_ID, true);
    this.publishState(DEVICE_ID, { ...this._state });
    this.publishPluginStatus('active');

    console.log(`[${this.pluginId}] Device registered — initial state:`, this._state);

    // Periodic toggle every 5 seconds.
    setInterval(() => {
      this._state.on         = !this._state.on;
      this._state.brightness = (this._state.brightness + 16) % 256;
      console.log(`[${this.pluginId}] Periodic tick — state:`, this._state);
      this.publishState(DEVICE_ID, { ...this._state });
    }, 5000);
  }

  onCommand(deviceId, payload) {
    console.log(`[${this.pluginId}] Command for ${deviceId}:`, payload);
    // Merge only known attributes to prevent arbitrary state pollution.
    for (const key of Object.keys(CAPABILITIES)) {
      if (key in payload) this._state[key] = payload[key];
    }
    this.publishState(deviceId, { ...this._state });
    console.log(`[${this.pluginId}] State after command:`, this._state);
  }
}

// ---------------------------------------------------------------------------
// CLI arg parsing (--key value)
// ---------------------------------------------------------------------------
function arg(name) {
  const i = process.argv.indexOf(`--${name}`);
  return i !== -1 ? process.argv[i + 1] : undefined;
}

const plugin = new VirtualLightPlugin({
  id:         arg('id'),
  brokerHost: arg('broker'),
  brokerPort: arg('port') ? parseInt(arg('port'), 10) : undefined,
  password:   arg('password'),
});

console.log('Virtual light plugin starting');
console.log(`  Device ID : ${DEVICE_ID}`);
console.log(`  Broker    : ${plugin.brokerHost}:${plugin.brokerPort}`);
console.log('Press Ctrl-C to stop');

const client = plugin.run();

// Publish offline on graceful shutdown.
process.on('SIGINT', () => {
  console.log('\nShutting down…');
  plugin.publishAvailability(DEVICE_ID, false);
  plugin.publishPluginStatus('offline');
  setTimeout(() => { client.end(true); process.exit(0); }, 300);
});
