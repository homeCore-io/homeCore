'use strict';

jest.mock('mqtt');

const mqtt = require('mqtt');
const { PluginBase } = require('../index');

// Concrete plugin for testing
class TestPlugin extends PluginBase {
  constructor(options) {
    super({ pluginId: 'plugin.test', ...options });
    this.commands = [];
    this.connectCalled = false;
  }

  onConnect() {
    this.connectCalled = true;
  }

  onCommand(deviceId, payload) {
    this.commands.push({ deviceId, payload });
  }
}

describe('PluginBase', () => {
  let mockClient;

  beforeEach(() => {
    mockClient = {
      on:        jest.fn(),
      subscribe: jest.fn(),
      publish:   jest.fn(),
    };
    mqtt.connect.mockReturnValue(mockClient);
  });

  afterEach(() => {
    jest.clearAllMocks();
  });

  // ---------------------------------------------------------------------------
  // Connection
  // ---------------------------------------------------------------------------

  test('run() connects to the broker with correct URL and clientId', () => {
    const plugin = new TestPlugin();
    plugin.run();
    expect(mqtt.connect).toHaveBeenCalledWith(
      'mqtt://127.0.0.1:1883',
      expect.objectContaining({ clientId: 'plugin.test', clean: true }),
    );
  });

  test('run() sets credentials when password provided', () => {
    const plugin = new TestPlugin({ password: 'secret' });
    plugin.run();
    expect(mqtt.connect).toHaveBeenCalledWith(
      expect.any(String),
      expect.objectContaining({ username: 'plugin.test', password: 'secret' }),
    );
  });

  test('run() omits credentials when no password', () => {
    const plugin = new TestPlugin();
    plugin.run();
    const opts = mqtt.connect.mock.calls[0][1];
    expect(opts).not.toHaveProperty('username');
    expect(opts).not.toHaveProperty('password');
  });

  test('run() subscribes to cmd wildcard on connect', () => {
    const plugin = new TestPlugin();
    plugin.run();
    const connectHandler = mockClient.on.mock.calls.find(([e]) => e === 'connect')[1];
    connectHandler();
    expect(mockClient.subscribe).toHaveBeenCalledWith(
      'homecore/devices/+/cmd',
      { qos: 1 },
    );
  });

  test('run() calls onConnect() after broker connection', () => {
    const plugin = new TestPlugin();
    plugin.run();
    expect(plugin.connectCalled).toBe(false);
    const connectHandler = mockClient.on.mock.calls.find(([e]) => e === 'connect')[1];
    connectHandler();
    expect(plugin.connectCalled).toBe(true);
  });

  // ---------------------------------------------------------------------------
  // Publish methods
  // ---------------------------------------------------------------------------

  test('publishState sends retained QoS 1 message', () => {
    const plugin = new TestPlugin();
    plugin.run();
    plugin.publishState('light.01', { on: true, brightness: 200 });
    expect(mockClient.publish).toHaveBeenCalledWith(
      'homecore/devices/light.01/state',
      JSON.stringify({ on: true, brightness: 200 }),
      { retain: true, qos: 1 },
      expect.any(Function),
    );
  });

  test('publishStatePartial sends non-retained QoS 1 message', () => {
    const plugin = new TestPlugin();
    plugin.run();
    plugin.publishStatePartial('light.01', { brightness: 100 });
    expect(mockClient.publish).toHaveBeenCalledWith(
      'homecore/devices/light.01/state/partial',
      JSON.stringify({ brightness: 100 }),
      { retain: false, qos: 1 },
      expect.any(Function),
    );
  });

  test('registerDevice publishes to plugin register topic', () => {
    const plugin = new TestPlugin();
    plugin.run();
    const caps = { on: { type: 'boolean' } };
    plugin.registerDevice('light.01', 'Test Light', caps, 'living_room');

    expect(mockClient.publish).toHaveBeenCalledTimes(1);
    const [topic, payloadStr, opts] = mockClient.publish.mock.calls[0];
    const payload = JSON.parse(payloadStr);

    expect(topic).toBe('homecore/plugins/plugin.test/register');
    expect(opts).toEqual({ qos: 1 });
    expect(payload.device_id).toBe('light.01');
    expect(payload.plugin_id).toBe('plugin.test');
    expect(payload.name).toBe('Test Light');
    expect(payload.area).toBe('living_room');
    expect(payload.capabilities).toEqual(caps);
  });

  test('registerDevice uses null area when omitted', () => {
    const plugin = new TestPlugin();
    plugin.run();
    plugin.registerDevice('light.01', 'Test Light', {});
    const payload = JSON.parse(mockClient.publish.mock.calls[0][1]);
    expect(payload.area).toBeNull();
  });

  test('publishAvailability sends "online" when available=true', () => {
    const plugin = new TestPlugin();
    plugin.run();
    plugin.publishAvailability('light.01', true);
    expect(mockClient.publish).toHaveBeenCalledWith(
      'homecore/devices/light.01/availability',
      'online',
      { retain: true, qos: 1 },
      expect.any(Function),
    );
  });

  test('publishAvailability sends "offline" when available=false', () => {
    const plugin = new TestPlugin();
    plugin.run();
    plugin.publishAvailability('light.01', false);
    expect(mockClient.publish).toHaveBeenCalledWith(
      'homecore/devices/light.01/availability',
      'offline',
      { retain: true, qos: 1 },
      expect.any(Function),
    );
  });

  test('publishPluginStatus publishes to plugin status topic', () => {
    const plugin = new TestPlugin();
    plugin.run();
    plugin.publishPluginStatus('active');
    expect(mockClient.publish).toHaveBeenCalledWith(
      'homecore/plugins/plugin.test/status',
      'active',
      { retain: true, qos: 1 },
      expect.any(Function),
    );
  });

  // ---------------------------------------------------------------------------
  // Command routing
  // ---------------------------------------------------------------------------

  test('onCommand is called when a cmd message arrives', () => {
    const plugin = new TestPlugin();
    plugin.run();
    const messageHandler = mockClient.on.mock.calls.find(([e]) => e === 'message')[1];
    messageHandler(
      'homecore/devices/light.01/cmd',
      Buffer.from(JSON.stringify({ on: true })),
    );
    expect(plugin.commands).toHaveLength(1);
    expect(plugin.commands[0]).toEqual({ deviceId: 'light.01', payload: { on: true } });
  });

  test('invalid JSON in cmd payload is handled gracefully', () => {
    const plugin = new TestPlugin();
    plugin.run();
    const messageHandler = mockClient.on.mock.calls.find(([e]) => e === 'message')[1];
    messageHandler('homecore/devices/light.01/cmd', Buffer.from('not-json'));
    expect(plugin.commands).toHaveLength(1);
    expect(plugin.commands[0].payload).toHaveProperty('raw');
  });

  test('non-cmd topics are silently ignored', () => {
    const plugin = new TestPlugin();
    plugin.run();
    const messageHandler = mockClient.on.mock.calls.find(([e]) => e === 'message')[1];
    messageHandler('homecore/devices/light.01/state', Buffer.from('{}'));
    expect(plugin.commands).toHaveLength(0);
  });

  test('messages with wrong prefix are ignored', () => {
    const plugin = new TestPlugin();
    plugin.run();
    const messageHandler = mockClient.on.mock.calls.find(([e]) => e === 'message')[1];
    messageHandler('other/devices/light.01/cmd', Buffer.from('{}'));
    expect(plugin.commands).toHaveLength(0);
  });

  // ---------------------------------------------------------------------------
  // Safety checks
  // ---------------------------------------------------------------------------

  test('publish before run() logs a warning without throwing', () => {
    const plugin = new TestPlugin();
    const warn = jest.spyOn(console, 'warn').mockImplementation(() => {});
    plugin.publishState('light.01', { on: true });
    expect(warn).toHaveBeenCalled();
    warn.mockRestore();
  });

  test('onCommand throws if not overridden', () => {
    class BrokenPlugin extends PluginBase {}
    const p = new BrokenPlugin({ pluginId: 'plugin.broken' });
    expect(() => p.onCommand('d', {})).toThrow('must implement onCommand');
  });

  test('missing pluginId throws in constructor', () => {
    expect(() => new TestPlugin({ pluginId: undefined })).toThrow('pluginId is required');
  });

  // ---------------------------------------------------------------------------
  // Configuration
  // ---------------------------------------------------------------------------

  test('uses environment variables for broker config', () => {
    process.env.HC_BROKER_HOST = '10.0.0.1';
    process.env.HC_BROKER_PORT = '1884';
    try {
      const plugin = new TestPlugin({ pluginId: 'plugin.test' });
      expect(plugin.brokerHost).toBe('10.0.0.1');
      expect(plugin.brokerPort).toBe(1884);
    } finally {
      delete process.env.HC_BROKER_HOST;
      delete process.env.HC_BROKER_PORT;
    }
  });

  test('explicit params override env vars', () => {
    process.env.HC_BROKER_HOST = '10.0.0.1';
    try {
      const plugin = new TestPlugin({ brokerHost: '192.168.0.1' });
      expect(plugin.brokerHost).toBe('192.168.0.1');
    } finally {
      delete process.env.HC_BROKER_HOST;
    }
  });

  test('run() returns the mqtt client', () => {
    const plugin = new TestPlugin();
    const result = plugin.run();
    expect(result).toBe(mockClient);
  });
});
