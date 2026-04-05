// HomeCore Plugin SDK for .NET
//
// Provides PluginClient for connecting to the HomeCore MQTT broker,
// publishing device state, registering devices, and handling commands.
//
// Usage:
//   var client = new PluginClient(new PluginOptions { PluginId = "plugin.mydevice" });
//   client.OnCommand += (deviceId, payload) => { /* handle command */ };
//   await client.ConnectAsync();
//   await client.RegisterDeviceTypedAsync("my_device_1", "My Device", "switch");
//   await client.PublishStateAsync("my_device_1", new { on = true });
//   await client.RunAsync(); // blocks until cancellation

using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;
using Microsoft.Extensions.Logging;
using Microsoft.Extensions.Logging.Abstractions;
using MQTTnet;
using MQTTnet.Client;
using MQTTnet.Protocol;

namespace HomeCore.PluginSdk;

/// <summary>
/// Configuration options for <see cref="PluginClient"/>.
/// </summary>
public sealed class PluginOptions
{
    /// <summary>Plugin identifier used as MQTT client ID and for topic routing.</summary>
    public required string PluginId { get; init; }

    /// <summary>MQTT broker hostname. Falls back to HC_BROKER_HOST env var, then "127.0.0.1".</summary>
    public string? BrokerHost { get; init; }

    /// <summary>MQTT broker port. Falls back to HC_BROKER_PORT env var, then 1883.</summary>
    public int? BrokerPort { get; init; }

    /// <summary>MQTT password. Falls back to HC_PLUGIN_PASSWORD env var.</summary>
    public string? Password { get; init; }

    internal string EffectiveBrokerHost =>
        BrokerHost
        ?? Environment.GetEnvironmentVariable("HC_BROKER_HOST")
        ?? "127.0.0.1";

    internal int EffectiveBrokerPort =>
        BrokerPort
        ?? (int.TryParse(Environment.GetEnvironmentVariable("HC_BROKER_PORT"), out var p) ? p : 1883);

    internal string EffectivePassword =>
        Password
        ?? Environment.GetEnvironmentVariable("HC_PLUGIN_PASSWORD")
        ?? "";
}

/// <summary>
/// Management protocol options for <see cref="PluginClient.EnableManagementAsync"/>.
/// </summary>
public sealed class ManagementOptions
{
    /// <summary>Heartbeat interval in seconds (default 60).</summary>
    public int HeartbeatIntervalSecs { get; init; } = 60;

    /// <summary>Plugin version string included in heartbeats.</summary>
    public string? Version { get; init; }

    /// <summary>Path to the plugin config file (enables get_config/set_config).</summary>
    public string? ConfigPath { get; init; }
}

/// <summary>
/// Delegate for handling inbound device commands.
/// </summary>
public delegate Task CommandHandler(string deviceId, JsonElement payload);

/// <summary>
/// Delegate for handling management commands (set_log_level, custom actions).
/// </summary>
public delegate Task<JsonObject?> ManagementCommandHandler(string action, JsonElement command);

/// <summary>
/// Connected HomeCore plugin client.
/// Publishes device state, registers devices, subscribes to commands,
/// and optionally runs the management protocol (heartbeat + remote config).
/// </summary>
public sealed class PluginClient : IAsyncDisposable
{
    private readonly PluginOptions _options;
    private readonly IMqttClient _mqtt;
    private readonly ILogger _logger;
    private readonly DateTime _startedAt = DateTime.UtcNow;

    private ManagementOptions? _mgmt;
    private CancellationTokenSource? _heartbeatCts;

    /// <summary>The plugin identifier.</summary>
    public string PluginId => _options.PluginId;

    /// <summary>Fired when a device command arrives on homecore/devices/{id}/cmd.</summary>
    public event CommandHandler? OnCommand;

    /// <summary>Fired when a management command arrives that is not handled internally.
    /// Return a JsonObject response or null to use the default "unknown action" response.</summary>
    public event ManagementCommandHandler? OnManagementCommand;

    /// <summary>Fired after the MQTT connection is established.</summary>
    public event Func<Task>? OnConnected;

    public PluginClient(PluginOptions options, ILogger? logger = null)
    {
        _options = options ?? throw new ArgumentNullException(nameof(options));
        _logger = logger ?? NullLogger.Instance;

        var factory = new MqttFactory();
        _mqtt = factory.CreateMqttClient();

        _mqtt.ApplicationMessageReceivedAsync += HandleMessageAsync;
    }

    // ── Connection ────────────────────────────────────────────────────────

    /// <summary>Connect to the HomeCore MQTT broker.</summary>
    public async Task ConnectAsync(CancellationToken ct = default)
    {
        var builder = new MqttClientOptionsBuilder()
            .WithTcpServer(_options.EffectiveBrokerHost, _options.EffectiveBrokerPort)
            .WithClientId(_options.PluginId)
            .WithCleanSession(true)
            .WithKeepAlivePeriod(TimeSpan.FromSeconds(30));

        var password = _options.EffectivePassword;
        if (!string.IsNullOrEmpty(password))
            builder.WithCredentials(_options.PluginId, password);

        var mqttOptions = builder.Build();
        await _mqtt.ConnectAsync(mqttOptions, ct);
        _logger.LogInformation("Connected to HomeCore broker at {Host}:{Port}",
            _options.EffectiveBrokerHost, _options.EffectiveBrokerPort);

        // Subscribe to device commands for this plugin.
        await _mqtt.SubscribeAsync(
            new MqttTopicFilterBuilder()
                .WithTopic("homecore/devices/+/cmd")
                .WithQualityOfServiceLevel(MqttQualityOfServiceLevel.AtLeastOnce)
                .Build(),
            ct);

        if (OnConnected is not null)
            await OnConnected.Invoke();
    }

    // ── State Publishing ──────────────────────────────────────────────────

    /// <summary>Publish full device state (retained).</summary>
    public Task PublishStateAsync(string deviceId, object state, JsonObject? change = null)
    {
        var payload = WithChangeMetadata(state, change);
        return PublishAsync($"homecore/devices/{deviceId}/state", payload, retain: true);
    }

    /// <summary>Publish partial device state (JSON merge-patch, not retained).</summary>
    public Task PublishStatePartialAsync(string deviceId, object patch, JsonObject? change = null)
    {
        var payload = WithChangeMetadata(patch, change);
        return PublishAsync($"homecore/devices/{deviceId}/state/partial", payload, retain: false);
    }

    /// <summary>Publish full device state caused by an inbound command.</summary>
    public Task PublishStateForCommandAsync(
        string deviceId, object state, JsonElement commandPayload, string? fallbackSource = null)
    {
        var change = ChangeFromCommand(commandPayload, fallbackSource);
        return PublishStateAsync(deviceId, state, change);
    }

    /// <summary>Publish partial device state caused by an inbound command.</summary>
    public Task PublishStatePartialForCommandAsync(
        string deviceId, object patch, JsonElement commandPayload, string? fallbackSource = null)
    {
        var change = ChangeFromCommand(commandPayload, fallbackSource);
        return PublishStatePartialAsync(deviceId, patch, change);
    }

    // ── Availability ──────────────────────────────────────────────────────

    /// <summary>Publish device availability (retained).</summary>
    public Task PublishAvailabilityAsync(string deviceId, bool online) =>
        PublishRawAsync(
            $"homecore/devices/{deviceId}/availability",
            online ? "online" : "offline",
            retain: true);

    // ── Plugin Status ─────────────────────────────────────────────────────

    /// <summary>Publish plugin status: "active", "degraded", or "offline" (retained).</summary>
    public Task PublishPluginStatusAsync(string status) =>
        PublishRawAsync($"homecore/plugins/{PluginId}/status", status, retain: true);

    // ── Events ────────────────────────────────────────────────────────────

    /// <summary>Publish a structured event to homecore/events/{eventType}.</summary>
    public Task PublishEventAsync(string eventType, object payload) =>
        PublishAsync($"homecore/events/{eventType}", payload, retain: false);

    // ── Device Registration ───────────────────────────────────────────────

    /// <summary>Register a device with a JSON capability schema.</summary>
    public Task RegisterDeviceAsync(string deviceId, string name, object capabilities, string? area = null)
    {
        var payload = new JsonObject
        {
            ["device_id"] = deviceId,
            ["plugin_id"] = PluginId,
            ["name"] = name,
            ["capabilities"] = JsonSerializer.SerializeToNode(capabilities),
        };
        if (area is not null) payload["area"] = area;
        return PublishAsync($"homecore/plugins/{PluginId}/register", payload, retain: false);
    }

    /// <summary>Register a device by type name (HomeCore resolves capabilities from catalog).</summary>
    public Task RegisterDeviceTypedAsync(
        string deviceId, string name, string deviceType, string? area = null)
    {
        var payload = new JsonObject
        {
            ["device_id"] = deviceId,
            ["plugin_id"] = PluginId,
            ["name"] = name,
            ["device_type"] = deviceType,
        };
        if (area is not null) payload["area"] = area;
        return PublishAsync($"homecore/plugins/{PluginId}/register", payload, retain: false);
    }

    /// <summary>Register a device with all optional fields.</summary>
    public Task RegisterDeviceFullAsync(
        string deviceId, string name,
        string? deviceType = null, string? area = null, object? capabilities = null)
    {
        var payload = new JsonObject
        {
            ["device_id"] = deviceId,
            ["plugin_id"] = PluginId,
            ["name"] = name,
        };
        if (deviceType is not null) payload["device_type"] = deviceType;
        if (area is not null) payload["area"] = area;
        if (capabilities is not null) payload["capabilities"] = JsonSerializer.SerializeToNode(capabilities);
        return PublishAsync($"homecore/plugins/{PluginId}/register", payload, retain: false);
    }

    /// <summary>Publish a device capability schema (retained).</summary>
    public Task RegisterDeviceSchemaAsync(string deviceId, object schema) =>
        PublishAsync($"homecore/devices/{deviceId}/schema", schema, retain: true);

    /// <summary>Subscribe to command messages for a specific device.</summary>
    public Task SubscribeCommandsAsync(string deviceId) =>
        _mqtt.SubscribeAsync(
            new MqttTopicFilterBuilder()
                .WithTopic($"homecore/devices/{deviceId}/cmd")
                .WithQualityOfServiceLevel(MqttQualityOfServiceLevel.AtLeastOnce)
                .Build());

    /// <summary>Unregister a device: clear retained topics and publish unregister message.</summary>
    public async Task UnregisterDeviceAsync(string deviceId)
    {
        await ClearRetainedAsync($"homecore/devices/{deviceId}/state");
        await ClearRetainedAsync($"homecore/devices/{deviceId}/availability");
        await ClearRetainedAsync($"homecore/devices/{deviceId}/schema");
        await PublishAsync(
            $"homecore/plugins/{PluginId}/unregister",
            new { device_id = deviceId },
            retain: false);
    }

    // ── Management Protocol ───────────────────────────────────────────────

    /// <summary>
    /// Enable the management protocol: heartbeat publisher + command listener
    /// for get_config, set_config, set_log_level, and ping.
    /// Call after ConnectAsync, before RunAsync.
    /// </summary>
    public async Task EnableManagementAsync(ManagementOptions options, CancellationToken ct = default)
    {
        _mgmt = options;

        // Subscribe to management command topic.
        await _mqtt.SubscribeAsync(
            new MqttTopicFilterBuilder()
                .WithTopic($"homecore/plugins/{PluginId}/manage/cmd")
                .WithQualityOfServiceLevel(MqttQualityOfServiceLevel.AtLeastOnce)
                .Build(),
            ct);

        // Start heartbeat publisher.
        _heartbeatCts = CancellationTokenSource.CreateLinkedTokenSource(ct);
        _ = Task.Run(() => HeartbeatLoopAsync(options, _heartbeatCts.Token), _heartbeatCts.Token);

        _logger.LogInformation(
            "Management protocol enabled (heartbeat every {Interval}s)",
            options.HeartbeatIntervalSecs);
    }

    // ── Event Loop ────────────────────────────────────────────────────────

    /// <summary>
    /// Block until cancellation. The MQTT client handles messages via events.
    /// </summary>
    public async Task RunAsync(CancellationToken ct = default)
    {
        _logger.LogInformation("Plugin {PluginId} event loop running", PluginId);
        try
        {
            await Task.Delay(Timeout.Infinite, ct);
        }
        catch (OperationCanceledException)
        {
            _logger.LogInformation("Plugin {PluginId} shutting down", PluginId);
        }
    }

    public async ValueTask DisposeAsync()
    {
        _heartbeatCts?.Cancel();
        if (_mqtt.IsConnected)
            await _mqtt.DisconnectAsync();
        _mqtt.Dispose();
    }

    // ── Internal: Message Routing ─────────────────────────────────────────

    private async Task HandleMessageAsync(MqttApplicationMessageReceivedEventArgs e)
    {
        var topic = e.ApplicationMessage.Topic;
        var parts = topic.Split('/');

        // homecore/devices/{id}/cmd
        if (parts.Length == 4
            && parts[0] == "homecore"
            && parts[1] == "devices"
            && parts[3] == "cmd")
        {
            var deviceId = parts[2];
            JsonElement payload;
            try
            {
                var bytes = e.ApplicationMessage.PayloadSegment;
                payload = JsonDocument.Parse(bytes).RootElement;
            }
            catch
            {
                var raw = Encoding.UTF8.GetString(e.ApplicationMessage.PayloadSegment);
                payload = JsonDocument.Parse($"{{\"raw\":\"{Escape(raw)}\"}}").RootElement;
            }

            if (OnCommand is not null)
            {
                try { await OnCommand.Invoke(deviceId, payload); }
                catch (Exception ex)
                {
                    _logger.LogWarning(ex, "Command handler failed for {DeviceId}", deviceId);
                }
            }
            return;
        }

        // homecore/plugins/{id}/manage/cmd
        if (_mgmt is not null
            && parts.Length == 5
            && parts[0] == "homecore"
            && parts[1] == "plugins"
            && parts[3] == "manage"
            && parts[4] == "cmd")
        {
            try
            {
                var bytes = e.ApplicationMessage.PayloadSegment;
                var cmd = JsonDocument.Parse(bytes).RootElement;
                var response = await HandleManagementCommandAsync(cmd);
                await PublishAsync(
                    $"homecore/plugins/{PluginId}/manage/response",
                    response,
                    retain: false);
            }
            catch (Exception ex)
            {
                _logger.LogWarning(ex, "Management command handling failed");
            }
        }
    }

    // ── Internal: Management ──────────────────────────────────────────────

    private async Task<JsonObject> HandleManagementCommandAsync(JsonElement cmd)
    {
        var action = cmd.TryGetProperty("action", out var a) ? a.GetString() ?? "" : "";
        var requestId = cmd.TryGetProperty("request_id", out var r) ? r.GetString() ?? "" : "";

        switch (action)
        {
            case "ping":
                return new JsonObject
                {
                    ["request_id"] = requestId,
                    ["status"] = "ok",
                };

            case "get_config":
                if (_mgmt?.ConfigPath is { } path)
                {
                    try
                    {
                        var content = await File.ReadAllTextAsync(path);
                        return new JsonObject
                        {
                            ["request_id"] = requestId,
                            ["status"] = "ok",
                            ["data"] = content,
                        };
                    }
                    catch (Exception ex)
                    {
                        return ErrorResponse(requestId, $"failed to read config: {ex.Message}");
                    }
                }
                return ErrorResponse(requestId, "no config path configured");

            case "set_config":
                if (_mgmt?.ConfigPath is { } setPath)
                {
                    if (cmd.TryGetProperty("config", out var cfg))
                    {
                        try
                        {
                            var configStr = cfg.ValueKind == JsonValueKind.String
                                ? cfg.GetString()!
                                : cfg.GetRawText();
                            await File.WriteAllTextAsync(setPath, configStr);
                            return new JsonObject
                            {
                                ["request_id"] = requestId,
                                ["status"] = "ok",
                            };
                        }
                        catch (Exception ex)
                        {
                            return ErrorResponse(requestId, $"failed to write config: {ex.Message}");
                        }
                    }
                    return ErrorResponse(requestId, "missing 'config' field");
                }
                return ErrorResponse(requestId, "no config path configured");

            case "set_log_level":
                var level = cmd.TryGetProperty("level", out var lv) ? lv.GetString() ?? "info" : "info";
                _logger.LogInformation("Management: log level change requested to {Level}", level);
                return new JsonObject
                {
                    ["request_id"] = requestId,
                    ["status"] = "ok",
                    ["note"] = "log level change acknowledged",
                };

            default:
                // Delegate to user handler if registered.
                if (OnManagementCommand is not null)
                {
                    var result = await OnManagementCommand.Invoke(action, cmd);
                    if (result is not null) return result;
                }
                return ErrorResponse(requestId, $"unknown action: {action}");
        }
    }

    private async Task HeartbeatLoopAsync(ManagementOptions options, CancellationToken ct)
    {
        using var timer = new PeriodicTimer(TimeSpan.FromSeconds(options.HeartbeatIntervalSecs));
        while (await timer.WaitForNextTickAsync(ct))
        {
            var uptime = (long)(DateTime.UtcNow - _startedAt).TotalSeconds;
            var payload = new JsonObject
            {
                ["timestamp"] = DateTime.UtcNow.ToString("o"),
                ["version"] = options.Version,
                ["uptime_secs"] = uptime,
            };
            await PublishAsync($"homecore/plugins/{PluginId}/heartbeat", payload, retain: false);
        }
    }

    // ── Internal: Publish Helpers ─────────────────────────────────────────

    private Task PublishAsync(string topic, object payload, bool retain)
    {
        var json = JsonSerializer.Serialize(payload);
        return PublishRawAsync(topic, json, retain);
    }

    private async Task PublishRawAsync(string topic, string payload, bool retain)
    {
        var msg = new MqttApplicationMessageBuilder()
            .WithTopic(topic)
            .WithPayload(Encoding.UTF8.GetBytes(payload))
            .WithQualityOfServiceLevel(MqttQualityOfServiceLevel.AtLeastOnce)
            .WithRetainFlag(retain)
            .Build();
        await _mqtt.PublishAsync(msg);
    }

    private Task ClearRetainedAsync(string topic) =>
        PublishRawAsync(topic, "", retain: true);

    // ── Internal: Change Metadata ─────────────────────────────────────────

    private object WithChangeMetadata(object payload, JsonObject? change)
    {
        if (change is null) return payload;

        // Serialize payload to JsonNode, inject _hc.change, return.
        var node = JsonSerializer.SerializeToNode(payload);
        if (node is JsonObject obj)
        {
            var hc = new JsonObject { ["change"] = JsonNode.Parse(change.ToJsonString()) };
            obj["_hc"] = hc;
            return obj;
        }
        return payload;
    }

    /// <summary>Extract _hc.command metadata from an inbound command payload.</summary>
    public static JsonObject? ExtractCommandChange(JsonElement commandPayload)
    {
        if (commandPayload.TryGetProperty("_hc", out var hc)
            && hc.TryGetProperty("command", out var cmd)
            && cmd.ValueKind == JsonValueKind.Object)
        {
            return JsonNode.Parse(cmd.GetRawText()) as JsonObject;
        }
        return null;
    }

    /// <summary>Build a change metadata object from a command payload.</summary>
    public JsonObject ChangeFromCommand(JsonElement commandPayload, string? fallbackSource = null)
    {
        var extracted = ExtractCommandChange(commandPayload);
        if (extracted is not null) return extracted;

        return new JsonObject
        {
            ["changed_at"] = DateTime.UtcNow.ToString("o"),
            ["kind"] = "homecore",
            ["source"] = fallbackSource ?? PluginId,
        };
    }

    private static JsonObject ErrorResponse(string requestId, string error) =>
        new()
        {
            ["request_id"] = requestId,
            ["status"] = "error",
            ["error"] = error,
        };

    private static string Escape(string s) =>
        s.Replace("\\", "\\\\").Replace("\"", "\\\"");
}
