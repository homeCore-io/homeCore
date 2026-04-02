'use strict';

/**
 * homecore-plugin-sdk — Node.js SDK for HomeCore device plugins.
 *
 * @example
 * const { PluginBase } = require('homecore-plugin-sdk');
 *
 * class MyLightPlugin extends PluginBase {
 *   constructor() {
 *     super({ pluginId: 'plugin.my_light' });
 *   }
 *
 *   onCommand(deviceId, payload) {
 *     console.log(`Command for ${deviceId}:`, payload);
 *     this.publishState(deviceId, { on: payload.on ?? false });
 *   }
 *
 *   onConnect() {
 *     this.registerDevice('light.01', 'My Light', {
 *       on: { type: 'boolean' },
 *       brightness: { type: 'integer', minimum: 0, maximum: 255 },
 *     });
 *   }
 * }
 *
 * new MyLightPlugin().run();
 */

const mqtt = require('mqtt');

/**
 * Base class for HomeCore plugins written in Node.js.
 *
 * Subclasses must override {@link PluginBase#onCommand} and optionally
 * {@link PluginBase#onConnect}.  Call {@link PluginBase#run} to connect.
 */
class PluginBase {
  /**
   * @param {object} options
   * @param {string} options.pluginId     - Unique plugin identifier.
   * @param {string} [options.brokerHost] - MQTT broker host (default: 127.0.0.1).
   * @param {number} [options.brokerPort] - MQTT broker port (default: 1883).
   * @param {string} [options.password]   - MQTT password for this plugin.
   */
  constructor({
    pluginId,
    brokerHost = process.env.HC_BROKER_HOST || '127.0.0.1',
    brokerPort = parseInt(process.env.HC_BROKER_PORT || '1883', 10),
    password   = process.env.HC_PLUGIN_PASSWORD || '',
  } = {}) {
    if (!pluginId) throw new Error('pluginId is required');
    this.pluginId   = pluginId;
    this.brokerHost = brokerHost;
    this.brokerPort = brokerPort;
    this.password   = password;
    /** @type {import('mqtt').MqttClient|null} */
    this._client = null;
  }

  // ---------------------------------------------------------------------------
  // Public API
  // ---------------------------------------------------------------------------

  /**
   * Publish a device state update (retained, QoS 1).
   *
   * @param {string} deviceId - Canonical HomeCore device identifier.
   * @param {object} state    - Attribute map to publish.
   * @param {object} [options] - Optional publish metadata.
   * @param {object} [options.change] - `_hc.change` payload describing provenance.
   */
  publishState(deviceId, state, options = {}) {
    const topic   = `homecore/devices/${deviceId}/state`;
    const payload = JSON.stringify(this._withStateChangeMetadata(state, options.change));
    this._publish(topic, payload, { retain: true, qos: 1 });
  }

  /**
   * Publish a device registration payload.
   *
   * @param {string} deviceId       - Stable unique device identifier.
   * @param {string} name           - Human-readable label.
   * @param {object} capabilities   - JSON Schema object for device attributes.
   * @param {string|null} [area]    - Optional room/zone assignment.
   */
  registerDevice(deviceId, name, capabilities, area = null) {
    const topic   = `homecore/plugins/${this.pluginId}/register`;
    const payload = JSON.stringify({ device_id: deviceId, plugin_id: this.pluginId, name, area, capabilities });
    this._publish(topic, payload, { qos: 1 });
  }

  /**
   * Register a device by type name.
   *
   * Instead of providing a full capability schema, supply a `deviceType` string
   * that HomeCore resolves against its built-in device-type catalog.  This is the
   * recommended registration path for well-known device categories.
   *
   * Example types: `"light"`, `"switch"`, `"motion_sensor"`, `"contact_sensor"`,
   * `"temperature_sensor"`, `"power_monitor"`, `"cover"`, `"lock"`,
   * `"climate"`, `"virtual_switch"`, …
   *
   * @param {string}      deviceId   - Stable unique device identifier.
   * @param {string}      name       - Human-readable label.
   * @param {string}      deviceType - Type name from the device-type catalog.
   * @param {string|null} [area]     - Optional room/zone assignment.
   */
  registerDeviceTyped(deviceId, name, deviceType, area = null) {
    const topic   = `homecore/plugins/${this.pluginId}/register`;
    const payload = JSON.stringify({ device_id: deviceId, plugin_id: this.pluginId, name, area, device_type: deviceType });
    this._publish(topic, payload, { qos: 1 });
  }

  /**
   * Retire a device from HomeCore by clearing retained topics and publishing
   * a plugin-scoped unregister message.
   *
   * @param {string} deviceId - Stable unique device identifier.
   */
  unregisterDevice(deviceId) {
    this._publish(`homecore/devices/${deviceId}/state`, '', { retain: true, qos: 1 });
    this._publish(`homecore/devices/${deviceId}/availability`, '', { retain: true, qos: 1 });
    this._publish(`homecore/devices/${deviceId}/schema`, '', { retain: true, qos: 1 });
    const topic = `homecore/plugins/${this.pluginId}/unregister`;
    const payload = JSON.stringify({ device_id: deviceId });
    this._publish(topic, payload, { qos: 1 });
  }

  /**
   * Publish a partial state update (JSON merge-patch, QoS 1, not retained).
   *
   * Use this for high-frequency sensors that send diffs rather than full state.
   *
   * @param {string} deviceId - Canonical HomeCore device identifier.
   * @param {object} patch    - Attributes to merge into the current state.
   * @param {object} [options] - Optional publish metadata.
   * @param {object} [options.change] - `_hc.change` payload describing provenance.
   */
  publishStatePartial(deviceId, patch, options = {}) {
    const topic   = `homecore/devices/${deviceId}/state/partial`;
    const payload = JSON.stringify(this._withStateChangeMetadata(patch, options.change));
    this._publish(topic, payload, { retain: false, qos: 1 });
  }

  /**
   * Publish a state update that is the direct result of a HomeCore command.
   *
   * This preserves `_hc.command` metadata as `_hc.change` so rules can tell the
   * resulting state update came from HomeCore.
   *
   * @param {string} deviceId - Canonical HomeCore device identifier.
   * @param {object} state - Attribute map to publish.
   * @param {object} commandPayload - Original HomeCore command payload.
   * @param {string} [fallbackSource] - Source label if the command had no metadata.
   */
  publishStateForCommand(deviceId, state, commandPayload, fallbackSource = this.pluginId) {
    this.publishState(deviceId, state, {
      change: this.changeFromCommand(commandPayload, fallbackSource),
    });
  }

  /**
   * Publish a partial state update that is the direct result of a HomeCore command.
   *
   * @param {string} deviceId - Canonical HomeCore device identifier.
   * @param {object} patch - Merge-patch payload to publish.
   * @param {object} commandPayload - Original HomeCore command payload.
   * @param {string} [fallbackSource] - Source label if the command had no metadata.
   */
  publishStatePartialForCommand(deviceId, patch, commandPayload, fallbackSource = this.pluginId) {
    this.publishStatePartial(deviceId, patch, {
      change: this.changeFromCommand(commandPayload, fallbackSource),
    });
  }

  /**
   * Publish an availability heartbeat (retained, QoS 1).
   *
   * @param {string}  deviceId  - Target device.
   * @param {boolean} available - `true` for online, `false` for offline.
   */
  publishAvailability(deviceId, available) {
    const topic   = `homecore/devices/${deviceId}/availability`;
    const payload = available ? 'online' : 'offline';
    this._publish(topic, payload, { retain: true, qos: 1 });
  }

  /**
   * Publish plugin status to ``homecore/plugins/{pluginId}/status`` (retained).
   *
   * @param {string} status - One of ``"active"``, ``"degraded"``, ``"offline"``.
   */
  publishPluginStatus(status) {
    const topic = `homecore/plugins/${this.pluginId}/status`;
    this._publish(topic, status, { retain: true, qos: 1 });
  }

  // ---------------------------------------------------------------------------
  // Subclass hooks
  // ---------------------------------------------------------------------------

  /**
   * Called when a command message arrives for one of this plugin's devices.
   * Subclasses must implement this method.
   *
   * @param {string} deviceId - The target device.
   * @param {object} payload  - Decoded JSON command payload.
   */
  // eslint-disable-next-line no-unused-vars
  onCommand(deviceId, payload) {
    throw new Error(`${this.constructor.name} must implement onCommand(deviceId, payload)`);
  }

  /**
   * Called after the broker connection is established.
   * Override to register devices and subscribe to additional topics.
   */
  onConnect() {}

  /**
   * Extract a HomeCore change record from an inbound command payload.
   *
   * @param {object} commandPayload - Decoded JSON command payload.
   * @returns {object|null}
   */
  extractCommandChange(commandPayload) {
    if (!commandPayload || typeof commandPayload !== 'object' || Array.isArray(commandPayload)) {
      return null;
    }
    const hc = commandPayload._hc;
    if (!hc || typeof hc !== 'object' || Array.isArray(hc)) {
      return null;
    }
    const change = hc.command;
    if (!change || typeof change !== 'object' || Array.isArray(change)) {
      return null;
    }
    return { ...change };
  }

  /**
   * Resolve a command payload into a concrete HomeCore-originated change record.
   *
   * @param {object} commandPayload - Decoded JSON command payload.
   * @param {string} [fallbackSource] - Source label if the command had no metadata.
   * @returns {object}
   */
  changeFromCommand(commandPayload, fallbackSource = this.pluginId) {
    return this.extractCommandChange(commandPayload) || {
      changed_at: new Date().toISOString(),
      kind: 'homecore',
      source: fallbackSource,
    };
  }

  // ---------------------------------------------------------------------------
  // Lifecycle
  // ---------------------------------------------------------------------------

  /**
   * Connect to the broker and start the event loop.
   * Returns the underlying mqtt.js client for advanced use.
   *
   * @returns {import('mqtt').MqttClient}
   */
  run() {
    const url = `mqtt://${this.brokerHost}:${this.brokerPort}`;
    const opts = {
      clientId: this.pluginId,
      clean: true,
      ...(this.password ? { username: this.pluginId, password: this.password } : {}),
    };

    this._client = mqtt.connect(url, opts);

    this._client.on('connect', () => {
      console.log(`[${this.pluginId}] Connected to ${url}`);
      // Subscribe to commands for all devices managed by this plugin
      this._client.subscribe('homecore/devices/+/cmd', { qos: 1 });
      this.onConnect();
    });

    this._client.on('message', (topic, message) => {
      const parts = topic.split('/');
      // Route homecore/devices/{deviceId}/cmd → onCommand
      if (parts.length === 4 && parts[0] === 'homecore' && parts[1] === 'devices' && parts[3] === 'cmd') {
        const deviceId = parts[2];
        let payload;
        try {
          payload = JSON.parse(message.toString());
        } catch {
          payload = { raw: message.toString() };
        }
        this.onCommand(deviceId, payload);
      }
    });

    this._client.on('error', (err) => {
      console.error(`[${this.pluginId}] MQTT error:`, err.message);
    });

    this._client.on('reconnect', () => {
      console.log(`[${this.pluginId}] Reconnecting…`);
    });

    return this._client;
  }

  // ---------------------------------------------------------------------------
  // Private helpers
  // ---------------------------------------------------------------------------

  _publish(topic, payload, opts = {}) {
    if (!this._client) {
      console.warn(`[${this.pluginId}] publish called before run(): topic=${topic}`);
      return;
    }
    this._client.publish(topic, payload, opts, (err) => {
      if (err) console.error(`[${this.pluginId}] publish error on ${topic}:`, err.message);
    });
  }

  _withStateChangeMetadata(payload, change) {
    if (!change || !payload || typeof payload !== 'object' || Array.isArray(payload)) {
      return payload;
    }
    const nextPayload = { ...payload };
    const nextHc = nextPayload._hc && typeof nextPayload._hc === 'object' && !Array.isArray(nextPayload._hc)
      ? { ...nextPayload._hc }
      : {};
    nextHc.change = { ...change };
    nextPayload._hc = nextHc;
    return nextPayload;
  }
}

module.exports = { PluginBase };
